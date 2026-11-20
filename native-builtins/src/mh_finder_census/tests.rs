use super::*;

/// The first / repeat split: the first call on an address is first, a
/// second is not, and a moved epoch forgets.
#[test]
fn first_invoke_splits_by_address_and_epoch() {
    assert!(first_invoke(7, 1, 0x1000));
    assert!(!first_invoke(7, 1, 0x1000));
    assert!(first_invoke(7, 1, 0x2000));
    assert!(first_invoke(7, 2, 0x1000));
    assert!(first_invoke(8, 2, 0x1000));
}

/// Wave 94: the dump's quantile reads a sorted sample.
#[test]
fn quantiles_of_sorted_samples() {
    assert_eq!(quantile(&[], 0.5), 0);
    assert_eq!(quantile(&[1, 2, 3], 0.5), 2);
    assert_eq!(quantile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), 9);
    let a = BodySample { total: 30, share: [0; BODY_SHARES] };
    let b = BodySample { total: 10, share: [0; BODY_SHARES] };
    let c = BodySample { total: 20, share: [0; BODY_SHARES] };
    assert_eq!(median_of(&[&a, &b, &c], |s| s.total), 20);
}

/// Wave 94: a body share is charged only while a body sample is open.
#[test]
fn a_body_share_is_charged_only_inside_an_open_sample() {
    body_share_add(BodyShare::Reentry, 5);
    assert_eq!(BODY_ACC.with(|a| a.get().1), [0; BODY_SHARES]);
    BODY_ACC.with(|a| a.set((Some(std::time::Instant::now()), [0; BODY_SHARES])));
    body_share_add(BodyShare::Reentry, 5);
    body_share_add(BodyShare::Resolve, 7);
    let (_, shares) = BODY_ACC.with(|a| a.replace((None, [0; BODY_SHARES])));
    assert_eq!(shares[BodyShare::Reentry as usize], 5);
    assert_eq!(shares[BodyShare::Resolve as usize], 7);
    assert_eq!(shares[BodyShare::ClassWork as usize], 0);
}

/// Every phase has a distinct slot and a label.
#[test]
fn phases_are_dense_and_labelled() {
    for (i, phase) in Phase::ALL.iter().enumerate() {
        assert_eq!(*phase as usize, i);
        assert!(!phase.label().trim().is_empty());
    }
}
