use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    future::Future,
    io,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use futures::task::noop_waker_ref;
use gpui::Subscription;

use super::{
    ActivationFingerprint, AsyncStop, CapabilityId, CapabilityKey, ComponentGeneration,
    ComponentId, DependencyDeclaration, DependencyResolution, DependencyResolver,
    DependencyResolverError, DependencySnapshot, DisposeError, EffectScope,
    ExclusiveCapabilitySlot, ExclusiveSlotError, PreparedCapability, ResolvedDependency, ScopeId,
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct TestCatalog {
    revision: usize,
}

struct TestCatalogCapability;

impl CapabilityKey for TestCatalogCapability {
    type Handle = TestCatalog;

    const NAME: &'static str = "nostra.test.catalog";
}

const TEST_CATALOG_PROVIDER: ComponentId = ComponentId::new("nostra.test.catalog-default");

fn required_test_dependencies() -> [DependencyDeclaration; 2] {
    [
        DependencyDeclaration::required::<TestServiceCapability>(),
        DependencyDeclaration::required::<TestCatalogCapability>(),
    ]
}

fn resolve_test_dependencies(
    service_slot: &ExclusiveCapabilitySlot<TestServiceCapability>,
    catalog_slot: &ExclusiveCapabilitySlot<TestCatalogCapability>,
    declarations: &[DependencyDeclaration],
) -> DependencyResolution {
    let mut resolver = DependencyResolver::new();
    resolver
        .include(service_slot)
        .expect("unique test service source");
    resolver
        .include(catalog_slot)
        .expect("unique test catalog source");
    resolver.resolve(declarations)
}

fn resolved_binding<K: CapabilityKey>(fingerprint: &ActivationFingerprint) -> ResolvedDependency {
    fingerprint
        .bindings()
        .iter()
        .copied()
        .find(|binding| binding.capability() == CapabilityId::of::<K>())
        .expect("resolved capability binding")
}

#[derive(Default)]
struct FakeRequiredConsumer {
    active: Option<ActivationFingerprint>,
    mounts: usize,
    unmounts: usize,
    observed_services: Vec<&'static str>,
    observed_catalog_revisions: Vec<usize>,
}

impl FakeRequiredConsumer {
    fn reconcile(&mut self, resolution: DependencyResolution) {
        match resolution {
            DependencyResolution::Pending(_) => {
                if self.active.take().is_some() {
                    self.unmounts += 1;
                }
            }
            DependencyResolution::Ready(snapshot) => {
                let fingerprint = snapshot.activation_fingerprint().clone();
                if self.active.as_ref() == Some(&fingerprint) {
                    return;
                }
                if self.active.is_some() {
                    self.unmounts += 1;
                }
                self.mounts += 1;
                self.observed_services.push(
                    snapshot
                        .lease::<TestServiceCapability>()
                        .expect("required test service")
                        .handle()
                        .implementation,
                );
                self.observed_catalog_revisions.push(
                    snapshot
                        .lease::<TestCatalogCapability>()
                        .expect("required test catalog")
                        .handle()
                        .revision,
                );
                self.active = Some(fingerprint);
            }
        }
    }
}

fn install_test_catalog(
    slot: &mut ExclusiveCapabilitySlot<TestCatalogCapability>,
) -> super::ProviderRegistration<TestCatalogCapability> {
    let candidate = slot
        .prepare_candidate(TEST_CATALOG_PROVIDER, || {
            Ok::<_, ()>(TestCatalog { revision: 1 })
        })
        .expect("test catalog candidate");
    slot.install(candidate).expect("install test catalog")
}

fn ready_snapshot(resolution: DependencyResolution) -> DependencySnapshot {
    match resolution {
        DependencyResolution::Ready(snapshot) => snapshot,
        DependencyResolution::Pending(pending) => {
            panic!(
                "expected ready dependencies, missing {:?}",
                pending.missing()
            )
        }
    }
}

#[test]
fn required_consumer_activates_after_providers_register() {
    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let mut catalog_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let dependencies = required_test_dependencies();
    let mut consumer = FakeRequiredConsumer::default();

    let initial = resolve_test_dependencies(&service_slot, &catalog_slot, &dependencies);
    let DependencyResolution::Pending(pending) = &initial else {
        panic!("empty Provider slots must keep the consumer pending");
    };
    assert_eq!(
        pending.missing(),
        [
            CapabilityId::of::<TestCatalogCapability>(),
            CapabilityId::of::<TestServiceCapability>(),
        ]
    );
    consumer.reconcile(initial);
    assert_eq!(consumer.mounts, 0);
    assert!(consumer.active.is_none());

    let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot.install(service).expect("install test service");
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));
    assert_eq!(consumer.mounts, 0);

    install_test_catalog(&mut catalog_slot);
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));

    assert_eq!(consumer.mounts, 1);
    assert_eq!(consumer.unmounts, 0);
    assert_eq!(consumer.observed_services, ["built-in-default"]);
    assert_eq!(consumer.observed_catalog_revisions, [1]);
}

