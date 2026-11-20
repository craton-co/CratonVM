#[test]
fn rlrefl_the_registry_forgets_a_gate_refusal_and_keeps_a_sticky_ban() {
    const C: &str = "cratonvm/test/RlreflIndyLateForget";
    let verdicts = crate::JitVerdictRegistry::default();
    let id = cratonvm_types::ClassId::new(2181);
    let other = cratonvm_types::ClassId::new(2182);
    let hash = crate::ir_method_memo_hash(C, "k", "(II)I");
    let key = verdicts.ir_refusal_key(hash, id);
    assert!(!verdicts.forget_ir_input_refusal(C, "k", "(II)I", id), "nothing recorded");
    verdicts.ir_refusals().note_refused_for_inputs(key);
    assert!(!verdicts.forget_ir_input_refusal(C, "k", "(II)I", other), "another class");
    assert!(verdicts.ir_refusals().is_recorded(key));
    assert!(verdicts.forget_ir_input_refusal(C, "k", "(II)I", id));
    assert!(!verdicts.ir_refusals().is_recorded(key));
    verdicts.note_ir_method_refused(C, "k", "(II)I", id);
    assert!(!verdicts.forget_ir_input_refusal(C, "k", "(II)I", id), "a sticky ban stays");
    assert!(verdicts.ir_method_ban_holds(C, "k", "(II)I", id));
}
