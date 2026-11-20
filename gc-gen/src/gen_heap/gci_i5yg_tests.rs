use super::{next_young_trigger, GenerationalHeap, TriggerFeedback};

const M: usize = 1 << 20;

/// Off, every in-band pause that finds a trial pending is counted (the
/// trial is dropped); on, the same pause is counted and judged, and the
/// latch it sets keeps the next over-goal pause from opening a trial, so
/// the later in-band pause finds none.
#[test]
fn gci_i5yg_in_band_trials_are_counted_on_both_arms() {
    let (floor, ceiling, cap) = (8 * M, 64 * M, 128 * M);
    let run = |judge: bool| {
        let mut fb = TriggerFeedback {
            judge_in_band: judge,
            ..TriggerFeedback::default()
        };
        let mut t = 64 * M;
        for (pause, copied) in [(203, 77), (200, 78), (205, 79), (100, 79), (30, 79)] {
            t = next_young_trigger(&mut fb, t, floor, ceiling, cap, pause, 200, (copied * M) as u64);
        }
        (fb.census.adapt_in_band_trials, fb.census.reverts)
    };
    assert_eq!(run(false), (2, 0), "historical arm: two trials dropped");
    assert_eq!(run(true), (1, 1), "judged arm: one trial judged, then latched");

    // No trial pending: an in-band pause is not counted.
    let mut fb = TriggerFeedback::default();
    let t = next_young_trigger(&mut fb, 64 * M, floor, ceiling, cap, 100, 200, M as u64);
    assert_eq!((t, fb.census.adapt_in_band_trials), (64 * M, 0));
}

/// The key is on the shutdown line, last, and starts at zero.
#[test]
fn gci_i5yg_the_young_trigger_line_carries_the_in_band_count() {
    let heap = GenerationalHeap::with_sizes(M, M);
    let line = heap.young_trigger_census_line();
    assert!(line.ends_with(" adapt_in_band_trials=0"), "{line}");
}
