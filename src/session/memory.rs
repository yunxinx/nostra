use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use super::catalog::project_session_summary;
use super::{
    CatalogError, CatalogPage, CatalogQuery, ChatMessageRead, ChatMessageReferenceStore,
    ChatMessageSearchCursor, ChatMessageSearchPage, ChatMessageSearchQuery, ChatReferenceError,
    EntryId, ResolvedSessionState, SessionBranchPreview, SessionBranchTreeSnapshot, SessionDomain,
    SessionEntry, SessionEntryKind, SessionError, SessionHeader, SessionId, SessionSummary,
    SessionTreeSnapshot,
    reference::{
        ChatMessageUnavailableReason, message_from_entry, preview_from_node, unavailable,
        validate_reference,
    },
    resolve_session, session_branch_preview, session_branch_tree_snapshot, session_tree_snapshot,
    validate_appended_kind,
};

/// Capability boundary shared by the in-memory implementation and local
/// JSONL/SQLite stores. This interface is deliberately synchronous: the
/// GPUI adapter can schedule these operations off the render thread later.
pub trait SessionLifecycleStore {
    fn create_session(&mut self, header: SessionHeader) -> Result<SessionId, SessionError>;
    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError>;
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&EntryId>,
    ) -> Result<ResolvedSessionState, SessionError>;
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
}

pub trait SessionStore: SessionLifecycleStore + SessionTreeStore + SessionFlushStore {}

impl<T> SessionStore for T where T: SessionLifecycleStore + SessionTreeStore + SessionFlushStore {}

struct MemorySession {
    entries: Vec<SessionEntry>,
    leaf: Option<EntryId>,
    domain: SessionDomain,
}

#[derive(Default)]
pub struct InMemorySessionStore {
    sessions: HashMap<SessionId, MemorySession>,
    #[cfg(test)]
    append_calls: usize,
    #[cfg(test)]
    fail_append_at: Option<usize>,
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
        self.fail_append_at = Some(call.max(1));
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
        self.sessions.insert(
            id.clone(),
            MemorySession {
                entries: vec![header_entry],
                leaf,
                domain,
            },
        );
        Ok(id)
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<SessionEntryKind>,
    ) -> Result<Vec<EntryId>, SessionError> {
        #[cfg(test)]
        {
            self.append_calls = self.append_calls.saturating_add(1);
            if self.fail_append_at == Some(self.append_calls) {
                self.fail_append_at = None;
                return Err(SessionError::io(std::io::Error::other(
                    "injected in-memory append failure",
                )));
            }
        }
        let session = self
            .sessions
            .get_mut(session_id)
            .ok_or_else(|| SessionError::SessionNotFound(session_id.clone()))?;
        let mut parent = session.leaf.clone();
        let mut known_ids = session
            .entries
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<HashSet<_>>();
        let mut appended = Vec::with_capacity(entries.len());
        for kind in entries {
            if matches!(kind, SessionEntryKind::Header(_)) {
                return Err(SessionError::InvalidEntryKind);
            }
            validate_appended_kind(&kind, &known_ids, session.domain)?;
            let entry = SessionEntry::new(EntryId::new(), parent.clone(), kind);
            parent = match &entry.kind {
                SessionEntryKind::Leaf(leaf) => {
                    leaf.target_id.clone().or_else(|| Some(entry.id.clone()))
                }
                _ => Some(entry.id.clone()),
            };
            known_ids.insert(entry.id.clone());
            appended.push(entry);
        }
        let ids = appended.iter().map(|entry| entry.id.clone()).collect();
        session.entries.extend(appended);
        session.leaf = parent;
        Ok(ids)
    }

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
}

impl ChatMessageReferenceStore for InMemorySessionStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        let mut messages = Vec::new();
        let limit = query.bounded_limit();
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
                let safe = super::ReferencedMessage::from_message(&message.message);
                if !safe
                    .searchable_text()
                    .to_lowercase()
                    .contains(&query.text.to_lowercase())
                {
                    continue;
                }
                messages.push(preview_from_node(
                    summary.session_id.clone(),
                    entry.id.clone(),
                    entry.timestamp,
                    summary.title.clone(),
                    summary.created_at,
                    safe,
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
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        llm::{ContentBlock, Message, Role, Usage},
        session::{MessageEntry, SessionDomain},
    };

    fn message(text: &str) -> SessionEntryKind {
        SessionEntryKind::Message(MessageEntry {
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

    #[test]
    fn creates_appends_loads_and_flushes() {
        let mut store = InMemorySessionStore::new();
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let ids = store
            .append(&id, vec![message("hello"), message("world")])
            .expect("append");
        let state = store.load_session(&id, Some(&ids[1])).expect("load");
        assert_eq!(state.messages.len(), 2);
        store.flush().expect("flush");
        store.shutdown().expect("shutdown");
    }

    #[test]
    fn explicit_leaf_switch_does_not_change_history_facts() {
        let mut store = InMemorySessionStore::new();
        let header = SessionHeader::new(SessionDomain::Chat, None);
        let id = header.session_id.clone();
        store.create_session(header).expect("create");
        let first = store.append(&id, vec![message("first")]).expect("append")[0].clone();
        let _second = store.append(&id, vec![message("second")]).expect("append");
        let facts_before_switch = store.entries(&id).expect("entries").len();
        store.set_leaf(&id, Some(&first)).expect("switch leaf");
        let state = store.load_session(&id, None).expect("load");
        assert_eq!(state.messages.len(), 1);
        assert_eq!(state.messages[0].entry_id, first);
        let facts = store.entries(&id).expect("entries");
        assert_eq!(facts.len(), facts_before_switch + 1);
        assert!(matches!(
            facts.last().map(|entry| &entry.kind),
            Some(SessionEntryKind::Leaf(_))
        ));
    }
}
