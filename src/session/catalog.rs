use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter, types::Type};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::llm::{ModelSelection, Role};

use super::reference::ReferencedMessage;
use super::tree::{message_preview, resolve_session};
use super::{
    EntryId, ProjectIdentity, SessionDomain, SessionEntry, SessionEntryKind, SessionHeader,
    SessionId,
};

#[derive(Clone, Debug)]
pub(crate) struct MessageNodeRow {
    pub session_id: SessionId,
    pub entry_id: EntryId,
    pub timestamp: i64,
    pub message: ReferencedMessage,
    pub session_title: String,
    pub session_created_at: i64,
}

pub(crate) const CATALOG_SCHEMA_VERSION: i64 = 4;
pub(crate) const DEFAULT_PAGE_SIZE: usize = 30;

#[derive(Debug, Error)]
pub enum CatalogError {
    #[error("failed to access session catalog: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("failed to access session catalog file: {0}")]
    Io(#[from] std::io::Error),
    #[error("session catalog contains invalid data: {0}")]
    Corrupt(String),
    #[error("unsupported session catalog schema version {0}")]
    UnsupportedVersion(i64),
    #[error("catalog is scoped to `{expected}`, but request targeted `{actual}`")]
    DomainMismatch {
        expected: SessionDomain,
        actual: SessionDomain,
    },
    #[error("failed to serialize catalog message: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogCursor {
    pub created_at: i64,
    pub session_id: SessionId,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogQuery {
    pub project_id: Option<String>,
    pub cursor: Option<CatalogCursor>,
    pub limit: usize,
}

impl CatalogQuery {
    #[must_use]
    pub fn first_page() -> Self {
        Self {
            project_id: None,
            cursor: None,
            limit: DEFAULT_PAGE_SIZE,
        }
    }

    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            limit: limit.max(1),
            ..Self::first_page()
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionSummary {
    pub session_id: SessionId,
    pub domain: SessionDomain,
    pub project: Option<ProjectIdentity>,
    pub title: String,
    pub preview: Option<String>,
    pub model: Option<ModelSelection>,
    pub total_tokens: u64,
    pub created_at: i64,
    pub updated_at: i64,
    /// Local implementation detail used to locate the append-only source.
    /// Product callers restore through a typed store capability rather than
    /// opening this path themselves.
    pub(crate) jsonl_path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogPage {
    pub sessions: Vec<SessionSummary>,
    pub next_cursor: Option<CatalogCursor>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepairReport {
    pub scanned: usize,
    pub rebuilt: usize,
    pub removed: usize,
    pub issues: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct Catalog {
    path: PathBuf,
    domain: SessionDomain,
    connection: Connection,
    needs_repair: bool,
    #[cfg(test)]
    full_projection_writes: u64,
    #[cfg(test)]
    incremental_projection_writes: u64,
}

impl Catalog {
    pub(crate) fn open(path: PathBuf, domain: SessionDomain) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let invalid_header = path.exists() && !has_sqlite_header(&path);
        if invalid_header {
            remove_sqlite_sidecars(&path);
        }

        match Self::open_connection(&path)
            .and_then(|connection| Self::initialize(connection, path.clone(), domain))
        {
            Ok(mut catalog) => {
                catalog.needs_repair = invalid_header;
                Ok(catalog)
            }
            Err(first_error) => {
                // The index is disposable. Preserve a corrupt file for
                // diagnosis, then rebuild an empty catalog from the source log.
                if path.exists() {
                    remove_sqlite_sidecars(&path);
                    let backup = path.with_extension("sqlite.corrupt");
                    let _ = std::fs::remove_file(&backup);
                    std::fs::rename(&path, backup)?;
                }
                let mut catalog = Self::open_connection(&path)
                    .and_then(|connection| Self::initialize(connection, path, domain))
                    .map_err(|_| first_error)?;
                catalog.needs_repair = true;
                Ok(catalog)
            }
        }
    }

    pub(crate) fn needs_repair(&self) -> bool {
        self.needs_repair
    }

    pub(crate) fn clear_repair_needed(&mut self) {
        self.needs_repair = false;
    }

    fn open_connection(path: &Path) -> Result<Connection, CatalogError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        Ok(connection)
    }

    fn initialize(
        connection: Connection,
        path: PathBuf,
        domain: SessionDomain,
    ) -> Result<Self, CatalogError> {
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        // There is no on-disk schema migration path. A catalog is a disposable
        // projection, so any version other than the empty database or the
        // current schema is rebuilt from the JSONL source by `open`.
        if version != 0 && version != CATALOG_SCHEMA_VERSION {
            return Err(CatalogError::UnsupportedVersion(version));
        }
        if version == 0 {
            if has_table(&connection, "sessions")?
                || has_table(&connection, "message_nodes")?
                || has_table(&connection, "repair_state")?
            {
                return Err(CatalogError::Corrupt(
                    "schema version 0 contains an existing catalog table".to_string(),
                ));
            }
            let schema_sql = format!(
                "CREATE TABLE IF NOT EXISTS sessions (
                    session_id TEXT PRIMARY KEY NOT NULL,
                    domain TEXT NOT NULL,
                    project_id TEXT,
                    canonical_path TEXT,
                    display_name TEXT,
                    title TEXT NOT NULL,
                    preview TEXT,
                    model_profile_id TEXT,
                    model_id TEXT,
                    total_tokens INTEGER NOT NULL DEFAULT 0,
                    created_at INTEGER NOT NULL,
                    updated_at INTEGER NOT NULL,
                    jsonl_path TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS sessions_domain_created
                    ON sessions(domain, created_at DESC, session_id DESC);
                CREATE INDEX IF NOT EXISTS sessions_project_created
                    ON sessions(domain, project_id, created_at DESC, session_id DESC);
                CREATE TABLE IF NOT EXISTS message_nodes (
                    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
                    entry_id TEXT NOT NULL,
                    parent_id TEXT,
                    timestamp INTEGER NOT NULL,
                    role TEXT NOT NULL,
                    preview TEXT,
                    searchable_text TEXT NOT NULL,
                    searchable_folded TEXT NOT NULL,
                    message_json TEXT NOT NULL,
                    PRIMARY KEY(session_id, entry_id)
                );
                CREATE INDEX IF NOT EXISTS message_nodes_session_timestamp
                    ON message_nodes(session_id, timestamp, entry_id);
                CREATE INDEX IF NOT EXISTS message_nodes_search
                    ON message_nodes(session_id, searchable_folded);
                CREATE TABLE IF NOT EXISTS repair_state (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                PRAGMA user_version = {CATALOG_SCHEMA_VERSION};"
            );
            connection.execute_batch(&schema_sql)?;
        }
        if version == CATALOG_SCHEMA_VERSION
            && !has_column(&connection, "message_nodes", "searchable_folded")?
        {
            return Err(CatalogError::Corrupt(
                "catalog message_nodes table is missing searchable_folded".to_string(),
            ));
        }
        if domain == SessionDomain::Agent {
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS projects (
                    project_id TEXT PRIMARY KEY NOT NULL,
                    canonical_path TEXT NOT NULL,
                    display_name TEXT NOT NULL,
                    updated_at INTEGER NOT NULL
                );",
            )?;
        }
        Ok(Self {
            path,
            domain,
            connection,
            needs_repair: false,
            #[cfg(test)]
            full_projection_writes: 0,
            #[cfg(test)]
            incremental_projection_writes: 0,
        })
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

    pub(crate) fn upsert_session(
        &mut self,
        header: &SessionHeader,
        entries: &[SessionEntry],
        jsonl_path: &Path,
    ) -> Result<(), CatalogError> {
        let projection = SessionProjection::from_entries(header, entries)?;
        self.upsert_projection(header, &projection, jsonl_path)
    }

    pub(crate) fn upsert_projection(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        jsonl_path: &Path,
    ) -> Result<(), CatalogError> {
        self.write_projection(header, projection, &projection.messages, jsonl_path, true)
    }

    pub(crate) fn append_projection(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        appended_messages: &[MessageNodeProjection],
        jsonl_path: &Path,
    ) -> Result<(), CatalogError> {
        self.write_projection(header, projection, appended_messages, jsonl_path, false)
    }

    fn write_projection(
        &mut self,
        header: &SessionHeader,
        projection: &SessionProjection,
        messages: &[MessageNodeProjection],
        jsonl_path: &Path,
        replace_messages: bool,
    ) -> Result<(), CatalogError> {
        let tx = self.connection.transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                session_id, domain, project_id, canonical_path, display_name,
                title, preview, model_profile_id, model_id, total_tokens,
                created_at, updated_at, jsonl_path
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
            ON CONFLICT(session_id) DO UPDATE SET
                domain = excluded.domain,
                project_id = excluded.project_id,
                canonical_path = excluded.canonical_path,
                display_name = excluded.display_name,
                title = excluded.title,
                preview = excluded.preview,
                model_profile_id = excluded.model_profile_id,
                model_id = excluded.model_id,
                total_tokens = excluded.total_tokens,
                created_at = sessions.created_at,
                updated_at = excluded.updated_at,
                jsonl_path = excluded.jsonl_path",
            params![
                header.session_id.to_string(),
                header.domain.prefix(),
                projection.project_id.as_deref(),
                projection.canonical_path.as_deref(),
                projection.display_name.as_deref(),
                projection.catalog_title(),
                projection.preview.as_deref(),
                projection
                    .model
                    .as_ref()
                    .map(|model| model.profile_id.clone()),
                projection
                    .model
                    .as_ref()
                    .map(|model| model.model_id.clone()),
                projection.total_tokens.min(i64::MAX as u64) as i64,
                header.created_at,
                projection.updated_at,
                jsonl_path.to_string_lossy().into_owned(),
            ],
        )?;

        if replace_messages {
            tx.execute(
                "DELETE FROM message_nodes WHERE session_id = ?1",
                params![header.session_id.to_string()],
            )?;
        }
        for node in messages {
            tx.execute(
                "INSERT INTO message_nodes (
                    session_id, entry_id, parent_id, timestamp, role, preview,
                    searchable_text, searchable_folded, message_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(session_id, entry_id) DO UPDATE SET
                    parent_id = excluded.parent_id,
                    timestamp = excluded.timestamp,
                    role = excluded.role,
                    preview = excluded.preview,
                    searchable_text = excluded.searchable_text,
                    searchable_folded = excluded.searchable_folded,
                    message_json = excluded.message_json",
                params![
                    header.session_id.to_string(),
                    node.entry_id.to_string(),
                    node.parent_id.as_ref().map(ToString::to_string),
                    node.timestamp,
                    node.role,
                    node.preview.as_deref(),
                    node.searchable_text.as_str(),
                    node.searchable_folded.as_str(),
                    node.message_json.as_str(),
                ],
            )?;
        }
        if let Some(project) = &header.project {
            tx.execute(
                "INSERT INTO projects (project_id, canonical_path, display_name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(project_id) DO UPDATE SET
                    canonical_path = excluded.canonical_path,
                    display_name = excluded.display_name,
                    updated_at = excluded.updated_at",
                params![
                    project.project_id,
                    project.canonical_path.to_string_lossy().into_owned(),
                    project.display_name,
                    projection.updated_at,
                ],
            )?;
        }
        tx.commit()?;
        #[cfg(test)]
        if replace_messages {
            self.full_projection_writes = self.full_projection_writes.saturating_add(1);
        } else {
            self.incremental_projection_writes =
                self.incremental_projection_writes.saturating_add(1);
        }
        Ok(())
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

    pub(crate) fn path_for(&self, session_id: &SessionId) -> Result<Option<PathBuf>, CatalogError> {
        self.connection
            .query_row(
                "SELECT jsonl_path FROM sessions WHERE session_id = ?1 AND domain = ?2",
                params![session_id.to_string(), self.domain.prefix()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map(|path| path.map(PathBuf::from))
            .map_err(CatalogError::from)
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

    pub(crate) fn search_message_nodes(
        &self,
        folded_query: &str,
        cursor: Option<&super::ChatMessageSearchCursor>,
        limit: usize,
    ) -> Result<Vec<MessageNodeRow>, CatalogError> {
        let fetch_limit = limit.saturating_add(1).min(i64::MAX as usize);
        let mut sql = String::from(
            "SELECT n.session_id, n.entry_id, n.timestamp, n.message_json,
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
                let message = serde_json::from_str(&row.get::<_, String>(3)?).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(3, Type::Text, Box::new(error))
                })?;
                Ok(MessageNodeRow {
                    session_id,
                    entry_id,
                    timestamp: row.get(2)?,
                    message,
                    session_title: row.get(4)?,
                    session_created_at: row.get(5)?,
                })
            },
        )?;
        rows.map(|row| row.map_err(CatalogError::from)).collect()
    }

    pub(crate) fn delete_session(&mut self, session_id: &SessionId) -> Result<(), CatalogError> {
        let tx = self.connection.transaction()?;
        tx.execute(
            "DELETE FROM sessions WHERE session_id = ?1 AND domain = ?2",
            params![session_id.to_string(), self.domain.prefix()],
        )?;
        if self.domain == SessionDomain::Agent {
            tx.execute(
                "DELETE FROM projects
                 WHERE project_id NOT IN (SELECT DISTINCT project_id FROM sessions WHERE project_id IS NOT NULL)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn remove_stale(
        &mut self,
        valid_session_ids: &HashSet<SessionId>,
    ) -> Result<usize, CatalogError> {
        let mut statement = self
            .connection
            .prepare("SELECT session_id FROM sessions WHERE domain = ?1")?;
        let existing =
            statement.query_map(params![self.domain.prefix()], |row| row.get::<_, String>(0))?;
        let mut stale = Vec::new();
        for id in existing {
            let encoded = id?;
            match SessionId::from_str(&encoded) {
                Ok(session_id) if valid_session_ids.contains(&session_id) => {}
                _ => stale.push(encoded),
            }
        }
        drop(statement);
        let tx = self.connection.transaction()?;
        for session_id in &stale {
            tx.execute(
                "DELETE FROM sessions WHERE session_id = ?1 AND domain = ?2",
                params![session_id, self.domain.prefix()],
            )?;
        }
        if self.domain == SessionDomain::Agent {
            tx.execute(
                "DELETE FROM projects
                 WHERE project_id NOT IN (SELECT DISTINCT project_id FROM sessions WHERE project_id IS NOT NULL)",
                [],
            )?;
        }
        tx.commit()?;
        Ok(stale.len())
    }

    pub(crate) fn mark_repair(&mut self, value: &str) -> Result<(), CatalogError> {
        self.connection.execute(
            "INSERT INTO repair_state (key, value) VALUES ('last_repair', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![value],
        )?;
        Ok(())
    }
}

fn remove_sqlite_sidecars(path: &Path) {
    let wal = path.with_extension("sqlite-wal");
    let shm = path.with_extension("sqlite-shm");
    let _ = std::fs::remove_file(wal);
    let _ = std::fs::remove_file(shm);
}

fn has_sqlite_header(path: &Path) -> bool {
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut header = [0_u8; 16];
    std::io::Read::read_exact(&mut file, &mut header).is_ok() && header == *b"SQLite format 3\0"
}

fn has_table(connection: &Connection, table: &str) -> Result<bool, CatalogError> {
    connection
        .query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1
            )",
            params![table],
            |row| row.get(0),
        )
        .map_err(CatalogError::from)
}

fn has_column(connection: &Connection, table: &str, column: &str) -> Result<bool, CatalogError> {
    let pragma = match table {
        "message_nodes" => "SELECT name FROM pragma_table_info('message_nodes')",
        "sessions" => "SELECT name FROM pragma_table_info('sessions')",
        "repair_state" => "SELECT name FROM pragma_table_info('repair_state')",
        _ => return Ok(false),
    };
    let mut statement = connection.prepare(pragma)?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(0)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Clone, Debug)]
pub(crate) struct SessionProjection {
    project_id: Option<String>,
    canonical_path: Option<String>,
    display_name: Option<String>,
    title: Option<String>,
    preview: Option<String>,
    model: Option<ModelSelection>,
    total_tokens: u64,
    tokens_by_turn: HashMap<String, u64>,
    updated_at: i64,
    messages: Vec<MessageNodeProjection>,
}

#[derive(Clone, Debug)]
pub(crate) struct MessageNodeProjection {
    entry_id: EntryId,
    parent_id: Option<EntryId>,
    timestamp: i64,
    role: &'static str,
    preview: Option<String>,
    searchable_text: String,
    searchable_folded: String,
    message_json: String,
}

impl SessionProjection {
    pub(crate) fn from_entries(
        header: &SessionHeader,
        entries: &[SessionEntry],
    ) -> Result<Self, CatalogError> {
        let active_ids = resolve_session(entries, None)
            .map_err(|error| CatalogError::Corrupt(error.to_string()))?
            .path
            .into_iter()
            .collect::<HashSet<_>>();
        let project = header.project.as_ref();
        let mut projection = Self {
            project_id: project.map(|project| project.project_id.clone()),
            canonical_path: project
                .map(|project| project.canonical_path.to_string_lossy().into_owned()),
            display_name: project.map(|project| project.display_name.clone()),
            title: None,
            preview: None,
            model: header.initial_model.clone(),
            total_tokens: 0,
            tokens_by_turn: HashMap::new(),
            updated_at: header.created_at,
            messages: Vec::new(),
        };
        for entry in entries {
            projection.apply_entry_metadata(entry);
            if active_ids.contains(&entry.id)
                && let Some(node) = message_node(entry)?
            {
                projection.messages.push(node);
            }
        }
        Ok(projection)
    }

    pub(crate) fn append_entries(
        &mut self,
        entries: &[SessionEntry],
    ) -> Result<Vec<MessageNodeProjection>, CatalogError> {
        debug_assert!(
            entries
                .iter()
                .all(|entry| !matches!(entry.kind, SessionEntryKind::Leaf(_))),
            "leaf changes must rebuild the active message projection"
        );
        let appended_messages = entries
            .iter()
            .map(message_node)
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        for entry in entries {
            self.apply_entry_metadata(entry);
        }
        self.messages.extend(appended_messages.iter().cloned());
        Ok(appended_messages)
    }

    fn apply_entry_metadata(&mut self, entry: &SessionEntry) {
        self.updated_at = self.updated_at.max(entry.timestamp);
        match &entry.kind {
            SessionEntryKind::Message(message) => {
                let text = message_preview(&message.message);
                if self.title.is_none() && message.message.role == Role::User {
                    self.title = text.clone();
                }
                if text.is_some() {
                    self.preview = text.clone();
                }
                record_usage(
                    &mut self.total_tokens,
                    &mut self.tokens_by_turn,
                    message.turn_id.as_deref(),
                    message.usage.total_tokens,
                );
                if let Some(selection) = &message.model {
                    self.model = Some(selection.clone());
                }
            }
            SessionEntryKind::TurnResult(result) => {
                record_usage(
                    &mut self.total_tokens,
                    &mut self.tokens_by_turn,
                    result.turn_id.as_deref(),
                    result.usage.total_tokens,
                );
            }
            SessionEntryKind::ConfigChange(change) => {
                self.model = Some(change.model.clone());
            }
            _ => {}
        }
    }

    fn catalog_title(&self) -> &str {
        self.title.as_deref().unwrap_or("Untitled session")
    }

    fn summary(&self, header: &SessionHeader, jsonl_path: PathBuf) -> SessionSummary {
        SessionSummary {
            session_id: header.session_id.clone(),
            domain: header.domain,
            project: header.project.clone(),
            title: self.catalog_title().to_string(),
            preview: self.preview.clone(),
            model: self.model.clone(),
            total_tokens: self.total_tokens,
            created_at: header.created_at,
            updated_at: self.updated_at,
            jsonl_path,
        }
    }
}

fn message_node(entry: &SessionEntry) -> Result<Option<MessageNodeProjection>, CatalogError> {
    let SessionEntryKind::Message(message) = &entry.kind else {
        return Ok(None);
    };
    let safe_message = ReferencedMessage::from_message(&message.message);
    let searchable_text = safe_message.searchable_text();
    Ok(Some(MessageNodeProjection {
        entry_id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp: entry.timestamp,
        role: role_name(message.message.role),
        preview: message_preview(&message.message),
        searchable_folded: searchable_text.to_lowercase(),
        searchable_text,
        message_json: serde_json::to_string(&safe_message)?,
    }))
}

pub(crate) fn project_session_summary(
    header: &SessionHeader,
    entries: &[SessionEntry],
    jsonl_path: PathBuf,
) -> Result<SessionSummary, CatalogError> {
    Ok(SessionProjection::from_entries(header, entries)?.summary(header, jsonl_path))
}

fn record_usage(
    total: &mut u64,
    tokens_by_turn: &mut HashMap<String, u64>,
    turn_id: Option<&str>,
    tokens: u64,
) {
    if let Some(turn_id) = turn_id {
        tokens_by_turn
            .entry(turn_id.to_string())
            .and_modify(|current| {
                if tokens > *current {
                    *total = total.saturating_add(tokens.saturating_sub(*current));
                    *current = tokens;
                }
            })
            .or_insert_with(|| {
                *total = total.saturating_add(tokens);
                tokens
            });
    } else {
        *total = total.saturating_add(tokens);
    }
}

fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
    let encoded_id: String = row.get(0)?;
    let session_id = SessionId::from_str(&encoded_id).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, Type::Text, Box::new(error))
    })?;
    let domain = match row.get::<_, String>(1)?.as_str() {
        "chat" => SessionDomain::Chat,
        "agent" => SessionDomain::Agent,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                1,
                Type::Text,
                format!("unknown session domain `{other}`").into(),
            ));
        }
    };
    if session_id.domain() != domain {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            1,
            Type::Text,
            format!("session id domain does not match row domain `{domain}`").into(),
        ));
    }
    let project_id: Option<String> = row.get(2)?;
    let canonical_path: Option<String> = row.get(3)?;
    let display_name: Option<String> = row.get(4)?;
    let project = match project_id {
        Some(project_id) => {
            let identity = ProjectIdentity {
                project_id,
                canonical_path: PathBuf::from(canonical_path.unwrap_or_default()),
                display_name: display_name.unwrap_or_default(),
            };
            identity.validate().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
            })?;
            if domain == SessionDomain::Chat {
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    2,
                    Type::Text,
                    "chat catalog row unexpectedly contains a project".into(),
                ));
            }
            Some(identity)
        }
        None => None,
    };
    let profile_id: Option<String> = row.get(7)?;
    let model_id: Option<String> = row.get(8)?;
    let model = profile_id
        .zip(model_id)
        .map(|(profile_id, model_id)| ModelSelection {
            profile_id,
            model_id,
        });
    Ok(SessionSummary {
        session_id,
        domain,
        project,
        title: row.get(5)?,
        preview: row.get(6)?,
        model,
        total_tokens: row.get::<_, i64>(9)?.max(0) as u64,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        jsonl_path: PathBuf::from(row.get::<_, String>(12)?),
    })
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::Developer => "developer",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}
