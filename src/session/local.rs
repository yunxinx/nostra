use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use thiserror::Error;

use crate::paths;

use super::{
    CatalogError, ChatMessageRead, ChatMessageReferenceStore, ChatMessageSearchCursor,
    ChatMessageSearchPage, ChatMessageSearchQuery, ChatReferenceError, EntryId, JsonlLoader,
    JsonlRecorder, ProjectSessionStore, ResolvedSessionState, SessionBranchPreview,
    SessionBranchTreeSnapshot, SessionCatalogStore, SessionDomain, SessionEntry, SessionEntryKind,
    SessionError, SessionFlushStore, SessionHeader, SessionId, SessionLifecycleStore,
    SessionSummary, SessionTreeSnapshot, SessionTreeStore,
    catalog::{Catalog, CatalogPage, CatalogQuery, RepairReport},
    reference::{
        ChatMessageUnavailableReason, message_from_entry, preview_from_node, unavailable,
        validate_reference,
    },
    resolve_session, session_branch_preview, session_branch_tree_snapshot, session_tree_snapshot,
};

#[derive(Debug, Error)]
pub enum LocalStoreError {
    #[error("session data root is unavailable")]
    DataRootUnavailable,
    #[error("session store domain mismatch: expected `{expected}`, got `{actual}`")]
    DomainMismatch {
        expected: SessionDomain,
        actual: SessionDomain,
    },
    #[error("session `{0}` does not exist in the local store")]
    SessionNotFound(SessionId),
    #[error("session operation lock is poisoned")]
    OperationPoisoned,
    #[error("session operation failed: {0}")]
    Session(#[from] SessionError),
    #[error("session catalog operation failed: {0}")]
    Catalog(#[from] CatalogError),
    #[error("failed to enumerate session files: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStoreConfig {
    pub data_root: PathBuf,
    pub domain: SessionDomain,
    pub max_open_handles: usize,
}

impl LocalStoreConfig {
    #[must_use]
    pub fn new(data_root: impl Into<PathBuf>, domain: SessionDomain) -> Self {
        Self {
            data_root: data_root.into(),
            domain,
            max_open_handles: 16,
        }
    }

    pub fn default_for(domain: SessionDomain) -> Result<Self, LocalStoreError> {
        let root = paths::nostra_config_dir().ok_or(LocalStoreError::DataRootUnavailable)?;
        Ok(Self::new(root, domain))
    }

    #[must_use]
    pub fn with_max_open_handles(mut self, max_open_handles: usize) -> Self {
        self.max_open_handles = max_open_handles.max(1);
        self
    }

    #[must_use]
    pub fn storage_root(&self) -> PathBuf {
        self.data_root.join(self.domain.prefix())
    }

    #[must_use]
    pub fn sessions_root(&self) -> PathBuf {
        self.storage_root().join("sessions")
    }

    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.storage_root().join("index.sqlite")
    }
}

struct LocalHandle {
    header: SessionHeader,
    path: PathBuf,
    recorder: JsonlRecorder,
    entries: Vec<SessionEntry>,
    last_used: u64,
    operation_lock: Mutex<()>,
}

pub struct LocalSessionStore {
    config: LocalStoreConfig,
    catalog: Catalog,
    handles: HashMap<SessionId, LocalHandle>,
    access_counter: u64,
}

impl LocalSessionStore {
    pub fn open(config: LocalStoreConfig) -> Result<Self, LocalStoreError> {
        fs::create_dir_all(config.sessions_root())?;
        let catalog = Catalog::open(config.index_path(), config.domain)?;
        Ok(Self {
            config,
            catalog,
            handles: HashMap::new(),
            access_counter: 0,
        })
    }

    pub fn open_default(domain: SessionDomain) -> Result<Self, LocalStoreError> {
        Self::open(LocalStoreConfig::default_for(domain)?)
    }

    #[must_use]
    pub fn config(&self) -> &LocalStoreConfig {
        &self.config
    }

    #[must_use]
    pub fn catalog_path(&self) -> &Path {
        self.catalog.path()
    }

    #[must_use]
    pub fn open_handle_count(&self) -> usize {
        self.handles.len()
    }

    pub fn list(&self, query: CatalogQuery) -> Result<CatalogPage, LocalStoreError> {
        Ok(self.catalog.list(&query)?)
    }

    /// Read one catalog row without opening or scanning its JSONL source.
    pub fn get_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, LocalStoreError> {
        Ok(self.catalog.get(session_id)?)
    }

