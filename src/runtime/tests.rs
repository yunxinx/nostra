use std::{
    cell::{Cell, RefCell},
    collections::HashSet,
    future::Future,
    io,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::Duration,
};

use futures::task::noop_waker_ref;
use gpui::Subscription;

use super::{
    ActivationFingerprint, AsyncStop, CapabilityId, CapabilityKey, ComponentGeneration,
    ComponentId, ComponentLifecycle, ComponentSnapshot, ComponentSnapshotDetails,
    ComponentSnapshotViolation, ContributionRevision, DependencyDeclaration, DependencyResolution,
    DependencyResolver, DependencyResolverError, DependencySnapshot, DesiredRevision, DisposeError,
    EffectScope, ExclusiveCapabilitySlot, ExclusiveSlotError, MissingDependencySnapshot,
    PreparedCapability, ReconcileFailureKind, ReconcileObserver, ReconcileStage, ReconcileStatus,
    ResolvedDependency, RuntimeComponentState, RuntimeDiagnostic, RuntimeResourceCounts,
    RuntimeSnapshot, RuntimeSnapshotError, ScopeId, ScopeKind, ScopeLocalReconciler, ScopeState,
    ScopeTree, ScopedComponentId, StartupPolicy,
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

fn resolved_test_service_binding() -> ResolvedDependency {
    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let service = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot
        .install(service)
        .expect("install built-in default");
    resolved_test_service_binding_from(&service_slot)
}

fn resolved_test_service_binding_from(
    service_slot: &ExclusiveCapabilitySlot<TestServiceCapability>,
) -> ResolvedDependency {
    let mut resolver = DependencyResolver::new();
    resolver
        .include(service_slot)
        .expect("unique test service source");
    let snapshot = ready_snapshot(
        resolver.resolve(&[DependencyDeclaration::required::<TestServiceCapability>()]),
    );
    resolved_binding::<TestServiceCapability>(snapshot.activation_fingerprint())
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

struct DropObservedStop {
    dropped: Rc<Cell<bool>>,
    events: Rc<RefCell<Vec<&'static str>>>,
}

impl AsyncStop for DropObservedStop {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        self.events.borrow_mut().push("conversation-stop-started");
        Box::pin(futures::future::pending())
    }
}

impl Drop for DropObservedStop {
    fn drop(&mut self) {
        self.dropped.set(true);
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

struct PrepareTransitionGuard {
    in_flight: Rc<Cell<usize>>,
}

impl Drop for PrepareTransitionGuard {
    fn drop(&mut self) {
        self.in_flight.set(self.in_flight.get() - 1);
    }
}

struct RevisionStopBarrier {
    revision: u8,
    attempts: Rc<Cell<usize>>,
    released: Rc<Cell<bool>>,
    events: Rc<RefCell<Vec<String>>>,
}

struct BlockedStop {
    revision: u8,
    released: Rc<Cell<bool>>,
    attempts: Rc<Cell<usize>>,
}

impl AsyncStop for RevisionStopBarrier {
    fn stop(&mut self) -> Pin<Box<dyn Future<Output = Result<(), DisposeError>> + '_>> {
        self.attempts.set(self.attempts.get() + 1);
        self.events
            .borrow_mut()
            .push(format!("stop-start-{}", self.revision));
        let revision = self.revision;
        let released = Rc::clone(&self.released);
        let events = Rc::clone(&self.events);
        Box::pin(futures::future::poll_fn(move |_| {
            if released.get() {
                events.borrow_mut().push(format!("stop-finish-{revision}"));
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        }))
    }
}

struct RevisionLifecycle {
    prepare_calls: Rc<RefCell<Vec<u8>>>,
    events: Rc<RefCell<Vec<String>>>,
    in_flight: Rc<Cell<usize>>,
    max_in_flight: Rc<Cell<usize>>,
    blocked_prepare: Option<(u8, Rc<Cell<bool>>)>,
    blocked_stop: Option<BlockedStop>,
    fail_first_stop: Option<(u8, Rc<RefCell<Vec<&'static str>>>)>,
    fail_prepare: Option<u8>,
    fail_activation_once: Option<(u8, Rc<Cell<bool>>)>,
}

impl RevisionLifecycle {
    fn new(events: Rc<RefCell<Vec<String>>>) -> Self {
        Self {
            prepare_calls: Rc::new(RefCell::new(Vec::new())),
            events,
            in_flight: Rc::new(Cell::new(0)),
            max_in_flight: Rc::new(Cell::new(0)),
            blocked_prepare: None,
            blocked_stop: None,
            fail_first_stop: None,
            fail_prepare: None,
            fail_activation_once: None,
        }
    }
}

impl ComponentLifecycle<u8> for RevisionLifecycle {
    type Prepared = u8;

    fn prepare<'a>(
        &'a mut self,
        _revision: super::DesiredRevision,
        desired: &'a u8,
        effects: &'a mut EffectScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Self::Prepared>> + 'a>> {
        let desired = *desired;
        self.prepare_calls.borrow_mut().push(desired);
        let in_flight = self.in_flight.get() + 1;
        self.in_flight.set(in_flight);
        self.max_in_flight
            .set(self.max_in_flight.get().max(in_flight));
        let guard = PrepareTransitionGuard {
            in_flight: Rc::clone(&self.in_flight),
        };
        let events = Rc::clone(&self.events);
        let prepare_release = self
            .blocked_prepare
            .as_ref()
            .filter(|(revision, _)| *revision == desired)
            .map(|(_, released)| Rc::clone(released));
        let fail_prepare = self.fail_prepare == Some(desired);

        Box::pin(async move {
            if let Some(released) = prepare_release {
                futures::future::poll_fn(move |_| {
                    if released.get() {
                        Poll::Ready(())
                    } else {
                        Poll::Pending
                    }
                })
                .await;
            }
            if fail_prepare {
                let events = Rc::clone(&events);
                effects.own_sync(move || {
                    events
                        .borrow_mut()
                        .push(format!("candidate-undo-{desired}"));
                });
                return Err(anyhow::anyhow!("candidate preparation failed"));
            }
            drop(guard);
            Ok(desired)
        })
    }

    fn activate<'a>(
        &'a mut self,
        _revision: super::DesiredRevision,
        desired: &'a u8,
        prepared: Self::Prepared,
        effects: &'a mut EffectScope,
    ) -> anyhow::Result<()> {
        let desired = *desired;
        let events = Rc::clone(&self.events);
        let stop_barrier = self
            .blocked_stop
            .as_ref()
            .filter(|stop| stop.revision == desired)
            .map(|stop| (Rc::clone(&stop.released), Rc::clone(&stop.attempts)));
        let failing_stop_events = self
            .fail_first_stop
            .as_ref()
            .filter(|(revision, _)| *revision == desired)
            .map(|(_, events)| Rc::clone(events));
        let fail_activation = self
            .fail_activation_once
            .as_ref()
            .filter(|(revision, _)| *revision == desired)
            .map(|(_, failed)| Rc::clone(failed));

        if prepared != desired {
            anyhow::bail!("prepared revision does not match desired revision");
        }
        let undo_events = Rc::clone(&events);
        effects.own_sync(move || {
            undo_events.borrow_mut().push(format!("undo-{desired}"));
        });
        if let Some((released, attempts)) = stop_barrier {
            effects.own_async(RevisionStopBarrier {
                revision: desired,
                attempts,
                released,
                events: Rc::clone(&events),
            });
        }
        if let Some(events) = failing_stop_events {
            effects.own_async(FailingOnceStop {
                failed: false,
                events,
            });
        }
        if fail_activation.is_some_and(|failed| !failed.replace(true)) {
            anyhow::bail!("candidate activation failed");
        }
        events.borrow_mut().push(format!("publish-{desired}"));
        Ok(())
    }
}

struct ObservedActivationLifecycle {
    observer: ReconcileObserver,
    observed_stage: Rc<Cell<Option<ReconcileStage>>>,
}

impl ComponentLifecycle<u8> for ObservedActivationLifecycle {
    type Prepared = u8;

    fn prepare<'a>(
        &'a mut self,
        _revision: DesiredRevision,
        desired: &'a u8,
        _effects: &'a mut EffectScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Self::Prepared>> + 'a>> {
        Box::pin(futures::future::ready(Ok(*desired)))
    }

    fn activate<'a>(
        &'a mut self,
        _revision: DesiredRevision,
        desired: &'a u8,
        prepared: Self::Prepared,
        _effects: &'a mut EffectScope,
    ) -> anyhow::Result<()> {
        if prepared != *desired {
            anyhow::bail!("prepared revision does not match desired revision");
        }
        self.observed_stage.set(
            self.observer
                .transition()
                .map(|transition| transition.stage()),
        );
        Ok(())
    }
}

const RECONCILE_COMPONENT: ComponentId = ComponentId::new("nostra.test.reconcile");

#[test]
fn activation_transition_is_observable_until_publication_finishes() {
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let observer = reconciler.observer();
    let observed_stage = Rc::new(Cell::new(None));
    let mut lifecycle = ObservedActivationLifecycle {
        observer: observer.clone(),
        observed_stage: Rc::clone(&observed_stage),
    };

    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    assert_eq!(observed_stage.get(), Some(ReconcileStage::Activating));
    assert!(observer.transition().is_none());
    assert!(observer.last_failure().is_none());
}

#[test]
fn desired_changes_during_one_transition_are_serialized_and_converge() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let first_mount_released = Rc::new(Cell::new(false));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    lifecycle.blocked_prepare = Some((1, Rc::clone(&first_mount_released)));
    let prepare_calls = Rc::clone(&lifecycle.prepare_calls);
    let max_in_flight = Rc::clone(&lifecycle.max_in_flight);
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();

    let mut reconcile = Box::pin(reconciler.reconcile(&mut lifecycle));
    assert!(matches!(poll_once(reconcile.as_mut()), Poll::Pending));
    target
        .set_desired(Some(2))
        .expect("second desired revision");
    target
        .set_desired(Some(3))
        .expect("latest desired revision");

    assert_eq!(*prepare_calls.borrow(), [1]);
    assert_eq!(max_in_flight.get(), 1);

    first_mount_released.set(true);
    let Poll::Ready(ReconcileStatus::Settled { active, .. }) = poll_once(reconcile.as_mut()) else {
        panic!("reconciliation must settle on the latest desired revision");
    };
    assert!(active);
    drop(reconcile);

    assert_eq!(reconciler.active_desired(), Some(&3));
    assert_eq!(*prepare_calls.borrow(), [1, 3]);
    assert_eq!(max_in_flight.get(), 1);
    assert_eq!(*events.borrow(), ["publish-3"]);
}

#[test]
fn successor_is_not_published_until_old_quiescence_completes() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let old_stop_released = Rc::new(Cell::new(false));
    let old_stop_attempts = Rc::new(Cell::new(0));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    lifecycle.blocked_stop = Some(BlockedStop {
        revision: 1,
        released: Rc::clone(&old_stop_released),
        attempts: Rc::clone(&old_stop_attempts),
    });
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();

    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    target
        .set_desired(Some(2))
        .expect("successor desired revision");

    let mut replace = Box::pin(reconciler.reconcile(&mut lifecycle));
    assert!(matches!(poll_once(replace.as_mut()), Poll::Pending));
    assert_eq!(old_stop_attempts.get(), 1);
    assert_eq!(*events.borrow(), ["publish-1", "stop-start-1"]);

    old_stop_released.set(true);
    assert!(matches!(
        poll_once(replace.as_mut()),
        Poll::Ready(ReconcileStatus::Settled { active: true, .. })
    ));
    drop(replace);

    assert_eq!(reconciler.active_desired(), Some(&2));
    assert_eq!(
        *events.borrow(),
        [
            "publish-1",
            "stop-start-1",
            "stop-finish-1",
            "undo-1",
            "publish-2",
        ]
    );
}

