use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

#[cfg(test)]
use std::sync::mpsc::{Receiver, SyncSender};

use super::catalog::project_session_summary;
use super::{
    AppendValidationState, CatalogError, CatalogPage, CatalogQuery, ChatMessageRead,
    ChatMessageReferenceStore, ChatMessageSearchCursor, ChatMessageSearchPage,
    ChatMessageSearchQuery, ChatReferenceError, EntryId, ResolvedSessionState,
    SessionBranchPreview, SessionBranchTreeSnapshot, SessionDomain, SessionEntry, SessionEntryKind,
    SessionError, SessionHeader, SessionId, SessionSummary, SessionTreeSnapshot,
    reference::{
        ChatMessageUnavailableReason, message_from_entry, preview_from_node,
        searchable_text_from_message, unavailable, validate_reference,
    },
    resolve_session, session_branch_preview, session_branch_tree_snapshot, session_tree_snapshot,
};

/// Read-only transcript restore capability. Catalog and reference adapters
/// use this narrower seam instead of receiving session mutation authority.
pub trait SessionReadStore {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError>;
}

/// Mutation boundary shared by the in-memory implementation and local
/// JSONL/SQLite stores. This interface is deliberately synchronous: GPUI
/// adapters schedule it off the render thread.
pub trait SessionLifecycleStore: SessionReadStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError>;
    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError>;
    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError>;
    /// Permanently remove one session and its searchable projection.
    ///
    /// Deletion is idempotent so a UI retry after an ambiguous completion
    /// cannot resurrect or strand a conversation.
    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError>;
}

pub trait SessionTreeStore {
    fn set_leaf(
        &mut self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<(), SessionError>;
    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError>;
    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError>;
    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &EntryId,
    ) -> Result<SessionBranchPreview, SessionError>;
    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError>;
}

pub trait SessionFlushStore {
    fn flush(&mut self) -> Result<(), SessionError>;
    fn shutdown(&mut self) -> Result<(), SessionError>;
}

/// Read-only catalog capability used by product code to page session metadata
/// without knowing whether the adapter is backed by SQLite or memory.
pub trait SessionCatalogStore {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError>;
    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError>;
}

/// Agent callers use this capability instead of an unscoped session id. It
/// keeps a stable project identity as a restore boundary while leaving Chat
/// sessions project-free.
pub trait ProjectSessionStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError>;
    fn load_project_session(
        &self,
        project_id: &str,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError>;
    fn get_project_identity(
        &self,
        project_id: &str,
    ) -> Result<Option<super::ProjectIdentity>, CatalogError>;
    /// Page over persisted Agent projects in stable `(updated_at DESC,
    /// project_id DESC)` order.  The catalog derives counts and last-updated
    /// timestamps from session rows; callers never read JSONL or the
    /// `projects` table directly.
    fn list_projects(
        &self,
        query: super::ProjectCatalogQuery,
    ) -> Result<super::ProjectCatalogPage, CatalogError>;
}

pub trait SessionStore: SessionLifecycleStore + SessionTreeStore + SessionFlushStore {}

impl<T> SessionStore for T where T: SessionLifecycleStore + SessionTreeStore + SessionFlushStore {}

struct MemorySession {
    entries: Vec<SessionEntry>,
    leaf: Option<EntryId>,
    domain: SessionDomain,
    validation: AppendValidationState,
}

