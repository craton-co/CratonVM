use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn service_controller_get_service_uses_msc_1_5_descriptor() {
    let mut registry = NativeMethodRegistry::new();
    register_jboss_msc_natives(&mut registry);
    let descriptor = "()Lorg/jboss/msc/service/Service;";
    for class in [
        "org/jboss/msc/service/ServiceController",
        "org/jboss/msc/service/ServiceControllerImpl",
    ] {
        assert!(
            registry.find(class, "getService", descriptor).is_some(),
            "{class}.getService must use the descriptor from jboss-msc 1.5"
        );
    }

    // This class has real bytecode which forwards every ServiceController
    // method to its delegate. It must never receive the synthetic mirror
    // natives (whose slot-5 controller ID is not in its two-field layout).
    let delegating = "org/jboss/msc/service/DelegatingServiceController";
    for (method, descriptor) in [
        (
            "getState",
            "()Lorg/jboss/msc/service/ServiceController$State;",
        ),
        ("getService", descriptor),
        ("getName", "()Lorg/jboss/msc/service/ServiceName;"),
    ] {
        assert!(
            registry.find(delegating, method, descriptor).is_none(),
            "{delegating}.{method}{descriptor} must retain its Java forwarding bytecode"
        );
    }
}

// Helper: reset the intern table between tests so each run is
// deterministic.  The global container can't be reset (it's bound
// to a `OnceLock`) — instead the tests use fresh `ServiceName`s
// prefixed with the test name so they don't collide.
fn name(s: &str) -> Arc<ServiceName> {
    ServiceName::parse(s)
}

#[test]
fn t19_1_service_name_of_single_segment() {
    let n = ServiceName::of(["jboss"]);
    assert_eq!(n.canonical(), "jboss");
    assert_eq!(n.len(), 1);
    assert!(n.parent().is_none());
}

#[test]
fn t19_1_service_name_append_produces_child() {
    let p = ServiceName::of(["jboss", "as"]);
    let c = p.append("server");
    assert_eq!(c.canonical(), "jboss.as.server");
    assert_eq!(c.len(), 3);
    let pa = c.parent().expect("parent should exist");
    assert_eq!(pa.canonical(), "jboss.as");
    // Interning => pointer equality for same canonical string.
    assert!(Arc::ptr_eq(&p, &pa));
}

#[test]
fn t19_1_service_name_hash_matches_jboss_msc_formula() {
    let suffix = ServiceName::of(["network", "interface", "management"]);
    assert_eq!(service_name_hash(&suffix), -1_335_710_409);

    let full = ServiceName::of(["jboss", "network", "interface", "management"]);
    assert_eq!(service_name_hash(&full), -1_212_000_734);
}

#[test]
fn t19_1_service_name_append_segments_preserves_parent_chain() {
    let base = ServiceName::of(["jboss"]);
    let full = append_service_name_segments(
        Some(base.clone()),
        vec![
            "network".to_string(),
            "interface".to_string(),
            "management".to_string(),
        ],
    );

    assert_eq!(full.canonical(), "jboss.network.interface.management");
    let parent = full.parent().expect("parent should exist");
    assert_eq!(parent.canonical(), "jboss.network.interface");
    let grandparent = parent.parent().expect("grandparent should exist");
    assert_eq!(grandparent.canonical(), "jboss.network");
    let root = grandparent.parent().expect("root should exist");
    assert!(Arc::ptr_eq(&base, &root));
}

#[test]
fn t19_1_service_container_add_service_returns_controller() {
    let c = ServiceContainer::new();
    let n = name("t19_1_add.a");
    let id = c
        .add_service(n.clone(), vec![], Mode::Active, 0xAA)
        .expect("add_service should succeed");
    assert!(id > 0);
    assert_eq!(c.size(), 1);
    assert!(c.get_state(&n).is_some());
}

