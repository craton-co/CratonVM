use super::note_optimizing_catch_exit;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_millis(1_000);

/// A burst reaches the threshold exactly once: the threshold-th catch
/// answers `true`, every other one `false`.
#[test]
fn a_burst_fires_exactly_once_at_the_threshold() {
    let mut counts = rustc_hash::FxHashMap::default();
    let t0 = Instant::now();
    let fired: Vec<bool> = (0..40u64)
        .map(|i| {
            let at = t0 + Duration::from_millis(i);
            note_optimizing_catch_exit(&mut counts, 7u32, at, 16, WINDOW, 64)
        })
        .collect();
    assert_eq!(fired.iter().filter(|f| **f).count(), 1);
    assert!(fired[15], "the 16th catch is the one that fires");
}

/// Catches spaced wider than the window never accumulate: a method that
/// catches now and then keeps its optimizing body.
#[test]
fn a_slow_trickle_never_fires() {
    let mut counts = rustc_hash::FxHashMap::default();
    let t0 = Instant::now();
    for i in 0..100u64 {
        let at = t0 + Duration::from_millis(i * 1_001);
        assert!(!note_optimizing_catch_exit(
            &mut counts,
            7u32,
            at,
            16,
            WINDOW,
            64
        ));
    }
}

/// Two methods count separately, and a full table restarts rather than
/// growing.
#[test]
fn keys_are_independent_and_the_table_is_bounded() {
    let mut counts = rustc_hash::FxHashMap::default();
    let t0 = Instant::now();
    for _ in 0..15 {
        assert!(!note_optimizing_catch_exit(
            &mut counts,
            1u32,
            t0,
            16,
            WINDOW,
            4
        ));
        assert!(!note_optimizing_catch_exit(
            &mut counts,
            2u32,
            t0,
            16,
            WINDOW,
            4
        ));
    }
    assert!(note_optimizing_catch_exit(
        &mut counts,
        1u32,
        t0,
        16,
        WINDOW,
        4
    ));
    for k in 10..20u32 {
        let _ = note_optimizing_catch_exit(&mut counts, k, t0, 16, WINDOW, 4);
        assert!(counts.len() <= 4);
    }
}
