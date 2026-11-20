//! Round 15 wave 5 (lane chain6; CH5W4-2 + CH5W4-4): the census counts
//! its events and states one soak verdict over its final totals.
use super::*;
use crate::config::VmConfig;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;

fn census(
    forced: u64,
    resumed: u64,
    uncharged: u64,
    refused: u64,
    points: u64,
    points_forced: u64,
) -> InlinedChainCensus {
    let c = InlinedChainCensus::default();
    c.forced.store(forced, Relaxed);
    c.forced_resumed.store(resumed, Relaxed);
    c.uncharged.store(uncharged, Relaxed);
    c.refused.store(refused, Relaxed);
    c.points.store(points, Relaxed);
    c.points_forced.store(points_forced, Relaxed);
    c
}

#[test]
fn the_verdict_names_the_first_failed_clause() {
    assert_eq!(census(5, 5, 5, 0, 3, 3).soak_verdict(true), Ok(()));
    let why =
        |c: InlinedChainCensus, uncharged_on: bool| c.soak_verdict(uncharged_on).unwrap_err();
    assert!(why(census(0, 0, 0, 0, 0, 0), true).starts_with("forced=0"));
    assert!(why(census(5, 4, 5, 0, 3, 3), true).starts_with("forced-resumed=4"));
    assert!(why(census(5, 5, 4, 0, 3, 3), true).starts_with("uncharged=4"));
    // `CRATONVM_DEOPT_EAGER_CHAINS_UNCHARGED=0`: charged trips are the
    // round-13 soak, not a failure.
    assert_eq!(census(5, 5, 0, 0, 3, 3).soak_verdict(false), Ok(()));
    assert!(why(census(5, 5, 5, 2, 3, 3), true).starts_with("refused=2"));
    // Coverage is stated beside the verdict (PARTIAL), not a failure.
    assert_eq!(census(5, 5, 5, 0, 4, 3).soak_verdict(true), Ok(()));
    // Order: the first failed clause is the one named.
    assert!(why(census(5, 4, 0, 2, 4, 3), true).starts_with("forced-resumed="));
}

/// Two published bodies of one method, each with two chain points; three
/// points forced: the census names only the other as never forced, in one
/// of the two bodies, with the body kind (round 16 lane chain: the bodies
/// are recorded at publication, CH5W4-3).
#[test]
fn the_never_forced_points_are_named() {
    use super::r16_chain_test_support::{body, trap};
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        let (a, b) = (body(&[3, 9]), body(&[3, 9]));
        shared.jit.jit_cache.note_forced_chain_publication(&a);
        shared.jit.jit_cache.note_forced_chain_publication(&b);
        let at = trap(&a, 0, true);
        assert!(claim_forced_chain_trip(&shared, &a, at));
        let at = trap(&b, 1, true);
        assert!(claim_forced_chain_trip(&shared, &b, at));
        let at = trap(&b, 0, true);
        assert!(claim_forced_chain_trip(&shared, &b, at));

        let c = &shared.jit.bridge.inlined_chain_census;
        assert_eq!(c.points.load(Relaxed), 4);
        assert_eq!(c.points_forced.load(Relaxed), 3);
        let (never, truncated) = c.never_forced_points();
        assert!(!truncated);
        assert_eq!(
            never,
            vec![(
                // Never published through `put`: the optimizing-OSR label.
                "T.inner:()V@9 <- T.outer:()V@7 (NullCheck) [osr-optimizing]".to_string(),
                1,
                2
            )]
        );
    })
    .join()
    .expect("test thread");
}

/// Every event counts once in `events`, whatever its kind.
#[test]
fn every_event_is_counted_once() {
    std::thread::spawn(|| {
        let shared = Arc::new(SharedVm::new(VmConfig::default()));
        note_inlined_chain_census(&shared, ChainCensusEvent::Refused);
        note_inlined_chain_census(&shared, ChainCensusEvent::OsrInPlace);
        note_inlined_chain_census(&shared, ChainCensusEvent::Pushed { frames: 2 });
        note_inlined_chain_census(&shared, ChainCensusEvent::Uncharged);
        let c = &shared.jit.bridge.inlined_chain_census;
        assert_eq!(c.events.load(Relaxed), 4);
        assert_eq!(c.frames.load(Relaxed), 2);
        assert!(c
            .census_line()
            .starts_with("[cratonvm-deopt] eager-chains: forced=0 "));
    })
    .join()
    .expect("test thread");
}