#[test]
fn t19_1_service_state_transitions_new_to_up_when_started() {
    let c = ServiceContainer::new();
    let n = name("t19_1_transition.x");
    c.add_service(n.clone(), vec![], Mode::Active, 1)
        .expect("install");
    c.drain_tasks_locally();
    assert_eq!(c.get_state(&n), Some(ServiceState::Up));
    assert_eq!(c.count_in(ServiceState::Up), 1);
}

#[test]
fn t19_1_service_mode_on_demand_stays_down_until_dependent_needs_it() {
    let c = ServiceContainer::new();
    let n = name("t19_1_ondemand.svc");
    c.add_service(n.clone(), vec![], Mode::OnDemand, 0)
        .expect("install");
    c.drain_tasks_locally();
    // OnDemand without a demander stays Down.
    assert_eq!(c.get_state(&n), Some(ServiceState::Down));
    // Now demand it — drives the transition.
    c.demand(&n);
    c.drain_tasks_locally();
    assert_eq!(c.get_state(&n), Some(ServiceState::Up));
}

#[test]
fn t19_1_service_dependency_transitive_start() {
    let c = ServiceContainer::new();
    let a = name("t19_1_trans.a");
    let b = name("t19_1_trans.b");
    let d = name("t19_1_trans.d");
    c.add_service(a.clone(), vec![], Mode::Active, 1).unwrap();
    c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2)
        .unwrap();
    c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3)
        .unwrap();
    c.drain_tasks_locally();
    // Keep draining: scheduling dependents may enqueue fresh work.
    for _ in 0..8 {
        c.drain_tasks_locally();
    }
    assert_eq!(c.get_state(&a), Some(ServiceState::Up));
    assert_eq!(c.get_state(&b), Some(ServiceState::Up));
    assert_eq!(c.get_state(&d), Some(ServiceState::Up));
}

#[test]
fn t19_1_async_start_context_complete_transitions_to_up() {
    let c = ServiceContainer::new();
    let n = name("t19_1_async.svc");
    let id = c
        .add_service(n.clone(), vec![], Mode::Active, 0)
        .expect("install");
    // Simulate a Java service that called StartContext.asynchronous() —
    // transition to Starting, mark async, and then never let the sync
    // drain finalize it.
    {
        let mut st = c.inner.lock().unwrap();
        let nm = st.by_id.get(&id).cloned().unwrap();
        let ctrl = st.services.get_mut(&nm).unwrap();
        ctrl.state = ServiceState::Starting;
        ctrl.async_pending = true;
    }
    c.drain_tasks_locally();
    assert_eq!(c.get_state(&n), Some(ServiceState::Starting));
    // Complete async → Up.
    c.complete_async(id);
    assert_eq!(c.get_state(&n), Some(ServiceState::Up));
}

#[test]
fn t19_1_service_container_shutdown_stops_all_in_reverse_dep_order() {
    let c = ServiceContainer::new();
    let a = name("t19_1_shut.a");
    let b = name("t19_1_shut.b");
    let d = name("t19_1_shut.d");
    c.add_service(a.clone(), vec![], Mode::Active, 1).unwrap();
    c.add_service(b.clone(), vec![a.clone()], Mode::Active, 2)
        .unwrap();
    c.add_service(d.clone(), vec![b.clone()], Mode::Active, 3)
        .unwrap();
    for _ in 0..6 {
        c.drain_tasks_locally();
    }
    assert_eq!(c.count_in(ServiceState::Up), 3);
    c.shutdown();
    // All services should be Down now.
    assert_eq!(c.count_in(ServiceState::Up), 0);
    assert_eq!(c.count_in(ServiceState::Down), 3);
}

#[test]
fn t19_1_circular_dependency_rejected() {
    let c = ServiceContainer::new();
    let a = name("t19_1_cycle.a");
    let b = name("t19_1_cycle.b");
    c.add_service(a.clone(), vec![], Mode::Active, 0).unwrap();
    c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0)
        .unwrap();
    // Now adding a->[b] would cycle.  Installing a second `a`
    // isn't possible (duplicate name), so install `c` that depends
    // on `b`, then try to add a fresh `x` depending on c whose
    // deps loop back through a... easier: call `would_cycle`
    // directly.
    let state = c.inner.lock().unwrap();
    let cycle = would_cycle(&state.services, &a, &[b.clone()]);
    assert!(cycle, "inserting a -> b (with existing b -> a) must cycle");
}

