use std::sync::Arc;

use super::super::{
    CatalogError, CatalogPage, CatalogQuery, ChatMessageRead, ChatMessageReferenceStore,
    ChatMessageSearchPage, ChatMessageSearchQuery, ChatReferenceError, ProjectIdentity,
    ProjectSessionStore, SessionBranchPreview, SessionBranchTreeSnapshot, SessionCatalogStore,
    SessionDomain, SessionError, SessionFlushStore, SessionId, SessionLifecycleStore,
    SessionReadStore, SessionStore, SessionStoresError, SessionSummary, SessionTreeSnapshot,
    SessionTreeStore,
};
use super::core::{OperationPermit, SessionOperationGuard, SharedStoreCore};

/// Session capabilities needed by one conversation and its optional reference
/// completion surface. The lifecycle store is scoped to Chat or Agent, while
/// references always read from the Chat domain.
#[derive(Clone)]
pub struct ConversationSessionServices {
    lifecycle: Result<SharedSessionStore, SessionStoresError>,
    references: Option<SharedChatReferenceStore>,
}

impl ConversationSessionServices {
    pub(super) fn new(
        lifecycle: Result<SharedSessionStore, SessionStoresError>,
        references: Option<SharedChatReferenceStore>,
    ) -> Self {
        Self {
            lifecycle,
            references,
        }
    }

    pub fn lifecycle(&self) -> Result<SharedSessionStore, SessionStoresError> {
        self.lifecycle.clone()
    }

    #[must_use]
    pub fn references(&self) -> Option<SharedChatReferenceStore> {
        self.references.clone()
    }
}

/// Mutable lifecycle capability for one session domain.
///
/// Catalog, project, and Chat-reference access use separate wrappers below,
/// so handing a read-only feature its adapter cannot accidentally grant it
/// append or delete authority.
#[derive(Clone)]
pub struct SharedSessionStore {
    core: SharedStoreCore,
    permit: Option<Arc<OperationPermit>>,
    domain: SessionDomain,
}

impl SharedSessionStore {
    #[must_use]
    pub fn new(
        domain: SessionDomain,
        store: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
    ) -> Self {
        Self {
            core: SharedStoreCore::new(store),
            permit: None,
            domain,
        }
    }

    pub(crate) fn reserve_operation(&self) -> Result<SessionOperationGuard, SessionError> {
        self.core.reserve_operation(self.domain)
    }

    pub(super) fn from_core(core: SharedStoreCore, domain: SessionDomain) -> Self {
        Self {
            core,
            permit: None,
            domain,
        }
    }

    pub(super) fn from_reserved_core(
        core: SharedStoreCore,
        permit: Arc<OperationPermit>,
        domain: SessionDomain,
    ) -> Self {
        Self {
            core,
            permit: Some(permit),
            domain,
        }
    }

    fn ensure_domain(&self, actual: SessionDomain) -> Result<(), SessionError> {
        if actual == self.domain {
            Ok(())
        } else {
            Err(SessionError::DomainMismatch {
                header: self.domain,
                id: actual,
            })
        }
    }
}

impl SessionReadStore for SharedSessionStore {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&super::super::EntryId>,
    ) -> Result<super::super::ResolvedSessionState, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core.lock()?.load_session(session_id, leaf)
    }
}

impl SessionLifecycleStore for SharedSessionStore {
    fn create_session(
        &mut self,
        header: super::super::SessionHeader,
    ) -> Result<SessionId, SessionError> {
        self.ensure_domain(header.domain)?;
        self.core
            .lock_mutation(self.permit.as_ref())?
            .create_session(header)
    }

    fn create_session_with_entries(
        &mut self,
        header: super::super::SessionHeader,
        entries: Vec<super::super::SessionEntryKind>,
    ) -> Result<(SessionId, Vec<super::super::EntryId>), SessionError> {
        self.ensure_domain(header.domain)?;
        self.core
            .lock_mutation(self.permit.as_ref())?
            .create_session_with_entries(header, entries)
    }

    fn append(
        &mut self,
        session_id: &SessionId,
        entries: Vec<super::super::SessionEntryKind>,
    ) -> Result<Vec<super::super::EntryId>, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core
            .lock_mutation(self.permit.as_ref())?
            .append(session_id, entries)
    }

    fn delete_session(&mut self, session_id: &SessionId) -> Result<(), SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core
            .lock_mutation(self.permit.as_ref())?
            .delete_session(session_id)
    }
}

