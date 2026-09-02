use std::{
    fs::Metadata,
    path::{Path, PathBuf},
    str::FromStr,
};

#[cfg(not(any(unix, windows)))]
use std::time::SystemTime;

#[cfg(test)]
use std::sync::{Mutex, OnceLock};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, ToSql, Transaction, params, params_from_iter,
    types::Type,
};

use super::{EntryId, ProjectIdentity, SessionDomain, SessionHeader, SessionId};

mod projection;
mod recovery;
mod schema;
mod types;

pub(crate) use projection::{CatalogRepairProjection, SessionProjection, project_session_summary};
use projection::{
    MessageNodeProjection, rebuild_projects_in_transaction, refresh_project_in_transaction,
    role_from_name, summary_from_row, write_projection_in_transaction,
};
use recovery::clear_replacement_marker;
pub use types::{
    CatalogCursor, CatalogError, CatalogPage, CatalogQuery, ProjectCatalogCursor,
    ProjectCatalogPage, ProjectCatalogQuery, ProjectSummary, RepairReport, SessionSummary,
};
pub(crate) use types::{MessageNodeRow, ProjectionIntent};

pub(crate) const CATALOG_SCHEMA_VERSION: i64 = 7;
pub(crate) const DEFAULT_PAGE_SIZE: usize = 30;
const REPAIR_REQUIRED_KEY: &str = "repair_required";
const PROJECTION_INTENT_PREFIX: &str = "projection_intent:";

#[derive(Debug)]
pub(crate) struct Catalog {
    path: PathBuf,
    domain: SessionDomain,
    connection: Connection,
    identity: CatalogFileIdentity,
    needs_repair: bool,
    #[cfg(test)]
    full_projection_writes: u64,
    #[cfg(test)]
    incremental_projection_writes: u64,
}

impl Catalog {
    pub(crate) fn needs_repair(&self) -> bool {
        self.needs_repair
    }

    pub(crate) fn mark_repair_required(&mut self) -> Result<(), CatalogError> {
        self.reopen_if_replaced()?;
        self.mark_repair_required_on_current_file()?;
        if self.reopen_if_replaced()? && self.reopen_if_replaced()? {
            return Err(CatalogError::ReplacedDuringOperation);
        }
        Ok(())
    }

    fn mark_repair_required_on_current_file(&mut self) -> Result<(), CatalogError> {
        self.connection.execute(
            "INSERT INTO repair_state (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![REPAIR_REQUIRED_KEY],
        )?;
        self.needs_repair = true;
        Ok(())
    }

    pub(crate) fn refresh_after_external_replacement(&mut self) -> Result<bool, CatalogError> {
        self.reopen_if_replaced()
    }

    fn reopen_if_replaced(&mut self) -> Result<bool, CatalogError> {
        if catalog_file_identity(&self.path).is_ok_and(|identity| identity == self.identity) {
            return Ok(false);
        }
        let mut reopened = Self::open(self.path.clone(), self.domain)?;
        // A replacement can be a healthy but stale disposable projection.
        // Re-arm a full source scan before switching connections so facts from
        // other sessions cannot disappear behind the externally swapped file.
        reopened.mark_repair_required_on_current_file()?;
        *self = reopened;
        Ok(true)
    }

    pub(crate) fn begin_projection_intent(
        &mut self,
        session_id: &SessionId,
    ) -> Result<ProjectionIntent, CatalogError> {
        let intent = ProjectionIntent::new(session_id);
        self.reopen_if_replaced()?;
        self.insert_projection_intent(&intent, session_id)?;
        if self.reopen_if_replaced()? {
            self.insert_projection_intent(&intent, session_id)?;
            if self.reopen_if_replaced()? {
                return Err(CatalogError::ReplacedDuringOperation);
            }
        }
        self.needs_repair = true;
        Ok(intent)
    }