#[test]
fn provider_generation_change_requires_consumer_remount() {
    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let mut catalog_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let dependencies = required_test_dependencies();
    let mut consumer = FakeRequiredConsumer::default();

    let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot.install(service).expect("install test service");
    install_test_catalog(&mut catalog_slot);
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));
    let initial = consumer.active.clone().expect("initial activation");
    let initial_service = resolved_binding::<TestServiceCapability>(&initial);

    let successor = service_slot
        .prepare_candidate(DEFAULT_TEST_PROVIDER, || {
            Ok::<_, ()>(TestService {
                implementation: "built-in-default-v2",
            })
        })
        .expect("successor candidate");
    service_slot
        .replace(successor)
        .expect("replace test service");
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));
    let successor = consumer.active.as_ref().expect("successor activation");
    let successor_service = resolved_binding::<TestServiceCapability>(successor);

    assert_ne!(consumer.active.as_ref(), Some(&initial));
    assert_eq!(initial_service.provider(), DEFAULT_TEST_PROVIDER);
    assert_eq!(successor_service.provider(), DEFAULT_TEST_PROVIDER);
    assert_eq!(initial_service.generation(), ComponentGeneration::INITIAL);
    assert_eq!(successor_service.generation().get(), 2);
    assert_eq!(consumer.mounts, 2);
    assert_eq!(consumer.unmounts, 1);
    assert_eq!(
        consumer.observed_services,
        ["built-in-default", "built-in-default-v2"]
    );
    assert_eq!(consumer.observed_catalog_revisions, [1, 1]);
}

#[test]
fn missing_required_dependency_produces_only_a_complete_pending_state() {
    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let mut catalog_slot = ExclusiveCapabilitySlot::<TestCatalogCapability>::new(TEST_SCOPE);
    let dependencies = required_test_dependencies();
    let mut consumer = FakeRequiredConsumer::default();

    let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot.install(service).expect("install test service");
    let catalog_registration = install_test_catalog(&mut catalog_slot);
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));
    assert_eq!(consumer.mounts, 1);

    assert!(catalog_slot.revoke(&catalog_registration));

    let resolution = resolve_test_dependencies(&service_slot, &catalog_slot, &dependencies);
    let DependencyResolution::Pending(pending) = &resolution else {
        panic!("missing catalog must keep the consumer pending");
    };

    assert_eq!(
        pending.missing(),
        [CapabilityId::of::<TestCatalogCapability>()]
    );
    consumer.reconcile(resolution);
    assert!(consumer.active.is_none());
    assert_eq!(consumer.mounts, 1);
    assert_eq!(consumer.unmounts, 1);
}

