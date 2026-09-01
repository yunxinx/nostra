//! Generation admission and stable Consumer remounting.

use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use futures::channel::oneshot;

use crate::llm::{
    GatewayError, GenerationEvent, GenerationHandle, GenerationRequest, GenerationRunner,
    GenerationService,
};

#[derive(Default)]
struct GenerationAdmissionState {
    accepting: bool,
    active: usize,
    idle_waiters: Vec<oneshot::Sender<()>>,
}

#[derive(Clone)]
struct GenerationAdmission {
    state: Arc<Mutex<GenerationAdmissionState>>,
}

impl GenerationAdmission {
    fn open() -> Self {
        Self {
            state: Arc::new(Mutex::new(GenerationAdmissionState {
                accepting: true,
                ..GenerationAdmissionState::default()
            })),
        }
    }

    fn reserve(&self) -> Option<GenerationPermit> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.accepting {
            return None;
        }
        state.active = state.active.saturating_add(1);
        Some(GenerationPermit {
            admission: self.clone(),
            active: true,
        })
    }

    async fn close_and_wait(&self) {
        let receiver = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.accepting = false;
            if state.active == 0 {
                None
            } else {
                let (sender, receiver) = oneshot::channel();
                state.idle_waiters.push(sender);
                Some(receiver)
            }
        };
        if let Some(receiver) = receiver {
            let _ = receiver.await;
        }
    }

    fn release(&self) {
        let waiters = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.active = state.active.saturating_sub(1);
            if state.active == 0 && !state.accepting {
                std::mem::take(&mut state.idle_waiters)
            } else {
                Vec::new()
            }
        };
        for waiter in waiters {
            let _ = waiter.send(());
        }
    }
}

struct GenerationPermit {
    admission: GenerationAdmission,
    active: bool,
}

impl Drop for GenerationPermit {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.admission.release();
        }
    }
}

struct BoundGenerationService {
    provider: Arc<dyn GenerationService>,
    admission: GenerationAdmission,
}

impl GenerationService for BoundGenerationService {
    fn start(&self, request: GenerationRequest) -> Result<GenerationHandle, GatewayError> {
        let permit = self
            .admission
            .reserve()
            .ok_or_else(|| GatewayError::configuration("generation capability is quiescing"))?;
        let handle = self.provider.start(request)?;
        Ok(GenerationHandle::from_runner(BoundGenerationRunner {
            handle,
            permit: Some(permit),
        }))
    }
}

struct BoundGenerationRunner {
    handle: GenerationHandle,
    permit: Option<GenerationPermit>,
}

impl GenerationRunner for BoundGenerationRunner {
    fn run<'a>(
        &'a mut self,
        on_event: &'a mut dyn FnMut(GenerationEvent) -> bool,
    ) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        Box::pin(async move {
            self.handle.run(on_event).await;
            self.permit = None;
        })
    }

    fn cancel(&mut self) -> Option<GenerationEvent> {
        let terminal = self.handle.cancel();
        self.permit = None;
        terminal
    }
}

pub(super) struct GenerationConsumerBinding {
    admission: GenerationAdmission,
    service: Arc<dyn GenerationService>,
}

impl GenerationConsumerBinding {
    pub(super) fn new(provider: Arc<dyn GenerationService>) -> Self {
        let admission = GenerationAdmission::open();
        let service = Arc::new(BoundGenerationService {
            provider,
            admission: admission.clone(),
        });
        Self { admission, service }
    }

    pub(super) fn service(&self) -> Arc<dyn GenerationService> {
        Arc::clone(&self.service)
    }

    pub(super) async fn quiesce(&self) {
        self.admission.close_and_wait().await;
    }
}

#[derive(Clone)]
pub(super) struct GenerationConsumerMount {
    current: Arc<Mutex<Arc<dyn GenerationService>>>,
    service: Arc<dyn GenerationService>,
}

impl GenerationConsumerMount {
    pub(super) fn new(service: Arc<dyn GenerationService>) -> Self {
        let current = Arc::new(Mutex::new(service));
        let mounted = Arc::new(MountedGenerationService {
            current: Arc::clone(&current),
        });
        Self {
            current,
            service: mounted,
        }
    }

    pub(super) fn service(&self) -> Arc<dyn GenerationService> {
        Arc::clone(&self.service)
    }

    pub(super) fn remount(&self, service: Arc<dyn GenerationService>) {
        *self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = service;
    }

    pub(super) fn unmount(&self) {
        self.remount(Arc::new(UnavailableGenerationService));
    }
}

struct MountedGenerationService {
    current: Arc<Mutex<Arc<dyn GenerationService>>>,
}

impl GenerationService for MountedGenerationService {
    fn start(&self, request: GenerationRequest) -> Result<GenerationHandle, GatewayError> {
        let service = self
            .current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        service.start(request)
    }
}

struct UnavailableGenerationService;

impl GenerationService for UnavailableGenerationService {
    fn start(&self, _request: GenerationRequest) -> Result<GenerationHandle, GatewayError> {
        Err(GatewayError::configuration(
            "generation capability is unavailable",
        ))
    }
}