    pub fn repair(&mut self) -> Result<RepairReport, LocalStoreError> {
        let mut report = RepairReport::default();
        let mut valid_ids = std::collections::HashSet::new();
        let paths = collect_jsonl_paths(&self.config.sessions_root())?;
        for path in paths {
            report.scanned += 1;
            let loaded = match JsonlLoader::load(&path) {
                Ok(loaded) => loaded,
                Err(error) => {
                    report.issues.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            for diagnostic in &loaded.diagnostics {
                report.issues.push(format!(
                    "{}:{} {}: {}",
                    path.display(),
                    diagnostic.line,
                    diagnostic.kind,
                    diagnostic.message
                ));
            }
            if loaded.truncated_tail {
                report.issues.push(format!(
                    "{}: interrupted trailing JSONL entry",
                    path.display()
                ));
            }
            let header = match loaded.header() {
                Ok(header) if header.domain == self.config.domain => header.clone(),
                Ok(header) => {
                    report.issues.push(format!(
                        "{}: expected domain `{}`, got `{}`",
                        path.display(),
                        self.config.domain,
                        header.domain
                    ));
                    continue;
                }
                Err(error) => {
                    report.issues.push(format!("{}: {error}", path.display()));
                    continue;
                }
            };
            match self.catalog.upsert_session(&header, &loaded.entries, &path) {
                Ok(()) => {
                    valid_ids.insert(header.session_id);
                    report.rebuilt += 1;
                }
                Err(error) => report.issues.push(format!("{}: {error}", path.display())),
            }
        }
        report.removed = self.catalog.remove_stale(&valid_ids)?;
        self.catalog.mark_repair(&now_millis().to_string())?;
        Ok(report)
    }

    pub fn delete_session(&mut self, session_id: &SessionId) -> Result<(), LocalStoreError> {
        let path = self
            .handles
            .get(session_id)
            .map(|handle| handle.path.clone())
            .or(self.catalog.path_for(session_id)?)
            .ok_or_else(|| LocalStoreError::SessionNotFound(session_id.clone()))?;
        self.handles.remove(session_id);
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(LocalStoreError::Io(error)),
        }
        self.catalog.delete_session(session_id)?;
        Ok(())
    }

    fn ensure_handle(&mut self, session_id: &SessionId) -> Result<(), LocalStoreError> {
        if !self.handles.contains_key(session_id) {
            let path = self
                .catalog
                .path_for(session_id)?
                .ok_or_else(|| LocalStoreError::SessionNotFound(session_id.clone()))?;
            let loaded = JsonlLoader::load(&path)?;
            let header = loaded.header()?.clone();
            self.validate_header(session_id, &header)?;
            let recorder = JsonlRecorder::open(&path)?;
            self.handles.insert(
                session_id.clone(),
                LocalHandle {
                    header,
                    path,
                    recorder,
                    entries: loaded.entries,
                    last_used: 0,
                    operation_lock: Mutex::new(()),
                },
            );
        }
        self.access_counter = self.access_counter.saturating_add(1);
        if let Some(handle) = self.handles.get_mut(session_id) {
            handle.last_used = self.access_counter;
        }
        self.evict_handles();
        Ok(())
    }

    fn evict_handles(&mut self) {
        while self.handles.len() > self.config.max_open_handles.max(1) {
            let candidate = self
                .handles
                .iter()
                .filter(|(_, handle)| !handle.recorder.has_pending())
                .min_by_key(|(_, handle)| handle.last_used)
                .map(|(id, _)| id.clone());
            let Some(candidate) = candidate else {
                break;
            };
            self.handles.remove(&candidate);
        }
    }

    fn validate_header(
        &self,
        session_id: &SessionId,
        header: &SessionHeader,
    ) -> Result<(), LocalStoreError> {
        if header.domain != self.config.domain {
            return Err(LocalStoreError::DomainMismatch {
                expected: self.config.domain,
                actual: header.domain,
            });
        }
        if header.session_id != session_id.clone() {
            return Err(LocalStoreError::Session(SessionError::SessionIdMismatch {
                expected: session_id.clone(),
                actual: header.session_id.clone(),
            }));
        }
        Ok(())
    }

    fn reload_entries(path: &Path) -> Result<Vec<SessionEntry>, LocalStoreError> {
        Ok(JsonlLoader::load(path)?.entries)
    }

    fn load_header_and_entries_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<(SessionHeader, Vec<SessionEntry>), SessionError> {
        let path = self
            .catalog
            .path_for(session_id)
            .map_err(session_io_error)?
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let loaded = JsonlLoader::load(path)?;
        let header = loaded.header()?;
        if header.domain != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: header.domain,
            });
        }
        if header.session_id != *session_id {
            return Err(SessionError::SessionIdMismatch {
                expected: session_id.clone(),
                actual: header.session_id.clone(),
            });
        }
        Ok((header.clone(), loaded.entries))
    }

    fn load_entries_for_session(
        &self,
        session_id: &SessionId,
    ) -> Result<Vec<SessionEntry>, SessionError> {
        Ok(self.load_header_and_entries_for_session(session_id)?.1)
    }

    fn flush_handle(&mut self, session_id: &SessionId) -> Result<(), LocalStoreError> {
        self.ensure_handle(session_id)?;
        let (header, path, entries) = {
            let handle = self
                .handles
                .get_mut(session_id)
                .ok_or_else(|| LocalStoreError::SessionNotFound(session_id.clone()))?;
            let _operation = handle
                .operation_lock
                .lock()
                .map_err(|_| LocalStoreError::OperationPoisoned)?;
            handle.recorder.flush()?;
            handle.entries = Self::reload_entries(&handle.path)?;
            (
                handle.header.clone(),
                handle.path.clone(),
                handle.entries.clone(),
            )
        };
        self.catalog.upsert_session(&header, &entries, &path)?;
        Ok(())
    }

    fn source_path_for_header(&self, header: &SessionHeader) -> PathBuf {
        let mut directory = self.config.sessions_root();
        if let Some(project) = &header.project {
            directory = directory.join(format!("--{}--", project.project_id));
        }
        directory.join(format!(
            "{}_{}.jsonl",
            header.created_at.max(0),
            header.session_id
        ))
    }

    #[cfg(test)]
    fn fail_next_append_for_test(&mut self, session_id: &SessionId) {
        if let Some(handle) = self.handles.get(session_id) {
            let _ = handle.recorder.fail_next_append_for_test();
        }
    }
}