fn reconcile_with_loading_order(
    consumer_first: bool,
    service_provider_first: bool,
) -> FakeRequiredConsumer {
    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let mut catalog_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let dependencies = required_test_dependencies();
    let mut consumer = FakeRequiredConsumer::default();

    if consumer_first {
        consumer.reconcile(resolve_test_dependencies(
            &service_slot,
            &catalog_slot,
            &dependencies,
        ));
    }

    let mut install_service = || {
        let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
            .expect("built-in default candidate");
        service_slot.install(service).expect("install test service");
    };
    if service_provider_first {
        install_service();
        install_test_catalog(&mut catalog_slot);
    } else {
        install_test_catalog(&mut catalog_slot);
        install_service();
    }
    consumer.reconcile(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &dependencies,
    ));
    consumer
}

#[test]
fn dependency_resolution_is_independent_of_registration_order() {
    let providers_first_service_first = reconcile_with_loading_order(false, true);
    let providers_first_catalog_first = reconcile_with_loading_order(false, false);
    let consumer_first_service_first = reconcile_with_loading_order(true, true);
    let consumer_first_catalog_first = reconcile_with_loading_order(true, false);

    for consumer in [
        &providers_first_catalog_first,
        &consumer_first_service_first,
        &consumer_first_catalog_first,
    ] {
        assert_eq!(consumer.active, providers_first_service_first.active);
        assert_eq!(consumer.mounts, 1);
        assert_eq!(consumer.unmounts, 0);
        assert_eq!(
            consumer.observed_services,
            providers_first_service_first.observed_services
        );
        assert_eq!(
            consumer.observed_catalog_revisions,
            providers_first_service_first.observed_catalog_revisions
        );
    }
    assert_eq!(providers_first_service_first.mounts, 1);
    assert_eq!(providers_first_service_first.unmounts, 0);

    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let mut catalog_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot.install(service).expect("install test service");
    install_test_catalog(&mut catalog_slot);
    let forward = ready_snapshot(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &required_test_dependencies(),
    ));
    let reversed = ready_snapshot(resolve_test_dependencies(
        &service_slot,
        &catalog_slot,
        &[
            DependencyDeclaration::required::<TestCatalogCapability>(),
            DependencyDeclaration::required::<TestServiceCapability>(),
        ],
    ));

    assert_eq!(
        forward.activation_fingerprint(),
        reversed.activation_fingerprint()
    );
}

#[test]
fn dependency_resolver_rejects_ambiguous_visible_sources() {
    let application_slot = ExclusiveCapabilitySlot::<TestServiceCapability>::new(TEST_SCOPE);
    let conversation_scope = ScopeId::new(2);
    let conversation_slot =
        ExclusiveCapabilitySlot::<TestServiceCapability>::new(conversation_scope);
    let mut resolver = DependencyResolver::new();

    resolver
        .include(&application_slot)
        .expect("first visible source");

    assert_eq!(
        resolver.include(&conversation_slot),
        Err(DependencyResolverError::DuplicateSource {
            capability: CapabilityId::of::<TestServiceCapability>(),
            existing_scope: TEST_SCOPE,
            attempted_scope: conversation_scope,
        })
    );
}

#[test]
fn component_without_dependencies_is_ready_with_an_empty_fingerprint() {
    let snapshot = ready_snapshot(DependencyResolver::new().resolve(&[]));

    assert!(snapshot.activation_fingerprint().bindings().is_empty());
    assert!(snapshot.lease::<TestServiceCapability>().is_none());
}

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut cx = Context::from_waker(noop_waker_ref());
    future.poll(&mut cx)
}

#[derive(Clone)]
struct TestQuiescenceBarrier {
    attempts: Rc<Cell<usize>>,
    released: Rc<Cell<bool>>,
    events: Rc<RefCell<Vec<&'static str>>>,
    started_event: &'static str,
    finished_event: &'static str,
}

impl AsyncStop for TestQuiescenceBarrier {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        self.attempts.set(self.attempts.get() + 1);
        self.events.borrow_mut().push(self.started_event);
        let released = Rc::clone(&self.released);
        let events = Rc::clone(&self.events);
        let finished_event = self.finished_event;
        Box::pin(futures::future::poll_fn(move |_| {
            if released.get() {
                events.borrow_mut().push(finished_event);
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }))
    }
}

