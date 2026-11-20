use super::*;

/// zt t21/pl10: the scaling is exact at HotSpot's Windows frequency
/// (100 ns ticks) and the identity at nanoseconds; milliseconds scale
/// the same way, and `ticks * 1000 / frequency` is the floor of the
/// milliseconds, as `Management::ticks_to_ms` reads it.
#[test]
fn zt21pl10_ticks_scale_nanoseconds_to_the_frequency() {
    assert_eq!(
        ticks_from_nanos_at(1_234_567_891, NANOS_FREQUENCY),
        1_234_567_891
    );
    assert_eq!(ticks_from_nanos_at(1_234_567_891, 10_000_000), 12_345_678);
    assert_eq!(ticks_from_millis_at(20, 10_000_000), 200_000);
    assert_eq!(ticks_from_millis_at(20, NANOS_FREQUENCY), 20_000_000);
    for ns in [0u64, 99, 100, 999_999, 1_000_000, 470_123_456, 1 << 50] {
        let ticks = ticks_from_nanos_at(ns, 10_000_000);
        assert_eq!(
            u64::try_from(ticks).unwrap() * 1000 / 10_000_000,
            ns / 1_000_000,
            "ns={ns}"
        );
    }
    assert_eq!(
        ticks_from_nanos_at(u64::MAX, i64::MAX),
        i64::MAX,
        "saturates"
    );
    assert_eq!(
        ticks_from_nanos_at(5, 0),
        0,
        "a non-positive frequency is 1"
    );
}

/// zt t21/pl10: nanoseconds under the kill on every OS; off Windows
/// always.
#[test]
fn zt21pl10_the_kill_keeps_nanoseconds() {
    crate::flags::with_thread_overrides(
        &[("CRATONVM_PERF_HRT_QPC_FREQUENCY", Some("0"))],
        || assert_eq!(hrt_frequency(), NANOS_FREQUENCY),
    );
    let on = crate::flags::with_thread_overrides(
        &[("CRATONVM_PERF_HRT_QPC_FREQUENCY", None)],
        hrt_frequency,
    );
    if cfg!(windows) {
        assert!(on > 0, "the performance counter's frequency: {on}");
    } else {
        assert_eq!(on, NANOS_FREQUENCY);
    }
}
