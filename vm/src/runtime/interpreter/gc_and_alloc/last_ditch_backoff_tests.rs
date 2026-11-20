use super::LastDitchBackoff;
use std::time::Duration;

/// Yields first, then sleeps that grow and are capped at 1 ms: the wait
/// parks rather than spins, and never oversleeps a quiescence by more
/// than a millisecond.
#[test]
fn the_backoff_yields_then_parks_with_a_capped_sleep() {
    for t in 1..=LastDitchBackoff::YIELD_TURNS {
        assert_eq!(LastDitchBackoff::pause_for(t), None);
    }
    let first = LastDitchBackoff::YIELD_TURNS + 1;
    assert_eq!(
        LastDitchBackoff::pause_for(first),
        Some(Duration::from_micros(50))
    );
    assert_eq!(
        LastDitchBackoff::pause_for(first + 1),
        Some(Duration::from_micros(100))
    );
    let mut prev = Duration::ZERO;
    for t in first..first + 64 {
        let d = LastDitchBackoff::pause_for(t).expect("sleeps past the yield turns");
        assert!(d >= prev && d <= Duration::from_millis(1));
        prev = d;
    }
    assert_eq!(prev, Duration::from_millis(1));
    assert_eq!(
        LastDitchBackoff::pause_for(u32::MAX),
        Some(Duration::from_millis(1))
    );
}