struct FailingOnceStop {
    failed: bool,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl AsyncStop for FailingOnceStop {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        if !self.failed {
            self.failed = true;
            return Box::pin(async {
                Err(DisposeError::new(io::Error::other(
                    "test quiescence failure",
                )))
            });
        }
        let events = Rc::clone(&self.events);
        Box::pin(async move {
            events.borrow_mut().push("async-stop-finished");
            Ok(())
        })
    }
}

#[test]
fn effect_scope_disposes_sync_effects_in_reverse_order_once() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut scope = EffectScope::new();
    for event in ["first", "second", "third"] {
        let events = Rc::clone(&events);
        scope.own_sync(move || events.borrow_mut().push(event));
    }

    futures::executor::block_on(scope.quiesce_and_dispose()).expect("dispose effect scope");
    futures::executor::block_on(scope.quiesce_and_dispose()).expect("repeat disposal is a no-op");

    assert_eq!(*events.borrow(), ["third", "second", "first"]);
}

#[test]
fn failed_mount_rolls_back_owned_subscription_and_sync_effects() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut scope = EffectScope::new();
    let first_events = Rc::clone(&events);
    scope.own_sync(move || first_events.borrow_mut().push("first-undo"));
    let subscription_events = Rc::clone(&events);
    scope.own_resource(Subscription::new(move || {
        subscription_events
            .borrow_mut()
            .push("subscription-dropped");
    }));
    let last_events = Rc::clone(&events);
    scope.own_sync(move || last_events.borrow_mut().push("last-undo"));

    let mount_result = Err::<(), _>("mount failed after acquiring resources");
    let error = match mount_result {
        Ok(()) => panic!("test mount must fail"),
        Err(error) => {
            futures::executor::block_on(scope.quiesce_and_dispose())
                .expect("rollback acquired effects");
            error
        }
    };

    assert_eq!(error, "mount failed after acquiring resources");
    assert_eq!(
        *events.borrow(),
        ["last-undo", "subscription-dropped", "first-undo"]
    );
}

#[test]
fn async_quiescence_blocks_earlier_effects_until_the_barrier_finishes() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(Cell::new(false));
    let attempts = Rc::new(Cell::new(0));
    let mut scope = EffectScope::new();
    let first_events = Rc::clone(&events);
    scope.own_sync(move || first_events.borrow_mut().push("first-undo"));
    scope.own_async(TestQuiescenceBarrier {
        attempts: Rc::clone(&attempts),
        released: Rc::clone(&released),
        events: Rc::clone(&events),
        started_event: "async-stop-started",
        finished_event: "async-stop-finished",
    });
    let last_events = Rc::clone(&events);
    scope.own_sync(move || last_events.borrow_mut().push("last-undo"));

    let mut dispose = Box::pin(scope.quiesce_and_dispose());
    assert!(matches!(poll_once(dispose.as_mut()), Poll::Pending));
    assert_eq!(*events.borrow(), ["last-undo", "async-stop-started"]);
    assert_eq!(attempts.get(), 1);

    released.set(true);
    assert!(matches!(poll_once(dispose.as_mut()), Poll::Ready(Ok(()))));
    drop(dispose);

    assert_eq!(
        *events.borrow(),
        [
            "last-undo",
            "async-stop-started",
            "async-stop-finished",
            "first-undo",
        ]
    );
}

#[test]
fn cancelling_disposal_keeps_the_async_barrier_owned_for_retry() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(Cell::new(false));
    let attempts = Rc::new(Cell::new(0));
    let mut scope = EffectScope::new();
    let first_events = Rc::clone(&events);
    scope.own_sync(move || first_events.borrow_mut().push("first-undo"));
    scope.own_async(TestQuiescenceBarrier {
        attempts: Rc::clone(&attempts),
        released: Rc::clone(&released),
        events: Rc::clone(&events),
        started_event: "async-stop-started",
        finished_event: "async-stop-finished",
    });

    {
        let mut cancelled = Box::pin(scope.quiesce_and_dispose());
        assert!(matches!(poll_once(cancelled.as_mut()), Poll::Pending));
    }
    assert_eq!(*events.borrow(), ["async-stop-started"]);
    assert_eq!(attempts.get(), 1);

    released.set(true);
    futures::executor::block_on(scope.quiesce_and_dispose())
        .expect("retry waits for the retained barrier");

    assert_eq!(attempts.get(), 2);
    assert_eq!(
        *events.borrow(),
        [
            "async-stop-started",
            "async-stop-started",
            "async-stop-finished",
            "first-undo",
        ]
    );
}

