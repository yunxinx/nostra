use std::{
    ops::{Deref, DerefMut},
    sync::{Arc, Condvar, Mutex, MutexGuard, TryLockError, mpsc},
    thread,
    time::{Duration, Instant},
};

use super::capabilities::SharedSessionStore;

use super::super::{
    CatalogError, ChatMessageReferenceStore, ProjectSessionStore, SessionCatalogStore,
    SessionError, SessionStore,
};

pub(super) trait SessionServiceStore:
    SessionStore + SessionCatalogStore + ProjectSessionStore + ChatMessageReferenceStore + Send
{
}

impl<T> SessionServiceStore for T where
    T: SessionStore + SessionCatalogStore + ProjectSessionStore + ChatMessageReferenceStore + Send
{
}

struct OperationState {
    accepting: bool,
    active: usize,
    closed: bool,
}

pub(super) struct OperationPermit {
    active: std::sync::atomic::AtomicBool,
}

pub(super) struct SharedStoreInner {
    pub(super) store: Mutex<Box<dyn SessionServiceStore>>,
    operation_state: Mutex<OperationState>,
    operations_idle: Condvar,
}

/// A synchronous mutation owns an operation slot for the whole duration of
/// its store lock. This closes the race where shutdown has stopped accepting
/// new work but an already-queued mutation could otherwise enter before the
/// final store barrier.
pub(super) struct StoreMutationGuard<'a> {
    core: &'a SharedStoreCore,
    store: MutexGuard<'a, Box<dyn SessionServiceStore>>,
    releases_slot: bool,
}

impl Deref for StoreMutationGuard<'_> {
    type Target = Box<dyn SessionServiceStore>;

    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

impl DerefMut for StoreMutationGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.store
    }
}

impl Drop for StoreMutationGuard<'_> {
    fn drop(&mut self) {
        if self.releases_slot {
            self.core.release_operation();
        }
    }
}

#[derive(Clone)]
pub(super) struct SharedStoreCore(pub(super) Arc<SharedStoreInner>);

#[cfg(not(test))]
pub(super) const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(not(test))]
pub(super) const SESSION_MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const SESSION_MAINTENANCE_TIMEOUT: Duration = Duration::from_millis(200);
#[cfg(not(test))]
pub(super) const SESSION_OPEN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
pub(super) const SESSION_OPEN_TIMEOUT: Duration = Duration::from_millis(200);

/// Keeps one persistence operation visible to application shutdown from the
/// moment foreground code schedules it until the background future settles.
/// Dropping a cancelled task releases the reservation automatically.
pub(crate) struct SessionOperationGuard {
    core: SharedStoreCore,
    permit: Arc<OperationPermit>,
    domain: super::super::SessionDomain,
}

impl Drop for SessionOperationGuard {
    fn drop(&mut self) {
        self.permit
            .active
            .store(false, std::sync::atomic::Ordering::Release);
        self.core.release_operation();
    }
}

impl SessionOperationGuard {
    pub(crate) fn authorized_store(&self) -> SharedSessionStore {
        SharedSessionStore::from_reserved_core(
            self.core.clone(),
            Arc::clone(&self.permit),
            self.domain,
        )
    }
}

impl SharedStoreCore {
    pub(super) fn new(
        store: impl SessionStore
        + SessionCatalogStore
        + ProjectSessionStore
        + ChatMessageReferenceStore
        + Send
        + 'static,
    ) -> Self {
        Self(Arc::new(SharedStoreInner {
            store: Mutex::new(Box::new(store)),
            operation_state: Mutex::new(OperationState {
                accepting: true,
                active: 0,
                closed: false,
            }),
            operations_idle: Condvar::new(),
        }))
    }

