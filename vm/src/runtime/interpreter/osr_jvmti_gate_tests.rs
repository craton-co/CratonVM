use crate::runtime::jvmti::{
    self, interp_only_events_active_for_vm, EventCallbacks, EventMode, JvmtiEventKind,
};
use std::sync::Arc;

/// OSR (and every whole-method JIT door, which asks the same question
/// through `jit_bridge::jvmti_requires_interpreter`) must stand down while
/// an agent listens for an event compiled code cannot post, and only then:
/// the gate follows the listener in and out, and only for its own VM.
#[test]
fn the_osr_gate_follows_interpreter_only_jvmti_listeners() {
    let _lock = jvmti::jvmti_registry_test_lock();
    // A VM identity no real `SharedVm` and no other test is ever handed
    // (gcg g2/k: drawn from the real counter; it was `0x7200_0000`).
    let vm = crate::vm::vm_init::test_vm_identity();
    let other_vm = crate::vm::vm_init::test_vm_identity();
    assert!(
        !interp_only_events_active_for_vm(vm),
        "no listener anywhere: the back edge must not be held interpreted"
    );
    for kind in [
        JvmtiEventKind::SingleStep,
        JvmtiEventKind::MethodEntry,
        JvmtiEventKind::MethodExit,
        JvmtiEventKind::FramePop,
        JvmtiEventKind::FieldAccess,
        JvmtiEventKind::FieldModification,
    ] {
        jvmti::install_manager_for_vm(vm, Arc::new(jvmti::JvmtiEventManager::new_for_vm(vm)));
        jvmti::install_manager_for_vm(
            other_vm,
            Arc::new(jvmti::JvmtiEventManager::new_for_vm(other_vm)),
        );
        let mgr = jvmti::manager_for_vm(vm).expect("row was just installed");
        mgr.set_event_notification_mode(EventMode::Enable, kind, None)
            .expect("enable on a fresh manager");
        mgr.set_event_callbacks(EventCallbacks::default())
            .expect("callbacks on a fresh manager");
        assert!(
            interp_only_events_active_for_vm(vm),
            "{kind:?} listener must hold compiled entry off"
        );
        assert!(
            !interp_only_events_active_for_vm(other_vm),
            "{kind:?}: another VM's agent must not force this VM interpreted"
        );
        jvmti::forget_vm_jvmti_state(vm);
        jvmti::forget_vm_jvmti_state(other_vm);
        assert!(
            !interp_only_events_active_for_vm(vm),
            "{kind:?}: compiled entry must come back once the agent is gone"
        );
    }
}