impl SessionTreeStore for SharedSessionStore {
    fn set_leaf(
        &mut self,
        session_id: &SessionId,
        leaf: Option<&super::super::EntryId>,
    ) -> Result<(), SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core
            .lock_mutation(self.permit.as_ref())?
            .set_leaf(session_id, leaf)
    }

    fn load_session_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core.lock()?.load_session_tree(session_id)
    }

    fn load_session_tree_for_leaf(
        &self,
        session_id: &SessionId,
        leaf: &super::super::EntryId,
    ) -> Result<SessionTreeSnapshot, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core
            .lock()?
            .load_session_tree_for_leaf(session_id, leaf)
    }

    fn load_branch_preview(
        &self,
        session_id: &SessionId,
        branch_root: &super::super::EntryId,
    ) -> Result<SessionBranchPreview, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core
            .lock()?
            .load_branch_preview(session_id, branch_root)
    }

    fn load_branch_tree(
        &self,
        session_id: &SessionId,
    ) -> Result<SessionBranchTreeSnapshot, SessionError> {
        self.ensure_domain(session_id.domain())?;
        self.core.lock()?.load_branch_tree(session_id)
    }
}

impl SessionFlushStore for SharedSessionStore {
    fn flush(&mut self) -> Result<(), SessionError> {
        self.core.lock_mutation(self.permit.as_ref())?.flush()
    }

    fn shutdown(&mut self) -> Result<(), SessionError> {
        self.core.shutdown()
    }
}

#[derive(Clone)]
pub struct SharedSessionCatalog {
    core: SharedStoreCore,
    domain: SessionDomain,
}

impl SharedSessionCatalog {
    pub(super) fn from_core(core: SharedStoreCore, domain: SessionDomain) -> Self {
        Self { core, domain }
    }

    fn ensure_catalog_domain(&self, actual: SessionDomain) -> Result<(), CatalogError> {
        if actual == self.domain {
            Ok(())
        } else {
            Err(CatalogError::DomainMismatch {
                expected: self.domain,
                actual,
            })
        }
    }

    fn ensure_session_domain(&self, actual: SessionDomain) -> Result<(), SessionError> {
        if actual == self.domain {
            Ok(())
        } else {
            Err(SessionError::DomainMismatch {
                header: self.domain,
                id: actual,
            })
        }
    }
}

impl SessionReadStore for SharedSessionCatalog {
    fn load_session(
        &self,
        session_id: &SessionId,
        leaf: Option<&super::super::EntryId>,
    ) -> Result<super::super::ResolvedSessionState, SessionError> {
        self.ensure_session_domain(session_id.domain())?;
        self.core.lock()?.load_session(session_id, leaf)
    }
}

impl SessionCatalogStore for SharedSessionCatalog {
    fn list_sessions(
        &self,
        domain: SessionDomain,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        self.ensure_catalog_domain(domain)?;
        self.core.lock_catalog()?.list_sessions(domain, query)
    }

    fn get_session_summary(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionSummary>, CatalogError> {
        self.ensure_catalog_domain(session_id.domain())?;
        self.core.lock_catalog()?.get_session_summary(session_id)
    }
}

#[derive(Clone)]
pub struct SharedChatReferenceStore(SharedStoreCore);

impl SharedChatReferenceStore {
    pub(super) fn from_core(core: SharedStoreCore) -> Self {
        Self(core)
    }
}

impl ChatMessageReferenceStore for SharedChatReferenceStore {
    fn search_chat_messages(
        &self,
        query: ChatMessageSearchQuery,
    ) -> Result<ChatMessageSearchPage, ChatReferenceError> {
        self.0
            .lock()
            .map_err(ChatReferenceError::Storage)?
            .search_chat_messages(query)
    }

    fn read_chat_message(
        &self,
        reference: &super::super::ChatMessageRef,
    ) -> Result<ChatMessageRead, ChatReferenceError> {
        self.0
            .lock()
            .map_err(ChatReferenceError::Storage)?
            .read_chat_message(reference)
    }
}

#[derive(Clone)]
pub struct SharedAgentProjectStore(SharedStoreCore);

impl SharedAgentProjectStore {
    pub(super) fn from_core(core: SharedStoreCore) -> Self {
        Self(core)
    }
}

impl ProjectSessionStore for SharedAgentProjectStore {
    fn list_project_sessions(
        &self,
        project_id: &str,
        query: CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        self.0
            .lock_catalog()?
            .list_project_sessions(project_id, query)
    }

    fn load_project_session(
        &self,
        project_id: &str,
        session_id: &SessionId,
        leaf: Option<&super::super::EntryId>,
    ) -> Result<super::super::ResolvedSessionState, SessionError> {
        self.0
            .lock()?
            .load_project_session(project_id, session_id, leaf)
    }

    fn get_project_identity(
        &self,
        project_id: &str,
    ) -> Result<Option<ProjectIdentity>, CatalogError> {
        self.0.lock_catalog()?.get_project_identity(project_id)
    }

    fn list_projects(
        &self,
        query: super::super::ProjectCatalogQuery,
    ) -> Result<super::super::ProjectCatalogPage, CatalogError> {
        self.0.lock_catalog()?.list_projects(query)
    }
}