    pub(super) fn lock(
        &self,
    ) -> Result<MutexGuard<'_, Box<dyn SessionServiceStore>>, SessionError> {
        loop {
            if self.is_closed()? {
                return Err(SessionError::StoreShuttingDown);
            }
            match self.0.store.try_lock() {
                Ok(store) => {
                    if self.is_closed()? {
                        return Err(SessionError::StoreShuttingDown);
                    }
                    return Ok(store);
                }
                Err(TryLockError::Poisoned(_)) => return Err(SessionError::StorePoisoned),
                Err(TryLockError::WouldBlock) => thread::yield_now(),
            }
        }
    }

    pub(super) fn lock_catalog(
        &self,
    ) -> Result<MutexGuard<'_, Box<dyn SessionServiceStore>>, CatalogError> {
        loop {
            if self.is_closed().map_err(|_| CatalogError::StorePoisoned)? {
                return Err(CatalogError::StoreShuttingDown);
            }
            match self.0.store.try_lock() {
                Ok(store) => {
                    if self.is_closed().map_err(|_| CatalogError::StorePoisoned)? {
                        return Err(CatalogError::StoreShuttingDown);
                    }
                    return Ok(store);
                }
                Err(TryLockError::Poisoned(_)) => return Err(CatalogError::StorePoisoned),
                Err(TryLockError::WouldBlock) => thread::yield_now(),
            }
        }
    }

    pub(super) fn is_closed(&self) -> Result<bool, SessionError> {
        self.0
            .operation_state
            .lock()
            .map(|state| state.closed)
            .map_err(|_| SessionError::StorePoisoned)
    }

    pub(super) fn lock_mutation(
        &self,
        permit: Option<&Arc<OperationPermit>>,
    ) -> Result<StoreMutationGuard<'_>, SessionError> {
        let releases_slot = match permit {
            Some(permit) if permit.active.load(std::sync::atomic::Ordering::Acquire) => false,
            Some(_) => return Err(SessionError::StoreShuttingDown),
            None => {
                self.reserve_operation_slot()?;
                true
            }
        };
        let store = match self.0.store.lock() {
            Ok(store) => store,
            Err(_) => {
                if releases_slot {
                    self.release_operation();
                }
                return Err(SessionError::StorePoisoned);
            }
        };
        Ok(StoreMutationGuard {
            core: self,
            store,
            releases_slot,
        })
    }

    pub(super) fn reserve_operation(
        &self,
        domain: super::super::SessionDomain,
    ) -> Result<SessionOperationGuard, SessionError> {
        self.reserve_operation_slot()?;
        Ok(SessionOperationGuard {
            core: self.clone(),
            permit: Arc::new(OperationPermit {
                active: std::sync::atomic::AtomicBool::new(true),
            }),
            domain,
        })
    }

    pub(super) fn reserve_operation_slot(&self) -> Result<(), SessionError> {
        let mut state = self
            .0
            .operation_state
            .lock()
            .map_err(|_| SessionError::StorePoisoned)?;
        if !state.accepting || state.closed {
            return Err(SessionError::StoreShuttingDown);
        }
        state.active = state.active.saturating_add(1);
        Ok(())
    }

    fn release_operation(&self) {
        let mut state = match self.0.operation_state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.active = state.active.saturating_sub(1);
        if state.active == 0 {
            self.0.operations_idle.notify_all();
        }
    }

    pub(super) fn begin_shutdown(&self) -> Result<(), SessionError> {
        let mut state = self
            .0
            .operation_state
            .lock()
            .map_err(|_| SessionError::StorePoisoned)?;
        if !state.accepting || state.closed {
            return Err(SessionError::StoreShuttingDown);
        }
        state.accepting = false;
        Ok(())
    }

    pub(super) fn finish_shutdown(&self, deadline: Instant) -> Result<(), SessionError> {
        let mut state = self
            .0
            .operation_state
            .lock()
            .map_err(|_| SessionError::StorePoisoned)?;
        while state.active > 0 {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                drop(state);
                self.mark_closed();
                return Err(SessionError::ShutdownTimeout);
            }
            let (next, wait) = self
                .0
                .operations_idle
                .wait_timeout(state, remaining)
                .map_err(|_| SessionError::StorePoisoned)?;
            state = next;
            if wait.timed_out() && state.active > 0 {
                drop(state);
                self.mark_closed();
                return Err(SessionError::ShutdownTimeout);
            }
        }
        if state.closed {
            drop(state);
            return Err(SessionError::StoreShuttingDown);
        }
        drop(state);
        let mut store = self.lock()?;
        let result = store.shutdown();
        // Keep the store lock through this transition. Any mutation that was
        // queued behind the final flush will observe `closed` before it can
        // access the underlying store.
        self.mark_closed();
        result
    }

    pub(super) fn start_shutdown(
        &self,
        deadline: Instant,
    ) -> Result<mpsc::Receiver<Result<(), SessionError>>, SessionError> {
        self.begin_shutdown()?;
        let core = self.clone();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        if let Err(error) = thread::Builder::new()
            .name("nostra-session-shutdown".to_string())
            .spawn(move || {
                let _ = finished_tx.send(core.finish_shutdown(deadline));
            })
        {
            self.mark_closed();
            return Err(SessionError::io(error));
        }
        Ok(finished_rx)
    }

    pub(super) fn shutdown(&self) -> Result<(), SessionError> {
        let deadline = Instant::now() + SESSION_SHUTDOWN_TIMEOUT;
        let receiver = self.start_shutdown(deadline)?;
        receive_shutdown(self, receiver, deadline)
    }

    pub(super) fn start_flush(
        &self,
    ) -> Result<mpsc::Receiver<Result<(), SessionError>>, SessionError> {
        if self.is_closed()? {
            return Err(SessionError::StoreShuttingDown);
        }
        let core = self.clone();
        let (finished_tx, finished_rx) = mpsc::sync_channel(1);
        thread::Builder::new()
            .name("nostra-session-flush".to_string())
            .spawn(move || {
                let result = core.lock_mutation(None).and_then(|mut store| store.flush());
                let _ = finished_tx.send(result);
            })
            .map_err(SessionError::io)?;
        Ok(finished_rx)
    }

    pub(super) fn mark_closed(&self) {
        let mut state = match self.0.operation_state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.accepting = false;
        state.closed = true;
        self.0.operations_idle.notify_all();
    }
}

pub(super) fn receive_shutdown(
    core: &SharedStoreCore,
    receiver: mpsc::Receiver<Result<(), SessionError>>,
    deadline: Instant,
) -> Result<(), SessionError> {
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
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The worker may be blocked in filesystem or SQLite code that the
            // standard library cannot cancel. Detach it, close the public
            // mutation boundary immediately, and let process exit terminate
            // any worker that never returns.
            core.mark_closed();
            Err(SessionError::ShutdownTimeout)
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            core.mark_closed();
            Err(SessionError::io(std::io::Error::other(
                "session shutdown worker disconnected",
            )))
        }
    }
}

pub(super) fn receive_maintenance(
    core: &SharedStoreCore,
    receiver: mpsc::Receiver<Result<(), SessionError>>,
    deadline: Instant,
    operation: &'static str,
) -> Result<(), SessionError> {
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
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // Synchronous filesystem work cannot be cancelled safely. The
            // worker retains the store lock and may finish later, while the
            // caller receives a bounded failure instead of blocking the
            // other persistence domain indefinitely.
            core.mark_closed();
            Err(SessionError::MaintenanceTimeout { operation })
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            core.mark_closed();
            Err(SessionError::io(std::io::Error::other(format!(
                "session {operation} worker disconnected"
            ))))
        }
    }
}