impl SessionLifecycleStore for LocalSessionStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
        if header.domain != self.config.domain {
            return Err(SessionError::DomainMismatch {
                header: self.config.domain,
                id: header.domain,
            });
        }
        header.validate()?;
        if self.handles.contains_key(&header.session_id)
            || self
                .catalog
                .get(&header.session_id)
                .map_err(session_io_error)?
                .is_some()
        {
            return Err(SessionError::SessionAlreadyExists(header.session_id));
        }
        let path = self.source_path_for_header(&header);
        let recorder = JsonlRecorder::create(&path, header.clone())?;
        let initial_entries = JsonlLoader::load(&path).map_err(session_io_error)?.entries;
        self.handles.insert(
            header.session_id.clone(),
            LocalHandle {
                header: header.clone(),
                path: path.clone(),
                recorder,
                entries: initial_entries.clone(),
                last_used: 0,
                operation_lock: Mutex::new(()),
            },
        );
        let result = self
            .catalog
            .upsert_session(&header, &initial_entries, &path)
            .map_err(session_io_error);
        result?;
        self.evict_handles();
        Ok(header.session_id)
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        self.ensure_handle(session_id).map_err(session_io_error)?;
        let (ids, header, path, committed_entries) = {
            let handle = self
                .handles
                .get_mut(session_id)
                .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
            let _operation = handle.operation_lock.lock().map_err(|_| SessionError::Io {
                source: std::io::Error::other("session operation lock is poisoned"),
            })?;
            let ids = match handle.recorder.append_batch(entries.clone()) {
                Ok(ids) => ids,
                Err(error) => {
                    return Err(error);
                }
            };
            handle.entries = Self::reload_entries(&handle.path).map_err(session_io_error)?;
            (
                ids,
                handle.header.clone(),
                handle.path.clone(),
                handle.entries.clone(),
            )
        };
        self.catalog
            .upsert_session(&header, &committed_entries, &path)
            .map_err(session_io_error)?;
        Ok(ids)
    }

    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        resolve_session(&self.load_entries_for_session(session_id)?, leaf)
    }
}

impl SessionTreeStore for LocalSessionStore {
    fn set_leaf(
        &mut self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<(), SessionError> {
        self.ensure_handle(session_id).map_err(session_io_error)?;
        let (header, path, entries) = {
            let handle = self
                .handles
                .get_mut(session_id)
                .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
            let _operation = handle.operation_lock.lock().map_err(|_| SessionError::Io {
                source: std::io::Error::other("session operation lock is poisoned"),
            })?;
            handle.recorder.set_leaf(leaf)?;
            handle.entries = Self::reload_entries(&handle.path).map_err(session_io_error)?;
            (
                handle.header.clone(),
                handle.path.clone(),
                handle.entries.clone(),
            )
        };
        self.catalog
            .upsert_session(&header, &entries, &path)
            .map_err(session_io_error)
    }

    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_tree_snapshot(&entries, None)
    }

    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_tree_snapshot(&entries, Some(leaf))
    }

    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &EntryId,
    ) -> Result<SessionBranchPreview, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_branch_preview(&entries, branch_root)
    }

    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError> {
        let entries = self.load_entries_for_session(session_id)?;
        session_branch_tree_snapshot(&entries, None)
    }
}

impl SessionFlushStore for LocalSessionStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        let session_ids = self.handles.keys().cloned().collect::<Vec<_>>();
        for session_id in session_ids {
            self.flush_handle(&session_id).map_err(session_io_error)?;
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        self.flush()?;
        self.handles.clear();
        Ok(())
    }
}

impl SessionCatalogStore for LocalSessionStore {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if domain != self.config.domain {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: domain,
            });
        }
        self.catalog.list(&query)
    }

    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        if session_id.domain() != self.config.domain {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: session_id.domain(),
            });
        }
        self.catalog.get(session_id)
    }
}

