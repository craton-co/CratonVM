use super::{osr_bodies_running, osr_exit_left_for_the_interpreter, OsrBodyRunning};
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::vm::SharedVm;
use crate::{JvmThread, ThreadId};
use cratonvm_jit::deopt::{DeoptCause, DeoptReason};
use std::sync::Arc;

/// `request_compiled_loop_exits` pauses only while a single-pass OSR body
/// of the VM is counted in, and the count is held exactly as long as the
/// door's guard.
#[test]
fn the_running_osr_body_count_is_held_for_the_guards_lifetime() {
    let shared = SharedVm::new(VmConfig::default());
    assert!(!osr_bodies_running(&shared));
    {
        let _outer = OsrBodyRunning::enter(&shared);
        let _nested = OsrBodyRunning::enter(&shared);
        assert!(osr_bodies_running(&shared));
    }
    assert!(!osr_bodies_running(&shared), "both guards counted out");
}

/// The transfer sink commits (rather than charges as a rejection) an exit
/// only when an `OsrExit` map fired while the frame's method must run
/// interpreted: a speculative guard, a missing cause, a missing frame or a
/// mode that is off all keep the old accounting.
#[test]
fn only_an_osr_exit_map_taken_under_interpreter_only_mode_is_committed() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        thread.frames.push(Frame::new(
            cratonvm_types::ClassId::new(0),
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
        let osr_exit = Some(DeoptCause {
            reason: DeoptReason::OsrExit,
            speculation_id: 0,
        });
        let guard = Some(DeoptCause {
            reason: DeoptReason::BoundsCheck,
            speculation_id: 0,
        });
        assert!(
            !osr_exit_left_for_the_interpreter(&shared, &thread, 0, osr_exit),
            "no agent: an OsrExit map is the test trigger's, not the mode's"
        );

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::SingleStep, None)
            .expect("enable SingleStep");
        assert!(osr_exit_left_for_the_interpreter(
            &shared, &thread, 0, osr_exit
        ));
        assert!(!osr_exit_left_for_the_interpreter(
            &shared, &thread, 0, guard
        ));
        assert!(!osr_exit_left_for_the_interpreter(
            &shared, &thread, 0, None
        ));
        assert!(
            !osr_exit_left_for_the_interpreter(&shared, &thread, 1, osr_exit),
            "no such frame"
        );
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}