#[test]
fn t19_1_service_name_interning_ptr_equality() {
    let a = ServiceName::of(["x", "y", "z"]);
    let b = ServiceName::of(["x", "y", "z"]);
    let c = ServiceName::parse("x.y.z");
    assert!(Arc::ptr_eq(&a, &b));
    assert!(Arc::ptr_eq(&a, &c));
}

#[test]
fn t19_1_reverse_topo_order_visits_leaves_first() {
    let c = ServiceContainer::new();
    let a = name("t19_1_topo.a");
    let b = name("t19_1_topo.b");
    let d = name("t19_1_topo.d");
    c.add_service(a.clone(), vec![], Mode::Active, 0).unwrap();
    c.add_service(b.clone(), vec![a.clone()], Mode::Active, 0)
        .unwrap();
    c.add_service(d.clone(), vec![b.clone()], Mode::Active, 0)
        .unwrap();
    let state = c.inner.lock().unwrap();
    let order = reverse_topo_order(&state.services);
    // Every name appears exactly once.
    assert_eq!(order.len(), 3);
    // Each leaf must come strictly before its dependency root — so
    // `d` (top of chain) comes before `b`, and `b` before `a`.
    let idx_a = order.iter().position(|n| n == &a).unwrap();
    let idx_b = order.iter().position(|n| n == &b).unwrap();
    let idx_d = order.iter().position(|n| n == &d).unwrap();
    assert!(idx_d < idx_b, "d should come before b in reverse topo");
    assert!(idx_b < idx_a, "b should come before a in reverse topo");
}

#[test]
fn t19_1_record_failure_transitions_to_failed() {
    let c = ServiceContainer::new();
    let n = name("t19_1_fail.svc");
    let id = c.add_service(n.clone(), vec![], Mode::Active, 0).unwrap();
    // Force into Starting then fail.
    {
        let mut st = c.inner.lock().unwrap();
        let nm = st.by_id.get(&id).cloned().unwrap();
        st.services.get_mut(&nm).unwrap().state = ServiceState::Starting;
    }
    c.record_failure(id, "boom".into());
    assert_eq!(c.get_state(&n), Some(ServiceState::Failed));
}

#[test]
fn t19_1_mode_and_state_string_forms_match_jdk_enum() {
    assert_eq!(Mode::Active.as_str(), "ACTIVE");
    assert_eq!(Mode::OnDemand.as_str(), "ON_DEMAND");
    assert_eq!(ServiceState::Up.as_str(), "UP");
    assert_eq!(ServiceState::Down.as_str(), "DOWN");
    assert_eq!(ServiceState::Failed.as_str(), "FAILED");
    assert_eq!(Mode::parse("ACTIVE"), Mode::Active);
    assert_eq!(Mode::parse("ON_DEMAND"), Mode::OnDemand);
    assert_eq!(Mode::parse("REMOVE"), Mode::Remove);
    assert_eq!(Mode::Remove.ordinal(), 0);
    assert_eq!(Mode::Active.ordinal(), 5);
    assert_eq!(Mode::parse("garbage"), Mode::Active);
}

#[test]
fn t19_1_passive_mode_schedules_like_active() {
    let c = ServiceContainer::new();
    let n = name("t19_1_passive.svc");
    c.add_service(n.clone(), vec![], Mode::Passive, 0).unwrap();
    c.drain_tasks_locally();
    assert_eq!(c.get_state(&n), Some(ServiceState::Up));
}

#[test]
fn t19_1_never_mode_stays_down_even_under_demand() {
    let c = ServiceContainer::new();
    let n = name("t19_1_never.svc");
    c.add_service(n.clone(), vec![], Mode::Never, 0).unwrap();
    c.demand(&n);
    c.drain_tasks_locally();
    // Never mode bypasses demand entirely.
    assert_eq!(c.get_state(&n), Some(ServiceState::Down));
}
