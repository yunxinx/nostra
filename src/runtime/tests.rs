use std::{cell::Cell, collections::HashSet, rc::Rc};

use super::{
    CapabilityId, CapabilityKey, ComponentGeneration, ComponentId, ExclusiveCapabilitySlot,
    ExclusiveSlotError, PreparedCapability, ScopeId,
};

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestService {
    implementation: &'static str,
}

struct TestServiceCapability;

impl CapabilityKey for TestServiceCapability {
    type Handle = TestService;

    const NAME: &'static str = "nostra.test.service";
}

const DEFAULT_TEST_PROVIDER: ComponentId = ComponentId::new("nostra.test.service-default");
const ALTERNATE_TEST_PROVIDER: ComponentId = ComponentId::new("nostra.test.service-alternate");
const TEST_SCOPE: ScopeId = ScopeId::new(7);

fn prepare_selected_test_provider(
    slot: &ExclusiveCapabilitySlot<TestServiceCapability>,
    selected: ComponentId,
) -> Result<PreparedCapability<TestServiceCapability>, &'static str> {
    match selected {
        DEFAULT_TEST_PROVIDER => slot.prepare_candidate(DEFAULT_TEST_PROVIDER, || {
            Ok(TestService {
                implementation: "built-in-default",
            })
        }),
        ALTERNATE_TEST_PROVIDER => slot.prepare_candidate(ALTERNATE_TEST_PROVIDER, || {
            Ok(TestService {
                implementation: "alternate",
            })
        }),
        _ => Err("selected provider is not part of this composition"),
    }
}

fn consume_test_service(slot: &ExclusiveCapabilitySlot<TestServiceCapability>) -> &'static str {
    slot.current()
        .expect("selected test service")
        .handle()
        .implementation
}

#[test]
fn exclusive_slots_require_an_explicit_provider_selection() {
    let empty = ExclusiveCapabilitySlot::<TestServiceCapability>::new(TEST_SCOPE);
    assert!(empty.current().is_none());

    let mut default_composition = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let default = prepare_selected_test_provider(&default_composition, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    default_composition
        .install(default)
        .expect("install explicitly selected built-in default");

    let mut alternate_composition = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let alternate = prepare_selected_test_provider(&alternate_composition, ALTERNATE_TEST_PROVIDER)
        .expect("alternate candidate");
    alternate_composition
        .install(alternate)
        .expect("install explicitly selected alternate");

    assert_eq!(
        consume_test_service(&default_composition),
        "built-in-default"
    );
    assert_eq!(consume_test_service(&alternate_composition), "alternate");
}

#[test]
fn exclusive_slots_reject_a_second_provider_without_replacement() {
    let mut slot = ExclusiveCapabilitySlot::<TestServiceCapability>::new(TEST_SCOPE);
    let default = prepare_selected_test_provider(&slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    slot.install(default).expect("install first provider");

    let alternate = prepare_selected_test_provider(&slot, ALTERNATE_TEST_PROVIDER)
        .expect("alternate candidate");
    let error = slot
        .install(alternate)
        .expect_err("second provider must require an explicit replacement");

    assert_eq!(
        error,
        ExclusiveSlotError::Occupied {
            capability: CapabilityId::of::<TestServiceCapability>(),
            scope: TEST_SCOPE,
            current_provider: DEFAULT_TEST_PROVIDER,
            attempted_provider: ALTERNATE_TEST_PROVIDER,
        }
    );
    assert_eq!(consume_test_service(&slot), "built-in-default");
}

#[test]
fn failed_candidate_preparation_preserves_the_current_provider() {
    let mut slot = ExclusiveCapabilitySlot::<TestServiceCapability>::new(TEST_SCOPE);
    let default = prepare_selected_test_provider(&slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    let registration = slot.install(default).expect("install default provider");

    let error = slot.prepare_candidate(ALTERNATE_TEST_PROVIDER, || {
        Err::<TestService, _>("alternate preparation failed")
    });
    let current = slot.current().expect("current provider remains available");

    assert_eq!(
        error.expect_err("candidate preparation must fail"),
        "alternate preparation failed"
    );
    assert_eq!(current.provider(), registration.provider());
    assert_eq!(current.generation(), registration.generation());
    assert_eq!(current.handle().implementation, "built-in-default");
}

#[test]
fn stale_provider_registrations_cannot_revoke_a_successor() {
    let mut slot = ExclusiveCapabilitySlot::<TestServiceCapability>::new(TEST_SCOPE);
    let default = prepare_selected_test_provider(&slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    let stale = slot.install(default).expect("install default provider");

    let alternate = prepare_selected_test_provider(&slot, ALTERNATE_TEST_PROVIDER)
        .expect("alternate candidate");
    let successor = slot
        .replace(alternate)
        .expect("replace default with alternate provider");

    assert_eq!(stale.generation(), ComponentGeneration::INITIAL);
    assert_eq!(successor.generation().get(), 2);
    assert!(!slot.revoke(&stale));
    assert_eq!(consume_test_service(&slot), "alternate");
    assert!(slot.revoke(&successor));
    assert!(!slot.revoke(&successor));
    assert!(slot.current().is_none());
}