    fn insert_projection_intent(
        &self,
        intent: &ProjectionIntent,
        session_id: &SessionId,
    ) -> Result<(), CatalogError> {
        self.connection.execute(
            "INSERT INTO repair_state (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![&intent.key, session_id.to_string()],
        )?;
        Ok(())
    }

    pub(crate) fn cancel_projection_intent(
        &mut self,
        intent: &ProjectionIntent,
    ) -> Result<(), CatalogError> {
        self.reopen_if_replaced()?;
        self.delete_projection_intent(intent)?;
        if self.reopen_if_replaced()? {
            self.delete_projection_intent(intent)?;
            if self.reopen_if_replaced()? {
                return Err(CatalogError::ReplacedDuringOperation);
            }
        }
        self.refresh_needs_repair()
    }

    fn delete_projection_intent(&self, intent: &ProjectionIntent) -> Result<(), CatalogError> {
        self.connection.execute(
            "DELETE FROM repair_state WHERE key = ?1",
            params![&intent.key],
        )?;
        Ok(())
    }

    fn refresh_needs_repair(&mut self) -> Result<(), CatalogError> {
        self.needs_repair = query_needs_repair(&self.connection)?;
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn projection_write_counts(&self) -> (u64, u64) {
        (
            self.full_projection_writes,
            self.incremental_projection_writes,
        )
    }

    #[cfg(test)]
    pub(crate) fn synchronous_level(&self) -> Result<i64, CatalogError> {
        self.connection
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .map_err(CatalogError::from)
    }

    pub(crate) fn upsert_projection_with_intents(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        jsonl_path: &Path,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        self.write_projection(
            header,
            projection,
            &projection.messages,
            jsonl_path,
            true,
            intents,
        )
    }

    pub(crate) fn append_projection_with_intents(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        appended_messages: &[MessageNodeProjection],
        jsonl_path: &Path,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        self.write_projection(
            header,
            projection,
            appended_messages,
            jsonl_path,
            false,
            intents,
        )
    }

    fn write_projection(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        messages: &[MessageNodeProjection],
        jsonl_path: &Path,
        replace_messages: bool,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        let force_full = self.reopen_if_replaced()?;
        let result = self.write_projection_once(
            header,
            projection,
            if force_full {
                &projection.messages
            } else {
                messages
            },
            jsonl_path,
            force_full || replace_messages,
            intents,
        );
        if let Err(error) = result {
            // A write-ahead intent may belong to an inode that was replaced
            // during this operation. Best-effort re-arm the current catalog,
            // even when the original transaction had its own exact marker.
            let _ = self.mark_repair_required();
            return Err(error);
        }
        let replaced_after_write = self.reopen_if_replaced()?;
        if replaced_after_write {
            if let Err(error) = self.write_projection_once(
                header,
                projection,
                &projection.messages,
                jsonl_path,
                true,
                intents,
            ) {
                let _ = self.mark_repair_required();
                return Err(error);
            }
            if self.reopen_if_replaced()? {
                return Err(CatalogError::ReplacedDuringOperation);
            }
        }
        self.refresh_needs_repair()?;
        #[cfg(test)]
        if force_full || replace_messages || replaced_after_write {
            self.full_projection_writes = self.full_projection_writes.saturating_add(1);
        } else {
            self.incremental_projection_writes =
                self.incremental_projection_writes.saturating_add(1);
        }
        Ok(())
    }

    fn write_projection_once(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        messages: &[MessageNodeProjection],
        jsonl_path: &Path,
        replace_messages: bool,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        let tx = self.connection.transaction()?;
        write_projection_in_transaction(
            &tx,
            header,
            projection,
            messages,
            jsonl_path,
            replace_messages,
        )?;
        if let Some(project) = &header.project {
            refresh_project_in_transaction(&tx, &project.project_id)?;
        }
        clear_projection_intents_in_transaction(&tx, intents)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn source_rows(&self) -> Result<Vec<SessionSummary>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT session_id, domain, project_id, canonical_path, display_name,
                    title, preview, model_profile_id, model_id, total_tokens,
                    created_at, updated_at, jsonl_path
             FROM sessions WHERE domain = ?1 ORDER BY session_id",
        )?;
        let rows = statement.query_map(params![self.domain.prefix()], summary_from_row)?;
        rows.map(|row| row.map_err(CatalogError::from)).collect()
    }

