use std::{cell::Cell, collections::HashSet, rc::Rc};

use super::{CapabilityId, CapabilityKey, ComponentGeneration, ComponentId, ScopeId};

#[test]
fn component_ids_are_stable_value_keys() {
    const SESSION: ComponentId = ComponentId::new("nostra.session.local");

    let same = ComponentId::new("nostra.session.local");
    let other = ComponentId::new("nostra.generation.gateway");
    let ids = HashSet::from([SESSION, other]);

    assert_eq!(SESSION, same);
    assert_ne!(SESSION, other);
    assert_eq!(SESSION.as_str(), "nostra.session.local");
    assert_eq!(SESSION.to_string(), "nostra.session.local");
    assert!(ids.contains(&same));
}

#[test]
#[should_panic(expected = "component ID must not be empty")]
fn component_ids_reject_empty_names() {
    let _ = ComponentId::new("");
}

#[test]
#[should_panic(expected = "component ID contains an unsupported character")]
fn component_ids_reject_unstable_names() {
    let _ = ComponentId::new("Nostra Session");
}

#[test]
fn scope_ids_preserve_runtime_identity() {
    const APPLICATION: ScopeId = ScopeId::new(0);
    let window = ScopeId::new(1);

    assert_eq!(APPLICATION.raw(), 0);
    assert_eq!(window.raw(), 1);
    assert_ne!(APPLICATION, window);
}

#[test]
fn component_generations_start_at_one_and_never_wrap() {
    let initial = ComponentGeneration::INITIAL;
    let second = initial.next().expect("second generation");
    let exhausted = ComponentGeneration::new(u64::MAX).expect("maximum generation");

    assert_eq!(initial.get(), 1);
    assert_eq!(second.get(), 2);
    assert!(ComponentGeneration::new(0).is_none());
    assert!(exhausted.next().is_none());
}

#[derive(Clone)]
struct ForegroundHandle(Rc<Cell<usize>>);

struct ForegroundCapability;

impl CapabilityKey for ForegroundCapability {
    type Handle = ForegroundHandle;

    const NAME: &'static str = "nostra.test.foreground";
}

struct AlternateForegroundCapability;

impl CapabilityKey for AlternateForegroundCapability {
    type Handle = ForegroundHandle;

    const NAME: &'static str = "nostra.test.foreground-alternate";
}

struct EmptyCapability;

impl CapabilityKey for EmptyCapability {
    type Handle = ();

    const NAME: &'static str = "";
}

struct UnstableCapability;

impl CapabilityKey for UnstableCapability {
    type Handle = ();

    const NAME: &'static str = "Nostra Capability";
}

#[test]
fn capability_erasure_preserves_the_marker_type() {
    let first = CapabilityId::of::<ForegroundCapability>();
    let same = CapabilityId::of::<ForegroundCapability>();
    let alternate = CapabilityId::of::<AlternateForegroundCapability>();

    assert_eq!(first, same);
    assert_ne!(first, alternate);
    assert_eq!(first.name(), ForegroundCapability::NAME);
    assert!(first.is::<ForegroundCapability>());
    assert!(!first.is::<AlternateForegroundCapability>());
}

#[test]
fn capability_handles_can_be_foreground_only() {
    fn clone_handle<K: CapabilityKey>(handle: &K::Handle) -> K::Handle {
        handle.clone()
    }

    let handle = ForegroundHandle(Rc::new(Cell::new(1)));
    let cloned = clone_handle::<ForegroundCapability>(&handle);
    cloned.0.set(2);

    assert_eq!(handle.0.get(), 2);
}

#[test]
#[should_panic(expected = "capability name must not be empty")]
fn capabilities_reject_empty_diagnostic_names() {
    let _ = CapabilityId::of::<EmptyCapability>();
}

#[test]
#[should_panic(expected = "capability name contains an unsupported character")]
fn capabilities_reject_unstable_diagnostic_names() {
    let _ = CapabilityId::of::<UnstableCapability>();
}
