use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{
    speculation_id, DeoptAction, DeoptCause, DeoptReason, DeoptimizationPoint, FrameState,
    FrameValue, ReconstructedFrame, ResumeSemantics,
};
use std::sync::Arc;

const BODY_CLASS: u32 = 73;

fn vm_with_body_class() -> Arc<SharedVm> {
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    shared.classes.class_manager.write().register_class_name(
        cratonvm_types::ClassLoaderId::Application,
        "T",
        ClassId::new(BODY_CLASS),
    );
    shared
}

fn caller_thread() -> JvmThread {
    let mut thread = JvmThread::new(ThreadId(0), "loop");
    thread.frames.push(Frame::new(
        ClassId::new(0),
        "Other".to_string(),
        "caller".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        8,
        4,
        &[],
    ));
    thread
}

/// A boxed point at bci 2, as the optimizing lowerer publishes a
/// back-edge poll's mode exit (`OsrExit`) or a guard.
fn boxed(reason: DeoptReason) -> Box<DeoptimizationPoint> {
    Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 2,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: speculation_id(2, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 2,
            locals: vec![FrameValue::StackSlot(-8)],
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::for_reason(reason),
    })
}

fn address(point: &DeoptimizationPoint) -> usize {
    // Cast: a point address, compared only.
    point as *const DeoptimizationPoint as usize
}

fn stashed(method_key: &str) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: method_key.to_string(),
        bci: 2,
        locals: Vec::new(),
        stack: Vec::new(),
        monitors: Vec::new(),
        semantics: ResumeSemantics::REEXECUTE,
        caller_frames: Vec::new(),
    }
}

fn cause(reason: DeoptReason) -> Option<DeoptCause> {
    Some(DeoptCause {
        reason,
        speculation_id: speculation_id(2, reason),
    })
}

/// A body whose only boxes are guards names no point a mode exit can
/// stash; one with a boxed `OsrExit` does, and its grant matches exactly
/// its `OsrExit` boxes: not a guard box, not an address inside a box, and
/// not after a later grant replaced it.
#[test]
fn an_optimizing_bodys_poll_exit_is_matched_by_its_box() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let vm = shared.vm_identity;
        let thread = caller_thread();
        let epoch = cratonvm_jit::jit_install_epoch();
        let boxes = vec![
            boxed(DeoptReason::OsrExit),
            boxed(DeoptReason::NullCheck),
            boxed(DeoptReason::OsrExit),
        ];
        let (exit, guard, other_exit) =
            (address(&boxes[0]), address(&boxes[1]), address(&boxes[2]));
        // An optimizing artifact's `deopt_points` are by-value copies whose
        // addresses no stub bakes; empty here.
        let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V").map(|b| {
            b.with_artifact_facts((0, 0), epoch)
                .with_boxed_exit_points(&boxes)
        });
        assert!(body.is_some_and(|b| b.exit_points_known()));
        let guards = vec![boxed(DeoptReason::NullCheck)];
        let guarded = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V").map(|b| {
            b.with_artifact_facts((0, 0), epoch)
                .with_boxed_exit_points(&guards)
        });
        assert!(!guarded.is_some_and(|b| b.exit_points_known()));

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let verdict = compiled_frame_exit_verdict(&shared, Some(&thread), body);
        assert_ne!(verdict & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY, 0);
        let grant = mode_exit_grant_for(&shared, exit).expect("the exit is recorded");
        assert_eq!(grant.class_id, ClassId::new(BODY_CLASS));
        assert!(mode_exit_grant_for(&shared, other_exit).is_some());
        assert_eq!(
            mode_exit_grant_for(&shared, guard),
            None,
            "a guard is no mode exit"
        );
        assert_eq!(
            mode_exit_grant_for(&shared, exit + 1),
            None,
            "not a point address"
        );

        // The sinks that hold only the stash.
        let frame = stashed("T.spin:()V");
        assert!(stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::OsrExit),
            exit
        ));
        assert!(granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::OsrExit),
            other_exit
        ));
        assert!(!granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::OsrExit),
            guard
        ));

        // A later grant (a single-pass body) replaces the record.
        let single_pass = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_artifact_facts((0x10_000, 0x10_400), epoch));
        assert_ne!(
            compiled_frame_exit_verdict(&shared, Some(&thread), single_pass)
                & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
            0
        );
        assert!(mode_exit_grant_for(&shared, 0x10_040).is_some());
        assert_eq!(mode_exit_grant_for(&shared, exit), None, "replaced");

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// For an optimizing body: a JDWP breakpoint in the method of a class
/// whose name resolves to ANOTHER class pulls that class's body back once
/// the poll knows its boxed exits (without them, the wave-17 rule withholds
/// the exit), and the stash sink judges the exit by the recorded class.
#[cfg(feature = "experimental-debug")]
#[test]
fn a_same_named_class_optimizing_body_leaves_and_is_judged_by_class_id() {
    use crate::debug::{events, jdwp_method_id, publish_debugger_gates, DebugState};
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        if crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity) {
            return; // a JVMTI listener already keeps everything interpreted
        }
        let thread = caller_thread();
        let alias_class = BODY_CLASS + 1;
        let mut ds = DebugState::new();
        ds.events.set_event_request(
            events::EventKind::Breakpoint,
            events::SuspendPolicy::None,
            vec![events::EventModifier::LocationOnly {
                class_id: u64::from(alias_class),
                method_id: jdwp_method_id("hot", "()V"),
                offset: 0,
            }],
        );
        publish_debugger_gates(&shared, &ds);

        let epoch = cratonvm_jit::jit_install_epoch();
        let boxes = vec![boxed(DeoptReason::OsrExit)];
        let exit = address(&boxes[0]);
        let without_facts = PollingBody::from_artifact(alias_class, "T.hot:()V");
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), without_facts),
            0,
            "the wave-17 rule: the name resolves to another class"
        );
        let alias = PollingBody::from_artifact(alias_class, "T.hot:()V").map(|b| {
            b.with_artifact_facts((0, 0), epoch)
                .with_boxed_exit_points(&boxes)
        });
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), alias),
            cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY,
            "the class holding the breakpoint leaves, whatever its name resolves to"
        );
        let frame = stashed("T.hot:()V");
        assert!(
            stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                exit
            ),
            "judged by the recorded class: not charged"
        );

        ds.events.clear_all();
        publish_debugger_gates(&shared, &ds);
    })
    .join()
    .expect("test thread");
}