#[derive(Default)]
pub struct InMemorySessionStore {
    sessions: HashMap<SessionId, MemorySession>,
    #[cfg(test)]
    append_calls: usize,
    #[cfg(test)]
    fail_append_calls: HashSet<usize>,
    #[cfg(test)]
    fail_next_delete: bool,
    #[cfg(test)]
    append_success_notifier: Option<SyncSender<usize>>,
    #[cfg(test)]
    create_after_commit_notifier: Option<SyncSender<()>>,
    #[cfg(test)]
    flush_started_notifier: Option<SyncSender<()>>,
    #[cfg(test)]
    flush_release: Option<Receiver<()>>,
    #[cfg(test)]
    shutdown_started_notifier: Option<SyncSender<()>>,
    #[cfg(test)]
    shutdown_release: Option<Receiver<()>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn entries(&self, session_id: &SessionId) -> Result<&[SessionEntry], SessionError> {
        self.sessions
            .get(session_id)
            .map(|session| session.entries.as_slice())
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))
    }

    pub fn contains(&self, session_id: &SessionId) -> bool {
        self.sessions.contains_key(session_id)
    }

    #[cfg(test)]
    pub(crate) fn fail_append_at_for_test(&mut self, call: usize) {
        self.fail_append_calls.insert(call.max(1));
    }

    #[cfg(test)]
    pub(crate) fn fail_next_delete_for_test(&mut self) {
        self.fail_next_delete = true;
    }

    #[cfg(test)]
    pub(crate) fn observe_append_success_for_test(&mut self, completed: SyncSender<usize>) {
        self.append_success_notifier = Some(completed);
    }

    #[cfg(test)]
    pub(crate) fn notify_create_after_commit_for_test(&mut self, committed: SyncSender<()>) {
        self.create_after_commit_notifier = Some(committed);
    }

    #[cfg(test)]
    pub(crate) fn observe_shutdown_for_test(
        &mut self,
        started: SyncSender<()>,
        release: Option<Receiver<()>>,
    ) {
        self.shutdown_started_notifier = Some(started);
        self.shutdown_release = release;
    }

    #[cfg(test)]
    pub(crate) fn observe_flush_for_test(
        &mut self,
        started: SyncSender<()>,
        release: Option<Receiver<()>>,
    ) {
        self.flush_started_notifier = Some(started);
        self.flush_release = release;
    }

    fn summary(session: &MemorySession) -> Result<SessionSummary, CatalogError> {
        let Some(SessionEntry {
            kind: SessionEntryKind::Header(header),
            ..
        }) = session.entries.first()
        else {
            return Err(CatalogError::Corrupt(
                "in-memory session is missing its header".to_string(),
            ));
        };
        project_session_summary(header, &session.entries, PathBuf::new())
    }
}

impl SessionLifecycleStore for InMemorySessionStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError> {
        header.validate()?;
        let id = header.session_id.clone();
        if self.sessions.contains_key(&id) {
            return Err(SessionError::SessionAlreadyExists(id));
        }
        let domain = header.domain;
        let header_entry = SessionEntry::header(header);
        let leaf = Some(header_entry.id.clone());
        let validation = AppendValidationState::from_entries(std::slice::from_ref(&header_entry))?;
        self.sessions.insert(
            id.clone(),
            MemorySession {
                entries: vec![header_entry],
                leaf,
                domain,
                validation,
            },
        );
        Ok(id)
    }

    fn create_session_with_entries(
        &mut self,
        header: SessionHeader,
        entries: Vec<SessionEntryKind>,
    ) -> Result<(SessionId, Vec<EntryId>), SessionError> {
        header.validate()?;
        let id = header.session_id.clone();
        if self.sessions.contains_key(&id) {
            return Err(SessionError::SessionAlreadyExists(id));
        }
        let domain = header.domain;
        let header_entry = SessionEntry::header(header);
        let validation = AppendValidationState::from_entries(std::slice::from_ref(&header_entry))?;
        let mut candidate = MemorySession {
            leaf: Some(header_entry.id.clone()),
            entries: vec![header_entry],
            domain,
            validation,
        };
        let entry_ids = append_memory_entries(&mut candidate, entries)?;
        self.sessions.insert(id.clone(), candidate);
        #[cfg(test)]
        if let Some(committed) = self.create_after_commit_notifier.take() {
            // Expose the exact window where facts are durable but the caller
            // has not yet received success, so cancellation tests do not
            // depend on background-executor scheduling luck.
            let _ = committed.send(());
        }
        Ok((id, entry_ids))
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        #[cfg(test)]
        {
            self.append_calls = self.append_calls.saturating_add(1);
            if self.fail_append_calls.remove(&self.append_calls) {
                return Err(SessionError::io(std::io::Error::other(
                    "injected in-memory append failure",
                )));
            }
        }
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let result = append_memory_entries(session, entries);
        #[cfg(test)]
        if result.is_ok()
            && let Some(notifier) = self.append_success_notifier.take()
        {
            let _ = notifier.send(self.append_calls);
        }
        result
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_delete) {
            return Err(SessionError::io(std::io::Error::other(
                "injected in-memory delete failure",
            )));
        }
        self.sessions.remove(session_id);
        Ok(())
    }
}

