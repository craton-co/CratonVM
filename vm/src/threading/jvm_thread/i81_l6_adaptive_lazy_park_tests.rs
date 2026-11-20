use super::*;

/// Below the streak every park runs the phase; at it, fifteen parks skip
/// and the sixteenth runs it again; a handoff resets both counters.
#[test]
fn the_skip_decision_follows_the_streak_and_rechecks() {
    assert_eq!(lazy_park_skip_decision(0, 0), (false, 0));
    assert_eq!(
        lazy_park_skip_decision(LAZY_PARK_MISSES_TO_SKIP - 1, 7),
        (false, 0)
    );
    let state = ParkState::new();
    for _ in 0..LAZY_PARK_MISSES_TO_SKIP {
        assert!(!state.lazy_phase_skip());
        state.note_lazy_phase_ran_out();
    }
    let skipped = (0..LAZY_PARK_RECHECK_EVERY)
        .filter(|_| state.lazy_phase_skip())
        .count();
    assert_eq!(skipped, usize::from(LAZY_PARK_RECHECK_EVERY) - 1);
    state.note_lazy_handoff();
    assert!(!state.lazy_phase_skip());
    // The streak saturates instead of wrapping back below the threshold.
    for _ in 0..300 {
        state.note_lazy_phase_ran_out();
    }
    assert!(state.lazy_phase_skip());
}
