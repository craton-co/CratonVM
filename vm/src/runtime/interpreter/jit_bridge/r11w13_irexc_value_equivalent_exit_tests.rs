use super::{
    note_osr_optimizing_value_equivalent_exit, osr_optimizing_already_refused,
    osr_optimizing_cached, osr_optimizing_refusal_key, remember_osr_optimizing_artifact,
    OsrBuildStamps, OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT,
};
use crate::config::VmConfig;
use crate::vm::SharedVm;
use cratonvm_jit::deopt::{
    DeoptAction, DeoptReason, DeoptimizationPoint, FrameState, ResumeSemantics,
};
use cratonvm_types::ClassId;
use std::sync::Arc;

fn point(reason: DeoptReason) -> Box<DeoptimizationPoint> {
    Box::new(DeoptimizationPoint {
        native_offset: 0,
        bci: 2,
        reason,
        action: DeoptAction::Reinterpret,
        speculation_id: cratonvm_jit::deopt::speculation_id(2, reason),
        frame_state: FrameState {
            class_id: 0,
            method_key: String::new(),
            bci: 2,
            locals: Vec::new(),
            stack: Vec::new(),
            monitors: Vec::new(),
            caller: None,
        },
        semantics: ResumeSemantics::REEXECUTE,
    })
}

/// An exact guard's exit is never counted (its frame throws and ends); a
/// value-equivalent one withdraws the memo entry, and refuses its key, at
/// the limit and not before.
#[test]
fn a_value_equivalent_guard_failing_on_every_entry_withdraws_the_body() {
    // Either kill switch changes what this pins, by design.
    if super::osr_optimizing_cache_disabled() || super::osr_optimizing_memo_disabled() {
        return;
    }
    let shared = SharedVm::new(VmConfig::default());
    let cid = ClassId::new(4343);
    // `iconst_0; istore_1; goto 2`, padded like `Frame::code`.
    let code: Arc<[u8]> = Arc::from(&[0x03u8, 0x3c, 0xa7, 0x00, 0x00, 0x00, 0x00][..]);
    let mut buf = cratonvm_jit::ExecutableBuffer::new(64).unwrap();
    buf.emit(&[0xC3]); // ret; metadata only
    let mut cm = cratonvm_jit::CompiledMethod::new(buf);
    cm.ir_osr_entries = vec![(2, 0, 0)];
    cm.ir_osr_sentinel_free = true;
    cm._deopt_point_boxes = vec![
        point(DeoptReason::NullCheck),
        point(DeoptReason::ReceiverTypeChanged),
    ];
    // A box's address survives the move into the `Arc`.
    let exact = &*cm._deopt_point_boxes[0] as *const DeoptimizationPoint as usize;
    let value_equivalent = &*cm._deopt_point_boxes[1] as *const DeoptimizationPoint as usize;
    let artifact = Arc::new(cm);
    let key = osr_optimizing_refusal_key(shared.vm_identity, cid, "run", "()V", 2);
    let pins: Arc<[cratonvm_jit::RetainedCode]> =
        Vec::<cratonvm_jit::RetainedCode>::new().into();
    let stamps = OsrBuildStamps::read(&shared, cid);
    remember_osr_optimizing_artifact(&shared, key, &artifact, &pins, stamps, &code, false);
    let names = ("r11w13/Loop", "run", "()V");
    let cached = || osr_optimizing_cached(&shared, cid, names, key, 2, &code).is_some();
    assert!(cached());
    for _ in 0..(2 * OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT) {
        note_osr_optimizing_value_equivalent_exit(&shared, &artifact, exact);
    }
    assert!(cached(), "an exact guard's exit is not counted");
    for _ in 1..OSR_OPTIMIZING_VALUE_EQUIVALENT_EXIT_LIMIT {
        note_osr_optimizing_value_equivalent_exit(&shared, &artifact, value_equivalent);
    }
    assert!(cached(), "below the limit the body is still served");
    assert!(!osr_optimizing_already_refused(&shared, key));
    note_osr_optimizing_value_equivalent_exit(&shared, &artifact, value_equivalent);
    assert!(!cached(), "withdrawn at the limit");
    assert!(
        osr_optimizing_already_refused(&shared, key),
        "and not rebuilt"
    );
}
