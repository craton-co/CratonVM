use super::{door_recv_memo_way, DOOR_RECV_MEMO_WAYS};
use std::sync::Arc;

/// The way is in range, a function of the key alone, and spreads a call
/// chain's caller methods over more than one way (the one-entry memo's
/// thrash is what the table exists to stop).
#[test]
fn w15_l2_memo_way_is_stable_and_spreads() {
    let d: Arc<str> = Arc::from("()V");
    let names: Vec<Arc<str>> = ["lock", "initialTryLock", "tryRelease", "release", "unlock"]
        .iter()
        .map(|n| Arc::from(*n))
        .collect();
    let mut used = [false; DOOR_RECV_MEMO_WAYS];
    for (i, n) in names.iter().enumerate() {
        let cid = 100 + i as u32;
        let w = door_recv_memo_way(cid, n, &d);
        assert!(w < DOOR_RECV_MEMO_WAYS);
        assert_eq!(w, door_recv_memo_way(cid, n, &d));
        used[w] = true;
    }
    assert!(used.iter().filter(|u| **u).count() > 1);
}

/// The invoke doors' coalescing keeps counts in calls: over any run of one
/// (site, receiver), the weights of the records it lets through add up to the
/// calls up to the last record, and a monomorphic site reaches the optimizing
/// tier's evidence bar (`MIN_GUARDED_RECEIVER_EVIDENCE`) after that many
/// calls, as an unsampled profile does. Counting one per sample left a hot
/// proxy site at 30 of 32 after ~27k calls, so the handler was never spliced.
#[test]
fn coalesced_records_weigh_every_call_they_stand_for() {
    use super::coalesced_record_weight as weight;
    let mut recorded = 0u32;
    let mut evidence_at = None;
    for nth in 1..=10_000u32 {
        if let Some(w) = weight(nth) {
            recorded += w;
            assert_eq!(recorded, nth, "the weights add up to the calls so far, at call {nth}");
            if evidence_at.is_none() && recorded >= cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE {
                evidence_at = Some(nth);
            }
        }
    }
    assert_eq!(evidence_at, Some(cratonvm_jit::MIN_GUARDED_RECEIVER_EVIDENCE));
    assert!((1..=10_000u32).filter(|&n| weight(n).is_some()).count() < 400, "still a sample");
}

/// The per-method streak layer loses no call: across runs that alternate
/// between two sites (a run ending mid-sample included), every site's
/// recorded and flushed calls add up to the calls it was fed.
#[test]
fn an_ended_coalescing_streak_flushes_its_carried_calls() {
    use super::coalesce_streak_step as step;
    let mut streak = (0usize, 0u32, 0u16, 0u32);
    let mut fed = std::collections::HashMap::<(usize, u32), u32>::new();
    let mut counted = std::collections::HashMap::<(usize, u32), u32>::new();
    let runs: [((usize, u32), u32, u32); 4] = [((5, 7), 1000, 3), ((9, u32::MAX), 77, 1), ((5, 7), 45, 32), ((9, u32::MAX), 1, 1)];
    for (site, records, weight) in runs {
        for _ in 0..records {
            *fed.entry(site).or_default() += weight;
            let (now, ended) = step(&mut streak, site.0, site.1, weight);
            if let Some(w) = now {
                *counted.entry(site).or_default() += w;
            }
            if let Some((pc, recv, calls)) = ended {
                *counted.entry((pc, recv)).or_default() += calls;
            }
        }
    }
    // The last run is one record, taken at once, so nothing is still carried.
    assert_eq!(counted, fed);
}