struct TypedSlotLifecycle {
    slot: Rc<RefCell<ExclusiveCapabilitySlot<TestServiceCapability>>>,
}

impl ComponentLifecycle<ComponentId> for TypedSlotLifecycle {
    type Prepared = PreparedCapability<TestServiceCapability>;

    fn prepare<'a>(
        &'a mut self,
        _revision: super::DesiredRevision,
        desired: &'a ComponentId,
        _effects: &'a mut EffectScope,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Self::Prepared>> + 'a>> {
        let implementation = match *desired {
            DEFAULT_TEST_PROVIDER => "built-in-default",
            ALTERNATE_TEST_PROVIDER => "alternate",
            _ => return Box::pin(async { anyhow::bail!("unknown test Provider") }),
        };
        let candidate = self.slot.borrow().prepare_candidate(*desired, || {
            Ok::<_, anyhow::Error>(TestService { implementation })
        });
        Box::pin(async move { candidate })
    }

    fn activate<'a>(
        &'a mut self,
        _revision: super::DesiredRevision,
        _desired: &'a ComponentId,
        prepared: Self::Prepared,
        effects: &'a mut EffectScope,
    ) -> anyhow::Result<()> {
        let registration = self.slot.borrow_mut().install(prepared)?;
        let slot = Rc::clone(&self.slot);
        effects.own_sync(move || {
            slot.borrow_mut().revoke(&registration);
        });
        Ok(())
    }
}

