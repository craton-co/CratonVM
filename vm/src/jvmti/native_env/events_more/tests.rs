use super::*;

/// Each wave-63 event wants its capability, and only the events HotSpot
/// refuses per thread are global.
#[test]
fn the_wave_63_events_want_their_capabilities() {
    for event in [
        EVENT_FIELD_ACCESS,
        EVENT_NATIVE_METHOD_BIND,
        EVENT_MONITOR_WAIT,
        EVENT_GC_START,
        EVENT_OBJECT_FREE,
        EVENT_VM_OBJECT_ALLOC,
        EVENT_SAMPLED_OBJECT_ALLOC,
        EVENT_VIRTUAL_THREAD_START,
    ] {
        assert!(
            event_capability(event).is_some_and(|c| c.iter().any(|&w| w != 0)),
            "{event}"
        );
    }
    assert!(is_global_event(EVENT_DATA_DUMP_REQUEST));
    assert!(!is_global_event(EVENT_MONITOR_WAIT));
}

/// Wave 83: the funnel posts the registered natives' binds only when the
/// event was enabled while the startup envs were bound, and stops for
/// good once it was disabled or enabled late (natives ran unwatched).
#[test]
fn the_rust_bind_tracking_starts_only_at_startup() {
    use RustBindTracking::{Lost, NotArmed, Tracking};
    assert_eq!(
        rust_bind_tracking_step(NotArmed, false, true),
        (NotArmed, false)
    );
    assert_eq!(
        rust_bind_tracking_step(NotArmed, false, false),
        (NotArmed, false)
    );
    assert_eq!(
        rust_bind_tracking_step(NotArmed, true, true),
        (Tracking, true)
    );
    assert_eq!(
        rust_bind_tracking_step(NotArmed, true, false),
        (Lost, false)
    );
    assert_eq!(
        rust_bind_tracking_step(Tracking, true, false),
        (Tracking, true)
    );
    assert_eq!(
        rust_bind_tracking_step(Tracking, false, false),
        (Lost, false)
    );
    assert_eq!(rust_bind_tracking_step(Lost, true, true), (Lost, false));
    assert_eq!(rust_bind_tracking_step(Lost, true, false), (Lost, false));
}

/// The sampling distance is positive and averages near the interval.
#[test]
fn the_sampling_distance_averages_the_interval() {
    let interval = 512 * 1024;
    let n = 4000;
    let sum: i64 = (0..n).map(|_| next_sample(interval)).sum();
    assert!((0..100).all(|_| next_sample(interval) >= 1));
    let mean = sum / n;
    assert!(mean > interval / 2 && mean < interval * 2, "mean {mean}");
}
