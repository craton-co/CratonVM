use super::IrRefusalMemo;

/// An input-stamped refusal is dropped; the site-trap policy's sticky ban
/// is kept.
#[test]
fn forget_drops_a_gate_refusal_and_keeps_a_sticky_ban() {
    let memo = IrRefusalMemo::default();
    let (stamped, sticky, absent) = (0x51u64, 0x52u64, 0x53u64);
    memo.note_refused_for_inputs(stamped);
    memo.note_refused(sticky);
    assert!(memo.is_recorded(stamped));
    assert!(memo.forget_input_refusal(stamped));
    assert!(!memo.is_recorded(stamped));
    assert!(!memo.forget_input_refusal(stamped), "nothing left to drop");
    assert!(!memo.forget_input_refusal(sticky));
    assert!(memo.holds_sticky_ban(sticky));
    assert!(!memo.forget_input_refusal(absent));
}

/// The registry forgets under the key the compile door reads: the method
/// hash and the declaring class id.
#[test]
fn the_registry_forgets_under_the_compile_doors_key() {
    const C: &str = "cratonvm/test/IndyLateForget";
    let verdicts = crate::JitVerdictRegistry::default();
    let id = cratonvm_types::ClassId::new(2131);
    let other = cratonvm_types::ClassId::new(2132);
    let hash = crate::ir_method_memo_hash(C, "k", "(II)I");
    let key = verdicts.ir_refusal_key(hash, id);
    verdicts.ir_refusals().note_refused_for_inputs(key);
    assert!(!verdicts.forget_ir_input_refusal(C, "k", "(II)I", other));
    assert!(verdicts.ir_refusal_is_recorded(hash, id));
    assert!(verdicts.forget_ir_input_refusal(C, "k", "(II)I", id));
    assert!(!verdicts.ir_refusal_is_recorded(hash, id));
    verdicts.note_ir_method_refused(C, "k", "(II)I", id);
    assert!(!verdicts.forget_ir_input_refusal(C, "k", "(II)I", id));
    assert!(verdicts.ir_method_ban_holds(C, "k", "(II)I", id));
}
