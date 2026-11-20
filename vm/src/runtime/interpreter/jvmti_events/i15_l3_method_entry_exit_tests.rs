use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
use cratonvm_jit::deopt::{DeoptCause, DeoptReason, ReconstructedFrame, ResumeSemantics};
use std::sync::Arc;

fn frame_of(method_name: &str) -> Frame {
    Frame::new(
        ClassId::new(0),
        "T".to_string(),
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

fn stashed(method_key: &str) -> ReconstructedFrame {
    ReconstructedFrame {
        class_id: 0,
        method_key: method_key.to_string(),
        bci: 7,
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
        speculation_id: cratonvm_jit::deopt::speculation_id(7, reason),
    })
}

/// No agent: nothing leaves. An interpreter-only JVMTI event: both bits,
/// with or without a `JvmThread`, unless a class of the process was
/// redefined, when only an OSR body (the innermost-frame bit) leaves.
#[test]
fn the_every_frame_bit_is_the_vm_wide_answer() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(frame_of("spin"));
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
        assert_eq!(compiled_frame_exit_verdict(&shared, None, None), 0);
        assert!(!every_compiled_frame_may_leave(&shared));

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
        if crate::classloading::any_class_redefined() {
            // Another test of this binary redefined a class.
            assert!(!every_compiled_frame_may_leave(&shared));
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), None),
                cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            );
            assert_eq!(compiled_frame_exit_verdict(&shared, None, None), 0);
        } else {
            assert!(every_compiled_frame_may_leave(&shared));
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), None),
                both
            );
            assert_eq!(
                compiled_frame_exit_verdict(&shared, None, None),
                both,
                "the VM-wide answer needs no thread"
            );
        }

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// An exit is "left for the interpreter" exactly when it is an `OsrExit`
/// and the mode holds; a stashed frame is judged by the cause beside it.
#[test]
fn only_an_osr_exit_under_the_mode_is_left_for_the_interpreter() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let id = ClassId::new(0);
        let frame = stashed("T.spin:()V");
        assert!(!exit_left_for_the_interpreter(
            &shared,
            DeoptReason::OsrExit,
            id,
            "spin",
            "()V"
        ));
        assert!(!stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::OsrExit),
            0
        ));

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        assert!(exit_left_for_the_interpreter(
            &shared,
            DeoptReason::OsrExit,
            id,
            "spin",
            "()V"
        ));
        assert!(
            !exit_left_for_the_interpreter(
                &shared,
                DeoptReason::BoundsCheck,
                id,
                "spin",
                "()V"
            ),
            "a guard is a failed speculation whatever the mode"
        );
        assert!(stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::OsrExit),
            0
        ));
        assert!(!stashed_exit_left_for_the_interpreter(
            &shared, &frame, None, 0
        ));
        assert!(!stashed_exit_left_for_the_interpreter(
            &shared,
            &frame,
            cause(DeoptReason::UncommonTrap),
            0
        ));
        assert!(
            !stashed_exit_left_for_the_interpreter(
                &shared,
                &stashed(""),
                cause(DeoptReason::OsrExit),
                0
            ),
            "a key-less frame names no method"
        );
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// The charging sinks: while the mode holds, an `OsrExit` is neither
/// recorded by `DeoptimizationController::deoptimize_in` nor by
/// `helpers::despeculate_trapped_method`, and a guard still is; with the
/// mode off the same `OsrExit` is charged as before.
#[test]
fn the_charging_sinks_decline_an_exit_left_for_the_interpreter() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let id = ClassId::new(0);
        let key = "I15L3Charge.spin:()V";
        let at_site = |reason: DeoptReason| {
            shared
                .jit
                .deopt_log
                .lock()
                .deopt_count_at_site(key, reason, 7)
        };

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        let action = crate::jit::helpers::DeoptimizationController::deoptimize_in(
            &shared,
            id,
            "I15L3Charge",
            "spin",
            "()V",
            DeoptReason::OsrExit,
            7,
        );
        assert_eq!(action, cratonvm_jit::deopt::DeoptAction::Reinterpret);
        crate::jit::helpers::despeculate_trapped_method(
            &shared,
            "I15L3Charge",
            "spin",
            "()V",
            id,
            DeoptReason::OsrExit,
            7,
        );
        assert_eq!(
            at_site(DeoptReason::OsrExit),
            0,
            "the agent's exit is not charged"
        );
        crate::jit::helpers::DeoptimizationController::deoptimize_in(
            &shared,
            id,
            "I15L3Charge",
            "spin",
            "()V",
            DeoptReason::BoundsCheck,
            7,
        );
        assert_eq!(
            at_site(DeoptReason::BoundsCheck),
            1,
            "a guard is still charged"
        );

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        crate::jit::helpers::DeoptimizationController::deoptimize_in(
            &shared,
            id,
            "I15L3Charge",
            "spin",
            "()V",
            DeoptReason::OsrExit,
            7,
        );
        assert_eq!(
            at_site(DeoptReason::OsrExit),
            1,
            "with the mode off an OsrExit is charged as before"
        );
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}
