use super::*;

#[test]
fn new_image_dimensions() {
    let img = BufferedImageData::new(320, 240, ImageType::IntArgb);
    assert_eq!(img.width(), 320);
    assert_eq!(img.height(), 240);
    assert_eq!(img.image_type(), ImageType::IntArgb);
    assert_eq!(img.get_data_buffer().len(), 320 * 240);
}

#[test]
fn try_new_rejects_pixel_count_overflow() {
    // 70000 x 70000 ~= 4.9e9 pixels: overflows u32. Must return None,
    // not panic in the multiply.
    assert!(BufferedImageData::try_new(70_000, 70_000, ImageType::IntArgb).is_none());
    assert!(BufferedImageData::try_new(0xFFFF_FFFF, 0xFFFF_FFFF, ImageType::IntArgb).is_none());
    // A normal size still succeeds.
    assert!(BufferedImageData::try_new(16, 16, ImageType::IntArgb).is_some());
}

#[test]
fn try_new_rejects_non_overflowing_raster_above_cap() {
    let side = 8_193u32;
    assert!(
        (side as usize) * (side as usize) > MAX_IMAGE_PIXELS,
        "test dimensions must exceed the cap without overflowing"
    );
    assert!(BufferedImageData::try_new(side, side, ImageType::IntArgb).is_none());
}

#[test]
fn registry_create_rejects_overflow() {
    let mut reg = ImageRegistry::new();
    assert!(reg.create(70_000, 70_000, ImageType::IntArgb).is_none());
    assert!(reg.create(8, 8, ImageType::IntArgb).is_some());
}

#[test]
fn new_argb_is_transparent() {
    let img = BufferedImageData::new(2, 2, ImageType::IntArgb);
    assert_eq!(img.get_rgb(0, 0), 0x0000_0000);
}

#[test]
fn new_rgb_is_opaque_black() {
    let img = BufferedImageData::new(2, 2, ImageType::IntRgb);
    assert_eq!(img.get_rgb(0, 0), 0xFF00_0000);
}

#[test]
fn set_get_pixel() {
    let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    img.set_rgb(5, 3, 0xFFFF0000);
    assert_eq!(img.get_rgb(5, 3), 0xFFFF0000);
    // Other pixels unchanged
    assert_eq!(img.get_rgb(0, 0), 0x0000_0000);
}

#[test]
#[should_panic(expected = "out of bounds")]
fn get_rgb_oob() {
    let img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    img.get_rgb(10, 0);
}

#[test]
#[should_panic(expected = "out of bounds")]
fn set_rgb_oob() {
    let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    img.set_rgb(0, 10, 0);
}

#[test]
fn region_read_write() {
    let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    let region = vec![0xFFAABBCC; 6]; // 3x2
    img.set_rgb_region(2, 3, 3, 2, &region);

    let read = img.get_rgb_region(2, 3, 3, 2);
    assert_eq!(read, region);

    // Spot check boundary pixels are untouched
    assert_eq!(img.get_rgb(1, 3), 0);
    assert_eq!(img.get_rgb(5, 3), 0);
}

#[test]
#[should_panic(expected = "exceeds image")]
fn region_oob() {
    let img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    img.get_rgb_region(8, 8, 5, 5);
}

#[test]
fn subimage() {
    let mut img = BufferedImageData::new(10, 10, ImageType::IntArgb);
    img.set_rgb(3, 4, 0xDEADBEEF);
    let sub = img.get_subimage(2, 3, 5, 5);
    assert_eq!(sub.width(), 5);
    assert_eq!(sub.height(), 5);
    // (3,4) in original -> (1,1) in subimage
    assert_eq!(sub.get_rgb(1, 1), 0xDEADBEEF);
}

#[test]
fn copy_to() {
    let mut src = BufferedImageData::new(4, 4, ImageType::IntArgb);
    src.set_rgb(2, 2, 0xCAFEBABE);
    let mut dst = BufferedImageData::new(4, 4, ImageType::IntArgb);
    src.copy_to(&mut dst);
    assert_eq!(dst.get_rgb(2, 2), 0xCAFEBABE);
}

#[test]
#[should_panic(expected = "dimension mismatch")]
fn copy_to_wrong_size() {
    let src = BufferedImageData::new(4, 4, ImageType::IntArgb);
    let mut dst = BufferedImageData::new(8, 8, ImageType::IntArgb);
    src.copy_to(&mut dst);
}

