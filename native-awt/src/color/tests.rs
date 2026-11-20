use super::*;

// ── Construction & accessors ──────────────────────────────────────

#[test]
fn new_opaque() {
    let c = Color::new(10, 20, 30);
    assert_eq!(c.red(), 10);
    assert_eq!(c.green(), 20);
    assert_eq!(c.blue(), 30);
    assert_eq!(c.alpha(), 255);
}

#[test]
fn with_alpha() {
    let c = Color::with_alpha(10, 20, 30, 128);
    assert_eq!(c.alpha(), 128);
    assert_eq!(c.red(), 10);
}

#[test]
fn from_argb() {
    let c = Color::from_argb(0x80FF0000);
    assert_eq!(c.alpha(), 0x80);
    assert_eq!(c.red(), 0xFF);
    assert_eq!(c.green(), 0);
    assert_eq!(c.blue(), 0);
}

#[test]
fn from_rgb_sets_alpha_255() {
    let c = Color::from_rgb(0x123456);
    assert_eq!(c.alpha(), 255);
    assert_eq!(c.red(), 0x12);
    assert_eq!(c.green(), 0x34);
    assert_eq!(c.blue(), 0x56);
}

#[test]
fn to_argb_roundtrip() {
    let c = Color::with_alpha(200, 100, 50, 128);
    let c2 = Color::from_argb(c.to_argb());
    assert_eq!(c, c2);
}

#[test]
fn to_rgb_strips_alpha() {
    let c = Color::with_alpha(0xFF, 0x00, 0xAB, 0x80);
    assert_eq!(c.to_rgb(), 0xFF00AB);
}

// ── Constants ────────────────────────────────────────────────────

#[test]
fn constant_colors() {
    assert_eq!(Color::BLACK.red(), 0);
    assert_eq!(Color::BLACK.green(), 0);
    assert_eq!(Color::BLACK.blue(), 0);
    assert_eq!(Color::BLACK.alpha(), 255);

    assert_eq!(Color::WHITE.red(), 255);
    assert_eq!(Color::WHITE.green(), 255);
    assert_eq!(Color::WHITE.blue(), 255);

    assert_eq!(Color::RED.red(), 255);
    assert_eq!(Color::RED.green(), 0);
    assert_eq!(Color::RED.blue(), 0);

    assert_eq!(Color::TRANSPARENT.alpha(), 0);
    assert_eq!(Color::TRANSPARENT.to_argb(), 0);
}

// ── brighter / darker ────────────────────────────────────────────

#[test]
fn brighter_basic() {
    let c = Color::new(100, 100, 100);
    let b = c.brighter();
    // 100 / 0.7 = 142.857 -> 142
    assert_eq!(b.red(), 142);
    assert_eq!(b.green(), 142);
    assert_eq!(b.blue(), 142);
    assert_eq!(b.alpha(), 255); // alpha preserved
}

#[test]
fn brighter_capped_at_255() {
    let c = Color::new(200, 200, 200);
    let b = c.brighter();
    // 200 / 0.7 = 285.7 -> capped to 255
    assert_eq!(b.red(), 255);
    assert_eq!(b.green(), 255);
    assert_eq!(b.blue(), 255);
}

#[test]
fn brighter_from_black() {
    // Java special case: pure black -> (3, 3, 3)
    let b = Color::BLACK.brighter();
    assert_eq!(b.red(), 3);
    assert_eq!(b.green(), 3);
    assert_eq!(b.blue(), 3);
}

#[test]
fn brighter_preserves_alpha() {
    let c = Color::with_alpha(100, 100, 100, 64);
    assert_eq!(c.brighter().alpha(), 64);
}

#[test]
fn darker_basic() {
    let c = Color::new(100, 100, 100);
    let d = c.darker();
    // 100 * 0.7 = 70
    assert_eq!(d.red(), 70);
    assert_eq!(d.green(), 70);
    assert_eq!(d.blue(), 70);
}

