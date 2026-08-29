use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
};

use rusqlite::{Transaction, params, types::Type};

use crate::llm::{ModelSelection, Role};

use super::super::reference::searchable_text_from_message;
use super::super::tree::{message_preview, resolve_session};
use super::super::{
    EntryId, ProjectIdentity, SessionDomain, SessionEntry, SessionEntryKind, SessionHeader,
    SessionId,
};
use super::{CatalogError, SessionSummary};

pub(crate) struct CatalogRepairProjection {
    pub(super) header: SessionHeader,
    pub(super) projection: SessionProjection,
    pub(super) jsonl_path: PathBuf,
}

impl CatalogRepairProjection {
    pub(crate) fn from_entries(
        header: SessionHeader,
        entries: &[SessionEntry],
        jsonl_path: PathBuf,
    ) -> Result<Self, CatalogError> {
        let projection = SessionProjection::from_entries(&header, entries)?;
        Ok(Self {
            header,
            projection,
            jsonl_path,
        })
    }

    pub(crate) fn session_id(&self) -> &SessionId {
        &self.header.session_id
    }

    pub(crate) fn jsonl_path(&self) -> &Path {
        &self.jsonl_path
    }
}

pub(super) fn write_projection_in_transaction(
    tx: &Transaction<'_>,
    header: &SessionHeader,
    projection: &SessionProjection,
    messages: &[MessageNodeProjection],
    jsonl_path: &Path,
    replace_messages: bool,
) -> Result<(), CatalogError> {
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
            created_at = excluded.created_at,
            updated_at = excluded.updated_at,
            jsonl_path = excluded.jsonl_path",
        params![
            header.session_id.to_string(),
            header.domain.prefix(),
            projection.project_id.as_deref(),
            projection.canonical_path.as_deref(),
            projection.display_name.as_deref(),
            projection.title.as_deref(),
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
                searchable_text, searchable_folded
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(session_id, entry_id) DO UPDATE SET
                parent_id = excluded.parent_id,
                timestamp = excluded.timestamp,
                role = excluded.role,
                preview = excluded.preview,
                searchable_text = excluded.searchable_text,
                searchable_folded = excluded.searchable_folded",
            params![
                header.session_id.to_string(),
                node.entry_id.to_string(),
                node.parent_id.as_ref().map(ToString::to_string),
                node.timestamp,
                node.role,
                node.preview.as_deref(),
                node.searchable_text.as_str(),
                node.searchable_folded.as_str(),
            ],
        )?;
    }
    Ok(())
}

pub(super) fn refresh_project_in_transaction(
    tx: &Transaction<'_>,
    project_id: &str,
) -> Result<(), CatalogError> {
    tx.execute(
        "DELETE FROM projects WHERE project_id = ?1",
        params![project_id],
    )?;
    tx.execute(
        "INSERT INTO projects (project_id, canonical_path, display_name, updated_at)
         SELECT project_id, canonical_path, display_name, updated_at
         FROM sessions
         WHERE domain = 'agent' AND project_id = ?1
         ORDER BY updated_at DESC, session_id DESC
         LIMIT 1",
        params![project_id],
    )?;
    Ok(())
}

pub(super) fn rebuild_projects_in_transaction(tx: &Transaction<'_>) -> Result<(), CatalogError> {
    tx.execute("DELETE FROM projects", [])?;
    tx.execute(
        "INSERT INTO projects (project_id, canonical_path, display_name, updated_at)
         SELECT current.project_id, current.canonical_path, current.display_name, current.updated_at
         FROM sessions current
         WHERE current.domain = 'agent'
           AND current.project_id IS NOT NULL
           AND NOT EXISTS (
                SELECT 1 FROM sessions newer
                WHERE newer.domain = 'agent'
                  AND newer.project_id = current.project_id
                  AND (
                    newer.updated_at > current.updated_at
                    OR (newer.updated_at = current.updated_at AND newer.session_id > current.session_id)
                  )
           )",
        [],
    )?;
    Ok(())
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
    pub(super) messages: Vec<MessageNodeProjection>,
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
            // `updated_at` describes the durable session fact stream, so a
            // rewind must not make the session appear older. User-visible
            // metadata, however, must only describe the resolved active path;
            // otherwise a discarded branch can leak its title, preview,
            // model, or token usage into the catalog.
            projection.observe_durable_entry(entry);
            if active_ids.contains(&entry.id) {
                projection.apply_active_entry_metadata(entry);
                if let Some(node) = message_node(entry)? {
                    projection.messages.push(node);
                }
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
            // Incremental projection is used only when this batch extends the
            // current leaf. Leaf changes take the full rebuild path above.
            self.observe_durable_entry(entry);
            self.apply_active_entry_metadata(entry);
        }
        self.messages.extend(appended_messages.iter().cloned());
        Ok(appended_messages)
    }

    fn observe_durable_entry(&mut self, entry: &SessionEntry) {
        self.updated_at = self.updated_at.max(entry.timestamp);
    }

    fn apply_active_entry_metadata(&mut self, entry: &SessionEntry) {
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

    fn summary(&self, header: &SessionHeader, jsonl_path: PathBuf) -> SessionSummary {
        SessionSummary {
            session_id: header.session_id.clone(),
            domain: header.domain,
            project: header.project.clone(),
            title: self.title.clone(),
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
    let searchable_text = searchable_text_from_message(&message.message);
    Ok(Some(MessageNodeProjection {
        entry_id: entry.id.clone(),
        parent_id: entry.parent_id.clone(),
        timestamp: entry.timestamp,
        role: role_name(message.message.role),
        preview: message_preview(&message.message),
        searchable_folded: searchable_text.to_lowercase(),
        searchable_text,
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

pub(super) fn summary_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionSummary> {
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

pub(super) fn role_from_name(value: &str, column: usize) -> rusqlite::Result<Role> {
    match value {
        "system" => Ok(Role::System),
        "developer" => Ok(Role::Developer),
        "user" => Ok(Role::User),
        "assistant" => Ok(Role::Assistant),
        "tool" => Ok(Role::Tool),
        other => Err(rusqlite::Error::FromSqlConversionFailure(
            column,
            Type::Text,
            format!("unknown message role `{other}`").into(),
        )),
    }
}
