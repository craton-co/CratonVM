use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

const BODY_CLASS: u32 = 70;

fn frame_of(method_name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "Other".to_string(),
        method_name.to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    )
}

/// `shared` with class `T` loaded as id [`BODY_CLASS`].
fn vm_with_body_class() -> Arc<SharedVm> {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    shared.classes.class_manager.write().register_class_name(
        cratonvm_types::ClassLoaderId::Application,
        "T",
        ClassId::new(BODY_CLASS),
    );
    shared
}

#[test]
fn an_artifact_names_its_body_by_owner_and_method_key() {
    let body = PollingBody::from_artifact(BODY_CLASS, "p/T.spin:(I)V")
        .expect("a published method key");
    assert_eq!(body.class_id, ClassId::new(BODY_CLASS));
    assert_eq!(body.class_name, "p/T");
    assert_eq!(body.method_name, "spin");
    assert_eq!(body.descriptor, "(I)V");
    assert!(
        PollingBody::from_artifact(cratonvm_types::jit_activation::NO_OWNER_CLASS, "T.m:()V")
            .is_none(),
        "never published"
    );
    assert!(PollingBody::from_artifact(1, "lambda-adapter->0x1000").is_none());
    assert!(PollingBody::from_artifact(1, "T.m").is_none());
    assert!(PollingBody::from_artifact(1, ".m:()V").is_none());
}

/// With no agent a named body is told nothing, and the negative fast path
/// the slow path asks first agrees.
#[test]
fn no_agent_tells_a_named_body_nothing() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(frame_of("caller"));
        let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");
        assert!(!compiled_frames_may_be_asked_to_leave(&shared));
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
        assert_eq!(compiled_frame_exit_verdict(&shared, None, body), 0);
    })
    .join()
    .expect("test thread");
}

/// An interpreter-only JVMTI event concerns every method: a named body is
/// told to leave whatever class of the process was redefined, because the
/// redefinition clause is its own class's (the wave-15 answer withheld
/// every method-entry exit once any class had been).
#[test]
fn a_named_body_leaves_under_the_vm_wide_mode_after_an_unrelated_redefinition() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(frame_of("caller"));
        let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        assert!(compiled_frames_may_be_asked_to_leave(&shared));
        let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
        // Class `T` of this VM was never redefined, so this holds whether
        // or not another test of the binary redefined some class.
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), body),
            both
        );
        // The unnamed body keeps the process-wide clause.
        let unnamed = compiled_frame_exit_verdict(&shared, None, None);
        if crate::classloading::any_class_redefined() {
            assert_eq!(unnamed, 0);
        } else {
            assert_eq!(unnamed, both);
        }

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// Interpreter round i2 wave 53, lane L2 (the wave-25 design of the i22-L2
/// caller-held proposal): `SAFEPOINT_VERDICT_NAMED_BODY` is set exactly for
/// a named body that must leave and that no compiled caller bound for a
/// caller-held CALL -- never for a bound one, one of unknown binding, the
/// unnamed body, or a body that need not leave.
#[test]
fn the_named_body_bit_spares_a_body_bound_by_a_caller_held_site() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(frame_of("caller"));
        let unbound = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_caller_held_binding(false));
        let bound = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_caller_held_binding(true));
        let unknown = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");
        let named = cratonvm_jit::SAFEPOINT_VERDICT_NAMED_BODY;
        let has_named = |v: i64| v & named != 0;

        // Nothing must leave: no bit for anyone.
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
            0
        );

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
            both | named,
            "an unbound named body leaves on the named bit too"
        );
        let spared = compiled_frame_exit_verdict(&shared, Some(&thread), bound);
        assert_eq!(spared, both, "a bound body keeps every other bit");
        assert!(!has_named(spared));
        assert!(
            !has_named(compiled_frame_exit_verdict(&shared, Some(&thread), unknown)),
            "an unknown binding is treated as bound"
        );
        assert!(
            !has_named(compiled_frame_exit_verdict(&shared, Some(&thread), None)),
            "the unnamed body never gets the named bit"
        );

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), unbound),
            0
        );
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// A JDWP breakpoint concerns its own method only: that method's body is
/// told to leave, another method's body and an unnamed body are not, and
/// neither is a body whose class name resolves to another class (a
/// by-name sink would charge its exit).
#[cfg(feature = "experimental-debug")]
#[test]
fn a_breakpoint_pulls_back_the_body_of_its_own_method_only() {
    use crate::debug::{events, jdwp_method_id, publish_debugger_gates, DebugState};
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
            return; // a JVMTI listener already keeps everything interpreted
        }
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(frame_of("caller"));
        let mut ds = DebugState::new();
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            vec![events::EventModifier::LocationOnly {
                class_id: u64::from(BODY_CLASS),
                method_id: jdwp_method_id("hot", "()V"),
                offset: 0,
            }],
        );
        // The same method of another class `T`, which the name does not
        // resolve to in this VM.
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            vec![events::EventModifier::LocationOnly {
                class_id: u64::from(BODY_CLASS + 1),
                method_id: jdwp_method_id("hot", "()V"),
                offset: 0,
            }],
        );
        publish_debugger_gates(&shared, &ds);
        assert!(compiled_frames_may_be_asked_to_leave(&shared));

        let hot = PollingBody::from_artifact(BODY_CLASS, "T.hot:()V");
        let cold = PollingBody::from_artifact(BODY_CLASS, "T.cold:()V");
        let alias = PollingBody::from_artifact(BODY_CLASS + 1, "T.hot:()V");
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), hot),
            cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
            "the body of the method holding the breakpoint leaves"
        );
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), cold), 0);
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), alias),
            0
        );
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), None),
            0,
            "an unnamed body is told only what concerns every method"
        );

        ds.events.clear_all();
        publish_debugger_gates(&shared, &ds);
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), hot), 0);
    })
    .join()
    .expect("test thread");
}