#[test]
fn activation_consumes_a_typed_capability_candidate_without_cloning() {
    let slot = Rc::new(RefCell::new(ExclusiveCapabilitySlot::new(TEST_SCOPE)));
    let mut lifecycle = TypedSlotLifecycle {
        slot: Rc::clone(&slot),
    };
    let mut reconciler =
        ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(DEFAULT_TEST_PROVIDER));
    let target = reconciler.target();

    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    let initial = slot.borrow().current().expect("initial typed Provider");
    assert_eq!(initial.provider(), DEFAULT_TEST_PROVIDER);
    assert_eq!(initial.generation(), ComponentGeneration::INITIAL);

    target
        .set_desired(Some(ALTERNATE_TEST_PROVIDER))
        .expect("alternate typed Provider revision");
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    let successor = slot.borrow().current().expect("successor typed Provider");
    assert_eq!(successor.provider(), ALTERNATE_TEST_PROVIDER);
    assert_eq!(successor.generation().get(), 2);
}

#[test]
fn failed_candidate_preparation_preserves_the_active_generation() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    let prepare_calls = Rc::clone(&lifecycle.prepare_calls);
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();

    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    lifecycle.fail_prepare = Some(2);
    target
        .set_desired(Some(2))
        .expect("failing candidate revision");

    let ReconcileStatus::Failed(failure) =
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("failed candidate preparation must be diagnostic");
    };

    assert_eq!(failure.stage(), ReconcileStage::Preparing);
    assert_eq!(failure.kind(), ReconcileFailureKind::Error);
    assert_eq!(reconciler.active_desired(), Some(&1));
    assert_eq!(*prepare_calls.borrow(), [1, 2]);
    assert_eq!(*events.borrow(), ["publish-1", "candidate-undo-2"]);
}

#[test]
fn cancelled_failure_rollback_keeps_candidate_effects_and_reports_the_barrier() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let rollback_released = Rc::new(Cell::new(false));
    let rollback_attempts = Rc::new(Cell::new(0));
    let activation_failed = Rc::new(Cell::new(false));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();

    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    lifecycle.blocked_stop = Some(BlockedStop {
        revision: 2,
        released: Rc::clone(&rollback_released),
        attempts: Rc::clone(&rollback_attempts),
    });
    lifecycle.fail_activation_once = Some((2, Rc::clone(&activation_failed)));
    target
        .set_desired(Some(2))
        .expect("failing activation revision");

    {
        let mut cancelled = Box::pin(reconciler.reconcile(&mut lifecycle));
        assert!(matches!(poll_once(cancelled.as_mut()), Poll::Pending));
    }

    let failure = reconciler
        .last_failure()
        .expect("cancelled rollback remains diagnostic");
    assert_eq!(failure.stage(), ReconcileStage::RollingBack);
    assert_eq!(failure.kind(), ReconcileFailureKind::Interrupted);
    assert_eq!(reconciler.active_desired(), None);
    assert_eq!(rollback_attempts.get(), 1);
    assert_eq!(*events.borrow(), ["publish-1", "undo-1", "stop-start-2"]);

    assert_eq!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Failed(failure)
    );
    assert_eq!(rollback_attempts.get(), 1);

    target
        .set_desired(None)
        .expect("new desired revision authorizes rollback retry");
    rollback_released.set(true);
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: false, .. }
    ));
    assert_eq!(rollback_attempts.get(), 2);
    assert_eq!(
        *events.borrow(),
        [
            "publish-1",
            "undo-1",
            "stop-start-2",
            "stop-start-2",
            "stop-finish-2",
            "undo-2",
        ]
    );
}

