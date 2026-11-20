use super::{
    callee_neg_fingerprint, callee_neg_fingerprint_hinted, jvmti_requires_interpreter,
    named_method_is_synchronized, osr_reentry_stamp,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_reader::class_access_flags::MethodAccessFlags;
use cratonvm_reader::method::ClassFileMethod;
use cratonvm_types::ClassId;
use std::sync::Arc;

/// A verdict recorded for one loader's copy of a class name must not
/// answer for another's: the hinted key differs per class id, and an
/// unhinted (or zero-hinted) probe keeps the historical name-only key.
#[test]
fn a_hinted_callee_negative_key_is_per_class() {
    let plain = callee_neg_fingerprint("a/B", "m", "()V");
    assert_eq!(
        callee_neg_fingerprint_hinted(None, "a/B", "m", "()V"),
        plain
    );
    assert_eq!(
        callee_neg_fingerprint_hinted(Some(ClassId::new(0)), "a/B", "m", "()V"),
        plain
    );
    let in_a = callee_neg_fingerprint_hinted(Some(ClassId::new(11)), "a/B", "m", "()V");
    let in_b = callee_neg_fingerprint_hinted(Some(ClassId::new(12)), "a/B", "m", "()V");
    assert_ne!(in_a, plain);
    assert_ne!(in_a, in_b);
    assert_ne!(in_a, 0, "0 is the empty-slot sentinel");
}

/// The synchronized refusal is asked of the class the door probes, not of
/// whatever the name resolves to.
#[test]
fn the_synchronized_probe_asks_the_probed_class() {
    let shared = SharedVm::new(VmConfig::default());
    let holder = {
        let mut cm = shared.classes.class_manager.write();
        let holder = cm
            .try_ensure_synthetic_class("cratonvm/test/SyncProbeHolder", 0)
            .expect("Compatible mode fabricates");
        cm.class_store
            .get_mut(holder)
            .expect("fabricated")
            .methods
            .push(ClassFileMethod {
                name: Arc::from("locked"),
                descriptor: Arc::from("()V"),
                access_flags: MethodAccessFlags::SYNCHRONIZED,
                attributes: vec![],
            });
        holder
    };
    assert!(named_method_is_synchronized(
        &shared,
        holder,
        "cratonvm/test/NameThatResolvesNowhere",
        "locked",
        "()V"
    ));
    assert!(named_method_is_synchronized(
        &shared,
        ClassId::new(0),
        "cratonvm/test/SyncProbeHolder",
        "locked",
        "()V"
    ));
    assert!(!named_method_is_synchronized(
        &shared,
        ClassId::new(0),
        "cratonvm/test/NameThatResolvesNowhere",
        "locked",
        "()V"
    ));
}

/// The OSR re-entry stamp no longer takes the deopt-log mutex: it moves
/// with the per-VM deopt counter, and only in the VM that deoptimized.
#[test]
fn the_osr_reentry_stamp_moves_with_this_vms_deopts_only() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let (Some(a0), Some(b0)) = (osr_reentry_stamp(&a), osr_reentry_stamp(&b)) else {
        return; // CRATONVM_JIT_OSR_REENTRY_MEMO=0
    };
    a.jit.bridge.note_deopt_event();
    let a1 = osr_reentry_stamp(&a).expect("memo on");
    let b1 = osr_reentry_stamp(&b).expect("memo on");
    assert_eq!(a1.1, a0.1 + 1);
    assert_eq!(b1.1, b0.1, "another VM's deopt is not this VM's");
}

/// The profile-replay watermark is per VM (it was one process-global
/// table searched by VM address).
#[test]
fn the_profile_replay_watermark_is_per_vm() {
    let a = SharedVm::new(VmConfig::default());
    let b = SharedVm::new(VmConfig::default());
    let ord = std::sync::atomic::Ordering::AcqRel;
    a.jit.bridge.profile_replay_watermark.fetch_max(40, ord);
    a.jit.bridge.profile_replay_watermark.fetch_max(7, ord);
    assert_eq!(
        a.jit
            .bridge
            .profile_replay_watermark
            .load(std::sync::atomic::Ordering::Acquire),
        40,
        "a visit that walked less must not roll the watermark back"
    );
    assert_eq!(
        b.jit
            .bridge
            .profile_replay_watermark
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
}

/// A JDWP breakpoint, step or suspension makes every door stand down, as a
/// JVMTI interpreter-only listener does: the dispatch loop is the only
/// place a breakpoint is delivered.
#[cfg(feature = "experimental-debug")]
#[test]
fn an_armed_debugger_request_makes_the_jit_stand_down() {
    let shared = SharedVm::new(VmConfig::default());
    let before = jvmti_requires_interpreter(&shared);
    shared
        .debug
        .breakpoints_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(jvmti_requires_interpreter(&shared));
    shared
        .debug
        .breakpoints_active
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(jvmti_requires_interpreter(&shared), before);
}

/// Without the JDWP surface the stand-down is the JVMTI question alone.
#[cfg(not(feature = "experimental-debug"))]
#[test]
fn without_the_debugger_surface_only_jvmti_decides() {
    let shared = SharedVm::new(VmConfig::default());
    assert_eq!(
        jvmti_requires_interpreter(&shared),
        crate::runtime::jvmti::interp_only_events_active_for_vm(shared.vm_identity)
    );
}
