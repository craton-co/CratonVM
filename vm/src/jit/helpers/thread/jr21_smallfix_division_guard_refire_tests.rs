use super::*;

#[test]
fn a_full_refusal_memo_is_re_decided_by_the_same_threads_repeats() {
    let verdicts = cratonvm_jit::JitVerdictRegistry::default();
    let cid = ClassId::new(4243);
    for i in 0..8192u32 {
        let name = "fill".to_string() + &i.to_string();
        verdicts.note_ir_method_refused("jr21/SfFill", &name, "()V", cid);
    }
    let site = ("jr21/SfDiv", "div", "(II)I");
    let h = cratonvm_jit::ir_method_memo_hash(site.0, site.1, site.2);
    let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    let mut decisions = 0;
    for _ in 0..64 {
        if note_division_guard_trap(
            &mut counts,
            7,
            DIVISION_GUARD_RETIER_THRESHOLD,
            DIVISION_GUARD_SITE_CAP,
        ) && site_trap_policy_must_apply(&verdicts, h, site, cid)
        {
            decisions += 1;
            verdicts.note_ir_method_refused(site.0, site.1, site.2, cid);
        }
    }
    assert!(
        !verdicts.ir_method_ban_holds(site.0, site.1, site.2, cid),
        "the positive control: the memo is full"
    );
    assert_eq!(decisions, 3, "exits 16, 32 and 64 each re-decide");
}

/// With the ban recorded, the re-fires are claim-set no-ops.
#[test]
fn a_recorded_ban_makes_every_re_fire_a_no_op() {
    let verdicts = cratonvm_jit::JitVerdictRegistry::default();
    let cid = ClassId::new(4244);
    let site = ("jr21/SfDivBanned", "div", "(II)I");
    let h = cratonvm_jit::ir_method_memo_hash(site.0, site.1, site.2);
    let mut counts: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
    let mut decisions = 0;
    for _ in 0..256 {
        if note_division_guard_trap(
            &mut counts,
            7,
            DIVISION_GUARD_RETIER_THRESHOLD,
            DIVISION_GUARD_SITE_CAP,
        ) && site_trap_policy_must_apply(&verdicts, h, site, cid)
        {
            decisions += 1;
            verdicts.note_ir_method_refused(site.0, site.1, site.2, cid);
        }
    }
    assert_eq!(decisions, 1);
}
