//! Application exit coordination for durable Providers.

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use crate::{
    preferences::{PreferenceHandle, Preferences},
    session::SessionStores,
};

/// Full durability budget used by an explicit menu or native-close request.
pub const NORMAL_EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounded budget used by GPUI's final application-quit observer phase.
pub const QUIT_FALLBACK_TIMEOUT: Duration = Duration::from_millis(150);

/// Results from the two independent durable exit operations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExitReport {
    pub session: Result<(), Arc<str>>,
    pub preferences: Result<(), Arc<str>>,
}

impl ExitReport {
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.session.is_ok() && self.preferences.is_ok()
    }

    pub(crate) fn dispose_error(&self) -> Option<anyhow::Error> {
        let mut failures = Vec::new();
        if let Err(error) = &self.session {
            failures.push(format!("session shutdown: {error}"));
        }
        if let Err(error) = &self.preferences {
            failures.push(format!("preference save: {error}"));
        }
        (!failures.is_empty()).then(|| anyhow::anyhow!(failures.join("; ")))
    }

    pub(crate) fn session_dispose_error(&self) -> Option<anyhow::Error> {
        self.session
            .as_ref()
            .err()
            .map(|error| anyhow::anyhow!("session shutdown: {error}"))
    }
}

#[derive(Default)]
struct ExitState {
    started: bool,
    result: Option<ExitReport>,
}

enum ExitOperation {
    Session(Result<(), Arc<str>>),
    Preferences(Result<(), Arc<str>>),
}

/// Owns application-scoped durable shutdown and preference persistence.
///
/// The first caller performs one durable operation for both Providers. Later
/// callers wait for and observe the same result, so composition teardown and
/// GPUI's final quit observer cannot run competing shutdowns.
#[derive(Clone)]
pub struct ExitCoordinator {
    stores: SessionStores,
    preference_handle: PreferenceHandle,
    state: Arc<(Mutex<ExitState>, Condvar)>,
}

impl ExitCoordinator {
    #[must_use]
    pub fn new(stores: SessionStores, preference_handle: PreferenceHandle) -> Self {
        Self {
            stores,
            preference_handle,
            state: Arc::new((Mutex::new(ExitState::default()), Condvar::new())),
        }
    }

    /// Start or join the one durable exit operation.
    ///
    /// Callers that own a foreground executor should schedule this future on
    /// their background executor because the durable operation is blocking.
    pub fn run(
        &self,
        snapshot: Preferences,
        timeout: Duration,
    ) -> impl Future<Output = ExitReport> + 'static {
        let coordinator = self.clone();
        async move { coordinator.run_blocking(snapshot, timeout) }
    }

    /// Execute the durable operation synchronously. This is intended for a
    /// background executor or a runtime teardown caller.
    #[must_use]
    pub fn run_blocking(&self, snapshot: Preferences, timeout: Duration) -> ExitReport {
        let (state_lock, completed) = &*self.state;
        let mut state = match state_lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(result) = &state.result {
            return result.clone();
        }
        if state.started {
            while state.result.is_none() {
                state = match completed.wait(state) {
                    Ok(state) => state,
                    Err(poisoned) => poisoned.into_inner(),
                };
            }
            if let Some(result) = state.result.clone() {
                return result;
            }
        }
        state.started = true;
        drop(state);

        let report = run_durable_exit(
            self.stores.clone(),
            self.preference_handle.clone(),
            snapshot,
            timeout,
        );
        let mut state = match state_lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.result = Some(report.clone());
        completed.notify_all();
        report
    }

    /// Whether the durable operation has completed.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        let (state_lock, _) = &*self.state;
        let state = match state_lock.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        state.result.is_some()
    }
}

fn run_durable_exit(
    stores: SessionStores,
    preference_handle: PreferenceHandle,
    snapshot: Preferences,
    timeout: Duration,
) -> ExitReport {
    let deadline = Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now);
    let (results_tx, results_rx) = mpsc::sync_channel(2);
    let mut pending = 0;
    let mut session = None;
    let mut preferences = None;

    let session_tx = results_tx.clone();
    let session_spawn = thread::Builder::new()
        .name("nostra-runtime-session-shutdown".to_string())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                let remaining = deadline.saturating_duration_since(Instant::now());
                stores.shutdown_with_timeout(remaining)
            }))
            .map_err(|_| Arc::<str>::from("session shutdown worker panicked"))
            .and_then(|result| result.map_err(|error| Arc::<str>::from(error.to_string())));
            let _ = session_tx.send(ExitOperation::Session(result));
        });
    match session_spawn {
        Ok(_) => pending += 1,
        Err(error) => {
            session = Some(Err(Arc::from(format!(
                "failed to start session shutdown worker: {error}"
            ))));
        }
    }

    let preferences_tx = results_tx.clone();
    let preferences_spawn = thread::Builder::new()
        .name("nostra-runtime-preference-save".to_string())
        .spawn(move || {
            let result = catch_unwind(AssertUnwindSafe(|| {
                preference_handle.save_snapshot(&snapshot)
            }))
            .map_err(|_| Arc::<str>::from("preference save worker panicked"))
            .and_then(|result| result.map_err(|error| Arc::<str>::from(error.to_string())));
            let _ = preferences_tx.send(ExitOperation::Preferences(result));
        });
    match preferences_spawn {
        Ok(_) => pending += 1,
        Err(error) => {
            preferences = Some(Err(Arc::from(format!(
                "failed to start preference save worker: {error}"
            ))));
        }
    }
    drop(results_tx);

    while pending > 0 {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let received = if remaining.is_zero() {
            results_rx.try_recv().map_err(|error| match error {
                mpsc::TryRecvError::Empty => mpsc::RecvTimeoutError::Timeout,
                mpsc::TryRecvError::Disconnected => mpsc::RecvTimeoutError::Disconnected,
            })
        } else {
            results_rx.recv_timeout(remaining)
        };
        match received {
            Ok(ExitOperation::Session(result)) => {
                if session.is_none() {
                    session = Some(result);
                    pending -= 1;
                }
            }
            Ok(ExitOperation::Preferences(result)) => {
                if preferences.is_none() {
                    preferences = Some(result);
                    pending -= 1;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    ExitReport {
        session: session.unwrap_or_else(|| Err(Arc::from("session shutdown timed out"))),
        preferences: preferences.unwrap_or_else(|| Err(Arc::from("preference save timed out"))),
    }
}
