use std::{cell::RefCell, rc::Rc};

use gpui::Global;

use super::{
    CatalogError, CatalogPage, CatalogQuery, InMemorySessionStore, LocalSessionStore,
    ProjectSessionStore, SessionBranchPreview, SessionBranchTreeSnapshot, SessionCatalogStore,
    SessionDomain, SessionError, SessionFlushStore, SessionId, SessionLifecycleStore, SessionStore,
    SessionSummary, SessionTreeSnapshot, SessionTreeStore,
};

trait SessionServiceStore: SessionStore + SessionCatalogStore + ProjectSessionStore {}

impl<T> SessionServiceStore for T where T: SessionStore + SessionCatalogStore + ProjectSessionStore {}

#[derive(Clone)]
pub struct SharedSessionStore(Rc<RefCell<Box<dyn SessionServiceStore>>>);

impl SharedSessionStore {
    #[must_use]
    pub fn new(
        store: impl SessionStore + SessionCatalogStore + ProjectSessionStore + 'static,
    ) -> Self {
        Self(Rc::new(RefCell::new(Box::new(store))))
    }
}

impl SessionLifecycleStore for SharedSessionStore {
    fn create_session(
        &mut self,
        header: super::SessionHeader,
    ) -> Result<super::SessionId, SessionError> {
        self.0.borrow_mut().create_session(header)
    }

    fn append(
        &mut self,
        session_id: &super::SessionId,
        entries: Vec<super::SessionEntryKind>,
    ) -> Result<Vec<super::EntryId>, SessionError> {
        self.0.borrow_mut().append(session_id, entries)
    }

    fn load_session(
        &self,
        session_id: &super::SessionId,
        leaf: Option<&super::EntryId>,
    ) -> Result<super::ResolvedSessionState, SessionError> {
        self.0.borrow().load_session(session_id, leaf)
    }
}

impl SessionTreeStore for SharedSessionStore {
    fn set_leaf(
        &mut self,
        session_id: &super::SessionId,
        leaf: Option<&super::EntryId>,
    ) -> Result<(), SessionError> {
        self.0.borrow_mut().set_leaf(session_id, leaf)
    }

    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        self.0.borrow().load_session_tree(session_id)
    }

    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &super::EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        self.0.borrow().load_session_tree_for_leaf(session_id, leaf)
    }

    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &super::EntryId,
    ) -> Result<SessionBranchPreview, SessionError> {
        self.0.borrow().load_branch_preview(session_id, branch_root)
    }

    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError> {
        self.0.borrow().load_branch_tree(session_id)
    }
}

impl SessionCatalogStore for SharedSessionStore {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        self.0.borrow().list_sessions(domain, query)
    }

    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        self.0.borrow().get_session_summary(session_id)
    }
}

impl ProjectSessionStore for SharedSessionStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        self.0.borrow().list_project_sessions(project_id, query)
    }

    fn load_project_session(
        &self,
        project_id: &str,
        session_id: &SessionId,
        leaf: Option<&super::EntryId>,
    ) -> Result<super::ResolvedSessionState, SessionError> {
        self.0
            .borrow()
            .load_project_session(project_id, session_id, leaf)
    }
}

impl SessionFlushStore for SharedSessionStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        self.0.borrow_mut().flush()
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        self.0.borrow_mut().shutdown()
    }
}

#[derive(Clone, Default)]
pub struct SessionStores {
    chat: Option<SharedSessionStore>,
    agent: Option<SharedSessionStore>,
}

impl Global for SessionStores {}

impl SessionStores {
    #[must_use]
    pub fn with_chat_store(
        store: impl SessionStore + SessionCatalogStore + ProjectSessionStore + 'static,
    ) -> Self {
        Self {
            chat: Some(SharedSessionStore::new(store)),
            agent: None,
        }
    }

    #[must_use]
    pub fn with_agent_store(
        store: impl SessionStore + SessionCatalogStore + ProjectSessionStore + 'static,
    ) -> Self {
        Self {
            chat: None,
            agent: Some(SharedSessionStore::new(store)),
        }
    }

    #[must_use]
    pub fn with_stores(
        chat: impl SessionStore + SessionCatalogStore + ProjectSessionStore + 'static,
        agent: impl SessionStore + SessionCatalogStore + ProjectSessionStore + 'static,
    ) -> Self {
        Self {
            chat: Some(SharedSessionStore::new(chat)),
            agent: Some(SharedSessionStore::new(agent)),
        }
    }

    #[must_use]
    pub fn open_default() -> Self {
        let chat = match LocalSessionStore::open_default(SessionDomain::Chat) {
            Ok(store) => SharedSessionStore::new(store),
            Err(error) => {
                eprintln!("failed to open persistent chat store, using memory fallback: {error:?}");
                SharedSessionStore::new(InMemorySessionStore::new())
            }
        };
        let agent = match LocalSessionStore::open_default(SessionDomain::Agent) {
            Ok(store) => SharedSessionStore::new(store),
            Err(error) => {
                eprintln!(
                    "failed to open persistent agent store, using memory fallback: {error:?}"
                );
                SharedSessionStore::new(InMemorySessionStore::new())
            }
        };
        Self {
            chat: Some(chat),
            agent: Some(agent),
        }
    }

    #[must_use]
    pub fn chat(&self) -> Option<SharedSessionStore> {
        self.chat.clone()
    }

    /// Return the same shared Chat adapter through its catalog capability.
    /// The future sidebar can page metadata without opening transcripts or
    /// reaching for a concrete local store.
    #[must_use]
    pub fn chat_catalog(&self) -> Option<SharedSessionStore> {
        self.chat.clone()
    }

    #[must_use]
    pub fn agent(&self) -> Option<SharedSessionStore> {
        self.agent.clone()
    }

    #[must_use]
    pub fn agent_catalog(&self) -> Option<SharedSessionStore> {
        self.agent.clone()
    }

    pub fn flush(&mut self) -> Result<(), SessionError> {
        if let Some(store) = &mut self.chat {
            store.flush()?;
        }
        if let Some(store) = &mut self.agent {
            store.flush()?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), SessionError> {
        if let Some(store) = &mut self.chat {
            store.shutdown()?;
        }
        if let Some(store) = &mut self.agent {
            store.shutdown()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{ProjectIdentity, SessionHeader};

    #[test]
    fn shared_agent_store_keeps_project_scoped_restore_capability() {
        let project_a = ProjectIdentity::new("/tmp/project-a", "Project A");
        let project_b = ProjectIdentity::new("/tmp/project-b", "Project B");
        let project_a_id = project_a.project_id.clone();
        let project_b_id = project_b.project_id.clone();

        let header = SessionHeader::new(SessionDomain::Agent, Some(project_b));
        let session_id = header.session_id.clone();
        let mut backing = InMemorySessionStore::new();
        backing
            .create_session(header)
            .expect("create agent session");
        let store = SharedSessionStore::new(backing);

        assert!(matches!(
            store.load_project_session(&project_a_id, &session_id, None),
            Err(SessionError::ProjectMismatch {
                expected,
                actual,
                ..
            }) if expected == project_a_id && actual == project_b_id
        ));
    }
}