impl SessionReadStore for InMemorySessionStore {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        resolve_session(&session.entries, leaf.or(session.leaf.as_ref()))
    }
}

fn append_memory_entries(
    session: &mut MemorySession,
    entries: Vec<SessionEntryKind>,
) -> Result<Vec<EntryId>, SessionError> {
    let mut parent = session.leaf.clone();
    let mut appended = Vec::with_capacity(entries.len());
    for kind in entries {
        if matches!(kind, SessionEntryKind::Header(_)) {
            return Err(SessionError::InvalidEntryKind);
        }
        let entry = SessionEntry::new(EntryId::new(), parent.clone(), kind);
        parent = match &entry.kind {
            SessionEntryKind::Leaf(leaf) => {
                leaf.target_id.clone().or_else(|| Some(entry.id.clone()))
            }
            _ => Some(entry.id.clone()),
        };
        appended.push(entry);
    }
    let validated = session
        .validation
        .validate_entries(appended.iter(), session.domain)?;
    let ids = appended.iter().map(|entry| entry.id.clone()).collect();
    session.validation.commit(validated);
    session.entries.extend(appended);
    session.leaf = parent;
    Ok(ids)
}

impl SessionTreeStore for InMemorySessionStore {
    fn set_leaf(
        &mut self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<(), SessionError> {
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        if let Some(leaf_id) = leaf
            && !session.entries.iter().any(|entry| &entry.id == leaf_id)
        {
            return Err(SessionError::LeafNotFound(leaf_id.clone()));
        }
        let entry = SessionEntry::new(
            EntryId::new(),
            session.leaf.clone(),
            SessionEntryKind::Leaf(super::Leaf {
                target_id: leaf.cloned(),
            }),
        );
        let validated = session
            .validation
            .validate_entries(std::slice::from_ref(&entry), session.domain)?;
        session.validation.commit(validated);
        session.entries.push(entry);
        session.leaf = Some(if let Some(leaf_id) = leaf {
            leaf_id.clone()
        } else {
            session
                .entries
                .last()
                .map(|entry| entry.id.clone())
                .ok_or(SessionError::MissingHeader)?
        });
        Ok(())
    }

    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        session_tree_snapshot(&session.entries, session.leaf.as_ref())
    }

    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        session_tree_snapshot(&session.entries, Some(leaf))
    }

    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &EntryId,
    ) -> Result<SessionBranchPreview, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        session_branch_preview(&session.entries, branch_root)
    }

    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError> {
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        session_branch_tree_snapshot(&session.entries, session.leaf.as_ref())
    }
}

impl SessionCatalogStore for InMemorySessionStore {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        if let Some(cursor) = &query.cursor
            && cursor.session_id.domain() != domain
        {
            return Err(CatalogError::DomainMismatch {
                expected: domain,
                actual: cursor.session_id.domain(),
            });
        }

        let mut sessions = self
            .sessions
            .values()
            .filter(|session| session.domain == domain)
            .map(Self::summary)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(project_id) = query.project_id.as_deref() {
            sessions.retain(|summary| {
                summary
                    .project
                    .as_ref()
                    .is_some_and(|project| project.project_id == project_id)
            });
        }
        if let Some(favorited) = query.favorited {
            sessions.retain(|summary| summary.favorited == favorited);
        }
        sessions.sort_by(|left, right| {
            right
                .created_at
                .cmp(&left.created_at)
                .then_with(|| right.session_id.cmp(&left.session_id))
        });
        if let Some(cursor) = &query.cursor {
            sessions.retain(|summary| {
                summary.created_at < cursor.created_at
                    || (summary.created_at == cursor.created_at
                        && summary.session_id < cursor.session_id)
            });
        }

