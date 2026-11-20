use super::*;

/// zt t6/ob4: the scaling arithmetic, its refusals, and no overflow at
/// a large cycle count.
#[test]
fn zt6ob4_cycle_scaling_and_tsc_rate_arithmetic() {
    assert_eq!(cycles_to_nanos(3_000_000_000, 3_000_000_000), Some(1_000_000_000));
    assert_eq!(cycles_to_nanos(2_000, 2_000_000_000), Some(1_000));
    assert_eq!(cycles_to_nanos(5, 0), None);
    assert_eq!(cycles_to_nanos(u64::MAX, 4_000_000_000), Some(4_611_686_018_427_387_903));
    // 2 ms of a 10 MHz counter against 4 million TSC ticks: 2 GHz.
    assert_eq!(
        tsc_hz_from((1_000, 4_001_000), (50, 20_050), 10_000_000),
        Some(2_000_000_000)
    );
    assert_eq!(tsc_hz_from((5, 9), (7, 7), 10_000_000), None, "no QPC step");
    assert_eq!(tsc_hz_from((9, 5), (7, 70), 10_000_000), None, "TSC ran back");
    assert_eq!(tsc_hz_from((5, 9), (7, 70), 0), None);
    assert_eq!(tsc_hz_from((5, 5), (7, 70), 10_000_000), None);    }