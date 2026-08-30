use std::{
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::super::{
    ChatMessageReferenceStore, LocalSessionStore, ProjectSessionStore, SessionCatalogStore,
    SessionDomain, SessionStore,
};
use super::capabilities::{
    ConversationSessionServices, SharedAgentProjectStore, SharedChatReferenceStore,
    SharedSessionCatalog, SharedSessionStore,
};
use super::core::{
    SESSION_MAINTENANCE_TIMEOUT, SESSION_OPEN_TIMEOUT, SESSION_SHUTDOWN_TIMEOUT, SharedStoreCore,
    receive_maintenance, receive_shutdown,
};

#[derive(Clone)]
pub(super) enum DomainStoreState {
    Ready(SharedStoreCore),
    Unavailable(Arc<str>),
}

impl DomainStoreState {
    fn unavailable(reason: impl Into<Arc<str>>) -> Self {
        Self::Unavailable(reason.into())
    }
}

#[derive(Clone)]
pub struct SessionStores {
    pub(super) chat: DomainStoreState,
    pub(super) agent: DomainStoreState,
}

impl Default for SessionStores {
    fn default() -> Self {
        Self {
            chat: DomainStoreState::unavailable("Chat session storage is not configured"),
            agent: DomainStoreState::unavailable("Agent session storage is not configured"),
        }
    }
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum SessionStoresError {
    #[error("{domain} session storage is unavailable: {reason}")]
    DomainUnavailable {
        domain: SessionDomain,
        reason: Arc<str>,
    },
    #[error("session store {operation} failed: {failures}")]
    Maintenance {
        operation: &'static str,
        failures: Arc<str>,
    },
}

impl SessionStores {
    #[must_use]
    pub fn with_chat_store(
        store: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
    ) -> Self {
        Self {
            chat: DomainStoreState::Ready(SharedStoreCore::new(store)),
            agent: DomainStoreState::unavailable("Agent session storage is not configured"),
        }
    }

    #[must_use]
    pub fn with_agent_store(
        store: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
    ) -> Self {
        Self {
            chat: DomainStoreState::unavailable("Chat session storage is not configured"),
            agent: DomainStoreState::Ready(SharedStoreCore::new(store)),
        }
    }

    #[must_use]
    pub fn with_stores(
        chat: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
        agent: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
    ) -> Self {
        Self {
            chat: DomainStoreState::Ready(SharedStoreCore::new(chat)),
            agent: DomainStoreState::Ready(SharedStoreCore::new(agent)),
        }
    }

    /// Open both domains independently. A failure remains attached to that
    /// domain and never substitutes a volatile in-memory store; the healthy
    /// domain remains available to the application.
    #[must_use]
    pub fn open_default() -> Self {
        Self::open_with(
            || open_local_domain(SessionDomain::Chat),
            || open_local_domain(SessionDomain::Agent),
        )
    }

    pub(super) fn open_with(
        open_chat: impl FnOnce() -> DomainStoreState + Send + 'static,
        open_agent: impl FnOnce() -> DomainStoreState + Send + 'static,
    ) -> Self {
        let deadline = Instant::now() + SESSION_OPEN_TIMEOUT;
        let chat = start_domain_open(SessionDomain::Chat, open_chat);
        let agent = start_domain_open(SessionDomain::Agent, open_agent);
        Self {
            chat: receive_domain_open(SessionDomain::Chat, chat, deadline),
            agent: receive_domain_open(SessionDomain::Agent, agent, deadline),
        }
    }

    pub fn chat(&self) -> Result<SharedSessionStore, SessionStoresError> {
        self.core(SessionDomain::Chat)
            .map(|core| SharedSessionStore::from_core(core, SessionDomain::Chat))
    }

    pub fn chat_catalog(&self) -> Result<SharedSessionCatalog, SessionStoresError> {
        self.core(SessionDomain::Chat)
            .map(|core| SharedSessionCatalog::from_core(core, SessionDomain::Chat))
    }

    pub fn chat_references(&self) -> Result<SharedChatReferenceStore, SessionStoresError> {
        self.core(SessionDomain::Chat)
            .map(SharedChatReferenceStore::from_core)
    }

    /// Project the Chat-domain capabilities consumed by a Chat conversation.
    pub fn chat_conversation(&self) -> ConversationSessionServices {
        ConversationSessionServices::new(self.chat(), None)
    }

    pub fn agent(&self) -> Result<SharedSessionStore, SessionStoresError> {
        self.core(SessionDomain::Agent)
            .map(|core| SharedSessionStore::from_core(core, SessionDomain::Agent))
    }

    /// Return the project-scoped Agent catalog and restore capability.
    ///
    /// Agent consumers must not receive the unscoped Chat catalog façade:
    ///
    /// ```compile_fail
    /// use nostra::session::{InMemorySessionStore, SessionStores};
    ///
    /// let stores = SessionStores::with_agent_store(InMemorySessionStore::new());
    /// let _ = stores.agent_catalog();
    /// ```
    pub fn agent_projects(&self) -> Result<SharedAgentProjectStore, SessionStoresError> {
        self.core(SessionDomain::Agent)
            .map(SharedAgentProjectStore::from_core)
    }

    /// Project the Agent lifecycle capability and Chat reference reader used
    /// by a project conversation.
    pub fn project_conversation(&self) -> ConversationSessionServices {
        ConversationSessionServices::new(self.agent(), self.chat_references().ok())
    }

    pub fn flush(&self) -> Result<(), SessionStoresError> {
        let deadline = Instant::now() + SESSION_MAINTENANCE_TIMEOUT;
        let mut failures = Vec::new();
        let mut jobs = Vec::new();
        for (domain, state) in [
            (SessionDomain::Chat, &self.chat),
            (SessionDomain::Agent, &self.agent),
        ] {
            let DomainStoreState::Ready(core) = state else {
                continue;
            };
            match core.start_flush() {
                Ok(receiver) => jobs.push((domain, core.clone(), receiver)),
                Err(error) => failures.push(format!("{domain}: {error}")),
            }
        }
        for (domain, core, receiver) in jobs {
            if let Err(error) = receive_maintenance(&core, receiver, deadline, "flush") {
                failures.push(format!("{domain}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(SessionStoresError::Maintenance {
                operation: "flush",
                failures: failures.join("; ").into(),
            })
        }
    }

    pub fn shutdown(&self) -> Result<(), SessionStoresError> {
        self.shutdown_with_timeout(SESSION_SHUTDOWN_TIMEOUT)
    }

    pub(crate) fn shutdown_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<(), SessionStoresError> {
        let deadline = Instant::now() + timeout;
        let mut failures = Vec::new();
        let mut jobs = Vec::new();
        for (domain, state) in [
            (SessionDomain::Chat, &self.chat),
            (SessionDomain::Agent, &self.agent),
        ] {
            let DomainStoreState::Ready(core) = state else {
                continue;
            };
            match core.start_shutdown(deadline) {
                Ok(receiver) => jobs.push((domain, core.clone(), receiver)),
                Err(error) => failures.push(format!("{domain}: {error}")),
            }
        }
        for (domain, core, receiver) in jobs {
            if let Err(error) = receive_shutdown(&core, receiver, deadline) {
                failures.push(format!("{domain}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(SessionStoresError::Maintenance {
                operation: "shutdown",
                failures: failures.join("; ").into(),
            })
        }
    }

    fn core(&self, domain: SessionDomain) -> Result<SharedStoreCore, SessionStoresError> {
        let state = match domain {
            SessionDomain::Chat => &self.chat,
            SessionDomain::Agent => &self.agent,
        };
        match state {
            DomainStoreState::Ready(core) => Ok(core.clone()),
            DomainStoreState::Unavailable(reason) => Err(SessionStoresError::DomainUnavailable {
                domain,
                reason: Arc::clone(reason),
            }),
        }
    }
}

fn open_local_domain(domain: SessionDomain) -> DomainStoreState {
    let opened = LocalSessionStore::open_default(domain).and_then(|mut store| {
        if let Some(report) = store.repair_if_needed()?
            && !report.issues.is_empty()
        {
            // One aggregate warning keeps startup diagnostics useful without
            // turning a damaged source into one disk write per malformed line.
            crate::logging::warn(
                "session",
                format_args!(
                    "{domain} catalog repair completed with issues: scanned={}, rebuilt={}, removed={}, issues={}",
                    report.scanned,
                    report.rebuilt,
                    report.removed,
                    report.issues.len()
                ),
            );
        }
        Ok(store)
    });
    match opened {
        Ok(store) => DomainStoreState::Ready(SharedStoreCore::new(store)),
        Err(error) => DomainStoreState::unavailable(error.to_string()),
    }
}

fn start_domain_open(
    domain: SessionDomain,
    open: impl FnOnce() -> DomainStoreState + Send + 'static,
) -> Result<mpsc::Receiver<DomainStoreState>, std::io::Error> {
    let (finished_tx, finished_rx) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name(format!("nostra-{domain}-session-open"))
        .spawn(move || {
            let _ = finished_tx.send(open());
        })?;
    Ok(finished_rx)
}

fn receive_domain_open(
    domain: SessionDomain,
    receiver: Result<mpsc::Receiver<DomainStoreState>, std::io::Error>,
    deadline: Instant,
) -> DomainStoreState {
    let receiver = match receiver {
        Ok(receiver) => receiver,
        Err(error) => {
            return DomainStoreState::unavailable(format!(
                "failed to start {domain} session storage opener: {error}"
            ));
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let received = if remaining.is_zero() {
        receiver.try_recv().map_err(|error| match error {
            mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
            mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
        })
    } else {
        receiver.recv_timeout(remaining)
    };
    match received {
        Ok(state) => state,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            DomainStoreState::unavailable(format!("timed out opening {domain} session storage"))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            DomainStoreState::unavailable(format!("{domain} session storage opener disconnected"))
        }
    }
}
