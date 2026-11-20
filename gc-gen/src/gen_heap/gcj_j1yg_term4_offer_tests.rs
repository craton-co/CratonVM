use super::*;

/// With the switch off the verdict is the pre-gcj conjunction for every
/// combination of the four other divert terms, and the census slot names
/// the first blocking term (GPU, explicit full GC, promotion guard).
#[test]
fn gcj_j1yg_term4_offer_is_the_old_conjunction_with_the_switch_off() {
    for bits in 0u8..16 {
        let promo = bits & 1 != 0;
        let coverage = bits & 2 != 0;
        let explicit = bits & 4 != 0;
        let gpu = bits & 8 != 0;
        let (offered, slot) = term4_offer_verdict(promo, coverage, explicit, gpu, false);
        assert_eq!(offered, !promo && !coverage && !explicit && !gpu, "bits={bits}");
        let want = if coverage || (!gpu && !explicit && !promo) {
            None
        } else if gpu {
            Some(T4O_GPU)
        } else if explicit {
            Some(T4O_EXPLICIT_FULL_GC)
        } else {
            Some(T4O_PROMOTION_RISK)
        };
        assert_eq!(slot, want, "bits={bits}");
    }
}

/// With the switch on, a cycle the promotion guard alone kept is offered
/// (and counted as admitted); a GPU veto or an explicit full collection
/// still keeps it, whatever the guard says.
#[test]
fn gcj_j1yg_the_switch_admits_only_the_promotion_guards_cycles() {
    assert_eq!(
        term4_offer_verdict(true, false, false, false, true),
        (true, Some(T4O_PROMOTION_RISK_ADMITTED))
    );
    assert_eq!(term4_offer_verdict(false, false, false, false, true), (true, None));
    assert_eq!(term4_offer_verdict(true, false, false, true, true), (false, Some(T4O_GPU)));
    assert_eq!(
        term4_offer_verdict(true, false, true, false, true),
        (false, Some(T4O_EXPLICIT_FULL_GC))
    );
    assert_eq!(term4_offer_verdict(true, true, false, false, true), (false, None));
}