#[test]
fn cancelled_stop_retains_effect_ownership_until_explicit_retry() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let stop_released = Rc::new(Cell::new(false));
    let stop_attempts = Rc::new(Cell::new(0));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    lifecycle.blocked_stop = Some(BlockedStop {
        revision: 1,
        released: Rc::clone(&stop_released),
        attempts: Rc::clone(&stop_attempts),
    });
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();
    let observer = reconciler.observer();
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    target
        .set_desired(None)
        .expect("component removal revision");

    {
        let mut cancelled = Box::pin(reconciler.reconcile(&mut lifecycle));
        assert!(matches!(poll_once(cancelled.as_mut()), Poll::Pending));
        let transition = observer
            .transition()
            .expect("stopping transition remains observable");
        assert_eq!(transition.stage(), ReconcileStage::Stopping);
        let snapshot = RuntimeSnapshot::new(
            [ComponentSnapshot::transitioning(
                StartupPolicy::MustActivate,
                &observer,
                transition.started_at() + Duration::from_secs(11),
                ComponentSnapshotDetails::default(),
            )
            .expect("quiescing component snapshot")],
            [],
            Duration::from_secs(10),
        )
        .expect("unique runtime component identities");
        assert_eq!(
            snapshot.components()[0].state(),
            RuntimeComponentState::Quiescing
        );
        assert!(matches!(
            snapshot.diagnostics(),
            [RuntimeDiagnostic::LongTransition {
                stage: ReconcileStage::Stopping,
                ..
            }]
        ));
    }

    assert!(observer.transition().is_none());
    let observed_failure = observer
        .last_failure()
        .expect("cancelled stop remains observable");
    assert_eq!(observed_failure.stage(), ReconcileStage::Stopping);
    assert_eq!(observed_failure.kind(), ReconcileFailureKind::Interrupted);
    assert_eq!(reconciler.active_desired(), Some(&1));
    assert_eq!(stop_attempts.get(), 1);
    assert_eq!(*events.borrow(), ["publish-1", "stop-start-1"]);

    let ReconcileStatus::Failed(failure) =
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("an interrupted stop must remain failed until explicitly retried");
    };
    assert_eq!(failure.stage(), ReconcileStage::Stopping);
    assert_eq!(failure.kind(), ReconcileFailureKind::Interrupted);
    assert_eq!(stop_attempts.get(), 1);

    target.retry().expect("explicit stop retry");
    stop_released.set(true);
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: false, .. }
    ));

    assert_eq!(stop_attempts.get(), 2);
    assert_eq!(reconciler.active_desired(), None);
    assert_eq!(
        *events.borrow(),
        [
            "publish-1",
            "stop-start-1",
            "stop-start-1",
            "stop-finish-1",
            "undo-1",
        ]
    );
}

#[test]
fn failed_stop_does_not_retry_without_a_new_request_revision() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let stop_events = Rc::new(RefCell::new(Vec::new()));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    lifecycle.fail_first_stop = Some((1, Rc::clone(&stop_events)));
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: true, .. }
    ));
    target
        .set_desired(None)
        .expect("component removal revision");

    let ReconcileStatus::Failed(first_failure) =
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("first stop attempt must report its failure");
    };
    assert_eq!(first_failure.stage(), ReconcileStage::Stopping);
    assert_eq!(first_failure.kind(), ReconcileFailureKind::Error);
    assert_eq!(reconciler.active_desired(), Some(&1));
    assert!(stop_events.borrow().is_empty());

    assert_eq!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Failed(first_failure)
    );
    assert_eq!(reconciler.active_desired(), Some(&1));
    assert!(stop_events.borrow().is_empty());

    target.retry().expect("explicit retry revision");
    assert!(matches!(
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle)),
        ReconcileStatus::Settled { active: false, .. }
    ));
    assert_eq!(*stop_events.borrow(), ["async-stop-finished"]);
    assert_eq!(*events.borrow(), ["publish-1", "undo-1"]);
}

#[test]
fn unchanged_desired_state_does_not_start_another_transition() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut lifecycle = RevisionLifecycle::new(Rc::clone(&events));
    let prepare_calls = Rc::clone(&lifecycle.prepare_calls);
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let target = reconciler.target();

    let ReconcileStatus::Settled {
        revision: initial_revision,
        active: true,
    } = futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("initial desired state must activate");
    };
    let unchanged_revision = target
        .set_desired(Some(1))
        .expect("unchanged desired state");
    let ReconcileStatus::Settled {
        revision: settled_revision,
        active: true,
    } = futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("unchanged desired state must stay active");
    };

    assert_eq!(unchanged_revision, initial_revision);
    assert_eq!(settled_revision, initial_revision);
    assert_eq!(*prepare_calls.borrow(), [1]);
    assert_eq!(*events.borrow(), ["publish-1"]);
}

fn scope_chain() -> (ScopeTree, ScopeId, ScopeId, ScopeId) {
    let mut scopes = ScopeTree::new();
    let application = scopes.application();
    let window = scopes.create_window().expect("window scope");
    let conversation = scopes
        .create_conversation(window)
        .expect("conversation scope");
    (scopes, application, window, conversation)
}