impl ProjectSessionStore for LocalSessionStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        mut query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if self.config.domain != SessionDomain::Agent {
            return Err(CatalogError::DomainMismatch {
                expected: self.config.domain,
                actual: SessionDomain::Agent,
            });
        }
        query.project_id = Some(project_id.to_string());
        self.catalog.list(&query)
    }

    fn load_project_session(
        &self,
        project_id: &str,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        if session_id.domain() != SessionDomain::Agent {
            return Err(SessionError::DomainMismatch {
                header: SessionDomain::Agent,
                id: session_id.domain(),
            });
        }
        let (header, entries) = self.load_header_and_entries_for_session(session_id)?;
        let actual = header
            .project
            .as_ref()
            .ok_or(SessionError::AgentMissingProject)?;
        if actual.project_id != project_id {
            return Err(SessionError::ProjectMismatch {
                session_id: session_id.clone(),
                expected: project_id.to_string(),
                actual: actual.project_id.clone(),
            });
        }
        resolve_session(&entries, leaf)
    }
}

impl ChatMessageReferenceStore for LocalSessionStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        if self.config.domain != SessionDomain::Chat {
            return Err(ChatReferenceError::Catalog(CatalogError::DomainMismatch {
                expected: SessionDomain::Chat,
                actual: self.config.domain,
            }));
        }
        let limit = query.bounded_limit();
        let mut rows = self
            .catalog
            .search_message_nodes(&query.text, query.cursor.as_ref(), limit)
            .map_err(ChatReferenceError::Catalog)?;
        let has_more = rows.len() > limit;
        if has_more {
            rows.pop();
        }
        let next_cursor = has_more
            .then(|| {
                rows.last().map(|row| ChatMessageSearchCursor {
                    timestamp: row.timestamp,
                    session_id: row.session_id.clone(),
                    entry_id: row.entry_id.clone(),
                })
            })
            .flatten();
        let messages = rows
            .into_iter()
            .map(|row| {
                preview_from_node(
                    row.session_id,
                    row.entry_id,
                    row.timestamp,
                    row.session_title,
                    row.session_created_at,
                    row.message,
                )
            })
            .collect();
        Ok(ChatMessageSearchPage {
            messages,
            next_cursor,
        })
    }

    fn read_chat_message(
        &self,
        reference: &super::ChatMessageRef,
    ) -> Result<ChatMessageRead, ChatReferenceError> {
        validate_reference(reference)?;
        if self.config.domain != SessionDomain::Chat {
            return Err(ChatReferenceError::Catalog(CatalogError::DomainMismatch {
                expected: SessionDomain::Chat,
                actual: self.config.domain,
            }));
        }
        let summary = self
            .catalog
            .get(&reference.session_id)
            .map_err(ChatReferenceError::Catalog)?
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::SessionDeleted))?;
        let path = summary.jsonl_path.clone();
        let loaded = JsonlLoader::load(&path).map_err(|error| match error {
            SessionError::Io { source } if source.kind() == std::io::ErrorKind::NotFound => {
                unavailable(reference, ChatMessageUnavailableReason::SessionDeleted)
            }
            _ => unavailable(reference, ChatMessageUnavailableReason::SourceCorrupt),
        })?;
        if !loaded.diagnostics.is_empty() || loaded.truncated_tail {
            return Err(unavailable(
                reference,
                ChatMessageUnavailableReason::SourceCorrupt,
            ));
        }
        match loaded.header() {
            Ok(header) if header.session_id == reference.session_id => {}
            _ => {
                return Err(unavailable(
                    reference,
                    ChatMessageUnavailableReason::SourceCorrupt,
                ));
            }
        }
        let entry = loaded
            .entries
            .iter()
            .find(|entry| entry.id == reference.entry_id)
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::MessageDeleted))?;
        message_from_entry(reference, &summary, entry)
    }
}

fn collect_jsonl_paths(root: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut pending = vec![root.to_path_buf()];
    let mut result = Vec::new();
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(directory)? {
            let path = item?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            {
                result.push(path);
            }
        }
    }
    Ok(result)
}

