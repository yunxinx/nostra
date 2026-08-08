use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, ToSql, params, params_from_iter, types::Type};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::llm::{ContentBlock, Message, ModelSelection, Role};

use super::reference::ReferencedMessage;
use super::tree::message_preview;
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

pub(crate) const CATALOG_SCHEMA_VERSION: i64 = 3;
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
}

impl Catalog {
    pub(crate) fn open(path: PathBuf, domain: SessionDomain) -> Result<Self, CatalogError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() && !has_sqlite_header(&path) {
            remove_sqlite_sidecars(&path);
        }

        match Self::open_connection(&path)
            .and_then(|connection| Self::initialize(connection, path.clone(), domain))
        {
            Ok(catalog) => Ok(catalog),
            Err(first_error) => {
                // The index is disposable. Preserve a corrupt file for
                // diagnosis, then rebuild an empty catalog from the source log.
                if path.exists() {
                    remove_sqlite_sidecars(&path);
                    let backup = path.with_extension("sqlite.corrupt");
                    let _ = std::fs::remove_file(&backup);
                    std::fs::rename(&path, backup)?;
                }
                Self::open_connection(&path)
                    .and_then(|connection| Self::initialize(connection, path, domain))
                    .map_err(|_| first_error)
            }
        }
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
            connection.execute_batch(
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
                    message_json TEXT NOT NULL,
                    PRIMARY KEY(session_id, entry_id)
                );
                CREATE INDEX IF NOT EXISTS message_nodes_session_timestamp
                    ON message_nodes(session_id, timestamp, entry_id);
                CREATE INDEX IF NOT EXISTS message_nodes_search
                    ON message_nodes(session_id, searchable_text);
                CREATE TABLE IF NOT EXISTS repair_state (
                    key TEXT PRIMARY KEY NOT NULL,
                    value TEXT NOT NULL
                );
                PRAGMA user_version = 3;",
            )?;
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
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn upsert_session(
        &mut self,
        header: &SessionHeader,
        entries: &[SessionEntry],
        jsonl_path: &Path,
    ) -> Result<(), CatalogError> {
        let projection = SessionProjection::from_entries(header, entries)?;
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
                projection.project_id,
                projection.canonical_path,
                projection.display_name,
                projection.title,
                projection.preview,
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

        tx.execute(
            "DELETE FROM message_nodes WHERE session_id = ?1",
            params![header.session_id.to_string()],
        )?;
        for node in projection.messages {
            tx.execute(
                "INSERT INTO message_nodes (
                    session_id, entry_id, parent_id, timestamp, role, preview,
                    searchable_text, message_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    header.session_id.to_string(),
                    node.entry_id.to_string(),
                    node.parent_id.map(|id| id.to_string()),
                    node.timestamp,
                    node.role,
                    node.preview,
                    node.searchable_text,
                    node.message_json,
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
        query: &str,
        cursor: Option<&super::ChatMessageSearchCursor>,
        limit: usize,
    ) -> Result<Vec<MessageNodeRow>, CatalogError> {
        let fetch_limit = limit.saturating_add(1).min(i64::MAX as usize);
        let mut sql = String::from(
            "SELECT n.session_id, n.entry_id, n.timestamp, n.message_json,
                    s.title, s.created_at
             FROM message_nodes n
             JOIN sessions s ON s.session_id = n.session_id AND s.domain = ?
             WHERE n.session_id IN (SELECT session_id FROM sessions WHERE domain = ?)
               AND lower(n.searchable_text) LIKE lower(?)",
        );
        let mut values: Vec<Box<dyn ToSql>> = vec![
            Box::new(self.domain.prefix().to_string()),
            Box::new(self.domain.prefix().to_string()),
            Box::new(format!("%{query}%")),
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

#[derive(Debug)]
struct SessionProjection {
    project_id: Option<String>,
    canonical_path: Option<String>,
    display_name: Option<String>,
    title: String,
    preview: Option<String>,
    model: Option<ModelSelection>,
    total_tokens: u64,
    updated_at: i64,
    messages: Vec<MessageNodeProjection>,
}

#[derive(Debug)]
struct MessageNodeProjection {
    entry_id: EntryId,
    parent_id: Option<EntryId>,
    timestamp: i64,
    role: &'static str,
    preview: Option<String>,
    searchable_text: String,
    message_json: String,
}

impl SessionProjection {
    fn from_entries(
        header: &SessionHeader,
        entries: &[SessionEntry],
    ) -> Result<Self, CatalogError> {
        let mut title = None;
        let mut preview = None;
        let mut model = header.initial_model.clone();
        let mut total_tokens = 0_u64;
        let mut tokens_by_turn = HashMap::new();
        let mut updated_at = header.created_at;
        let mut messages = Vec::new();
        for entry in entries {
            updated_at = updated_at.max(entry.timestamp);
            match &entry.kind {
                SessionEntryKind::Message(message) => {
                    let text = message_preview(&message.message);
                    if title.is_none() && message.message.role == Role::User {
                        title = text.clone();
                    }
                    if text.is_some() {
                        preview = text.clone();
                    }
                    record_usage(
                        &mut total_tokens,
                        &mut tokens_by_turn,
                        message.turn_id.as_deref(),
                        message.usage.total_tokens,
                    );
                    if let Some(selection) = &message.model {
                        model = Some(selection.clone());
                    }
                    messages.push(MessageNodeProjection {
                        entry_id: entry.id.clone(),
                        parent_id: entry.parent_id.clone(),
                        timestamp: entry.timestamp,
                        role: role_name(message.message.role),
                        preview: text,
                        searchable_text: message_search_text(&message.message),
                        message_json: serde_json::to_string(&ReferencedMessage::from_message(
                            &message.message,
                        ))?,
                    });
                }
                SessionEntryKind::TurnResult(result) => {
                    record_usage(
                        &mut total_tokens,
                        &mut tokens_by_turn,
                        result.turn_id.as_deref(),
                        result.usage.total_tokens,
                    );
                }
                SessionEntryKind::ConfigChange(change) => {
                    model = Some(change.model.clone());
                }
                _ => {}
            }
        }
        total_tokens = tokens_by_turn
            .values()
            .fold(total_tokens, |total, tokens| total.saturating_add(*tokens));
        let project = header.project.as_ref();
        Ok(Self {
            project_id: project.map(|project| project.project_id.clone()),
            canonical_path: project
                .map(|project| project.canonical_path.to_string_lossy().into_owned()),
            display_name: project.map(|project| project.display_name.clone()),
            title: title.unwrap_or_else(|| "Untitled session".to_string()),
            preview,
            model,
            total_tokens,
            updated_at,
            messages,
        })
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

pub(crate) fn project_session_summary(
    header: &SessionHeader,
    entries: &[SessionEntry],
    jsonl_path: PathBuf,
) -> Result<SessionSummary, CatalogError> {
    Ok(SessionProjection::from_entries(header, entries)?.summary(header, jsonl_path))
}

fn record_usage(
    unkeyed_total: &mut u64,
    tokens_by_turn: &mut HashMap<String, u64>,
    turn_id: Option<&str>,
    tokens: u64,
) {
    if let Some(turn_id) = turn_id {
        tokens_by_turn
            .entry(turn_id.to_string())
            .and_modify(|current| *current = (*current).max(tokens))
            .or_insert(tokens);
    } else {
        *unkeyed_total = unkeyed_total.saturating_add(tokens);
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

fn message_search_text(message: &Message) -> String {
    let mut text = String::new();
    for block in &message.content {
        if !text.is_empty() {
            text.push('\n');
        }
        match block {
            ContentBlock::Text { text: value, .. } => text.push_str(value),
            ContentBlock::Reasoning { reasoning } => text.push_str(&reasoning.display),
            ContentBlock::ToolCall { tool_call } => {
                text.push_str(&tool_call.name);
                if !tool_call.raw_arguments.is_empty() {
                    text.push('\n');
                    text.push_str(&tool_call.raw_arguments);
                }
            }
            ContentBlock::ToolResult { tool_result } => text.push_str(&tool_result.content),
        }
    }
    text.chars().take(16_384).collect()
}
