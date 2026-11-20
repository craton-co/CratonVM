use super::handoff_sleep_millis_with;

/// Wave 79, lane L6: no floor set, no stretch -- HotSpot sleeps what was
/// asked, right after a submit too.
#[test]
fn no_floor_set_sleeps_the_request() {
    for requested in [0, 1, 5, 10, 50, 75, 100, 101, 1_000] {
        for since in [None, Some(0), Some(500), Some(1_000), Some(5_000)] {
            assert_eq!(
                handoff_sleep_millis_with(requested, since, None, None),
                requested
            );
        }
    }
}

/// The opt-in floors still apply inside the window, and only there.
#[test]
fn opt_in_floors_apply_within_the_window_only() {
    assert_eq!(
        handoff_sleep_millis_with(100, Some(10), None, Some(3_000)),
        3_000
    );
    assert_eq!(
        handoff_sleep_millis_with(50, Some(1_000), None, Some(200)),
        200
    );
    assert_eq!(
        handoff_sleep_millis_with(100, Some(1_001), None, Some(3_000)),
        100
    );
    assert_eq!(handoff_sleep_millis_with(100, None, None, Some(3_000)), 100);
    assert_eq!(handoff_sleep_millis_with(5, Some(10), Some(20), None), 20);
    // The other arm's floor does not apply.
    assert_eq!(handoff_sleep_millis_with(5, Some(10), None, Some(3_000)), 5);
    assert_eq!(
        handoff_sleep_millis_with(100, Some(10), Some(3_000), None),
        100
    );
    // Outside both arms: unchanged.
    assert_eq!(
        handoff_sleep_millis_with(20, Some(10), Some(3_000), Some(3_000)),
        20
    );
}

/// A floor below the request leaves the request (it used to fall back
/// to the 3 s default).
#[test]
fn a_floor_below_the_request_is_ignored() {
    assert_eq!(
        handoff_sleep_millis_with(100, Some(10), None, Some(60)),
        100
    );
    assert_eq!(handoff_sleep_millis_with(8, Some(10), Some(2), None), 8);
}