#[test]
fn application_capabilities_are_inherited_by_window_and_conversation_scopes() {
    let (mut scopes, application, window, conversation) = scope_chain();

    assert_eq!(scopes.kind(application), Some(ScopeKind::Application));
    assert_eq!(scopes.kind(window), Some(ScopeKind::Window));
    assert_eq!(scopes.kind(conversation), Some(ScopeKind::Conversation));
    assert_eq!(scopes.parent(application), None);
    assert_eq!(scopes.parent(window), Some(application));
    assert_eq!(scopes.parent(conversation), Some(window));
    let candidate = scopes
        .capability_slot::<TestServiceCapability>(application)
        .expect("application capability slot")
        .prepare_candidate(DEFAULT_TEST_PROVIDER, || {
            Ok::<_, ()>(TestService {
                implementation: "application-default",
            })
        })
        .expect("application Provider candidate");
    scopes
        .capability_slot::<TestServiceCapability>(application)
        .expect("application capability slot")
        .install(candidate)
        .expect("install application Provider");

    let from_window = scopes
        .resolve::<TestServiceCapability>(window)
        .expect("resolve from window")
        .expect("inherited application Provider");
    let from_conversation = scopes
        .resolve::<TestServiceCapability>(conversation)
        .expect("resolve from conversation")
        .expect("inherited application Provider");

    assert_eq!(from_window.scope(), application);
    assert_eq!(from_conversation.scope(), application);
    assert_eq!(from_window.handle().implementation, "application-default");
    assert_eq!(
        from_conversation.handle().implementation,
        "application-default"
    );
    futures::executor::block_on(scopes.close(application)).expect("close inherited scopes");
}

#[test]
fn nearest_child_provider_overrides_inherited_capabilities() {
    let (mut scopes, application, window, conversation) = scope_chain();

    for (scope, provider, implementation) in [
        (application, DEFAULT_TEST_PROVIDER, "application"),
        (window, ALTERNATE_TEST_PROVIDER, "window"),
    ] {
        let candidate = scopes
            .capability_slot::<TestServiceCapability>(scope)
            .expect("scoped capability slot")
            .prepare_candidate(provider, || Ok::<_, ()>(TestService { implementation }))
            .expect("scoped Provider candidate");
        scopes
            .capability_slot::<TestServiceCapability>(scope)
            .expect("scoped capability slot")
            .install(candidate)
            .expect("install scoped Provider");
    }

    let inherited_window = scopes
        .resolve::<TestServiceCapability>(conversation)
        .expect("resolve window override")
        .expect("window Provider");
    assert_eq!(inherited_window.scope(), window);
    assert_eq!(inherited_window.handle().implementation, "window");

    let candidate = scopes
        .capability_slot::<TestServiceCapability>(conversation)
        .expect("conversation capability slot")
        .prepare_candidate(DEFAULT_TEST_PROVIDER, || {
            Ok::<_, ()>(TestService {
                implementation: "conversation",
            })
        })
        .expect("conversation Provider candidate");
    scopes
        .capability_slot::<TestServiceCapability>(conversation)
        .expect("conversation capability slot")
        .install(candidate)
        .expect("install conversation Provider");

    let nearest = scopes
        .resolve::<TestServiceCapability>(conversation)
        .expect("resolve conversation override")
        .expect("conversation Provider");
    assert_eq!(nearest.scope(), conversation);
    assert_eq!(nearest.handle().implementation, "conversation");
    futures::executor::block_on(scopes.close(application)).expect("close override scopes");
}

#[test]
fn closing_a_parent_waits_for_children_and_is_idempotent() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let conversation_released = Rc::new(Cell::new(false));
    let conversation_attempts = Rc::new(Cell::new(0));
    let (mut scopes, application, window, conversation) = scope_chain();

    for (scope, event) in [
        (application, "application-undo"),
        (window, "window-undo"),
        (conversation, "conversation-undo"),
    ] {
        let events = Rc::clone(&events);
        scopes
            .own_sync(scope, move || events.borrow_mut().push(event))
            .expect("scope-owned effect");
    }
    scopes
        .own_async(
            conversation,
            TestQuiescenceBarrier {
                attempts: Rc::clone(&conversation_attempts),
                released: Rc::clone(&conversation_released),
                events: Rc::clone(&events),
                started_event: "conversation-stop-started",
                finished_event: "conversation-stop-finished",
            },
        )
        .expect("conversation quiescence barrier");

    let mut close = Box::pin(scopes.close(application));
    assert!(matches!(poll_once(close.as_mut()), Poll::Pending));
    assert_eq!(conversation_attempts.get(), 1);
    assert_eq!(*events.borrow(), ["conversation-stop-started"]);
    drop(close);
    assert_eq!(scopes.state(application), Some(ScopeState::Open));
    assert_eq!(scopes.state(window), Some(ScopeState::Open));
    assert_eq!(scopes.state(conversation), Some(ScopeState::Closing));

    conversation_released.set(true);
    futures::executor::block_on(scopes.close(application)).expect("retry parent close");
    assert_eq!(conversation_attempts.get(), 2);
    assert_eq!(scopes.state(application), Some(ScopeState::Closed));
    assert_eq!(scopes.state(window), Some(ScopeState::Closed));
    assert_eq!(scopes.state(conversation), Some(ScopeState::Closed));
    assert_eq!(
        *events.borrow(),
        [
            "conversation-stop-started",
            "conversation-stop-started",
            "conversation-stop-finished",
            "conversation-undo",
            "window-undo",
            "application-undo",
        ]
    );

    futures::executor::block_on(scopes.close(application)).expect("repeated close is a no-op");
    assert_eq!(conversation_attempts.get(), 2);
    assert_eq!(events.borrow().len(), 6);
}

