use super::*;
use crate::config::VmConfig;
use crate::debug::{WITHDRAWAL_BY_JDWP, WITHDRAWAL_BY_JVMTI};
use crate::jit::tiered::{CompilationTier, MethodKey};
use crate::runtime::jvmti::{self, EventMode, JvmtiEventKind, JvmtiEventManager};
use std::sync::Arc;

/// A one-`RET` body, publishable under any key.
fn body() -> cratonvm_jit::CompiledMethod {
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).expect("executable buffer");
    buf.emit(&[0xC3]); // RET
    cratonvm_jit::CompiledMethod::new(buf)
}

#[test]
fn the_first_source_withdraws_every_body_and_the_last_reopens_the_gate() {
    if !EVERY_BODY_WITHDRAWAL_ENABLED {
        return;
    }
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let id = ClassId::new(0x3701);
    let key = MethodKey::with_class_id(id, "i37/l1/Hot", "run", "()V");
    let tiered = &shared.jit.tiered_manager;
    tiered.on_method_invocation(&key);
    tiered.compilation_complete(&key, CompilationTier::C1, 1);
    shared.jit.jit_cache.put(
        Arc::from("i37/l1/Hot"),
        Arc::from("run"),
        Arc::from("()V"),
        id,
        body(),
    );
    let published = shared
        .jit
        .jit_cache
        .get("i37/l1/Hot", "run", "()V", id)
        .expect("published");
    let gates = &shared.debug.debugger_gates;
    assert!(!gates.every_body_withdrawn(), "nothing without a debugger");
    let before = shared.jit.code_state_epoch();

    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
    assert!(gates.every_body_withdrawn());
    assert!(shared
        .jit
        .jit_cache
        .get("i37/l1/Hot", "run", "()V", id)
        .is_none());
    assert!(
        published.is_withdrawn_by_redefinition(),
        "a frame running it leaves at its next exit-capable point"
    );
    assert!(shared
        .jit
        .jit_cache
        .body_withdrawn_by_redefinition(&published));
    let withdrawn_at = shared.jit.code_state_epoch();
    assert!(withdrawn_at > before, "queued compile requests expire");
    if shared.jit.jit_cache.reports_withdrawals() {
        assert_eq!(tiered.current_tier(&key), CompilationTier::Interpreter);
    }

    // A second source finds the code withdrawn already.
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JVMTI, true);
    assert_eq!(shared.jit.code_state_epoch(), withdrawn_at);
    // The gate stays closed while either source is armed.
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
    assert!(gates.every_body_withdrawn());
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JVMTI, false);
    assert!(!gates.every_body_withdrawn(), "no source is armed");
    assert_eq!(
        gates.tier_up_held(),
        crate::debug::WITHDRAWAL_LINGER_MS != 0,
        "the tier-up strides stay closed for the linger (wave 39)"
    );
    assert_eq!(
        shared.jit.code_state_epoch(),
        withdrawn_at,
        "a disarm flushes nothing"
    );

    // Wave 39: the next request finds nothing compiled since, and takes
    // no second flush (a debugger's next step); queued compiles still
    // expire.
    let held_before = gates.withdrawals_held();
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
    assert!(gates.every_body_withdrawn());
    assert_eq!(gates.withdrawals_held(), held_before + 1, "no second flush");
    let fenced_at = shared.jit.code_state_epoch();
    assert!(fenced_at > withdrawn_at, "queued compile requests expire");
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);

    // Something compiled meanwhile (a door that does not ask the strides):
    // the next request withdraws again.
    shared.jit.jit_cache.put(
        Arc::from("i37/l1/Hot"),
        Arc::from("run"),
        Arc::from("()V"),
        id,
        body(),
    );
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
    assert_eq!(gates.withdrawals_held(), held_before + 1, "withdrawn again");
    assert!(shared.jit.code_state_epoch() > fenced_at);
    assert!(shared
        .jit
        .jit_cache
        .get("i37/l1/Hot", "run", "()V", id)
        .is_none());
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
    assert!(!gates.every_body_withdrawn());
}

/// Wave 39: the linger ends; a disarm of a source that was not armed
/// starts none.
#[test]
fn a_disarm_without_an_armed_source_starts_no_linger() {
    if !EVERY_BODY_WITHDRAWAL_ENABLED {
        return;
    }
    let shared = Arc::new(SharedVm::new(VmConfig::default()));
    let gates = &shared.debug.debugger_gates;
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
    assert!(!gates.tier_up_held(), "nothing was armed");
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
    assert!(gates.tier_up_held());
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
    assert_eq!(
        gates.tier_up_held(),
        crate::debug::WITHDRAWAL_LINGER_MS != 0
    );
    assert!(!gates.every_body_withdrawn());
    // A re-arm ends the linger; the bit holds the strides instead.
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, true);
    assert!(!gates.end_withdrawal_linger(), "the arm ended it");
    assert!(gates.tier_up_held());
    note_every_method_needs_the_interpreter(&shared, WITHDRAWAL_BY_JDWP, false);
}

/// The JVMTI source: a `MethodEntry` listener of the VM's manager raises
/// it (through `jvmti::publish_union_listener_flags`) and disabling the
/// event lowers it; an `Exception` listener does not, since the compiled
/// catch doors post that event themselves.
#[test]
fn a_method_entry_listener_holds_the_code_withdrawn_while_it_listens() {
    if !EVERY_BODY_WITHDRAWAL_ENABLED {
        return;
    }
    std::thread::spawn(|| {
        let _lock = jvmti::jvmti_registry_test_lock();
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let vm = shared.vm_identity;
        jvmti::install_manager_for_vm(vm, Arc::new(JvmtiEventManager::new_for_vm(vm)));
        jvmti::install_real_agent_env_bridge(&shared);
        let mgr = jvmti::manager_for_vm(vm).expect("a manager for this VM");
        let gates = &shared.debug.debugger_gates;
        assert!(!gates.every_body_withdrawn());

        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::Exception, None)
            .expect("enable Exception");
        assert!(
            !gates.every_body_withdrawn(),
            "Exception keeps the doors' answer only"
        );
        mgr.set_event_notification_mode(EventMode::Enable, JvmtiEventKind::MethodEntry, None)
            .expect("enable MethodEntry");
        assert!(gates.every_body_withdrawn());
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::MethodEntry, None)
            .expect("disable MethodEntry");
        assert!(!gates.every_body_withdrawn());
        mgr.set_event_notification_mode(EventMode::Disable, JvmtiEventKind::Exception, None)
            .expect("disable Exception");
        jvmti::forget_vm_jvmti_state(vm);
    })
    .join()
    .expect("test thread");
}
