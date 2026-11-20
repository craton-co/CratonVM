use super::{clamp_raster_dims, MAX_RASTER_DIM};

#[test]
fn clamp_raster_dims_passes_small_dims_through() {
    assert_eq!(clamp_raster_dims(10, 20), Some((10, 20, 200)));
    assert_eq!(clamp_raster_dims(1, 1), Some((1, 1, 1)));
}

#[test]
fn clamp_raster_dims_clamps_oversized_sides() {
    // Each side is clamped to MAX_RASTER_DIM independently.
    let (w, h, px) = clamp_raster_dims(u32::MAX, 4).unwrap();
    assert_eq!(w, MAX_RASTER_DIM);
    assert_eq!(h, 4);
    assert_eq!(px, MAX_RASTER_DIM as usize * 4);

    let (w, h, px) = clamp_raster_dims(u32::MAX, u32::MAX).unwrap();
    assert_eq!((w, h), (MAX_RASTER_DIM, MAX_RASTER_DIM));
    assert_eq!(px, MAX_RASTER_DIM as usize * MAX_RASTER_DIM as usize);
}

#[test]
fn clamp_raster_dims_no_u32_wrap_on_hostile_product() {
    // 0x10000 * 0x10000 == 0x1_0000_0000 — wraps to 0 in a u32 multiply
    // but must NOT here: the clamp caps both sides to 0x8000 so the
    // pixel count is the (large but exact) clamped product, never 0.
    let (w, h, px) = clamp_raster_dims(0x1_0000, 0x1_0000).unwrap();
    assert_eq!((w, h), (MAX_RASTER_DIM, MAX_RASTER_DIM));
    assert_ne!(px, 0);
    assert_eq!(px, MAX_RASTER_DIM as usize * MAX_RASTER_DIM as usize);
    // Sanity: the clamped product fits a u32 (1<<15 squared == 1<<30).
    assert!(px <= u32::MAX as usize);
}
