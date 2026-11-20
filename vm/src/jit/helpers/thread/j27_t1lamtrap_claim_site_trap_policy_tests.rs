use super::*;

const POLICIES: [SiteTrapPolicy; 3] = [
    SiteTrapPolicy::SiteTrap,
    SiteTrapPolicy::DivisionGuard,
    SiteTrapPolicy::HotCatch,
];

/// The first decision applies the policy AND leaves the IR ban in effect
/// (the bare `site_trap_policy_must_apply` does not record it on a claim);
/// a repeat under that ban stands down.
#[test]
fn the_first_decision_applies_and_records_the_ban() {
    for (i, policy) in POLICIES.into_iter().enumerate() {
        let verdicts = cratonvm_jit::JitVerdictRegistry::default();
        let cid = ClassId::new(9100 + i as u32);
        let m = ("j27/T1Lamtrap", "m", "()V");
        let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
        assert!(
            claim_site_trap_policy(&verdicts, h, m, cid, policy),
            "{policy:?}: the first decision applies"
        );
        assert!(
            verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid),
            "{policy:?}: the ban is recorded with the decision"
        );
        assert!(
            !claim_site_trap_policy(&verdicts, h, m, cid, policy),
            "{policy:?}: a repeat under the ban stands down"
        );
    }
}

/// With the refusal memo at its bound no ban can be recorded, so every
/// policy's repeat re-decides -- the hole LAMBDATRAP21-1 is about.
#[test]
fn a_repeat_without_a_ban_re_decides_under_every_policy() {
    let verdicts = cratonvm_jit::JitVerdictRegistry::default();
    let cid = ClassId::new(9200);
    for i in 0..8192u32 {
        let name = "fill".to_string() + &i.to_string();
        verdicts.note_ir_method_refused("j27/T1LamtrapFill", &name, "()V", cid);
    }
    for (i, policy) in POLICIES.into_iter().enumerate() {
        let name = "late".to_string() + &i.to_string();
        let m = ("j27/T1LamtrapLate", name.as_str(), "()V");
        let h = cratonvm_jit::ir_method_memo_hash(m.0, m.1, m.2);
        assert!(claim_site_trap_policy(&verdicts, h, m, cid, policy));
        assert!(
            !verdicts.ir_method_ban_holds(m.0, m.1, m.2, cid),
            "{policy:?}: the positive control, the memo is full"
        );
        assert!(
            claim_site_trap_policy(&verdicts, h, m, cid, policy),
            "{policy:?}: a repeat with no ban in effect re-decides"
        );
    }
}

/// The census words, and only a hot catch spares the trap counter.
#[test]
fn census_words_and_repeat_charging() {
    let words: Vec<_> = POLICIES.iter().map(|p| p.census_name()).collect();
    assert_eq!(words, ["site-trap", "division-guard", "hot-catch"]);
    let charged: Vec<_> = POLICIES.iter().map(|p| p.charges_site_trap_repeat()).collect();
    assert_eq!(charged, [true, true, false]);
}

/// One decision in one place: the helpers claim a site-trap decision in
/// exactly one production call (inside `site_trap_policy_must_apply`).
/// Before this lane `despeculate_trapped_method_inner` claimed inline too.
#[test]
fn the_site_trap_claim_has_one_production_caller() {
    let src = crate::jit::helpers::read_helpers_src_from_disk().replace("\r\n", "\n");
    // Split so this test's own text is not a match.
    let call = concat!("verdicts.claim_site_trap", "_decision(h, declaring_class_id)");
    let claims = src.matches(call).count();
    assert_eq!(claims, 1, "claim the decision only through claim_site_trap_policy");
}