#[test]
fn failed_child_stop_keeps_parent_scopes_open_until_retry() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let (mut scopes, application, window, conversation) = scope_chain();

    for (scope, event) in [
        (application, "application-undo"),
        (window, "window-undo"),
        (conversation, "conversation-undo"),
    ] {
        let events = Rc::clone(&events);
        scopes
            .own_sync(scope, move || events.borrow_mut().push(event))
            .expect("scope-owned effect");
    }
    scopes
        .own_async(
            conversation,
            FailingOnceStop {
                failed: false,
                events: Rc::clone(&events),
            },
        )
        .expect("conversation stop owner");

    let error = futures::executor::block_on(scopes.close(application))
        .expect_err("first child stop attempt fails");
    assert!(matches!(
        error,
        super::ScopeError::Dispose { scope, .. } if scope == conversation
    ));
    assert_eq!(scopes.state(application), Some(ScopeState::Open));
    assert_eq!(scopes.state(window), Some(ScopeState::Open));
    assert_eq!(scopes.state(conversation), Some(ScopeState::Closing));
    assert!(events.borrow().is_empty());

    futures::executor::block_on(scopes.close(application)).expect("retry parent close");
    assert_eq!(scopes.state(application), Some(ScopeState::Closed));
    assert_eq!(scopes.state(window), Some(ScopeState::Closed));
    assert_eq!(scopes.state(conversation), Some(ScopeState::Closed));
    assert_eq!(
        *events.borrow(),
        [
            "async-stop-finished",
            "conversation-undo",
            "window-undo",
            "application-undo",
        ]
    );
}

#[test]
fn dropping_a_tree_after_cancelled_close_does_not_cross_the_child_barrier() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let stop_dropped = Rc::new(Cell::new(false));
    let (mut scopes, application, _window, conversation) = scope_chain();
    let application_events = Rc::clone(&events);
    scopes
        .own_sync(application, move || {
            application_events.borrow_mut().push("application-undo");
        })
        .expect("application effect");
    let conversation_events = Rc::clone(&events);
    scopes
        .own_sync(conversation, move || {
            conversation_events.borrow_mut().push("conversation-undo");
        })
        .expect("conversation effect");
    scopes
        .own_async(
            conversation,
            DropObservedStop {
                dropped: Rc::clone(&stop_dropped),
                events: Rc::clone(&events),
            },
        )
        .expect("conversation quiescence barrier");

    {
        let mut close = Box::pin(scopes.close(application));
        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));
    }
    assert!(!stop_dropped.get());
    drop(scopes);

    assert!(stop_dropped.get());
    assert_eq!(*events.borrow(), ["conversation-stop-started"]);
}

#[test]
fn startup_audit_aggregates_required_pending_and_failed_components() {
    const FAILED_COMPONENT: ComponentId = ComponentId::new("nostra.test.startup-failed");
    const PENDING_COMPONENT: ComponentId = ComponentId::new("nostra.test.startup-pending");
    const BLOCKING_COMPONENT: ComponentId = ComponentId::new("nostra.test.startup-blocker");
    let pending = ComponentSnapshot::pending(
        PENDING_COMPONENT,
        TEST_SCOPE,
        StartupPolicy::MustActivate,
        DesiredRevision::INITIAL,
        ComponentSnapshotDetails::new(
            [],
            [
                MissingDependencySnapshot::blocked_by(
                    CapabilityId::of::<ForegroundCapability>(),
                    [ScopedComponentId::new(BLOCKING_COMPONENT, ScopeId::new(8))],
                ),
                MissingDependencySnapshot::direct(CapabilityId::of::<TestServiceCapability>()),
            ],
            RuntimeResourceCounts::default(),
        ),
    );
    let events = Rc::new(RefCell::new(Vec::new()));
    let mut lifecycle = RevisionLifecycle::new(events);
    lifecycle.fail_prepare = Some(1);
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, FAILED_COMPONENT, Some(1));
    let ReconcileStatus::Failed(failure) =
        futures::executor::block_on(reconciler.reconcile(&mut lifecycle))
    else {
        panic!("failed component must remain diagnostic");
    };
    let failed = ComponentSnapshot::failed(
        StartupPolicy::MustActivate,
        failure,
        ComponentSnapshotDetails::new(
            [resolved_test_service_binding()],
            [],
            RuntimeResourceCounts::new(4, 2, 3, 1),
        ),
    );
    let snapshot = RuntimeSnapshot::new(
        [pending, failed],
        [ContributionRevision::new(
            CapabilityId::of::<TestCatalogCapability>(),
            3,
        )],
        Duration::from_secs(30),
    )
    .expect("unique runtime snapshot identities");

    let error = snapshot
        .audit_startup()
        .expect_err("required pending and failed components block startup");
    assert_eq!(error.blockers().len(), 2);
    assert_eq!(error.blockers()[0].component(), FAILED_COMPONENT);
    assert_eq!(error.blockers()[0].state(), RuntimeComponentState::Failed);
    assert_eq!(error.blockers()[0].dependencies().len(), 1);
    assert_eq!(
        error.blockers()[0].dependencies()[0].provider(),
        DEFAULT_TEST_PROVIDER
    );
    assert_eq!(
        error.blockers()[0].dependencies()[0].generation(),
        ComponentGeneration::INITIAL
    );
    assert_eq!(
        error.blockers()[0].resource_counts(),
        RuntimeResourceCounts::new(4, 2, 3, 1)
    );
    assert_eq!(error.blockers()[1].component(), PENDING_COMPONENT);
    assert_eq!(error.blockers()[1].state(), RuntimeComponentState::Pending);
    assert_eq!(
        error.blockers()[1]
            .missing_dependencies()
            .iter()
            .map(MissingDependencySnapshot::capability)
            .collect::<Vec<_>>(),
        [
            CapabilityId::of::<ForegroundCapability>(),
            CapabilityId::of::<TestServiceCapability>(),
        ]
    );
    assert_eq!(
        error.blockers()[1].missing_dependencies()[0].blocking_chain(),
        [ScopedComponentId::new(BLOCKING_COMPONENT, ScopeId::new(8))]
    );
    assert_eq!(
        snapshot.contribution_revisions(),
        [ContributionRevision::new(
            CapabilityId::of::<TestCatalogCapability>(),
            3,
        )]
    );
    let message = error.to_string();
    assert!(message.contains(FAILED_COMPONENT.as_str()));
    assert!(message.contains(PENDING_COMPONENT.as_str()));
    assert!(message.contains(BLOCKING_COMPONENT.as_str()));
    assert!(message.contains(ForegroundCapability::NAME));
    assert!(message.contains("candidate preparation failed"));
}

