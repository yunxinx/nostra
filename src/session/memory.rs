use std::collections::{HashMap, HashSet};

use super::{
    EntryId, ResolvedSessionState, SessionDomain, SessionEntry, SessionEntryKind, SessionError,
    SessionHeader, SessionId, resolve_session, validate_appended_kind,
};

/// Capability boundary shared by the in-memory implementation and future local
/// JSONL/SQLite stores.  This first version is deliberately synchronous: the
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
}

pub trait SessionFlushStore {
    fn flush(&mut self) -> Result<(), SessionError>;
    fn shutdown(&mut self) -> Result<(), SessionError>;
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