#[test]
fn create_graphics_unique_ids() {
    let mut img = BufferedImageData::new(1, 1, ImageType::IntArgb);
    let g1 = img.create_graphics();
    let g2 = img.create_graphics();
    assert_ne!(g1, g2);
}

#[test]
fn data_buffer_mut() {
    let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
    let buf = img.get_data_buffer_mut();
    buf[0] = 0xFF112233;
    assert_eq!(img.get_rgb(0, 0), 0xFF112233);
}

#[test]
fn flush_is_noop() {
    let img = BufferedImageData::new(1, 1, ImageType::IntArgb);
    img.flush(); // just ensure it doesn't panic
}

#[test]
fn clone_is_independent() {
    let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
    img.set_rgb(0, 0, 0xAABBCCDD);
    let mut img2 = img.clone();
    img2.set_rgb(0, 0, 0x11223344);
    assert_eq!(img.get_rgb(0, 0), 0xAABBCCDD);
    assert_eq!(img2.get_rgb(0, 0), 0x11223344);
}

#[test]
fn png_encode_decode_preserves_argb_pixels() {
    let mut img = BufferedImageData::new(2, 2, ImageType::IntArgb);
    img.set_rgb(0, 0, 0xFFFF_0000);
    img.set_rgb(1, 0, 0xFF00_FF00);
    img.set_rgb(0, 1, 0xFF00_00FF);
    img.set_rgb(1, 1, 0x8040_3020);

    let encoded = img.encode(EncodedImageFormat::Png).expect("encode png");
    assert!(encoded.starts_with(b"\x89PNG"));

    let decoded = BufferedImageData::decode_encoded(&encoded).expect("decode png");
    assert_eq!(decoded.width(), 2);
    assert_eq!(decoded.height(), 2);
    assert_eq!(decoded.get_rgb(0, 0), 0xFFFF_0000);
    assert_eq!(decoded.get_rgb(1, 0), 0xFF00_FF00);
    assert_eq!(decoded.get_rgb(0, 1), 0xFF00_00FF);
    assert_eq!(decoded.get_rgb(1, 1), 0x8040_3020);
}

/// gc-common w25-d: an opaque image's PNG carries alpha 0xFF whatever the
/// stored top byte (HotSpot 25: `setRGB(0, 0, 0x00123456)` on a
/// `TYPE_INT_RGB` image reads back `ff123456` after a PNG round trip).
#[test]
fn w25d_png_of_an_opaque_image_is_opaque() {
    let mut img = BufferedImageData::new(2, 1, ImageType::IntRgb);
    img.set_rgb(0, 0, 0x0012_3456);
    let encoded = img.encode(EncodedImageFormat::Png).expect("encode png");
    let decoded = BufferedImageData::decode_encoded(&encoded).expect("decode png");
    assert_eq!(decoded.get_rgb(0, 0), 0xFF12_3456);
    assert_eq!(decoded.get_rgb(1, 0), 0xFF00_0000);
}

#[test]
fn jpeg_encode_decode_round_trips_dimensions() {
    let mut img = BufferedImageData::new(3, 2, ImageType::IntRgb);
    img.set_rgb(0, 0, 0xFFFF_0000);
    img.set_rgb(1, 0, 0xFF00_FF00);
    img.set_rgb(2, 0, 0xFF00_00FF);

    let encoded = img.encode(EncodedImageFormat::Jpeg).expect("encode jpeg");
    assert!(encoded.starts_with(&[0xFF, 0xD8]));

    let decoded = BufferedImageData::decode_encoded(&encoded).expect("decode jpeg");
    assert_eq!(decoded.width(), 3);
    assert_eq!(decoded.height(), 2);
}

// ── Registry tests ───────────────────────────────────────────────

#[test]
fn registry_create_get_destroy() {
    let mut reg = ImageRegistry::new();
    let id = reg.create(64, 64, ImageType::IntArgb).unwrap();
    assert!(reg.get(id).is_some());
    assert_eq!(reg.get(id).unwrap().width(), 64);

    reg.get_mut(id).unwrap().set_rgb(0, 0, 0xDEAD);
    assert_eq!(reg.get(id).unwrap().get_rgb(0, 0), 0xDEAD);

    assert!(reg.destroy(id));
    assert!(reg.get(id).is_none());
}

#[test]
fn registry_unique_ids() {
    let mut reg = ImageRegistry::new();
    let a = reg.create(1, 1, ImageType::IntArgb);
    let b = reg.create(1, 1, ImageType::IntArgb);
    assert_ne!(a, b);
    assert_eq!(reg.len(), 2);
}

