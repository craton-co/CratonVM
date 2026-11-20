use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, ResumeSemantics};
use std::sync::Arc;

const BODY_CLASS: u32 = 71;
/// A stand-in for an artifact's `deopt_points` range.
const POINTS: (usize, usize) = (0x10_000, 0x10_400);

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
        speculation_id: cratonvm_jit::deopt::speculation_id(2, reason),
    })
}

/// A body with its artifact facts is recorded when it is told to leave,
/// and only then; the record answers for a point inside its range, of
/// this VM, and nothing else.
#[test]
fn a_granted_exit_is_recorded_with_the_bodys_class_and_points() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let vm = shared.vm_identity;
        let thread = caller_thread();
        let epoch = cratonvm_jit::jit_install_epoch();
        let body = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_artifact_facts(POINTS, epoch));
        let inside = POINTS.0 + 0x40;

        // No agent: not told to leave, nothing recorded.
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), body), 0);
        assert_eq!(mode_exit_grant_for(&shared, inside), None);

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let verdict = compiled_frame_exit_verdict(&shared, Some(&thread), body);
        assert_ne!(verdict & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY, 0);
        let grant = mode_exit_grant_for(&shared, inside).expect("the exit is recorded");
        assert_eq!(grant.class_id, ClassId::new(BODY_CLASS));
        assert_eq!(mode_exit_grant_for(&shared, 0), None, "0 is never a point");
        assert_eq!(mode_exit_grant_for(&shared, POINTS.1), None, "past the end");
        assert_eq!(mode_exit_grant_for(&shared, POINTS.0 - 8), None, "before");
        let other = Arc::new(SharedVm::new(VmConfig::default()));
        assert_eq!(
            mode_exit_grant_for(&other, inside),
            None,
            "another VM's point is not this record's"
        );

        // The stash sink judges the exit by the record.
        let frame = stashed("T.spin:()V");
        assert!(stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::OsrExit),
            inside
        ));
        assert!(!stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::BoundsCheck),
            inside
        ));
        // Compiled after every flush of this VM's cache: current.
        assert!(granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::OsrExit),
            inside
        ));
        assert!(!granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::UncommonTrap),
            inside
        ));
        assert!(!granted_exit_of_a_current_body(&shared, None, inside));
        // A flush after the grant (a redefinition) makes the same record
        // name an obsolete body: its currency is re-read at each use.
        shared.jit.jit_cache.clear_all();
        assert!(!granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::OsrExit),
            inside
        ));

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// Item (b): a body compiled after its class's redefinition leaves under
/// the VM-wide mode; a body compiled before it (an obsolete body, which no
/// sink can resume) does not, and neither does one whose age is unknown.
#[test]
fn a_body_compiled_after_its_class_was_redefined_leaves_and_an_obsolete_one_does_not() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = vm_with_body_class();
        let vm = shared.vm_identity;
        let thread = caller_thread();
        // Class `T` redefined once, and the VM's cache flushed for it, as
        // `redefine_class_with` does.
        shared
            .classes
            .class_manager
            .read()
            .class_redefine_generation_handle(ClassId::new(BODY_CLASS))
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let before = cratonvm_jit::jit_install_epoch();
        shared.jit.jit_cache.clear_all();
        let after = cratonvm_jit::jit_install_epoch();
        let current = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_artifact_facts(POINTS, after));
        let obsolete = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V")
            .map(|b| b.with_artifact_facts((0x20_000, 0x20_400), before));
        let unknown = PollingBody::from_artifact(BODY_CLASS, "T.spin:()V");

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let polling = |body: Option<PollingBody<'_>>| {
            compiled_frame_exit_verdict(&shared, Some(&thread), body)
                & cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY
        };
        // `class_was_redefined` needs the process latch, which only a real
        // redefinition raises; without it the generation bump is unseen
        // and every named body leaves, as before this wave.
        let redefined_seen = crate::runtime::redefine_state::class_was_redefined(
            &shared,
            ClassId::new(BODY_CLASS),
        );
        if redefined_seen {
            assert_eq!(polling(obsolete), 0, "an obsolete body stays compiled");
            assert_eq!(polling(unknown), 0, "an unknown age is not current");
        } else {
            assert_ne!(polling(obsolete), 0);
            assert_ne!(polling(unknown), 0);
        }
        assert_ne!(polling(current), 0, "a current body leaves");
        // Its exit is the one the sinks may resume against the class as it
        // is now.
        assert!(granted_exit_of_a_current_body(
            &shared,
            cause(DeoptReason::OsrExit),
            POINTS.0
        ));

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// Item (c): a JDWP breakpoint in the method of a class whose name
/// resolves to ANOTHER class of the same name now pulls that class's body
/// back when the poll knows its artifact facts, and the stash sink judges
/// the exit by the recorded class, not by the name (wave 17 withheld it:
/// `a_breakpoint_pulls_back_the_body_of_its_own_method_only`, which keeps
/// covering a body without facts).
#[cfg(feature = "experimental-debug")]
#[test]
fn a_same_named_class_body_leaves_and_its_exit_is_judged_by_class_id() {
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
        let alias = PollingBody::from_artifact(alias_class, "T.hot:()V")
            .map(|b| b.with_artifact_facts(POINTS, epoch));
        let own = PollingBody::from_artifact(BODY_CLASS, "T.hot:()V")
            .map(|b| b.with_artifact_facts((0x30_000, 0x30_400), epoch));
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), own),
            0,
            "the name's own class holds no breakpoint"
        );
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
                POINTS.0 + 0x40
            ),
            "judged by the recorded class: not charged"
        );
        assert!(
            !stashed_exit_left_for_the_interpreter(
                &shared,
                &frame,
                cause(DeoptReason::OsrExit),
                0
            ),
            "without the point, by name: the name's own class holds no breakpoint"
        );

        ds.events.clear_all();
        publish_debugger_gates(&shared, &ds);
        assert_eq!(
            compiled_frame_exit_verdict(&shared, Some(&thread), alias),
            0
        );
    })
    .join()
    .expect("test thread");
}