        let limit = query.limit.max(1);
        let has_more = sessions.len() > limit;
        sessions.truncate(limit);
        let next_cursor = has_more
            .then(|| {
                sessions.last().map(|summary| super::CatalogCursor {
                    created_at: summary.created_at,
                    session_id: summary.session_id.clone(),
                })
            })
            .flatten();
        Ok(CatalogPage {
            sessions,
            next_cursor,
        })
    }

    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        self.sessions.get(session_id).map(Self::summary).transpose()
    }
}

impl ProjectSessionStore for InMemorySessionStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        mut query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        query.project_id = Some(project_id.to_string());
        self.list_sessions(SessionDomain::Agent, query)
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
        let session = self
            .sessions
            .get(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let Some(SessionEntry {
            kind: SessionEntryKind::Header(header),
            ..
        }) = session.entries.first()
        else {
            return Err(SessionError::MissingHeader);
        };
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
        resolve_session(&session.entries, leaf.or(session.leaf.as_ref()))
    }

    fn get_project_identity(
        &self,
        project_id: &str,
    ) -> Result<Option<super::ProjectIdentity>, CatalogError> {
        let mut winner: Option<(i64, &SessionId, super::ProjectIdentity)> = None;
        for (session_id, session) in &self.sessions {
            if session.domain != SessionDomain::Agent {
                continue;
            }
            let summary = Self::summary(session)?;
            let Some(project) = summary.project else {
                continue;
            };
            if project.project_id != project_id {
                continue;
            }
            let should_replace = winner.as_ref().is_none_or(|(updated_at, winner_id, _)| {
                (summary.updated_at, session_id) > (*updated_at, *winner_id)
            });
            if should_replace {
                winner = Some((summary.updated_at, session_id, project));
            }
        }
        Ok(winner.map(|(_, _, project)| project))
    }

    fn list_projects(
        &self,
        query: super::ProjectCatalogQuery,
    ) -> Result<super::ProjectCatalogPage, CatalogError> {
        use std::collections::HashMap as StdHashMap;

        // Aggregate per project: latest (updated_at, session_id) wins the
        // identity fields, matching the catalog's keyset order.
        let mut by_project: StdHashMap<String, (i64, SessionId, super::ProjectSummary)> =
            StdHashMap::new();
        for (session_id, session) in &self.sessions {
            if session.domain != SessionDomain::Agent {
                continue;
            }
            let summary = Self::summary(session)?;
            let Some(project) = summary.project else {
                continue;
            };
            match by_project.get_mut(&project.project_id) {
                Some((best_updated, best_id, existing)) => {
                    existing.session_count = existing.session_count.saturating_add(1);
                    if (summary.updated_at, session_id) > (*best_updated, best_id) {
                        *best_updated = summary.updated_at;
                        *best_id = session_id.clone();
                        existing.display_name = project.display_name.clone();
                        existing.canonical_path = project.canonical_path.clone();
                        existing.last_updated_at = summary.updated_at;
                    }
                }
                None => {
                    by_project.insert(
                        project.project_id.clone(),
                        (
                            summary.updated_at,
                            session_id.clone(),
                            super::ProjectSummary {
                                project_id: project.project_id.clone(),
                                display_name: project.display_name.clone(),
                                canonical_path: project.canonical_path.clone(),
                                session_count: 1,
                                last_updated_at: summary.updated_at,
                            },
                        ),
                    );
                }
            }
        }

        let mut projects: Vec<super::ProjectSummary> = by_project
            .into_values()
            .map(|(_, _, summary)| summary)
            .collect();
        projects.sort_by(|left, right| {
            right
                .last_updated_at
                .cmp(&left.last_updated_at)
                .then_with(|| right.project_id.cmp(&left.project_id))
        });
        if let Some(cursor) = &query.cursor {
            projects.retain(|summary| {
                summary.last_updated_at < cursor.updated_at
                    || (summary.last_updated_at == cursor.updated_at
                        && summary.project_id < cursor.project_id)
            });
        }

        let limit = query.limit.max(1);
        let has_more = projects.len() > limit;
        projects.truncate(limit);
        let next_cursor = has_more
            .then(|| {
                projects.last().map(|summary| super::ProjectCatalogCursor {
                    updated_at: summary.last_updated_at,
                    project_id: summary.project_id.clone(),
                })
            })
            .flatten();
        Ok(super::ProjectCatalogPage {
            projects,
            next_cursor,
        })
    }
}