#[test]
fn allowed_pending_components_remain_visible_without_blocking_startup() {
    const ACTIVE_COMPONENT: ComponentId = ComponentId::new("nostra.test.startup-active");
    const ON_DEMAND_COMPONENT: ComponentId = ComponentId::new("nostra.test.startup-on-demand");
    let snapshot = RuntimeSnapshot::new(
        [
            ComponentSnapshot::active(
                ACTIVE_COMPONENT,
                TEST_SCOPE,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            ),
            ComponentSnapshot::pending(
                ON_DEMAND_COMPONENT,
                TEST_SCOPE,
                StartupPolicy::AllowedPending,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::new(
                    [],
                    [MissingDependencySnapshot::direct(CapabilityId::of::<
                        TestServiceCapability,
                    >())],
                    RuntimeResourceCounts::default(),
                ),
            ),
        ],
        [],
        Duration::from_secs(30),
    )
    .expect("unique runtime snapshot identities");

    snapshot
        .audit_startup()
        .expect("allowed pending component does not block startup");
    assert_eq!(snapshot.components().len(), 2);
    assert_eq!(
        snapshot.components()[1].state(),
        RuntimeComponentState::Pending
    );
    assert_eq!(
        snapshot.components()[1].startup_policy(),
        StartupPolicy::AllowedPending
    );
}

#[test]
fn runtime_snapshot_rejects_duplicate_component_identities() {
    const COMPONENT: ComponentId = ComponentId::new("nostra.test.duplicate-snapshot");
    let error = RuntimeSnapshot::new(
        [
            ComponentSnapshot::active(
                COMPONENT,
                TEST_SCOPE,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            ),
            ComponentSnapshot::pending(
                COMPONENT,
                TEST_SCOPE,
                StartupPolicy::MustActivate,
                DesiredRevision::INITIAL,
                ComponentSnapshotDetails::default(),
            ),
        ],
        [],
        Duration::from_secs(30),
    )
    .expect_err("one scoped component cannot have contradictory states");

    assert_eq!(
        error,
        RuntimeSnapshotError::DuplicateComponent {
            component: COMPONENT,
            scope: TEST_SCOPE,
        }
    );
}

#[test]
fn runtime_snapshot_rejects_inconsistent_component_details() {
    const COMPONENT: ComponentId = ComponentId::new("nostra.test.invalid-snapshot");
    let missing = CapabilityId::of::<TestServiceCapability>();
    let active_error = RuntimeSnapshot::new(
        [ComponentSnapshot::active(
            COMPONENT,
            TEST_SCOPE,
            StartupPolicy::MustActivate,
            DesiredRevision::INITIAL,
            ComponentSnapshotDetails::new(
                [],
                [MissingDependencySnapshot::direct(missing)],
                RuntimeResourceCounts::default(),
            ),
        )],
        [],
        Duration::from_secs(30),
    )
    .expect_err("an active component cannot report a missing dependency");
    assert_eq!(
        active_error,
        RuntimeSnapshotError::InvalidComponent {
            component: COMPONENT,
            scope: TEST_SCOPE,
            violation: ComponentSnapshotViolation::MissingDependencyInState {
                state: RuntimeComponentState::Active,
                capability: missing,
            },
        }
    );

    let retained_counts = RuntimeResourceCounts::new(1, 1, 1, 1);
    let disposed_error = RuntimeSnapshot::new(
        [ComponentSnapshot::disposed(
            COMPONENT,
            TEST_SCOPE,
            StartupPolicy::AllowedPending,
            DesiredRevision::INITIAL,
            ComponentSnapshotDetails::new([], [], retained_counts),
        )],
        [],
        Duration::from_secs(30),
    )
    .expect_err("a disposed component cannot retain owned resources");
    assert_eq!(
        disposed_error,
        RuntimeSnapshotError::InvalidComponent {
            component: COMPONENT,
            scope: TEST_SCOPE,
            violation: ComponentSnapshotViolation::OwnedResourcesInState {
                state: RuntimeComponentState::Disposed,
                counts: retained_counts,
            },
        }
    );

    let conflicting_error = RuntimeSnapshot::new(
        [ComponentSnapshot::pending(
            COMPONENT,
            TEST_SCOPE,
            StartupPolicy::MustActivate,
            DesiredRevision::INITIAL,
            ComponentSnapshotDetails::new(
                [resolved_test_service_binding()],
                [MissingDependencySnapshot::direct(missing)],
                RuntimeResourceCounts::default(),
            ),
        )],
        [],
        Duration::from_secs(30),
    )
    .expect_err("one capability cannot be both resolved and missing");
    assert_eq!(
        conflicting_error,
        RuntimeSnapshotError::InvalidComponent {
            component: COMPONENT,
            scope: TEST_SCOPE,
            violation: ComponentSnapshotViolation::ResolvedAndMissing {
                capability: missing,
            },
        }
    );

    let mut service_slot = ExclusiveCapabilitySlot::new(TEST_SCOPE);
    let default = prepare_selected_test_provider(&service_slot, DEFAULT_TEST_PROVIDER)
        .expect("built-in default candidate");
    service_slot
        .install(default)
        .expect("install built-in default");
    let first_generation = resolved_test_service_binding_from(&service_slot);
    let alternate = prepare_selected_test_provider(&service_slot, ALTERNATE_TEST_PROVIDER)
        .expect("alternate candidate");
    service_slot
        .replace(alternate)
        .expect("replace built-in default");
    let second_generation = resolved_test_service_binding_from(&service_slot);
    assert_eq!(first_generation.generation(), ComponentGeneration::INITIAL);
    assert_eq!(second_generation.generation().get(), 2);
    let duplicate_binding_error = RuntimeSnapshot::new(
        [ComponentSnapshot::active(
            COMPONENT,
            TEST_SCOPE,
            StartupPolicy::MustActivate,
            DesiredRevision::INITIAL,
            ComponentSnapshotDetails::new(
                [first_generation, second_generation],
                [],
                RuntimeResourceCounts::default(),
            ),
        )],
        [],
        Duration::from_secs(30),
    )
    .expect_err("one capability cannot resolve to multiple Provider generations");
    assert_eq!(
        duplicate_binding_error,
        RuntimeSnapshotError::InvalidComponent {
            component: COMPONENT,
            scope: TEST_SCOPE,
            violation: ComponentSnapshotViolation::DuplicateResolvedDependency {
                capability: missing,
            },
        }
    );
}

