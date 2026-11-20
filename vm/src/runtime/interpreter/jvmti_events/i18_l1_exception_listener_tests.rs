use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
use std::sync::Arc;

/// Page `interpreter-L1-jvmti-exception-events-lost-for-throws-caught-in-compiled-code-FIXED-20260925`:
/// only the unwinder posts `Exception`, so while an agent listens for it
/// the JIT's doors stand down and every compiled frame of the VM is told
/// to leave at its next poll, exactly as for `MethodEntry`. Nothing is
/// asked once it stops listening.
#[test]
fn an_exception_listener_sends_compiled_frames_to_the_interpreter() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "thrower");
        thread.frames.push(Frame::new(
            ClassId::new(0),
            "T".to_string(),
            "spin".to_string(),
            "()V".to_string(),
            None,
            vec![0xb1],
            vec![],
            8,
            4,
            &[],
        ));
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
        assert!(!crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared));

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
            .expect("enable Exception");
        // Wave 92 (lane L1): the VM's own answer, which
        // `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED` turns off.
        if mgr.exception_events_need_the_interpreter() {
            assert!(
                crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared),
                "the doors stand down"
            );
            assert!(compiled_frames_may_be_asked_to_leave(&shared));
            let both = cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
                | cratonvm_jit::SAFEPOINT_VERDICT_POLLING_BODY;
            let expected = if crate::classloading::any_class_redefined() {
                // Another test of this binary redefined a class: only an
                // OSR body (the innermost-frame bit) may leave.
                cratonvm_jit::SAFEPOINT_VERDICT_INNERMOST_FRAME
            } else {
                both
            };
            assert_eq!(
                compiled_frame_exit_verdict(&shared, Some(&thread), None),
                expected
            );
        }

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
            .expect("disable Exception");
        assert!(!crate::runtime::interpreter::jit_bridge::jvmti_requires_interpreter(&shared));
        assert_eq!(compiled_frame_exit_verdict(&shared, Some(&thread), None), 0);
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}