    pub(crate) fn projection_intent_session_ids(&self) -> Result<Vec<SessionId>, CatalogError> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT value FROM repair_state
             WHERE key LIKE ?1 ORDER BY value",
        )?;
        let values = statement
            .query_map(params![format!("{PROJECTION_INTENT_PREFIX}%")], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        values
            .into_iter()
            .map(|value| {
                let session_id = value.parse::<SessionId>().map_err(|error| {
                    CatalogError::Corrupt(format!(
                        "invalid projection-intent session id `{value}`: {error}"
                    ))
                })?;
                if session_id.domain() != self.domain {
                    return Err(CatalogError::Corrupt(format!(
                        "projection intent `{session_id}` does not belong to `{}`",
                        self.domain
                    )));
                }
                Ok(session_id)
            })
            .collect()
    }

    pub(crate) fn apply_repair(
        &mut self,
        projections: &[CatalogRepairProjection],
        missing_session_ids: &[SessionId],
        unresolved_session_ids: &[SessionId],
        abandoned_session_ids: &[SessionId],
        completed_at: &str,
    ) -> Result<usize, CatalogError> {
        if self.reopen_if_replaced()? {
            // The projections and missing-row decisions were derived from the
            // previous catalog snapshot. A new full scan must reconcile the
            // replacement instead of applying stale repair decisions to it.
            return Err(CatalogError::ReplacedDuringOperation);
        }
        let tx = self.connection.transaction()?;
        for repaired in projections {
            write_projection_in_transaction(
                &tx,
                &repaired.header,
                &repaired.projection,
                &repaired.projection.messages,
                &repaired.jsonl_path,
                true,
            )?;
        }
        let mut removed = 0_usize;
        for session_id in missing_session_ids {
            removed = removed.saturating_add(tx.execute(
                "DELETE FROM sessions WHERE session_id = ?1 AND domain = ?2",
                params![session_id.to_string(), self.domain.prefix()],
            )?);
        }
        if self.domain == SessionDomain::Agent {
            rebuild_projects_in_transaction(&tx)?;
        }
        tx.execute(
            "INSERT INTO repair_state (key, value) VALUES ('last_repair', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![completed_at],
        )?;
        tx.execute(
            "DELETE FROM repair_state WHERE key = ?1",
            params![REPAIR_REQUIRED_KEY],
        )?;
        // Completing the catalog-wide pass resolves the generic scan marker,
        // but a per-session intent is complete only when this transaction
        // actually rebuilt that session or confirmed its source was deleted.
        // An unreadable/corrupt source keeps its exact recovery obligation for
        // the next startup instead of silently freezing a stale projection.
        for repaired in projections {
            clear_projection_intents_for_session_in_transaction(&tx, &repaired.header.session_id)?;
        }
        for session_id in missing_session_ids {
            clear_projection_intents_for_session_in_transaction(&tx, session_id)?;
        }
        for session_id in abandoned_session_ids {
            clear_projection_intents_for_session_in_transaction(&tx, session_id)?;
        }
        for session_id in unresolved_session_ids {
            tx.execute(
                "INSERT INTO repair_state (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![
                    format!("{PROJECTION_INTENT_PREFIX}repair:{session_id}"),
                    session_id.to_string()
                ],
            )?;
        }
        tx.commit()?;
        if self.reopen_if_replaced()? {
            return Err(CatalogError::ReplacedDuringOperation);
        }
        if let Err(error) = clear_replacement_marker(&self.path) {
            // The repair transaction is idempotent. If removing its external
            // crash marker cannot be made durable, re-arm the SQLite marker so
            // the next startup repeats the safe projection rebuild.
            let _ = self.mark_repair_required();
            self.needs_repair = true;
            return Err(error);
        }
        if self.reopen_if_replaced()? {
            return Err(CatalogError::ReplacedDuringOperation);
        }
        self.refresh_needs_repair()?;
        #[cfg(test)]
        {
            self.full_projection_writes = self
                .full_projection_writes
                .saturating_add(projections.len() as u64);
        }
        Ok(removed)
    }

    pub(crate) fn list(&self, query: &CatalogQuery) -> Result<CatalogPage, CatalogError> {
        let limit = query.limit.max(1);
        let fetch_limit = limit.saturating_add(1).min(i64::MAX as usize);
        let mut sql = String::from(
            "SELECT session_id, domain, project_id, canonical_path, display_name,
                    title, preview, model_profile_id, model_id, total_tokens,
                    created_at, updated_at, jsonl_path
             FROM sessions WHERE domain = ?",
        );
        let mut values: Vec<Box<dyn ToSql>> = vec![Box::new(self.domain.prefix().to_string())];
        if let Some(project_id) = &query.project_id {
            sql.push_str(" AND project_id = ?");
            values.push(Box::new(project_id.clone()));
        }
        if let Some(cursor) = &query.cursor {
            sql.push_str(" AND (created_at < ? OR (created_at = ? AND session_id < ?))");
            values.push(Box::new(cursor.created_at));
            values.push(Box::new(cursor.created_at));
            values.push(Box::new(cursor.session_id.to_string()));
        }
        sql.push_str(" ORDER BY created_at DESC, session_id DESC LIMIT ?");
        values.push(Box::new(fetch_limit as i64));

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            summary_from_row,
        )?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        let has_more = sessions.len() > limit;
        if has_more {
            sessions.pop();
        }
        let next_cursor = has_more
            .then(|| {
                sessions.last().map(|session| CatalogCursor {
                    created_at: session.created_at,
                    session_id: session.session_id.clone(),
                })
            })
            .flatten();
        Ok(CatalogPage {
            sessions,
            next_cursor,
        })
    }

    pub(crate) fn get(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        self.connection
            .query_row(
                "SELECT session_id, domain, project_id, canonical_path, display_name,
                        title, preview, model_profile_id, model_id, total_tokens,
                        created_at, updated_at, jsonl_path
                 FROM sessions WHERE session_id = ?1 AND domain = ?2",
                params![session_id.to_string(), self.domain.prefix()],
                summary_from_row,
            )
            .optional()
            .map_err(CatalogError::from)
    }

    pub(crate) fn get_project_identity(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectIdentity>, CatalogError> {
        self.connection
            .query_row(
                "SELECT canonical_path, display_name FROM projects WHERE project_id = ?1",
                params![project_id],
                |row| {
                    ProjectIdentity::from_parts(
                        project_id.to_string(),
                        PathBuf::from(row.get::<_, String>(0)?),
                        row.get::<_, String>(1)?,
                    )
                    .map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
                    })
                },
            )
            .optional()
            .map_err(CatalogError::from)
    }

    pub(crate) fn list_projects(
        &self,
        query: ProjectCatalogQuery,
    ) -> Result<ProjectCatalogPage, CatalogError> {
        let limit = query.limit.max(1);
        let fetch_limit = limit.saturating_add(1).min(i64::MAX as usize);
        // The `projects` table holds one row per project with the display
        // fields; `sessions` provides the per-project count and the
        // latest most-recent updated_at.  A LEFT JOIN keeps a project with
        // zero remaining sessions visible with count 0 and the
        // `projects.updated_at` fallback.
        let mut sql = String::from(
            "SELECT p.project_id, p.display_name, p.canonical_path,
                    COALESCE(s.session_count, 0) AS session_count,
                    COALESCE(s.last_updated_at, p.updated_at) AS last_updated_at
             FROM projects p
             LEFT JOIN (
                 SELECT project_id,
                        COUNT(*) AS session_count,
                        MAX(updated_at) AS last_updated_at
                 FROM sessions
                 WHERE domain = 'agent' AND project_id IS NOT NULL
                 GROUP BY project_id
             ) s ON s.project_id = p.project_id",
        );
        let mut values: Vec<Box<dyn ToSql>> = Vec::new();
        if let Some(cursor) = &query.cursor {
            sql.push_str(
                " WHERE (last_updated_at < ? OR (last_updated_at = ? AND p.project_id < ?))",
            );
            values.push(Box::new(cursor.updated_at));
            values.push(Box::new(cursor.updated_at));
            values.push(Box::new(cursor.project_id.clone()));
        }
        sql.push_str(" ORDER BY last_updated_at DESC, p.project_id DESC LIMIT ?");
        values.push(Box::new(fetch_limit as i64));

        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            |row| {
                let project_id: String = row.get(0)?;
                let display_name: String = row.get(1)?;
                let canonical_path: String = row.get(2)?;
                let session_count: i64 = row.get(3)?;
                let last_updated_at: i64 = row.get(4)?;
                Ok(ProjectSummary {
                    project_id,
                    display_name,
                    canonical_path: PathBuf::from(canonical_path),
                    session_count: session_count.max(0) as usize,
                    last_updated_at,
                })
            },
        )?;
        let mut projects = Vec::new();
        for row in rows {
            projects.push(row?);
        }
        let has_more = projects.len() > limit;
        if has_more {
            projects.pop();
        }
        let next_cursor = has_more
            .then(|| {
                projects.last().map(|project| ProjectCatalogCursor {
                    updated_at: project.last_updated_at,
                    project_id: project.project_id.clone(),
                })
            })
            .flatten();
        Ok(ProjectCatalogPage {
            projects,
            next_cursor,
        })
    }

    pub(crate) fn search_message_nodes(
        &self,
        folded_query: &str,
        cursor: Option<&super::ChatMessageSearchCursor>,
        limit: usize,
    ) -> Result<Vec<MessageNodeRow>, CatalogError> {
        let fetch_limit = limit.saturating_add(1).min(i64::MAX as usize);
        let mut sql = String::from(
            "SELECT n.session_id, n.entry_id, n.timestamp, n.role, n.preview,
                    s.title, s.created_at
             FROM message_nodes n
             JOIN sessions s ON s.session_id = n.session_id AND s.domain = ?
             WHERE instr(n.searchable_folded, ?) > 0",
        );
        let mut values: Vec<Box<dyn ToSql>> = vec![
            Box::new(self.domain.prefix().to_string()),
            Box::new(folded_query.to_string()),
        ];
        if let Some(cursor) = cursor {
            sql.push_str(
                " AND (n.timestamp < ? OR (n.timestamp = ? AND n.session_id < ?)
                    OR (n.timestamp = ? AND n.session_id = ? AND n.entry_id < ?))",
            );
            values.push(Box::new(cursor.timestamp));
            values.push(Box::new(cursor.timestamp));
            values.push(Box::new(cursor.session_id.to_string()));
            values.push(Box::new(cursor.timestamp));
            values.push(Box::new(cursor.session_id.to_string()));
            values.push(Box::new(cursor.entry_id.to_string()));
        }
        sql.push_str(" ORDER BY n.timestamp DESC, n.session_id DESC, n.entry_id DESC LIMIT ?");
        values.push(Box::new(fetch_limit as i64));
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement.query_map(
            params_from_iter(values.iter().map(|value| value.as_ref())),
            |row| {
                let session_id =
                    SessionId::from_str(&row.get::<_, String>(0)?).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
                    })?;
                let entry_id = EntryId::from_str(&row.get::<_, String>(1)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
                })?;
                let role = role_from_name(&row.get::<_, String>(3)?, 3)?;
                Ok(MessageNodeRow {
                    session_id,
                    entry_id,
                    timestamp: row.get(2)?,
                    role,
                    preview: row.get(4)?,
                    session_title: row.get(5)?,
                    session_created_at: row.get(6)?,
                })
            },
        )?;
        rows.map(|row| row.map_err(CatalogError::from)).collect()
    }

    pub(crate) fn delete_session_with_intents(
        &mut self,
        session_id: &SessionId,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        self.reopen_if_replaced()?;
        self.delete_session_once(session_id, intents)?;
        if self.reopen_if_replaced()? {
            self.delete_session_once(session_id, intents)?;
            if self.reopen_if_replaced()? {
                return Err(CatalogError::ReplacedDuringOperation);
            }
        }
        self.refresh_needs_repair()?;
        Ok(())
    }

    fn delete_session_once(
        &mut self,
        session_id: &SessionId,
        intents: &[ProjectionIntent],
    ) -> Result<(), CatalogError> {
        let tx = self.connection.transaction()?;
        let project_id = if self.domain == SessionDomain::Agent {
            tx.query_row(
                "SELECT project_id FROM sessions WHERE session_id = ?1 AND domain = ?2",
                params![session_id.to_string(), self.domain.prefix()],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten()
        } else {
            None
        };
        tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1 AND domain = ?2",
            params![session_id.to_string(), self.domain.prefix()],
        )?;
        if let Some(project_id) = project_id {
            // Deleting the current registry winner must promote the next
            // deterministic session, not leave stale path/display metadata.
            refresh_project_in_transaction(&tx, &project_id)?;
        }
        clear_projection_intents_in_transaction(&tx, intents)?;
        // A prior delete can lose its in-memory intent after unlinking the
        // source but before this transaction. Once permanent deletion is
        // confirmed, every older projection obligation for this session is
        // complete as well.
        clear_projection_intents_for_session_in_transaction(&tx, session_id)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
static REPLACEMENT_INTERRUPTION: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[cfg(test)]
static INITIALIZE_INTERRUPTION: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

#[cfg(test)]
static REPLACEMENT_INITIALIZATION_FAILURE: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn query_needs_repair(connection: &Connection) -> Result<bool, CatalogError> {
    Ok(connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM repair_state
            WHERE key = ?1 OR key LIKE ?2
        )",
        params![REPAIR_REQUIRED_KEY, format!("{PROJECTION_INTENT_PREFIX}%")],
        |row| row.get(0),
    )?)
}

