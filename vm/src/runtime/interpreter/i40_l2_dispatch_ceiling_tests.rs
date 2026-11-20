use super::{derive_jit_dispatch_depth_ceiling, derive_recorded_jit_dispatch_depth_ceiling};

/// Only a carrier above the default moves: it keeps the default's 4 MiB
/// of headroom instead of half of itself.
#[test]
fn the_dispatch_ceiling_scales_only_with_a_carrier_above_the_default() {
    const M: usize = 1 << 20;
    for small in [0, M, 2 * M, 8 * M] {
        assert_eq!(
            derive_recorded_jit_dispatch_depth_ceiling(small),
            derive_jit_dispatch_depth_ceiling(small)
        );
    }
    // A 32 MiB request runs on a 40 MiB carrier: (40 - 4) MiB / 8 KiB.
    assert_eq!(derive_recorded_jit_dispatch_depth_ceiling(40 * M), 4608);
    assert!(derive_jit_dispatch_depth_ceiling(40 * M) < 4608);
    // 256 MiB request: (264 - 4) MiB / 8 KiB.
    assert_eq!(derive_recorded_jit_dispatch_depth_ceiling(264 * M), 33_280);
}