#[test]
fn registry_destroy_nonexistent() {
    let mut reg = ImageRegistry::new();
    assert!(!reg.destroy(ImageId(999)));
}

// ── Bounded raster memory (bug awt-font-image #2) ────────────────────

#[test]
fn registry_total_bytes_tracks_create_and_destroy() {
    let mut reg = ImageRegistry::new();
    assert_eq!(reg.total_bytes(), 0);
    let id = reg.create(10, 10, ImageType::IntArgb).unwrap();
    assert_eq!(reg.total_bytes(), 10 * 10 * 4);
    reg.destroy(id);
    assert_eq!(reg.total_bytes(), 0);
}

#[test]
fn registry_lru_evicts_idle_when_over_budget() {
    // Budget holds two 10x10 (400 B) rasters but not three.
    let mut reg = ImageRegistry::with_budget(900);
    let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
    let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
    // `b` is released by Java (no live BufferedImage), so it becomes an
    // eligible eviction victim; `a` stays pinned (live).
    assert!(reg.unpin(b));
    // Touch `a` so `b` is also the least-recently-used.
    assert!(reg.get(a).is_some());
    // Third create pushes total to 1200 > 900: the idle, unpinned one
    // (`b`) is evicted; the live ones (`a`, `c`) are not.
    let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
    assert!(reg.get(a).is_some(), "live (pinned) image must survive");
    assert!(reg.get(c).is_some(), "just-created image must survive");
    assert!(reg.get(b).is_none(), "released, idle image is evicted");
    assert!(reg.total_bytes() <= 900);
}

#[test]
fn registry_never_evicts_pinned_live_image() {
    // Review fix: a still-referenced (pinned) image is NEVER evicted, even
    // when that pushes the registry well past the soft budget. Losing live
    // pixels is never acceptable.
    let mut reg = ImageRegistry::with_budget(900);
    let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
    let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
    // Everything stays pinned (the default): a third create overshoots the
    // budget but must not drop either live raster.
    let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
    assert!(reg.get(a).is_some(), "pinned image a must survive");
    assert!(reg.get(b).is_some(), "pinned image b must survive");
    assert!(reg.get(c).is_some(), "just-created image c must survive");
    assert_eq!(reg.len(), 3);
    // The registry is allowed to exceed the soft budget rather than corrupt
    // a live image.
    assert!(reg.total_bytes() > 900);
}

#[test]
fn registry_unpin_then_repin_protects_again() {
    let mut reg = ImageRegistry::with_budget(900);
    let a = reg.create(10, 10, ImageType::IntArgb).unwrap();
    let b = reg.create(10, 10, ImageType::IntArgb).unwrap();
    // Release then re-acquire `b`: it is live again and must not be evicted.
    assert!(reg.unpin(b));
    assert_eq!(reg.is_pinned(b), Some(false));
    assert!(reg.pin(b));
    assert_eq!(reg.is_pinned(b), Some(true));
    // Over-budget create: both `a` and `b` are pinned, so neither is dropped.
    let c = reg.create(10, 10, ImageType::IntArgb).unwrap();
    assert!(reg.get(a).is_some());
    assert!(
        reg.get(b).is_some(),
        "re-pinned image must survive eviction"
    );
    assert!(reg.get(c).is_some());
    assert_eq!(reg.len(), 3);
}

#[test]
fn registry_pin_helpers_on_unknown_id() {
    let mut reg = ImageRegistry::new();
    assert!(!reg.unpin(ImageId(999)));
    assert!(!reg.pin(ImageId(999)));
    assert_eq!(reg.is_pinned(ImageId(999)), None);
}

#[test]
fn registry_create_keeps_oversized_single_image() {
    // A single image larger than the whole budget is kept (the create must
    // still succeed); only the just-created image remains.
    let mut reg = ImageRegistry::with_budget(100);
    let id = reg.create(10, 10, ImageType::IntArgb).unwrap(); // 400 B > 100
    assert!(reg.get(id).is_some());
    assert_eq!(reg.len(), 1);
}

#[test]
fn image_type_has_alpha() {
    assert!(ImageType::IntArgb.has_alpha());
    assert!(ImageType::IntArgbPre.has_alpha());
    assert!(ImageType::Byte4Abgr.has_alpha());
    assert!(!ImageType::IntRgb.has_alpha());
    assert!(!ImageType::IntBgr.has_alpha());
    assert!(!ImageType::ByteGray.has_alpha());
}