#[test]
fn darker_towards_black() {
    let c = Color::new(1, 1, 1);
    let d = c.darker();
    // 1 * 0.7 = 0.7 -> 0
    assert_eq!(d.red(), 0);
    assert_eq!(d.green(), 0);
    assert_eq!(d.blue(), 0);
}

#[test]
fn darker_preserves_alpha() {
    let c = Color::with_alpha(200, 200, 200, 128);
    assert_eq!(c.darker().alpha(), 128);
}

#[test]
fn brighter_darker_roundtrip_approximate() {
    // brighter then darker should roughly get back to original
    let c = Color::new(100, 100, 100);
    let rt = c.brighter().darker();
    // 100 -> 142 -> 99  (rounding loss)
    assert!((rt.red() as i32 - 100).abs() <= 2);
}

// ── blend (Porter-Duff SRC_OVER) ─────────────────────────────────

#[test]
fn blend_opaque_fg_replaces() {
    let fg = Color::RED;
    let bg = Color::BLUE;
    let result = Color::blend(fg, bg);
    assert_eq!(result, Color::RED);
}

#[test]
fn blend_transparent_fg_keeps_bg() {
    let fg = Color::TRANSPARENT;
    let bg = Color::GREEN;
    let result = Color::blend(fg, bg);
    assert_eq!(result, Color::GREEN);
}

#[test]
fn blend_half_alpha() {
    let fg = Color::with_alpha(255, 0, 0, 128); // 50% red
    let bg = Color::new(0, 0, 255); // opaque blue
    let result = Color::blend(fg, bg);
    // Expected: roughly (128, 0, 127) with full alpha
    assert!(result.red() > 120 && result.red() < 136);
    assert!(result.blue() > 120 && result.blue() < 136);
    assert_eq!(result.alpha(), 255); // bg is opaque, so result is opaque
}

#[test]
fn blend_both_transparent() {
    let result = Color::blend(Color::TRANSPARENT, Color::TRANSPARENT);
    assert_eq!(result, Color::TRANSPARENT);
}

#[test]
fn blend_two_semitransparent() {
    let fg = Color::with_alpha(255, 0, 0, 128);
    let bg = Color::with_alpha(0, 0, 255, 128);
    let result = Color::blend(fg, bg);
    // out_a = 0.502 + 0.502 * 0.498 ≈ 0.752
    assert!(result.alpha() > 180 && result.alpha() < 200);
    // Red channel weighted more (fg)
    assert!(result.red() > result.blue());
}

// ── ColorSpace gamma ─────────────────────────────────────────────

#[test]
fn srgb_to_linear_zero_and_one() {
    assert!((ColorSpace::srgb_to_linear(0.0)).abs() < 1e-6);
    assert!((ColorSpace::srgb_to_linear(1.0) - 1.0).abs() < 1e-6);
}

#[test]
fn linear_to_srgb_zero_and_one() {
    assert!((ColorSpace::linear_to_srgb(0.0)).abs() < 1e-6);
    assert!((ColorSpace::linear_to_srgb(1.0) - 1.0).abs() < 1e-6);
}

#[test]
fn srgb_linear_roundtrip() {
    for i in 0..=10 {
        let s = i as f32 / 10.0;
        let l = ColorSpace::srgb_to_linear(s);
        let s2 = ColorSpace::linear_to_srgb(l);
        assert!(
            (s - s2).abs() < 1e-5,
            "roundtrip failed for {}: got {}",
            s,
            s2
        );
    }
}

#[test]
fn srgb_midpoint_darker_in_linear() {
    // sRGB 0.5 should map to linear ~0.214
    let lin = ColorSpace::srgb_to_linear(0.5);
    assert!(lin > 0.20 && lin < 0.23, "got {}", lin);
}

// ── Display ──────────────────────────────────────────────────────

#[test]
fn display_format() {
    let c = Color::new(10, 20, 30);
    assert_eq!(format!("{}", c), "Color[r=10,g=20,b=30,a=255]");
}
