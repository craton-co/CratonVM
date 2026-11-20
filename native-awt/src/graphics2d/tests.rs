use super::*;
use crate::renderer::AffineTransform;
use std::f64::consts::PI;

#[test]
fn test_create_and_dimensions() {
    let g = Graphics2DState::create(100, 50);
    assert_eq!(g.width(), 100);
    assert_eq!(g.height(), 50);
    assert_eq!(g.pixels().len(), 5000);
}

/// gc-common w25-d: `derive` (`Graphics.create()`) copies the settings,
/// not the pixels, and the child's changes do not leak into the parent.
#[test]
fn w25d_derive_copies_settings_into_a_fresh_buffer() {
    let mut parent = Graphics2DState::create(4, 4);
    parent.set_color(1, 2, 3, 255);
    parent.set_background(0xFF_445566);
    parent.translate(1.0, 2.0);
    parent.set_clip_rect(0, 0, 3, 3);
    parent.fill_rect(0, 0, 1, 1);
    let mut child = parent.derive(4, 4);
    assert_eq!(child.color(), 0xFF_010203);
    assert_eq!(child.background(), 0xFF_445566);
    assert_eq!(child.get_transform(), AffineTransform::translate(1.0, 2.0));
    assert_eq!(child.get_clip_bounds(), parent.get_clip_bounds());
    assert!(
        child.pixels().iter().all(|&p| p == 0),
        "pixels are not copied"
    );
    // The inherited translation places the child's drawing.
    child.fill_rect(0, 0, 1, 1);
    assert_eq!(child.pixels()[2 * 4 + 1], 0xFF_010203);
    child.set_color(9, 9, 9, 255);
    assert_eq!(parent.color(), 0xFF_010203);
    assert_eq!(
        (parent.derive(1, 1).width(), parent.derive(1, 1).height()),
        (1, 1)
    );
}

#[test]
fn test_set_color_and_fill() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 10, 10);
    for p in g.pixels() {
        assert_eq!(*p, 0xFF_FF0000);
    }
}

#[test]
fn test_draw_line_horizontal() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_color(0, 255, 0, 255);
    g.draw_line(2, 5, 10, 5);
    for x in 2..=10 {
        assert_eq!(g.pixels()[(5 * 20 + x) as usize], 0xFF_00FF00);
    }
}

#[test]
fn test_draw_rect_and_fill_rect() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_color(0, 0, 255, 255);
    g.fill_rect(3, 3, 5, 5);
    assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_0000FF);

    g.set_color(255, 0, 0, 255);
    g.draw_rect(3, 3, 5, 5);
    assert_eq!(g.pixels()[(3 * 20 + 3) as usize], 0xFF_FF0000);
}

#[test]
fn test_draw_oval() {
    let mut g = Graphics2DState::create(30, 30);
    g.set_color(255, 255, 0, 255);
    g.fill_oval(5, 5, 20, 20);
    assert_ne!(g.pixels()[(15 * 30 + 15) as usize], 0);
    assert_eq!(g.pixels()[(0 * 30 + 0) as usize], 0);
}

#[test]
fn test_draw_polygon() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_color(255, 0, 0, 255);
    let xs = [5, 15, 10];
    let ys = [2, 2, 18];
    g.fill_polygon(&xs, &ys);
    assert_ne!(g.pixels()[(8 * 20 + 10) as usize], 0);
}

#[test]
fn test_draw_polyline() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_color(0, 255, 255, 255);
    let xs = [0, 10, 19];
    let ys = [0, 10, 0];
    g.draw_polyline(&xs, &ys);
    assert_ne!(g.pixels()[(5 * 20 + 5) as usize], 0);
}

#[test]
fn test_clear_rect() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 10, 10);
    g.clear_rect(2, 2, 4, 4);
    assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_FFFFFF);
    assert_eq!(g.pixels()[(0 * 10 + 0) as usize], 0xFF_FF0000);
}

#[test]
fn test_clip_rect() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_clip_rect(5, 5, 5, 5);
    assert_eq!(g.get_clip_bounds(), Some(Rect::new(5, 5, 5, 5)));

    g.set_color(255, 255, 255, 255);
    g.fill_rect(0, 0, 20, 20);

    for y in 0..20 {
        for x in 0..20 {
            let inside = x >= 5 && x < 10 && y >= 5 && y < 10;
            let val = g.pixels()[(y * 20 + x) as usize];
            if inside {
                assert_eq!(val, 0xFF_FFFFFF);
            } else {
                assert_eq!(val, 0);
            }
        }
    }
}

