use std::{cell::RefCell, rc::Rc};

use gpui::Global;

use super::{
    InMemorySessionStore, LocalSessionStore, SessionDomain, SessionError, SessionFlushStore,
    SessionLifecycleStore, SessionStore, SessionTreeStore,
};

#[derive(Clone)]
pub struct SharedSessionStore(Rc<RefCell<Box<dyn SessionStore>>>);

impl SharedSessionStore {
    #[must_use]
    pub fn new(store: impl SessionStore + 'static) -> Self {
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
    pub fn with_chat_store(store: impl SessionStore + 'static) -> Self {
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
