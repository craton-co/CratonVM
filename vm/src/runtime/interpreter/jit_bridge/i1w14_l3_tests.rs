use super::{
    note_osr_optimizing_refusal, note_osr_refused_activation, note_osr_refused_activation_at,
    osr_loop_offer_allowed, osr_optimizing_already_refused, osr_optimizing_artifact_is_stale,
    osr_optimizing_cache_disabled, osr_optimizing_cached, osr_optimizing_memo_disabled,
    osr_optimizing_refusal_key, remember_osr_optimizing_artifact, OsrBuildStamps,
    OSR_REFUSED_ACTIVATIONS_BUDGET,
};
use crate::config::VmConfig;
use crate::runtime::frame::Frame;
use crate::vm::SharedVm;
use cratonvm_types::ClassId;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// An optimizing-OSR refusal judged the bytecode loaded when it was made:
/// this VM's redefinition re-opens it, another VM's does not.
#[test]
fn a_redefinition_reopens_this_vms_optimizing_osr_refusals() {
    if osr_optimizing_memo_disabled() {
        return;
    }
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1405);
    let key = osr_optimizing_refusal_key(first.vm_identity, cid, "m", "()V", 3);
    let now = |vm: &SharedVm| vm.jit.jit_cache.redefine_epoch();
    note_osr_optimizing_refusal(&first, key, now(&first));
    assert!(osr_optimizing_already_refused(&first, key));
    second.jit.note_class_redefinition();
    assert!(osr_optimizing_already_refused(&first, key));
    // A flush alone keeps it: the refusal is about the bytecode.
    first.jit.flush_code_cache();
    assert!(osr_optimizing_already_refused(&first, key));
    first.jit.note_class_redefinition();
    assert!(!osr_optimizing_already_refused(&first, key));
    note_osr_optimizing_refusal(&first, key, now(&first));
    assert!(
        osr_optimizing_already_refused(&first, key),
        "and re-recorded"
    );
}

/// Interpreter round i1 wave 15, lane L4: a refusal is stamped with the
/// redefine epoch read BEFORE its judgement, so a build that raced a
/// redefinition records a verdict about the OLD bytecode that is already
/// stale, and never replaces a current refusal of the new bytecode.
#[test]
fn a_refusal_judged_before_a_redefinition_is_born_stale() {
    if osr_optimizing_memo_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1506);
    let key = osr_optimizing_refusal_key(shared.vm_identity, cid, "m", "()V", 5);
    // The build reads its stamps, then the class is redefined under it.
    let judged_at = OsrBuildStamps::read(&shared, cid).jit_redefine_epoch;
    shared.jit.note_class_redefinition();
    note_osr_optimizing_refusal(&shared, key, judged_at);
    assert!(
        !osr_optimizing_already_refused(&shared, key),
        "the old bytecode's verdict does not refuse the new bytecode"
    );

    // A current refusal survives a late verdict about the old bytecode.
    let current = shared.jit.jit_cache.redefine_epoch();
    note_osr_optimizing_refusal(&shared, key, current);
    assert!(osr_optimizing_already_refused(&shared, key));
    note_osr_optimizing_refusal(&shared, key, judged_at);
    assert!(osr_optimizing_already_refused(&shared, key));
}

/// A refused-loop record of an older epoch is purged by the next refused
/// activation at a newer one, even when its own loop is never offered
/// again, so the one-load fast path of `osr_loop_offer_allowed` returns.
#[test]
fn a_refused_activation_at_a_new_epoch_purges_the_old_epochs_records() {
    let shared = SharedVm::new(VmConfig::default());
    let bridge = &shared.jit.bridge;
    let (gone, live) = ((3, 0xabc, 8), (3, 0xabc, 20));
    note_osr_refused_activation_at(bridge, gone, 5);
    assert_eq!(bridge.osr_refused_loop_count.load(Ordering::Acquire), 1);
    assert_eq!(note_osr_refused_activation_at(bridge, live, 6), 1);
    assert_eq!(
        bridge.osr_refused_loop_count.load(Ordering::Acquire),
        1,
        "the epoch-5 record is gone"
    );
    let has = |key| {
        bridge
            .osr_refused_loops
            .lock()
            .map(|m| m.contains_key(&key))
            .unwrap_or(false)
    };
    assert!(!has(gone));
    assert!(has(live));
}

const CLASS: &str = "cratonvm/test/L3CodeState";

fn loop_frame() -> Frame {
    Frame::new(
        ClassId::new(0x1403),
        CLASS.to_string(),
        "spin".to_string(),
        "()V".to_string(),
        None,
        vec![0xb1],
        vec![],
        1,
        1,
        &[],
    )
}

#[test]
fn another_vms_redefinition_or_flush_leaves_this_vms_loop_budget_spent() {
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let frame = loop_frame();
    for _ in 0..OSR_REFUSED_ACTIVATIONS_BUDGET {
        note_osr_refused_activation(&first, &frame, 4);
    }
    assert!(
        !osr_loop_offer_allowed(&first, &frame, 4),
        "the budget is spent"
    );
    second.jit.note_class_redefinition();
    second.jit.flush_code_cache();
    assert!(
        !osr_loop_offer_allowed(&first, &frame, 4),
        "another VM's redefinition and flush leave it spent"
    );
    first.jit.flush_code_cache();
    assert!(
        osr_loop_offer_allowed(&first, &frame, 4),
        "this VM's own flush forgets it"
    );
}

#[test]
fn another_vms_redefinition_leaves_this_vms_optimizing_osr_body_served() {
    if osr_optimizing_cache_disabled() || osr_optimizing_memo_disabled() {
        return;
    }
    let first = SharedVm::new(VmConfig::default());
    let second = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(0x1403);
    // `iconst_0; istore_1; goto 2`, padded like `Frame::code`.
    let code: Arc<[u8]> = Arc::from(&[0x03u8, 0x3c, 0xa7, 0x00, 0x00, 0x00, 0x00][..]);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64)
        .expect("executable buffer alloc must succeed in tests");
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.ir_osr_entries = vec![(2, 0, 0)];
    cm.ir_osr_sentinel_free = true;
    let artifact = Arc::new(cm);
    let key = osr_optimizing_refusal_key(first.vm_identity, cid, "run", "()V", 2);
    let pins: Arc<[cratonvm_jit::RetainedCode]> =
        Vec::<cratonvm_jit::RetainedCode>::new().into();
    let stamps = OsrBuildStamps::read(&first, cid);
    remember_osr_optimizing_artifact(&first, key, &artifact, &pins, stamps, &code, false);
    let names = (CLASS, "run", "()V");
    let cached = || osr_optimizing_cached(&first, cid, names, key, 2, &code).is_some();
    assert!(cached());

    second.jit.note_class_redefinition();
    assert!(!osr_optimizing_artifact_is_stale(
        &first,
        stamps.code_state_epoch
    ));
    assert!(
        cached(),
        "another VM's redefinition does not stale this VM's body"
    );

    first.jit.note_class_redefinition();
    assert!(osr_optimizing_artifact_is_stale(
        &first,
        stamps.code_state_epoch
    ));
    assert!(!cached(), "this VM's own redefinition does");
}
