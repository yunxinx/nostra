use std::{cell::RefCell, rc::Rc};

use gpui::Global;

use super::{
    CatalogError, CatalogPage, CatalogQuery, InMemorySessionStore, LocalSessionStore,
    SessionCatalogStore, SessionDomain, SessionError, SessionFlushStore, SessionId,
    SessionLifecycleStore, SessionStore, SessionSummary, SessionTreeStore,
};

trait SessionServiceStore: SessionStore + SessionCatalogStore {}

impl<T> SessionServiceStore for T where T: SessionStore + SessionCatalogStore {}

#[derive(Clone)]
pub struct SharedSessionStore(Rc<RefCell<Box<dyn SessionServiceStore>>>);

impl SharedSessionStore {
    #[must_use]
    pub fn new(store: impl SessionStore + SessionCatalogStore + 'static) -> Self {
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
}

impl Global for SessionStores {}

impl SessionStores {
    #[must_use]
    pub fn with_chat_store(store: impl SessionStore + SessionCatalogStore + 'static) -> Self {
        Self {
            chat: Some(SharedSessionStore::new(store)),
        }
    }

    #[must_use]
    pub fn open_default() -> Self {
        match LocalSessionStore::open_default(SessionDomain::Chat) {
            Ok(store) => Self::with_chat_store(store),
            Err(error) => {
                eprintln!("failed to open persistent chat store, using memory fallback: {error:?}");
                Self::with_chat_store(InMemorySessionStore::new())
            }
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

    pub fn flush(&mut self) -> Result<(), SessionError> {
        if let Some(store) = &mut self.chat {
            store.flush()?;
        }
        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<(), SessionError> {
        if let Some(store) = &mut self.chat {
            store.shutdown()?;
        }
        Ok(())
    }
}
