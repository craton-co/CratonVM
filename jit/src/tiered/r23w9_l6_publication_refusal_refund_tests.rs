use super::*;

#[test]
fn a_refused_publication_hands_its_charge_back_until_the_refunds_run_out() {
    assert_eq!(publication_refusal_refund(1, 0, true), Some((0, 1)));
    assert_eq!(publication_refusal_refund(3, 5, true), Some((2, 6)));
    assert_eq!(
        publication_refusal_refund(3, MAX_PUBLICATION_REFUSAL_REFUNDS, true),
        None,
        "bounded: the old budget applies after the refunds"
    );
}

#[test]
fn nothing_is_refunded_when_off_or_uncharged() {
    assert_eq!(publication_refusal_refund(2, 0, false), None);
    assert_eq!(publication_refusal_refund(0, 0, true), None);
}

/// Total attempts stay bounded: the charged budget plus the refunds.
#[test]
fn the_refunds_bound_the_attempts() {
    let (mut fails, mut refunds, mut attempts) = (0u32, 0u8, 0u32);
    while fails < MAX_TIER_FAIL_RETRIES {
        attempts += 1;
        fails += 1;
        if let Some((f, r)) = publication_refusal_refund(fails, refunds, true) {
            fails = f;
            refunds = r;
        }
        assert!(attempts < 100);
    }
    assert_eq!(
        attempts,
        MAX_TIER_FAIL_RETRIES + u32::from(MAX_PUBLICATION_REFUSAL_REFUNDS)
    );
}