#[test]
fn runtime_snapshot_rejects_duplicate_contribution_revisions() {
    let registry = CapabilityId::of::<TestCatalogCapability>();
    let error = RuntimeSnapshot::new(
        [],
        [
            ContributionRevision::new(registry, 1),
            ContributionRevision::new(registry, 2),
        ],
        Duration::from_secs(30),
    )
    .expect_err("one registry cannot publish contradictory revisions");

    assert_eq!(
        error,
        RuntimeSnapshotError::DuplicateContributionRevision { registry }
    );
}

#[test]
fn long_transition_is_diagnostic_without_changing_lifecycle_state() {
    let events = Rc::new(RefCell::new(Vec::new()));
    let prepare_released = Rc::new(Cell::new(false));
    let mut lifecycle = RevisionLifecycle::new(events);
    lifecycle.blocked_prepare = Some((1, Rc::clone(&prepare_released)));
    let mut reconciler = ScopeLocalReconciler::new(TEST_SCOPE, RECONCILE_COMPONENT, Some(1));
    let observer = reconciler.observer();
    let mut reconcile = Box::pin(reconciler.reconcile(&mut lifecycle));

    assert!(matches!(poll_once(reconcile.as_mut()), Poll::Pending));
    let transition = observer
        .transition()
        .expect("pending preparation remains observable");
    assert_eq!(transition.stage(), ReconcileStage::Preparing);
    let details = ComponentSnapshotDetails::new(
        [resolved_test_service_binding()],
        [],
        RuntimeResourceCounts::new(3, 1, 1, 0),
    );
    let snapshot = RuntimeSnapshot::new(
        [ComponentSnapshot::transitioning(
            StartupPolicy::MustActivate,
            &observer,
            transition.started_at() + Duration::from_secs(11),
            details,
        )
        .expect("transitioning component snapshot")],
        [ContributionRevision::new(
            CapabilityId::of::<TestCatalogCapability>(),
            7,
        )],
        Duration::from_secs(10),
    )
    .expect("unique runtime snapshot identities");

    assert_eq!(
        snapshot.components()[0].state(),
        RuntimeComponentState::Preparing
    );
    assert_eq!(snapshot.components()[0].dependencies().len(), 1);
    assert_eq!(
        snapshot.components()[0].dependencies()[0].generation(),
        ComponentGeneration::INITIAL
    );
    assert!(snapshot.components()[0].missing_dependencies().is_empty());
    assert_eq!(
        snapshot.components()[0].resource_counts(),
        RuntimeResourceCounts::new(3, 1, 1, 0)
    );
    assert_eq!(
        snapshot.components()[0]
            .transition()
            .expect("preparing transition snapshot")
            .started_at(),
        transition.started_at()
    );
    assert!(observer.last_failure().is_none());
    assert!(matches!(poll_once(reconcile.as_mut()), Poll::Pending));
    let [
        RuntimeDiagnostic::LongTransition {
            component,
            scope,
            revision,
            stage,
            elapsed,
        },
    ] = snapshot.diagnostics()
    else {
        panic!("one long-transition diagnostic is required");
    };
    assert_eq!(*component, RECONCILE_COMPONENT);
    assert_eq!(*scope, TEST_SCOPE);
    assert_eq!(*revision, DesiredRevision::INITIAL);
    assert_eq!(*stage, ReconcileStage::Preparing);
    assert_eq!(*elapsed, Duration::from_secs(11));

    prepare_released.set(true);
    assert!(matches!(
        poll_once(reconcile.as_mut()),
        Poll::Ready(ReconcileStatus::Settled { active: true, .. })
    ));
    drop(reconcile);
    assert!(observer.transition().is_none());
    assert_eq!(reconciler.active_desired(), Some(&1));
}