#[test]
fn test_transform_translate() {
    let mut g = Graphics2DState::create(20, 20);
    g.translate(5.0, 5.0);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 3, 3);
    assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
    assert_eq!(g.pixels()[(7 * 20 + 7) as usize], 0xFF_FF0000);
    assert_eq!(g.pixels()[(0 * 20 + 0) as usize], 0);
}

#[test]
fn test_transform_scale() {
    let mut g = Graphics2DState::create(20, 20);
    g.scale(2.0, 2.0);
    g.set_color(0, 255, 0, 255);
    // Logical rect (1,1)-(3,3) => pixel coords at 2x: (2,2),(2,4),(4,2),(4,4)
    g.fill_rect(1, 1, 2, 2);
    assert_eq!(g.pixels()[(2 * 20 + 2) as usize], 0xFF_00FF00);
    assert_eq!(g.pixels()[(4 * 20 + 4) as usize], 0xFF_00FF00);
}

#[test]
fn test_transform_rotate() {
    let mut g = Graphics2DState::create(20, 20);
    g.translate(10.0, 10.0);
    g.rotate(PI / 2.0);
    g.set_color(255, 0, 0, 255);
    // Logical (5, 0) -> rotated -> (0, 5) + translate(10,10) = (10, 15)
    g.fill_rect(5, 0, 1, 1);
    assert_eq!(g.pixels()[(15 * 20 + 10) as usize], 0xFF_FF0000);
}

#[test]
fn test_save_restore() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_color(255, 0, 0, 255);
    g.translate(3.0, 3.0);
    g.save();

    g.set_color(0, 255, 0, 255);
    g.translate(100.0, 100.0);

    g.restore();
    g.fill_rect(0, 0, 1, 1);
    assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_FF0000);
}

#[test]
fn test_dispose_no_ops() {
    let mut g = Graphics2DState::create(10, 10);
    g.dispose();
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 10, 10);
    for p in g.pixels() {
        assert_eq!(*p, 0);
    }
}

#[test]
fn test_linear_gradient_fill() {
    let mut g = Graphics2DState::create(10, 1);
    g.set_paint(Paint::LinearGradient {
        x1: 0.0,
        y1: 0.0,
        x2: 9.0,
        y2: 0.0,
        color1: 0xFF_000000,
        color2: 0xFF_FFFFFF,
        cyclic: false,
    });
    g.fill_rect(0, 0, 10, 1);
    let first = g.pixels()[0];
    let last = g.pixels()[9];
    let first_r = (first >> 16) & 0xFF;
    let last_r = (last >> 16) & 0xFF;
    assert!(
        first_r < 30,
        "first pixel red should be near 0, got {}",
        first_r
    );
    assert!(
        last_r > 225,
        "last pixel red should be near 255, got {}",
        last_r
    );
}

#[test]
fn test_alpha_blending() {
    let mut g = Graphics2DState::create(1, 1);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 1, 1);
    g.set_color(0, 0, 255, 128);
    g.fill_rect(0, 0, 1, 1);
    let p = g.pixels()[0];
    let r = (p >> 16) & 0xFF;
    let b = p & 0xFF;
    assert!(b > r, "blue {} should exceed red {}", b, r);
}

#[test]
fn test_draw_string_produces_pixels() {
    let mut g = Graphics2DState::create(100, 20);
    g.set_color(255, 255, 255, 255);
    g.draw_string("Hello", 5, 15);
    let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
    assert!(drawn > 0, "draw_string should produce some pixels");
}

#[test]
fn test_draw_image() {
    let mut g = Graphics2DState::create(10, 10);
    let img = vec![0xFF_00FF00; 4];
    g.draw_image(&img, 2, 2, 3, 3);
    assert_eq!(g.pixels()[(3 * 10 + 3) as usize], 0xFF_00FF00);
    assert_eq!(g.pixels()[(4 * 10 + 4) as usize], 0xFF_00FF00);
}

#[test]
fn test_draw_image_scaled() {
    let mut g = Graphics2DState::create(20, 20);
    let img = vec![0xFF_FF0000; 4];
    g.draw_image_scaled(&img, 2, 2, 5, 5, 6, 6);
    assert_eq!(g.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
    assert_eq!(g.pixels()[(10 * 20 + 10) as usize], 0xFF_FF0000);
}

#[test]
fn test_copy_area() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_color(0, 0, 255, 255);
    g.fill_rect(0, 0, 3, 3);
    g.copy_area(0, 0, 3, 3, 10, 10);
    assert_eq!(g.pixels()[(10 * 20 + 10) as usize], 0xFF_0000FF);
}

#[test]
fn test_rendering_hint_antialias() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_rendering_hint(RenderingHintKey::Antialiasing, RenderingHintValue::On);
    assert!(g.rendering_hints.antialias);
    g.set_rendering_hint(RenderingHintKey::Antialiasing, RenderingHintValue::Off);
    assert!(!g.rendering_hints.antialias);
}