fn session_io_error(error: impl std::fmt::Display) -> SessionError {
    SessionError::Io {
        source: std::io::Error::other(error.to_string()),
    }
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::llm::{ContentBlock, Message, ModelSelection, Role, Usage};
    use crate::session::{
        BranchSummary, Compaction, ConfigChange, InMemorySessionStore, JsonlWriter,
        ProjectIdentity, SessionStore, TranscriptReplay,
    };

    fn message(text: &str) -> SessionEntryKind {
        SessionEntryKind::Message(super::super::MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: text.into(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: None,
            model: None,
            usage: Usage::default(),
        })
    }

    fn message_with_metadata(
        text: &str,
        model: Option<ModelSelection>,
        tokens: u64,
    ) -> SessionEntryKind {
        SessionEntryKind::Message(super::super::MessageEntry {
            message: Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: text.into(),
                    provider_metadata: Default::default(),
                }],
                provider_metadata: Default::default(),
            },
            turn_id: Some("turn-1".into()),
            model,
            usage: Usage {
                total_tokens: tokens,
                ..Usage::default()
            },
        })
    }

    fn exercise_agent_tree_contract<S>(store: &mut S, project: ProjectIdentity) -> SessionId
    where
        S: SessionStore + SessionCatalogStore,
    {
        let project_id = project.project_id.clone();
        let mut header = SessionHeader::new(SessionDomain::Agent, Some(project));
        header.initial_model = Some(ModelSelection {
            profile_id: "initial-profile".into(),
            model_id: "initial-model".into(),
        });
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create agent session");
        store
            .append(
                &session_id,
                vec![SessionEntryKind::ConfigChange(ConfigChange {
                    model: ModelSelection {
                        profile_id: "current-profile".into(),
                        model_id: "current-model".into(),
                    },
                    system_prompt: Some("current system".into()),
                })],
            )
            .expect("append config");
        let root = store
            .append(&session_id, vec![message("root")])
            .expect("append root")[0]
            .clone();
        let original = store
            .append(&session_id, vec![message("original")])
            .expect("append original")[0]
            .clone();
        store
            .set_leaf(&session_id, Some(&root))
            .expect("select root");
        store
            .append(
                &session_id,
                vec![SessionEntryKind::BranchSummary(BranchSummary {
                    from_id: original.clone(),
                    summary: "summarized original branch".into(),
                })],
            )
            .expect("append branch summary");
        let replacement = store
            .append(&session_id, vec![message("replacement")])
            .expect("append replacement")[0]
            .clone();
        store
            .append(
                &session_id,
                vec![SessionEntryKind::TranscriptReplay(
                    TranscriptReplay::TerminalSnapshot {
                        terminal_id: "terminal-1".into(),
                        title: Some("cargo test".into()),
                        content: "ok".into(),
                    },
                )],
            )
            .expect("append transcript replay");
        store
            .append(
                &session_id,
                vec![SessionEntryKind::Compaction(Compaction {
                    summary: "older work".into(),
                    first_kept_entry_id: replacement.clone(),
                    tokens_before: 50,
                })],
            )
            .expect("append compaction");

        let state = store
            .load_session(&session_id, None)
            .expect("restore agent");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].entry_id, replacement);
        assert_eq!(state.transcript_replays.len(), 1);
        assert_eq!(
            state.latest_compaction.expect("compaction").tokens_before,
            50
        );
        assert_eq!(
            state.latest_config.expect("config").model.model_id,
            "current-model"
        );

        let tree = store.load_session_tree(&session_id).expect("tree");
        assert_eq!(tree.rows[0].branch_choices.len(), 2);
        let branches = store.load_branch_tree(&session_id).expect("branch tree");
        assert_eq!(branches.nodes.len(), 3);
        let preview = store
            .load_branch_preview(&session_id, &original)
            .expect("branch preview");
        assert_eq!(preview.common_parent_id, Some(root));
        assert_eq!(preview.snapshot.rows.len(), 2);

        let page = store
            .list_sessions(
                SessionDomain::Agent,
                CatalogQuery {
                    project_id: Some(project_id.clone()),
                    ..CatalogQuery::first_page()
                },
            )
            .expect("project catalog");
        assert_eq!(page.sessions.len(), 1);
        assert!(
            store
                .list_sessions(
                    SessionDomain::Agent,
                    CatalogQuery {
                        project_id: Some("project-018f0000-0000-7000-8000-000000000000".into()),
                        ..CatalogQuery::first_page()
                    },
                )
                .expect("unknown project")
                .sessions
                .is_empty()
        );
        session_id
    }

    fn assert_project_scoped_restore_isolation<S>(store: &mut S)
    where
        S: SessionStore + ProjectSessionStore,
    {
        let project_a = ProjectIdentity::new("/tmp/agent-project-a", "Agent Project A");
        let project_b = ProjectIdentity::new("/tmp/agent-project-b", "Agent Project B");
        let project_a_id = project_a.project_id.clone();
        let project_b_id = project_b.project_id.clone();

        let header_a = SessionHeader::new(SessionDomain::Agent, Some(project_a));
        let session_a = header_a.session_id.clone();
        store
            .create_session(header_a)
            .expect("create project A session");
        store
            .append(&session_a, vec![message("project A discussion")])
            .expect("append project A message");

        let header_b = SessionHeader::new(SessionDomain::Agent, Some(project_b));
        let session_b = header_b.session_id.clone();
        store
            .create_session(header_b)
            .expect("create project B session");

        let page = store
            .list_project_sessions(&project_a_id, CatalogQuery::first_page())
            .expect("list project A sessions");
        assert_eq!(page.sessions.len(), 1);
        assert_eq!(page.sessions[0].session_id, session_a);

        let restored = store
            .load_project_session(&project_a_id, &session_a, None)
            .expect("restore project A session");
        assert_eq!(restored.messages.len(), 1);
        assert!(matches!(
            store.load_project_session(&project_a_id, &session_b, None),
            Err(SessionError::ProjectMismatch {
                expected,
                actual,
                ..
            }) if expected == project_a_id && actual == project_b_id
        ));
    }

    #[test]
    fn memory_and_local_agent_stores_share_tree_and_replay_contracts() {
        let project = ProjectIdentity::new("/tmp/agent-project", "Agent Project");
        let mut memory = InMemorySessionStore::new();
        exercise_agent_tree_contract(&mut memory, project.clone());

        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Agent);
        let mut local = LocalSessionStore::open(config.clone()).expect("open local agent");
        let session_id = exercise_agent_tree_contract(&mut local, project.clone());
        local.shutdown().expect("shutdown local agent");

        let reopened = LocalSessionStore::open(config).expect("reopen local agent");
        assert_eq!(
            reopened
                .load_branch_tree(&session_id)
                .expect("reloaded branch tree")
                .nodes
                .len(),
            3
        );
    }

    #[test]
    fn project_scoped_restore_rejects_sessions_from_another_project() {
        let mut memory = InMemorySessionStore::new();
        assert_project_scoped_restore_isolation(&mut memory);

        let root = tempfile::tempdir().expect("tempdir");
        let mut local =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
                .expect("open local agent");
        assert_project_scoped_restore_isolation(&mut local);
    }

    #[test]
    fn local_chat_store_round_trips_and_lists_without_read_timestamp_mutation() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config).expect("open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let session_id = header.session_id.clone();
        store.create_session(header).expect("create");
        store
            .append(&session_id, vec![message("hello")])
            .expect("append");
        let before = store
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions;
        let state = store.load_session(&session_id, None).expect("load");
        let after = store
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions;
        assert_eq!(state.messages.len(), 1);
        assert_eq!(before, after);
        assert_eq!(before[0].preview.as_deref(), Some("hello"));
        assert_eq!(before[0].title, "hello");
        store.shutdown().expect("shutdown");

        let mut reopened =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("reopen");
        assert_eq!(
            reopened
                .load_session(&session_id, None)
                .expect("reload")
                .messages
                .len(),
            1
        );
        reopened.shutdown().expect("shutdown");
    }

    #[test]
    fn chat_and_agent_roots_and_catalogs_are_isolated() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut chat =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("chat");
        let mut agent =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
                .expect("agent");
        let chat_header = SessionHeader::new(SessionDomain::Chat, None);
        let chat_id = chat_header.session_id.clone();
        chat.create_session(chat_header).expect("chat create");
        let agent_header = SessionHeader::new(
            SessionDomain::Agent,
            Some(super::super::ProjectIdentity::new(
                "/tmp/project",
                "project",
            )),
        );
        let agent_id = agent_header.session_id.clone();
        agent.create_session(agent_header).expect("agent create");
        assert_ne!(chat.catalog_path(), agent.catalog_path());
        let chat_index = rusqlite::Connection::open(chat.catalog_path()).expect("chat index");
        let chat_projects: i64 = chat_index
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
                [],
                |row| row.get(0),
            )
            .expect("chat schema");
        let agent_index = rusqlite::Connection::open(agent.catalog_path()).expect("agent index");
        let agent_projects: i64 = agent_index
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'projects'",
                [],
                |row| row.get(0),
            )
            .expect("agent schema");
        assert_eq!(chat_projects, 0);
        assert_eq!(agent_projects, 1);
        assert_eq!(
            chat.list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .len(),
            1
        );
        assert_eq!(
            agent
                .list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .len(),
            1
        );
        assert!(chat.load_session(&agent_id, None).is_err());
        assert!(agent.load_session(&chat_id, None).is_err());
    }

    #[test]
    fn pagination_uses_creation_cursor_without_duplicates() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open");
        for index in 0..35 {
            let mut header = SessionHeader::new(SessionDomain::Chat, None);
            header.created_at = index;
            store.create_session(header).expect("create");
        }
        let first = store
            .list(CatalogQuery::with_limit(30))
            .expect("first page");
        let second = store
            .list(CatalogQuery {
                cursor: first.next_cursor.clone(),
                ..CatalogQuery::with_limit(30)
            })
            .expect("second page");
        assert_eq!(first.sessions.len(), 30);
        assert_eq!(second.sessions.len(), 5);
        let first_ids = first
            .sessions
            .iter()
            .map(|session| session.session_id.clone())
            .collect::<std::collections::HashSet<_>>();
        assert!(
            second
                .sessions
                .iter()
                .all(|session| !first_ids.contains(&session.session_id))
        );
    }

    #[test]
    fn repair_rebuilds_a_deleted_catalog_and_delete_is_permanent() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config.clone()).expect("open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let source = store
            .list(CatalogQuery::first_page())
            .expect("list")
            .sessions[0]
            .jsonl_path
            .clone();
        fs::remove_file(store.catalog_path()).expect("remove index");
        drop(store);
        let mut rebuilt = LocalSessionStore::open(config).expect("reopen");
        assert!(
            rebuilt
                .list(CatalogQuery::first_page())
                .expect("empty")
                .sessions
                .is_empty()
        );
        let report = rebuilt.repair().expect("repair");
        assert_eq!(report.rebuilt, 1);
        assert!(
            rebuilt
                .list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .len()
                == 1
        );
        rebuilt.delete_session(&id).expect("delete");
        assert!(!source.exists());
        assert!(
            rebuilt
                .list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .is_empty()
        );
        let index = rusqlite::Connection::open(rebuilt.catalog_path()).expect("index");
        let message_nodes: i64 = index
            .query_row(
                "SELECT COUNT(*) FROM message_nodes WHERE session_id = ?1",
                rusqlite::params![id.to_string()],
                |row| row.get(0),
            )
            .expect("message projection count");
        assert_eq!(message_nodes, 0);
    }

    #[test]
    fn catalog_projects_metadata_and_filters_agent_projects() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
                .expect("open");
        let project = super::super::ProjectIdentity::new(root.path().join("project"), "Demo");
        let project_id = project.project_id.clone();
        let mut header = SessionHeader::new(SessionDomain::Agent, Some(project));
        header.created_at = 10;
        header.initial_model = Some(ModelSelection {
            profile_id: "profile".into(),
            model_id: "model".into(),
        });
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        store
            .append(
                &id,
                vec![message_with_metadata(
                    "indexed discussion",
                    Some(ModelSelection {
                        profile_id: "profile-2".into(),
                        model_id: "model-2".into(),
                    }),
                    7,
                )],
            )
            .expect("append");
        let summary = store.get_summary(&id).expect("summary").expect("row");
        assert_eq!(summary.title, "indexed discussion");
        assert_eq!(summary.preview.as_deref(), Some("indexed discussion"));
        assert_eq!(summary.total_tokens, 7);
        assert_eq!(
            summary.model.as_ref().map(|model| model.model_id.as_str()),
            Some("model-2")
        );
        assert_eq!(
            store
                .list(CatalogQuery {
                    project_id: Some(project_id),
                    ..CatalogQuery::first_page()
                })
                .expect("filtered list")
                .sessions
                .len(),
            1
        );
    }

    #[test]
    fn agent_project_location_updates_keep_sessions_in_the_same_stable_bucket() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Agent))
                .expect("open");
        let first_project = ProjectIdentity::new(root.path().join("first"), "First");
        let project_id = first_project.project_id.clone();
        let first_header = SessionHeader::new(SessionDomain::Agent, Some(first_project));
        store
            .create_session(first_header)
            .expect("first agent session");

        let moved_project = ProjectIdentity::from_parts(
            project_id.clone(),
            root.path().join("moved"),
            "Moved project",
        )
        .expect("moved project");
        let second_header = SessionHeader::new(SessionDomain::Agent, Some(moved_project.clone()));
        store
            .create_session(second_header)
            .expect("second agent session");
        let page = store
            .list(CatalogQuery {
                project_id: Some(project_id.clone()),
                ..CatalogQuery::first_page()
            })
            .expect("project page");
        assert_eq!(page.sessions.len(), 2);
        assert!(page.sessions.iter().all(|summary| {
            summary.project.as_ref().map(|project| &project.project_id) == Some(&project_id)
        }));

        let index = rusqlite::Connection::open(store.catalog_path()).expect("index");
        let stored_path: String = index
            .query_row(
                "SELECT canonical_path FROM projects WHERE project_id = ?1",
                rusqlite::params![project_id],
                |row| row.get(0),
            )
            .expect("project registry row");
        assert_eq!(stored_path, moved_project.canonical_path.to_string_lossy());
    }

    #[test]
    fn repair_reindexes_external_append_and_removes_missing_sources() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config.clone()).expect("open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let path = store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .jsonl_path;
        JsonlWriter::open(&path)
            .expect("writer")
            .append(message("external append"))
            .expect("append");
        assert_eq!(
            store
                .get_summary(&id)
                .expect("summary")
                .expect("row")
                .preview,
            None
        );
        let report = store.repair().expect("repair");
        assert_eq!(report.rebuilt, 1);
        assert_eq!(
            store
                .get_summary(&id)
                .expect("summary")
                .expect("row")
                .preview
                .as_deref(),
            Some("external append")
        );
        fs::remove_file(path).expect("remove source");
        let report = store.repair().expect("repair stale");
        assert_eq!(report.removed, 1);
        assert!(store.get_summary(&id).expect("summary").is_none());
    }

    #[test]
    fn repair_reports_corrupt_jsonl_without_blocking_other_sessions() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config).expect("open");
        let first = SessionHeader::new(SessionDomain::Chat, None);
        let first_id = first.session_id.clone();
        store.create_session(first).expect("first create");
        let second = SessionHeader::new(SessionDomain::Chat, None);
        let second_id = second.session_id.clone();
        store.create_session(second).expect("second create");
        let first_path = store
            .get_summary(&first_id)
            .expect("summary")
            .expect("first row")
            .jsonl_path;
        fs::OpenOptions::new()
            .append(true)
            .open(first_path)
            .expect("open source")
            .write_all(b"{not-json}\n")
            .expect("append corrupt line");

        let report = store.repair().expect("repair");
        assert_eq!(report.rebuilt, 2);
        assert!(!report.issues.is_empty());
        assert!(
            store
                .get_summary(&first_id)
                .expect("first summary")
                .is_some()
        );
        assert!(
            store
                .get_summary(&second_id)
                .expect("second summary")
                .is_some()
        );
    }

    #[test]
    fn corrupt_catalog_is_rebuilt_without_losing_jsonl_sources() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut store = LocalSessionStore::open(config.clone()).expect("open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let source = store
            .get_summary(&id)
            .expect("summary")
            .expect("row")
            .jsonl_path;
        store.shutdown().expect("shutdown");
        fs::write(config.index_path(), b"not a sqlite database").expect("corrupt index");
        let mut reopened = LocalSessionStore::open(config).expect("reopen");
        assert!(
            reopened
                .list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .is_empty()
        );
        assert!(source.exists());
        assert!(reopened.repair().expect("repair").rebuilt == 1);
        assert_eq!(
            reopened
                .list(CatalogQuery::first_page())
                .expect("list")
                .sessions
                .len(),
            1
        );
    }

    #[test]
    fn handle_cache_is_bounded_and_delete_closes_active_handle() {
        let root = tempfile::tempdir().expect("tempdir");
        let config =
            LocalStoreConfig::new(root.path(), SessionDomain::Chat).with_max_open_handles(1);
        let mut store = LocalSessionStore::open(config).expect("open");
        let mut ids = Vec::new();
        for _ in 0..3 {
            let header = SessionHeader::new(SessionDomain::Chat, None);
            ids.push(header.session_id.clone());
            store.create_session(header).expect("create");
            assert!(store.open_handle_count() <= 1);
        }
        store.delete_session(&ids[2]).expect("delete");
        assert!(store.open_handle_count() <= 1);
        assert!(store.get_summary(&ids[2]).expect("summary").is_none());
    }

    #[test]
    fn flush_retries_pending_entries_and_is_idempotent() {
        let root = tempfile::tempdir().expect("tempdir");
        let mut store =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        store.fail_next_append_for_test(&id);
        assert!(store.append(&id, vec![message("pending")]).is_err());
        store.flush().expect("retry pending");
        store.flush().expect("second flush");
        assert_eq!(
            store
                .get_summary(&id)
                .expect("summary")
                .expect("row")
                .preview
                .as_deref(),
            Some("pending")
        );
        store.shutdown().expect("shutdown");
        assert_eq!(store.open_handle_count(), 0);
    }

    fn exercise_store_contract(store: &mut dyn super::super::SessionStore) {
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let ids = store
            .append(&id, vec![message("first"), message("second")])
            .expect("append");
        assert_eq!(
            store.load_session(&id, None).expect("load").messages.len(),
            2
        );
        store.set_leaf(&id, Some(&ids[0])).expect("set leaf");
        assert_eq!(
            store.load_session(&id, None).expect("load").messages.len(),
            1
        );
        store.flush().expect("flush");
        store.shutdown().expect("shutdown");
    }

    #[test]
    fn memory_and_local_stores_share_the_lifecycle_contract() {
        let mut memory = super::super::InMemorySessionStore::new();
        exercise_store_contract(&mut memory);

        let root = tempfile::tempdir().expect("tempdir");
        let mut local =
            LocalSessionStore::open(LocalStoreConfig::new(root.path(), SessionDomain::Chat))
                .expect("open");
        exercise_store_contract(&mut local);
    }

    #[test]
    fn concurrent_flushes_keep_the_catalog_and_source_readable() {
        let root = tempfile::tempdir().expect("tempdir");
        let config = LocalStoreConfig::new(root.path(), SessionDomain::Chat);
        let mut seed = LocalSessionStore::open(config.clone()).expect("seed open");
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        seed.create_session(header).expect("create");
        seed.append(&id, vec![message("concurrent")])
            .expect("append");
        seed.shutdown().expect("seed shutdown");

        let mut left = LocalSessionStore::open(config.clone()).expect("left open");
        let mut right = LocalSessionStore::open(config.clone()).expect("right open");
        left.ensure_handle(&id).expect("left handle");
        right.ensure_handle(&id).expect("right handle");
        std::thread::scope(|scope| {
            let left_flush = scope.spawn(|| left.flush());
            let right_flush = scope.spawn(|| right.flush());
            assert!(left_flush.join().expect("left thread").is_ok());
            assert!(right_flush.join().expect("right thread").is_ok());
        });

        let reopened = LocalSessionStore::open(config).expect("reopen");
        assert_eq!(
            reopened
                .load_session(&id, None)
                .expect("load")
                .messages
                .len(),
            1
        );
    }
}