impl ChatMessageReferenceStore for InMemorySessionStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        let mut messages = Vec::new();
        let limit = query.bounded_limit();
        let folded_query = query.text.to_lowercase();
        for session in self
            .sessions
            .values()
            .filter(|session| session.domain == SessionDomain::Chat)
        {
            let summary = Self::summary(session).map_err(ChatReferenceError::Catalog)?;
            let active_ids = resolve_session(&session.entries, session.leaf.as_ref())
                .map_err(ChatReferenceError::Storage)?
                .path
                .into_iter()
                .collect::<HashSet<_>>();
            for entry in &session.entries {
                if !active_ids.contains(&entry.id) {
                    continue;
                }
                let SessionEntryKind::Message(message) = &entry.kind else {
                    continue;
                };
                let searchable = searchable_text_from_message(&message.message);
                if !searchable.to_lowercase().contains(&folded_query) {
                    continue;
                }
                messages.push(preview_from_node(
                    summary.session_id.clone(),
                    entry.id.clone(),
                    entry.timestamp,
                    summary.title.clone(),
                    summary.created_at,
                    message.message.role,
                    super::tree::message_preview(&message.message),
                ));
            }
        }
        messages.sort_by(|left, right| {
            right
                .timestamp
                .cmp(&left.timestamp)
                .then_with(|| right.reference.session_id.cmp(&left.reference.session_id))
                .then_with(|| right.reference.entry_id.cmp(&left.reference.entry_id))
        });
        if let Some(cursor) = &query.cursor {
            messages.retain(|message| {
                message.timestamp < cursor.timestamp
                    || (message.timestamp == cursor.timestamp
                        && message.reference.session_id < cursor.session_id)
                    || (message.timestamp == cursor.timestamp
                        && message.reference.session_id == cursor.session_id
                        && message.reference.entry_id < cursor.entry_id)
            });
        }
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        let next_cursor = has_more
            .then(|| {
                messages.last().map(|message| ChatMessageSearchCursor {
                    timestamp: message.timestamp,
                    session_id: message.reference.session_id.clone(),
                    entry_id: message.reference.entry_id.clone(),
                })
            })
            .flatten();
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
        let session = self
            .sessions
            .get(&reference.session_id)
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::SessionDeleted))?;
        if session.domain != SessionDomain::Chat {
            return Err(ChatReferenceError::InvalidReference(
                SessionError::ReferenceSourceNotChat,
            ));
        }
        let summary = Self::summary(session).map_err(ChatReferenceError::Catalog)?;
        let entry = session
            .entries
            .iter()
            .find(|entry| entry.id == reference.entry_id)
            .ok_or_else(|| unavailable(reference, ChatMessageUnavailableReason::MessageDeleted))?;
        let active = resolve_session(&session.entries, session.leaf.as_ref())
            .map_err(ChatReferenceError::Storage)?;
        if !active.path.iter().any(|id| id == &reference.entry_id) {
            return Err(unavailable(
                reference,
                ChatMessageUnavailableReason::MessageDeleted,
            ));
        }
        message_from_entry(reference, &summary, entry)
    }
}

impl SessionFlushStore for InMemorySessionStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        #[cfg(test)]
        {
            if let Some(started) = self.flush_started_notifier.take() {
                let _ = started.send(());
            }
            if let Some(release) = self.flush_release.take() {
                let _ = release.recv();
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        #[cfg(test)]
        {
            if let Some(started) = self.shutdown_started_notifier.take() {
                let _ = started.send(());
            }
            if let Some(release) = self.shutdown_release.take() {
                let _ = release.recv();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