#[test]
fn failed_async_stop_retains_ownership_and_order_for_retry() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut scope = EffectScope::new();
    let first_events = Rc::clone(&events);
    scope.own_sync(move || first_events.borrow_mut().push("first-undo"));
    scope.own_async(FailingOnceStop {
        failed: false,
        events: Rc::clone(&events),
    });

    let error = futures::executor::block_on(scope.quiesce_and_dispose())
        .expect_err("first stop attempt fails");
    assert_eq!(error.to_string(), "test quiescence failure");
    assert!(events.borrow().is_empty());

    futures::executor::block_on(scope.quiesce_and_dispose())
        .expect("second stop attempt completes");
    assert_eq!(*events.borrow(), ["async-stop-finished", "first-undo"]);
}

#[test]
fn multiple_async_stops_run_serially_in_reverse_order() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let first_released = Rc::new(Cell::new(false));
    let first_attempts = Rc::new(Cell::new(0));
    let second_released = Rc::new(Cell::new(false));
    let second_attempts = Rc::new(Cell::new(0));
    let mut scope = EffectScope::new();
    scope.own_async(TestQuiescenceBarrier {
        attempts: Rc::clone(&first_attempts),
        released: Rc::clone(&first_released),
        events: Rc::clone(&events),
        started_event: "first-stop-started",
        finished_event: "first-stop-finished",
    });
    scope.own_async(TestQuiescenceBarrier {
        attempts: Rc::clone(&second_attempts),
        released: Rc::clone(&second_released),
        events: Rc::clone(&events),
        started_event: "second-stop-started",
        finished_event: "second-stop-finished",
    });

    let mut dispose = Box::pin(scope.quiesce_and_dispose());
    assert!(matches!(poll_once(dispose.as_mut()), Poll::Pending));
    assert_eq!(*events.borrow(), ["second-stop-started"]);
    assert_eq!(first_attempts.get(), 0);
    assert_eq!(second_attempts.get(), 1);

    second_released.set(true);
    assert!(matches!(poll_once(dispose.as_mut()), Poll::Pending));
    assert_eq!(
        *events.borrow(),
        [
            "second-stop-started",
            "second-stop-finished",
            "first-stop-started",
        ]
    );
    assert_eq!(first_attempts.get(), 1);

    first_released.set(true);
    assert!(matches!(poll_once(dispose.as_mut()), Poll::Ready(Ok(()))));
    drop(dispose);
    assert_eq!(
        *events.borrow(),
        [
            "second-stop-started",
            "second-stop-finished",
            "first-stop-started",
            "first-stop-finished",
        ]
    );
}

#[test]
fn dropping_scope_after_cancel_does_not_cross_the_async_barrier() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let released = Rc::new(Cell::new(false));
    let attempts = Rc::new(Cell::new(0));
    let mut scope = EffectScope::new();
    let first_events = Rc::clone(&events);
    scope.own_sync(move || first_events.borrow_mut().push("first-undo"));
    scope.own_async(TestQuiescenceBarrier {
        attempts,
        released,
        events: Rc::clone(&events),
        started_event: "async-stop-started",
        finished_event: "async-stop-finished",
    });
    let last_events = Rc::clone(&events);
    scope.own_sync(move || last_events.borrow_mut().push("last-undo"));

    {
        let mut cancelled = Box::pin(scope.quiesce_and_dispose());
        assert!(matches!(poll_once(cancelled.as_mut()), Poll::Pending));
    }
    drop(scope);

    assert_eq!(*events.borrow(), ["last-undo", "async-stop-started"]);
}
