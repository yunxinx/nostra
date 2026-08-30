//! Reversible mount effects with explicit asynchronous quiescence.

use std::{fmt, future::Future, mem, pin::Pin};

pub type DisposeError = anyhow::Error;

/// An owned resource that must stop completely before earlier effects are undone.
///
/// The returned future may be cancelled. Implementations must retain enough state
/// for `stop` to be called again and must only return success after the resource is
/// quiescent.
pub trait AsyncStop: 'static {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>>;
}

enum EffectEntry {
    Sync(Option<Box<dyn FnOnce() + 'static>>),
    Async(Box<dyn AsyncStop>),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EffectScopeState {
    #[default]
    Active,
    Disposing,
    Disposed,
}

/// Owns every reversible side effect produced by one component mount.
///
/// Disposal is serial and follows reverse registration order. A failed or
/// cancelled asynchronous stop remains owned and blocks all earlier effects.
/// Dropping a live scope stops at the first asynchronous owner: dropping that
/// owner may request cancellation, while older effects remain active because
/// only `quiesce_and_dispose` can establish quiescence.
#[derive(Default)]
#[must_use = "effect scopes must be explicitly quiesced and disposed"]
pub struct EffectScope {
    entries: Vec<EffectEntry>,
    state: EffectScopeState,
}

impl EffectScope {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn own_sync(&mut self, undo: impl FnOnce() + 'static) {
        self.assert_accepting_effects();
        self.entries.push(EffectEntry::Sync(Some(Box::new(undo))));
    }

    pub fn own_resource<T: 'static>(&mut self, resource: T) {
        self.own_sync(move || drop(resource));
    }

    pub fn own_async(&mut self, stop: impl AsyncStop) {
        self.assert_accepting_effects();
        self.entries.push(EffectEntry::Async(Box::new(stop)));
    }

    pub async fn quiesce_and_dispose(&mut self) -> Result<(), DisposeError> {
        if self.state == EffectScopeState::Disposed {
            return Ok(());
        }
        self.state = EffectScopeState::Disposing;
        loop {
            let Some(entry) = self.entries.last_mut() else {
                self.state = EffectScopeState::Disposed;
                return Ok(());
            };
            let result = match entry {
                EffectEntry::Sync(undo) => {
                    if let Some(undo) = undo.take() {
                        undo();
                    }
                    Ok(())
                }
                EffectEntry::Async(stop) => stop.stop().await,
            };
            result?;
            self.entries.pop();
        }
    }

    /// Releases effects until an asynchronous owner requires quiescence.
    ///
    /// Returns `false` after dropping that owner to request cancellation while
    /// retaining every earlier effect that may still be in use.
    pub(crate) fn release_for_drop(&mut self) -> bool {
        self.state = EffectScopeState::Disposing;
        let mut entries = mem::take(&mut self.entries);
        while let Some(entry) = entries.pop() {
            match entry {
                EffectEntry::Sync(Some(undo)) => undo(),
                EffectEntry::Sync(None) => {}
                EffectEntry::Async(stop) => {
                    drop(stop);
                    crate::logging::error(
                        "runtime.effect",
                        format_args!(
                            "effect scope dropped before quiescence; retaining {} earlier effects",
                            entries.len()
                        ),
                    );
                    mem::forget(entries);
                    return false;
                }
            }
        }
        self.state = EffectScopeState::Disposed;
        true
    }

    #[track_caller]
    fn assert_accepting_effects(&self) {
        assert!(
            self.state == EffectScopeState::Active,
            "cannot register an effect after scope disposal has started"
        );
    }
}

impl fmt::Debug for EffectScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EffectScope")
            .field("effect_count", &self.entries.len())
            .field("state", &self.state)
            .finish()
    }
}

impl Drop for EffectScope {
    fn drop(&mut self) {
        self.release_for_drop();
    }
}