fn clear_projection_intents_in_transaction(
    tx: &Transaction<'_>,
    intents: &[ProjectionIntent],
) -> Result<(), CatalogError> {
    for intent in intents {
        tx.execute(
            "DELETE FROM repair_state WHERE key = ?1",
            params![&intent.key],
        )?;
    }
    Ok(())
}

fn clear_projection_intents_for_session_in_transaction(
    tx: &Transaction<'_>,
    session_id: &SessionId,
) -> Result<(), CatalogError> {
    tx.execute(
        "DELETE FROM repair_state WHERE key LIKE ?1 AND value = ?2",
        params![
            format!("{PROJECTION_INTENT_PREFIX}%"),
            session_id.to_string()
        ],
    )?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CatalogFileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial_number: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
    #[cfg(not(any(unix, windows)))]
    created: Option<SystemTime>,
}

impl CatalogFileIdentity {
    fn from_metadata(metadata: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;

            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;

            Self {
                volume_serial_number: metadata.volume_serial_number(),
                file_index: metadata.file_index(),
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            Self {
                created: metadata.created().ok(),
            }
        }
    }
}

fn catalog_file_identity(path: &Path) -> Result<CatalogFileIdentity, CatalogError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CatalogError::UnsafePath(path.to_path_buf()));
    }
    Ok(CatalogFileIdentity::from_metadata(&metadata))
}