#[test]
fn test_set_stroke() {
    let mut g = Graphics2DState::create(20, 20);
    g.set_stroke(StrokeSpec {
        width: 3.0,
        cap: CapStyle::Round,
        join: JoinStyle::Bevel,
    });
    assert_eq!(g.stroke.width, 3.0);
    assert_eq!(g.stroke.cap, CapStyle::Round);
    assert_eq!(g.stroke.join, JoinStyle::Bevel);
}

#[test]
fn test_draw_arc() {
    let mut g = Graphics2DState::create(40, 40);
    g.set_color(255, 0, 0, 255);
    g.draw_arc(5, 5, 30, 30, 0, 90);
    let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
    assert!(drawn > 0, "draw_arc should produce pixels");
}

#[test]
fn test_fill_arc() {
    let mut g = Graphics2DState::create(40, 40);
    g.set_color(0, 255, 0, 255);
    g.fill_arc(5, 5, 30, 30, 0, 90);
    let drawn = g.pixels().iter().filter(|&&p| p != 0).count();
    assert!(drawn > 5, "fill_arc should produce many pixels");
}

#[test]
fn test_bilinear_interpolation_scaled_blit() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_rendering_hint(
        RenderingHintKey::Interpolation,
        RenderingHintValue::BilinearInterpolation,
    );
    let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
    g.draw_image_scaled(&src, 2, 2, 0, 0, 4, 4);

    assert_eq!(g.pixels()[0], 0xFF_FF0000);
    assert_eq!(g.pixels()[3], 0xFF_00FF00);
    let mid = g.pixels()[(1 * 10 + 1) as usize];
    let a = (mid >> 24) & 0xFF;
    assert_eq!(a, 255);
    let r = (mid >> 16) & 0xFF;
    assert!(
        r < 255 && r > 0,
        "midpoint red should be blended, got {}",
        r
    );
}

#[test]
fn test_bicubic_interpolation_scaled_blit() {
    // Round-10 PERF Fix 1: bicubic interpolation should produce a
    // fully opaque, blended midpoint distinct from the source corner
    // colours (real 4×4 cubic-convolution, not the old "alias for
    // bilinear" fallback).
    let mut g = Graphics2DState::create(10, 10);
    g.set_rendering_hint(
        RenderingHintKey::Interpolation,
        RenderingHintValue::BicubicInterpolation,
    );
    let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
    g.draw_image_scaled(&src, 2, 2, 0, 0, 4, 4);

    assert_eq!(g.pixels()[0], 0xFF_FF0000);
    let mid = g.pixels()[(1 * 10 + 1) as usize];
    let a = (mid >> 24) & 0xFF;
    // Cubic weights sum to ~1 so a fully-opaque input produces a
    // fully-opaque output after clamping.
    assert_eq!(a, 255);
    let r = (mid >> 16) & 0xFF;
    assert!(
        r < 255 && r > 0,
        "midpoint red should be blended, got {}",
        r
    );
}

#[test]
fn test_negative_dimensions_no_panic() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, -5, -5);
    g.draw_rect(0, 0, -5, -5);
    g.draw_oval(0, 0, -5, -5);
    g.fill_oval(0, 0, -5, -5);
    g.draw_arc(0, 0, -5, -5, 0, 90);
    g.fill_arc(0, 0, -5, -5, 0, 90);
    for p in g.pixels() {
        assert_eq!(*p, 0);
    }
}

#[test]
fn test_set_font() {
    let mut g = Graphics2DState::create(10, 10);
    g.set_font("Monospaced", 1, 14);
    assert_eq!(g.font.family, "Monospaced");
    assert_eq!(g.font.style, 1);
    assert_eq!(g.font.size, 14);
}

#[test]
fn test_affine_concatenate_order() {
    let mut g = Graphics2DState::create(20, 5);
    g.scale(2.0, 1.0);
    g.translate(5.0, 0.0);
    g.set_color(255, 0, 0, 255);
    g.fill_rect(0, 0, 1, 1);
    // Logical (0,0) -> translate -> (5,0) -> scale -> (10,0)
    assert_eq!(g.pixels()[(0 * 20 + 10) as usize], 0xFF_FF0000);
}

#[test]
fn test_affine_invert_roundtrip() {
    let t = AffineTransform::translate(7.0, -3.0);
    let s = AffineTransform::scale(2.0, 0.5);
    let combined = t.concatenate(&s);
    let inv = combined.invert().unwrap();
    let id = combined.concatenate(&inv);
    assert!(
        id.is_identity(),
        "T * T^-1 should be identity, got {:?}",
        id
    );
}
