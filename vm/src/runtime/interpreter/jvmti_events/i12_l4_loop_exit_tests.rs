use super::*;
use crate::config::VmConfig;
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use crate::threading::jvm_thread::ThreadId;
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

/// The verdict is the OSR gate's own question about the innermost frame:
/// an interpreter-only JVMTI event in this VM turns it on, and turning the
/// event off turns it off again. No frame, no verdict. And with no OSR
/// body running in the VM, the request pauses nothing.
#[test]
fn the_loop_exit_verdict_follows_the_interpreter_only_mode() {
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        let mut thread = JvmThread::new(ThreadId(0), "loop");
        assert!(
            !compiled_loop_must_leave(&shared, &thread),
            "no frame: nothing to leave"
        );
        thread.frames.push(frame_of("spin"));
        assert!(
            !compiled_loop_must_leave(&shared, &thread),
            "no agent: the loop stays compiled"
        );

        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        assert!(
            compiled_loop_must_leave(&shared, &thread),
            "MethodEntry is posted by the interpreter only: the loop must leave"
        );
        // Wave 15: the mode concerns every compiled frame, so the request
        // no longer depends on a running OSR body; a fixture built
        // without `Vm::new` still has no owning handle to pause with.
        assert!(
            !request_compiled_loop_exits(&shared),
            "a unit fixture has no owning handle, so no pause is taken"
        );

        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        assert!(
            !compiled_loop_must_leave(&shared, &thread),
            "the mode is off again: stay compiled"
        );
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}

/// Wave 13: the loop-exit request repeats its pause only when the last one
/// froze a peer (which passed no poll) while an OSR body still runs, and
/// its total grace stays a few milliseconds.
#[test]
fn the_loop_exit_pause_is_repeated_only_for_a_frozen_peer_of_a_running_body() {
    assert!(loop_exit_pause_again(true, true));
    assert!(!loop_exit_pause_again(true, false), "the loop left");
    assert!(
        !loop_exit_pause_again(false, true),
        "every poll was reached"
    );
    assert!(!loop_exit_pause_again(false, false));
    let mut total = std::time::Duration::ZERO;
    let mut grace = LOOP_EXIT_GRACE;
    for _ in 0..LOOP_EXIT_ATTEMPTS {
        total += grace;
        grace = grace.saturating_mul(2);
    }
    assert!(
        total <= std::time::Duration::from_millis(20),
        "the grace slices of one request stay bounded: {total:?}"
    );
}
