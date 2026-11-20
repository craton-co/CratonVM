use super::*;

#[test]
fn a_repeat_re_decides_only_while_the_ir_ban_is_missing() {
    let verdicts = cratonvm_jit::JitVerdictRegistry::default();
    let cid = ClassId::new(4242);
    let first = ("jr21/Rv2", "m", "()V");
    let h = cratonvm_jit::ir_method_memo_hash(first.0, first.1, first.2);
    assert!(
        site_trap_policy_must_apply(&verdicts, h, first, cid),
        "the first decision is claimed"
    );
    verdicts.note_ir_method_refused(first.0, first.1, first.2, cid);
    assert!(
        !site_trap_policy_must_apply(&verdicts, h, first, cid),
        "a repeat under a recorded ban does nothing"
    );
    // Fill the refusal memo to its bound with other methods: a NEW
    // method's ban can no longer be recorded.
    for i in 0..8192u32 {
        let name = "fill".to_string() + &i.to_string();
        verdicts.note_ir_method_refused("jr21/Rv2Fill", &name, "()V", cid);
    }
    let late = ("jr21/Rv2", "n", "()V");
    let h2 = cratonvm_jit::ir_method_memo_hash(late.0, late.1, late.2);
    assert!(site_trap_policy_must_apply(&verdicts, h2, late, cid));
    verdicts.note_ir_method_refused(late.0, late.1, late.2, cid);
    assert!(
        !verdicts.ir_method_ban_holds(late.0, late.1, late.2, cid),
        "the positive control: the memo is full"
    );
    assert!(
        site_trap_policy_must_apply(&verdicts, h2, late, cid),
        "a repeat with no ban in effect re-decides"
    );
    assert!(
        !site_trap_policy_must_apply(&verdicts, h, first, cid),
        "the recorded ban still holds"
    );
}

/// t1 wave 8, lane LAMBDATRAP: a repeat that lands between the first
/// decision's claim and its ban record (another thread) records the ban
/// itself and does not apply the policy a second time.
#[test]
fn a_repeat_racing_the_first_ban_record_does_not_re_decide() {
    let verdicts = cratonvm_jit::JitVerdictRegistry::default();
    let cid = ClassId::new(7175);
    let m = ("t1/LambdatrapRace", "m", "()V");
    let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
    assert!(site_trap_policy_must_apply(&verdicts, h, m, cid), "thread A claims");
    // Thread A has not yet called `note_ir_method_refused`.
    assert!(!verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid));
    assert!(
        !site_trap_policy_must_apply(&verdicts, h, m, cid),
        "thread B's repeat records the ban and stands down"
    );
    assert!(verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid));
}
