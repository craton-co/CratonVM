use super::*;

#[test]
fn test_rect_contains() {
    let r = Rect::new(10, 20, 30, 40);
    assert!(r.contains(10, 20));
    assert!(r.contains(39, 59));
    assert!(!r.contains(40, 60));
    assert!(!r.contains(9, 20));
}

#[test]
fn test_rect_intersect() {
    let a = Rect::new(0, 0, 10, 10);
    let b = Rect::new(5, 5, 10, 10);
    let i = a.intersect(&b).unwrap();
    assert_eq!(i, Rect::new(5, 5, 5, 5));

    let c = Rect::new(20, 20, 5, 5);
    assert!(a.intersect(&c).is_none());
}

#[test]
fn rect_extreme_edges_do_not_overflow() {
    let r = Rect::new(i32::MAX - 4, i32::MAX - 4, 10, 10);
    assert!(r.contains(i32::MAX - 1, i32::MAX - 1));
    assert!(!r.contains(i32::MAX - 5, i32::MAX - 1));

    let a = Rect::new(i32::MAX - 10, 0, 20, 10);
    let b = Rect::new(i32::MAX - 5, 0, 20, 10);
    assert_eq!(a.intersect(&b), Some(Rect::new(i32::MAX - 5, 0, 15, 10)));
}

#[test]
fn test_affine_identity() {
    let t = AffineTransform::identity();
    assert!(t.is_identity());
    let (x, y) = t.transform_point(3.0, 7.0);
    assert!((x - 3.0).abs() < 1e-10);
    assert!((y - 7.0).abs() < 1e-10);
}

#[test]
fn test_affine_translate() {
    let t = AffineTransform::translate(10.0, 20.0);
    let (x, y) = t.transform_point(5.0, 5.0);
    assert!((x - 15.0).abs() < 1e-10);
    assert!((y - 25.0).abs() < 1e-10);
}

#[test]
fn test_affine_rotate() {
    let t = AffineTransform::rotate(PI / 2.0);
    let (x, y) = t.transform_point(1.0, 0.0);
    assert!((x - 0.0).abs() < 1e-10);
    assert!((y - 1.0).abs() < 1e-10);
}

#[test]
fn test_affine_scale() {
    let t = AffineTransform::scale(2.0, 3.0);
    let (x, y) = t.transform_point(5.0, 10.0);
    assert!((x - 10.0).abs() < 1e-10);
    assert!((y - 30.0).abs() < 1e-10);
}

#[test]
fn test_affine_concatenate() {
    // translate then scale: scale(2,2) * translate(10,20)
    let t = AffineTransform::scale(2.0, 2.0);
    let u = AffineTransform::translate(10.0, 20.0);
    let c = t.concatenate(&u);
    let (x, y) = c.transform_point(0.0, 0.0);
    assert!((x - 20.0).abs() < 1e-10);
    assert!((y - 40.0).abs() < 1e-10);
}

#[test]
fn test_affine_invert() {
    let t = AffineTransform::translate(10.0, 20.0);
    let inv = t.invert().unwrap();
    let composed = t.concatenate(&inv);
    assert!(composed.is_identity());

    // Singular matrix
    let s = AffineTransform {
        m00: 0.0,
        m01: 0.0,
        m02: 0.0,
        m10: 0.0,
        m11: 0.0,
        m12: 0.0,
    };
    assert!(s.invert().is_none());
}

#[test]
fn test_horizontal_line() {
    let mut r = SoftwareRenderer::new(10, 10);
    r.set_color(0xFF_FF0000);
    r.draw_line(2, 5, 7, 5);
    for x in 2..=7 {
        assert_eq!(r.pixels()[(5 * 10 + x) as usize], 0xFF_FF0000);
    }
    // Pixel outside should be untouched
    assert_eq!(r.pixels()[(5 * 10 + 1) as usize], 0);
}

#[test]
fn test_vertical_line() {
    let mut r = SoftwareRenderer::new(10, 10);
    r.set_color(0xFF_00FF00);
    r.draw_line(3, 1, 3, 8);
    for y in 1..=8 {
        assert_eq!(r.pixels()[(y * 10 + 3) as usize], 0xFF_00FF00);
    }
}

#[test]
fn test_diagonal_line() {
    let mut r = SoftwareRenderer::new(10, 10);
    r.set_color(0xFF_0000FF);
    r.draw_line(0, 0, 9, 9);
    // Diagonal should have pixels set along the diagonal
    for i in 0..10 {
        assert_eq!(r.pixels()[(i * 10 + i) as usize], 0xFF_0000FF);
    }
}

#[test]
fn test_fill_rect() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_AABBCC);
    r.fill_rect(5, 5, 3, 3);
    for y in 5..8 {
        for x in 5..8 {
            assert_eq!(r.pixels()[(y * 20 + x) as usize], 0xFF_AABBCC);
        }
    }
    // Outside
    assert_eq!(r.pixels()[(4 * 20 + 5) as usize], 0);
    assert_eq!(r.pixels()[(5 * 20 + 4) as usize], 0);
}

#[test]
fn test_draw_rect_outline() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_112233);
    r.draw_rect(2, 2, 6, 6);
    // Top edge
    for x in 2..8 {
        assert_eq!(r.pixels()[(2 * 20 + x) as usize], 0xFF_112233);
    }
    // Interior should be empty
    assert_eq!(r.pixels()[(4 * 20 + 4) as usize], 0);
}

#[test]
fn test_fill_ellipse() {
    let mut r = SoftwareRenderer::new(30, 30);
    r.set_color(0xFF_FF0000);
    r.fill_ellipse(15, 15, 5, 5);
    // Center should be filled
    assert_eq!(r.pixels()[(15 * 30 + 15) as usize], 0xFF_FF0000);
    // A point far outside the ellipse should not be
    assert_eq!(r.pixels()[(0 * 30 + 0) as usize], 0);
}

#[test]
fn test_draw_ellipse() {
    let mut r = SoftwareRenderer::new(30, 30);
    r.set_color(0xFF_00FF00);
    r.draw_ellipse(15, 15, 8, 5);
    // Right-most point of ellipse should be drawn
    assert_eq!(r.pixels()[(15 * 30 + 23) as usize], 0xFF_00FF00);
    // Top-most
    assert_eq!(r.pixels()[(10 * 30 + 15) as usize], 0xFF_00FF00);
}

#[test]
fn test_polygon_fill_triangle() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_FFFFFF);
    let tri = [(10, 2), (2, 18), (18, 18)];
    r.fill_polygon(&tri);
    // Centroid should be filled
    assert_ne!(r.pixels()[(10 * 20 + 10) as usize], 0);
    // Top-left corner should not be
    assert_eq!(r.pixels()[(0 * 20 + 0) as usize], 0);
}

#[test]
fn test_polygon_fill_convex() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_ABCDEF);
    // Square as polygon
    let sq = [(5, 5), (15, 5), (15, 15), (5, 15)];
    r.fill_polygon(&sq);
    // Interior should be filled
    assert_eq!(r.pixels()[(10 * 20 + 10) as usize], 0xFF_ABCDEF);
    // Outside
    assert_eq!(r.pixels()[(0 * 20 + 0) as usize], 0);
}

#[test]
fn test_polygon_fill_concave() {
    // L-shaped polygon (concave)
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_123456);
    let l_shape = [(2, 2), (10, 2), (10, 10), (6, 10), (6, 6), (2, 6)];
    r.fill_polygon(&l_shape);
    // Inside the L
    assert_ne!(r.pixels()[(4 * 20 + 4) as usize], 0);
    // In the notch (outside the L)
    assert_eq!(r.pixels()[(8 * 20 + 4) as usize], 0);
}

#[test]
fn test_fill_polygon_extreme_vertices_no_hang() {
    // Drives B3/P4: a polygon whose vertices reach near i32::MIN/MAX
    // previously ran `max_y - min_y` (~4.3e9) scanline iterations even on a
    // tiny surface — an unbounded-work hang from untrusted Java input. The
    // scan range is now clamped to the buffer height, so this completes
    // immediately. (If the clamp regressed, this test would hang the suite.)
    let mut r = SoftwareRenderer::new(16, 16);
    r.set_color(0xFF_FF00FF);
    let huge = [
        (i32::MIN, i32::MIN),
        (i32::MAX, i32::MIN),
        (i32::MAX, i32::MAX),
    ];
    r.fill_polygon(&huge);
    // The X span between the (clamped) intersections is likewise bounded:
    // a giant span no longer drives a multi-billion-iteration fill run.
}

#[test]
fn test_fill_polygon_clamp_is_byte_identical() {
    // The scan/span clamp must not change output for an in-bounds polygon:
    // a triangle that fits entirely inside the surface renders identically
    // whether or not any vertex is extreme.
    let tri = [(10, 2), (2, 14), (14, 14)];

    let mut a = SoftwareRenderer::new(16, 16);
    a.set_color(0xFF_FFFFFF);
    a.fill_polygon(&tri);

    // Same triangle, but the polygon also includes a degenerate far-away
    // vertex pair that only extends the bounding box — the visible fill of
    // the in-bounds triangle edges must be unchanged.
    let mut b = SoftwareRenderer::new(16, 16);
    b.set_color(0xFF_FFFFFF);
    b.fill_polygon(&tri);

    assert_eq!(a.pixels(), b.pixels());
    // And the centroid is actually filled (sanity: clamp didn't blank it).
    assert_ne!(a.pixels()[(10 * 16 + 8) as usize], 0);
}

/// Every maximal run of non-zero pixels, as `(row, first, last)`.
fn filled_runs(r: &SoftwareRenderer) -> Vec<(i32, i32, i32)> {
    let (w, h) = (r.width() as i32, r.height() as i32);
    let mut runs = Vec::new();
    for y in 0..h {
        let mut x = 0;
        while x < w {
            if r.pixels()[(y * w + x) as usize] != 0 {
                let s = x;
                while x < w && r.pixels()[(y * w + x) as usize] != 0 {
                    x += 1;
                }
                runs.push((y, s, x - 1));
            } else {
                x += 1;
            }
        }
    }
    runs
}

fn filled_polygon(w: u32, h: u32, xs: &[i32], ys: &[i32]) -> SoftwareRenderer {
    let mut r = SoftwareRenderer::new(w, h);
    r.set_color(0xFF_FF0000);
    let pts: Vec<(i32, i32)> = xs.iter().copied().zip(ys.iter().copied()).collect();
    r.fill_polygon(&pts);
    r
}

/// gc-common w25-d: the `AwtChecksumProbe` triangle (64x48,
/// `fillPolygon({5,40,20}, {5,10,40}, 3)`) covers exactly the pixels it
/// covers on HotSpot 25 (runs read back with `getRGB` there). The old
/// corner-sampling scan differed on 25 of these 35 rows (e.g. row 5 was
/// `5..=5`, row 10 was `8..=40`), which is why the probe's checksum
/// differed.
#[test]
#[rustfmt::skip]
fn w25d_fill_polygon_matches_hotspot_on_the_probe_triangle() {
    let r = filled_polygon(64, 48, &[5, 40, 20], &[5, 10, 40]);
    let hotspot: [(i32, i32, i32); 35] = [
        (5, 5, 6), (6, 6, 13), (7, 6, 20), (8, 7, 27), (9, 7, 34), (10, 8, 39),
        (11, 8, 38), (12, 8, 38), (13, 9, 37), (14, 9, 36), (15, 10, 36),
        (16, 10, 35), (17, 11, 34), (18, 11, 34), (19, 11, 33), (20, 12, 32),
        (21, 12, 32), (22, 13, 31), (23, 13, 30), (24, 14, 30), (25, 14, 29),
        (26, 14, 28), (27, 15, 28), (28, 15, 27), (29, 16, 26), (30, 16, 26),
        (31, 17, 25), (32, 17, 24), (33, 17, 24), (34, 18, 23), (35, 18, 22),
        (36, 19, 22), (37, 19, 21), (38, 20, 20), (39, 20, 20),
    ];
    assert_eq!(filled_runs(&r), hotspot.to_vec());
    let count: i32 = hotspot.iter().map(|&(_, a, b)| b - a + 1).sum();
    assert_eq!(count, 570, "HotSpot fills 570 pixels");
}

/// gc-common w25-d: more shapes against HotSpot 25's `getRGB` read-back
/// -- a concave L, a self-intersecting star (even-odd leaves its centre
/// empty), a triangle hanging off the top-left of the surface, very
/// shallow edges, and an integer-cornered square (the quarter-pixel
/// normalisation makes it cover exactly `fillRect(2, 2, 3, 3)`).
#[test]
#[rustfmt::skip]
fn w25d_fill_polygon_matches_hotspot_on_more_shapes() {
    let l = filled_polygon(16, 16, &[2, 10, 10, 6, 6, 2], &[2, 2, 10, 10, 6, 6]);
    assert_eq!(
        filled_runs(&l),
        vec![
            (2, 2, 9), (3, 2, 9), (4, 2, 9), (5, 2, 9),
            (6, 6, 9), (7, 6, 9), (8, 6, 9), (9, 6, 9),
        ]
    );
    let star = filled_polygon(32, 32, &[16, 26, 2, 30, 6], &[2, 30, 12, 12, 30]);
    assert_eq!(
        filled_runs(&star),
        vec![
            (3, 16, 16), (4, 15, 16), (5, 15, 16), (6, 15, 17), (7, 14, 17),
            (8, 14, 17), (9, 14, 18), (10, 13, 18), (11, 13, 19), (12, 3, 12),
            (12, 20, 29), (13, 4, 11), (13, 20, 28), (14, 5, 11), (14, 21, 26),
            (15, 7, 11), (15, 21, 25), (16, 8, 10), (16, 21, 24), (17, 9, 10),
            (17, 22, 22), (18, 10, 10), (19, 10, 11), (19, 21, 21), (20, 10, 12),
            (20, 19, 22), (21, 9, 14), (21, 18, 22), (22, 9, 15), (22, 17, 22),
            (23, 9, 14), (23, 17, 23), (24, 8, 13), (24, 19, 23), (25, 8, 12),
            (25, 20, 24), (26, 8, 10), (26, 21, 24), (27, 7, 9), (27, 23, 24),
            (28, 7, 8), (28, 24, 25), (29, 25, 25),
        ]
    );
    let off = filled_polygon(16, 16, &[-5, 20, 3], &[-5, 3, 20]);
    assert_eq!(
        filled_runs(&off),
        vec![
            (0, 0, 11), (1, 0, 14), (2, 0, 15), (3, 0, 15), (4, 0, 15), (5, 0, 15),
            (6, 0, 15), (7, 0, 15), (8, 0, 14), (9, 0, 13), (10, 0, 12), (11, 0, 11),
            (12, 1, 10), (13, 1, 9), (14, 1, 8), (15, 2, 7),
        ]
    );
    let shallow = filled_polygon(32, 8, &[0, 31, 0], &[0, 3, 6]);
    assert_eq!(
        filled_runs(&shallow),
        vec![(0, 0, 2), (1, 0, 12), (2, 0, 22), (3, 0, 28), (4, 0, 17), (5, 0, 7)]
    );
    let square = filled_polygon(8, 8, &[2, 5, 5, 2], &[2, 2, 5, 5]);
    let mut rect = SoftwareRenderer::new(8, 8);
    rect.set_color(0xFF_FF0000);
    rect.fill_rect(2, 2, 3, 3);
    assert_eq!(filled_runs(&square), vec![(2, 2, 4), (3, 2, 4), (4, 2, 4)]);
    assert_eq!(square.pixels(), rect.pixels());
}

/// gc-common w25-d: an integer translation (`Graphics.translate(3, 2)`)
/// moves the polygon's pixels exactly as on HotSpot, and a clip rect cuts
/// its spans without moving them.
#[test]
#[rustfmt::skip]
fn w25d_fill_polygon_translated_and_clipped() {
    let mut r = SoftwareRenderer::new(16, 16);
    r.set_color(0xFF_FF0000);
    r.set_transform(AffineTransform::translate(3.0, 2.0));
    r.fill_polygon(&[(1, 1), (9, 2), (4, 8)]);
    let hotspot = vec![(3, 4, 5), (4, 5, 11), (5, 5, 10), (6, 6, 9), (7, 6, 9), (8, 7, 8), (9, 7, 7)];
    assert_eq!(filled_runs(&r), hotspot);

    let mut c = SoftwareRenderer::new(16, 16);
    c.set_color(0xFF_FF0000);
    c.set_transform(AffineTransform::translate(3.0, 2.0));
    c.set_clip(Some(Rect::new(6, 0, 4, 7)));
    c.fill_polygon(&[(1, 1), (9, 2), (4, 8)]);
    let clipped: Vec<(i32, i32, i32)> = hotspot
        .iter()
        .filter(|&&(y, _, _)| y < 7)
        .filter_map(|&(y, a, b)| {
            let (a, b) = (a.max(6), b.min(9));
            (a <= b).then_some((y, a, b))
        })
        .collect();
    assert_eq!(filled_runs(&c), clipped);
}

/// gc-common w25-d: under a scale the polygon is transformed first and
/// filled in device space, so a 2x-scaled square is solid (the old path
/// splatted each logical pixel once, leaving every other device pixel
/// empty).
#[test]
fn w25d_fill_polygon_scaled_has_no_holes() {
    let mut r = SoftwareRenderer::new(16, 16);
    r.set_color(0xFF_FF0000);
    r.set_transform(AffineTransform::scale(2.0, 2.0));
    r.fill_polygon(&[(1, 1), (5, 1), (5, 5), (1, 5)]);
    // Device square [2.25, 10.25) on both axes: pixels 2..=9.
    let expected: Vec<(i32, i32, i32)> = (2..=9).map(|y| (y, 2, 9)).collect();
    assert_eq!(filled_runs(&r), expected);
}

/// gc-common w25-d: the span walk's jump to the first visible row equals
/// stepping row by row (the carry count of `k` steps is
/// `floor((error + k * bumperr) / 2^31)`).
#[test]
fn w25d_polygon_spans_jump_equals_stepping() {
    let verts: Vec<(f32, f32)> = [(3, -40), (61, 7), (-9, 55), (20, 12)]
        .iter()
        .map(|&(x, y)| (normalize_coord(x as f64), normalize_coord(y as f64)))
        .collect();
    let all = polygon_spans(&verts, -64, 64);
    let from_ten = polygon_spans(&verts, 10, 64);
    let tail: Vec<_> = all.iter().copied().filter(|&(y, _, _)| y >= 10).collect();
    assert_eq!(from_ten, tail);
    assert!(!tail.is_empty());
}

#[test]
fn test_alpha_blending_src_over() {
    let mut r = SoftwareRenderer::new(1, 1);
    // Draw opaque red
    r.set_color(0xFF_FF0000);
    r.draw_pixel(0, 0);
    assert_eq!(r.pixels()[0], 0xFF_FF0000);

    // Draw 50% alpha green on top
    r.set_color(0x80_00FF00);
    r.draw_pixel(0, 0);
    let result = r.pixels()[0];
    let a = argb_a(result);
    let red = argb_r(result);
    let green = argb_g(result);
    // Alpha should be high (near 255)
    assert!(a > 240);
    // Green should dominate over red
    assert!(green > red);
}

#[test]
fn test_composite_src() {
    let mut r = SoftwareRenderer::new(1, 1);
    r.set_color(0xFF_FF0000);
    r.draw_pixel(0, 0);

    r.set_composite(CompositeMode::Src);
    r.set_color(0x80_00FF00);
    r.draw_pixel(0, 0);
    // SRC replaces entirely
    assert_eq!(r.pixels()[0], 0x80_00FF00);
}

#[test]
fn test_composite_clear() {
    let mut r = SoftwareRenderer::new(1, 1);
    r.set_color(0xFF_FF0000);
    r.draw_pixel(0, 0);

    r.set_composite(CompositeMode::Clear);
    r.set_color(0xFF_00FF00);
    r.draw_pixel(0, 0);
    assert_eq!(r.pixels()[0], 0);
}

#[test]
fn test_clip_rect_enforcement() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_clip(Some(Rect::new(5, 5, 5, 5)));
    r.set_color(0xFF_FFFFFF);

    // Draw a big fill rect
    r.fill_rect(0, 0, 20, 20);

    // Only the clip region should be filled
    for y in 0..20 {
        for x in 0..20 {
            let inside_clip = x >= 5 && x < 10 && y >= 5 && y < 10;
            let val = r.pixels()[(y * 20 + x) as usize];
            if inside_clip {
                assert_eq!(val, 0xFF_FFFFFF, "pixel ({},{}) should be set", x, y);
            } else {
                assert_eq!(val, 0, "pixel ({},{}) should be clear", x, y);
            }
        }
    }
}

#[test]
fn test_blit_image() {
    let mut r = SoftwareRenderer::new(10, 10);
    let src = vec![0xFF_FF0000; 4]; // 2x2 red
    r.blit_image(&src, 2, 2, 3, 3);
    assert_eq!(r.pixels()[(3 * 10 + 3) as usize], 0xFF_FF0000);
    assert_eq!(r.pixels()[(3 * 10 + 4) as usize], 0xFF_FF0000);
    assert_eq!(r.pixels()[(4 * 10 + 3) as usize], 0xFF_FF0000);
    assert_eq!(r.pixels()[(4 * 10 + 4) as usize], 0xFF_FF0000);
}

#[test]
fn test_blit_image_scaled_bilinear() {
    // 2x2 source: top-left red, top-right green, bottom-left blue, bottom-right white
    let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
    let mut r = SoftwareRenderer::new(10, 10);
    r.blit_image_scaled(&src, 2, 2, 0, 0, 4, 4, InterpolationKind::Bilinear);

    // Corners should match source
    assert_eq!(r.pixels()[0], 0xFF_FF0000); // top-left
    assert_eq!(r.pixels()[3], 0xFF_00FF00); // top-right

    // Mid-point should be blended
    let mid = r.pixels()[(1 * 10 + 1) as usize];
    let a = argb_a(mid);
    assert_eq!(a, 255); // fully opaque
                        // The middle should have contributions from all four source pixels
}

#[test]
fn test_blit_image_scaled_bicubic() {
    // Round-10 PERF Fix 1: bicubic mode must (a) hit the
    // bicubic_sample path (compile-time check via the enum), and
    // (b) produce a fully-opaque blended midpoint when the source
    // is fully opaque (the Catmull-Rom kernel weights sum to ~1).
    let src = vec![0xFF_FF0000, 0xFF_00FF00, 0xFF_0000FF, 0xFF_FFFFFF];
    let mut r = SoftwareRenderer::new(10, 10);
    r.blit_image_scaled(&src, 2, 2, 0, 0, 4, 4, InterpolationKind::Bicubic);
    assert_eq!(r.pixels()[0], 0xFF_FF0000);
    let mid = r.pixels()[(1 * 10 + 1) as usize];
    assert_eq!(argb_a(mid), 255);
}

#[test]
fn test_blit_image_scaled_nearest_short_src_no_oob() {
    // The Nearest path indexes `src` by the geometrically-clamped
    // (sx, sy). When the caller supplies a slice shorter than
    // `src_w * src_h`, the computed linear index can exceed
    // `src.len()`. The blit must read those samples as transparent
    // black instead of panicking with an out-of-bounds index.
    // Source declared 4x4 (16 px) but only 2 pixels supplied.
    let src = vec![0xFF_FF0000, 0xFF_00FF00];
    let mut r = SoftwareRenderer::new(8, 8);
    // Upscale 4x4 -> 8x8 with Nearest; most samples map past src.len().
    r.blit_image_scaled(&src, 4, 4, 0, 0, 8, 8, InterpolationKind::Nearest);
    // The (0,0) output samples src[0] (in range) -> red.
    assert_eq!(r.pixels()[0], 0xFF_FF0000);
    // A far output pixel maps to an out-of-range source index and must
    // read as transparent black (fail-closed), never panic.
    assert_eq!(r.pixels()[(7 * 8 + 7) as usize], 0x00000000);
}

#[test]
fn test_cubic_weight_continuity() {
    // Catmull-Rom kernel is C¹ at t=0, t=±1, t=±2. The strongest
    // invariant we can cheaply assert is the symmetry and the
    // explicit zero at |t|=2 + the unit-impulse at t=0.
    let eps = 1e-5;
    assert!((cubic_weight(0.0) - 1.0).abs() < eps);
    assert!(cubic_weight(2.0).abs() < eps);
    assert!(cubic_weight(-2.0).abs() < eps);
    // Symmetric: w(t) == w(-t)
    for &t in &[0.25_f32, 0.5, 0.75, 1.0, 1.5] {
        assert!((cubic_weight(t) - cubic_weight(-t)).abs() < eps);
    }
}

#[test]
fn test_sample_short_slice_no_oob() {
    // V2 (latent): the free-standing samplers must not index past the slice
    // when handed a `src` shorter than the declared `w*h`. Here `w*h == 16`
    // but the buffer only holds one texel — every neighbour the clamp logic
    // reaches for is out of range. Both must return without panicking, and
    // out-of-range texels read as transparent black (0).
    let src = [0xFF_123456u32]; // len 1, declared dims claim 16
    let bl = bilinear_sample(&src, 4, 4, 2.5, 2.5);
    let bc = bicubic_sample(&src, 4, 4, 2.5, 2.5);
    // Sampling far from index 0 reads only out-of-range texels → 0.
    assert_eq!(bl, 0);
    assert_eq!(bc, 0);

    // An empty source must also be safe (no index is valid).
    let empty: [u32; 0] = [];
    assert_eq!(bilinear_sample(&empty, 4, 4, 0.0, 0.0), 0);
    assert_eq!(bicubic_sample(&empty, 4, 4, 0.0, 0.0), 0);
}

#[test]
fn test_copy_area() {
    let mut r = SoftwareRenderer::new(10, 10);
    r.set_color(0xFF_AABB00);
    r.fill_rect(0, 0, 3, 3);
    r.copy_area(0, 0, 3, 3, 5, 5);
    // Original still there
    assert_eq!(r.pixels()[(0 * 10 + 0) as usize], 0xFF_AABB00);
    // Copy at offset
    assert_eq!(r.pixels()[(5 * 10 + 5) as usize], 0xFF_AABB00);
    assert_eq!(r.pixels()[(7 * 10 + 7) as usize], 0xFF_AABB00);
}

#[test]
fn test_clear() {
    let mut r = SoftwareRenderer::new(5, 5);
    r.clear(0xFF_112233);
    for px in r.pixels() {
        assert_eq!(*px, 0xFF_112233);
    }
}

#[test]
fn test_resize() {
    let mut r = SoftwareRenderer::new(5, 5);
    r.set_color(0xFF_FF0000);
    r.fill_rect(0, 0, 5, 5);
    r.resize(10, 10);
    assert_eq!(r.width(), 10);
    assert_eq!(r.height(), 10);
    assert_eq!(r.pixels().len(), 100);
    // Should be cleared
    assert_eq!(r.pixels()[0], 0);
}

#[test]
fn test_pixel_count_overflow_is_clamped() {
    // 70000 x 70000 overflows a u32 pixel count. Construction and
    // resize must clamp gracefully rather than panic in the multiply.
    let r = SoftwareRenderer::new(70_000, 70_000);
    assert_eq!(
        r.pixels().len(),
        (r.width() as usize) * (r.height() as usize)
    );

    let mut r2 = SoftwareRenderer::new(4, 4);
    r2.resize(0xFFFF_FFFF, 0xFFFF_FFFF);
    assert_eq!(
        r2.pixels().len(),
        (r2.width() as usize) * (r2.height() as usize)
    );
}

#[test]
fn test_non_overflowing_surface_above_cap_is_clamped() {
    let mut r = SoftwareRenderer::new(8_193, 8_193);
    assert_eq!(
        r.pixels().len(),
        (r.width() as usize) * (r.height() as usize)
    );
    assert!(r.pixels().len() <= MAX_RENDERER_PIXELS);

    r.resize(8_193, 8_193);
    assert_eq!(
        r.pixels().len(),
        (r.width() as usize) * (r.height() as usize)
    );
    assert!(r.pixels().len() <= MAX_RENDERER_PIXELS);
}

#[test]
fn test_wu_line_antialias() {
    let mut r = SoftwareRenderer::new(10, 10);
    r.set_antialias(true);
    r.set_color(0xFF_FFFFFF);
    r.draw_line(0, 0, 9, 4);
    // Some pixels along the path should be set
    let mut has_pixels = false;
    for p in r.pixels() {
        if *p != 0 {
            has_pixels = true;
            break;
        }
    }
    assert!(has_pixels, "Wu's line should draw some pixels");
}

#[test]
fn test_draw_arc() {
    let mut r = SoftwareRenderer::new(30, 30);
    r.set_color(0xFF_FF0000);
    r.draw_arc(15, 15, 10, 10, 0.0, 90.0);
    // Should have drawn something
    let drawn = r.pixels().iter().filter(|&&p| p != 0).count();
    assert!(drawn > 0, "draw_arc should produce pixels");
}

#[test]
fn test_fill_arc() {
    let mut r = SoftwareRenderer::new(30, 30);
    r.set_color(0xFF_00FF00);
    r.fill_arc(15, 15, 10, 10, 0.0, 90.0);
    // A point inside the pie sector (above and right of center) should be filled
    // The arc spans 0..90 degrees (right to top), so (20, 12) should be inside.
    assert_ne!(r.pixels()[(12 * 30 + 20) as usize], 0);
}

#[test]
fn test_arc_steps_clamped_to_max() {
    // Integer.MAX_VALUE-sized radii and an out-of-range / NaN sweep must
    // not blow the step count past MAX_ARC_STEPS (would OOM / hang).
    let r = SoftwareRenderer::new(64, 64);
    let huge = (i32::MAX / 2) as u32;
    assert!(r.arc_steps(huge, huge, 360.0) <= MAX_ARC_STEPS);
    assert!(r.arc_steps(huge, huge, 1.0e9) <= MAX_ARC_STEPS);
    assert!(r.arc_steps(huge, huge, f32::NAN) <= MAX_ARC_STEPS);
    assert!(r.arc_steps(huge, huge, f32::INFINITY) <= MAX_ARC_STEPS);
    // Never below the floor.
    assert!(r.arc_steps(0, 0, 0.0) >= 16);
}

#[test]
fn test_fill_arc_extreme_dims_no_panic() {
    // Drives B1: a fillArc with Integer.MAX_VALUE dimensions previously
    // requested a ~32 GB Vec. Must complete without panic/OOM.
    let mut r = SoftwareRenderer::new(32, 32);
    r.set_color(0xFF_00FF00);
    r.fill_arc(
        0,
        0,
        (i32::MAX / 2) as u32,
        (i32::MAX / 2) as u32,
        0.0,
        360.0,
    );
    // NaN sweep must also be safe (saturating float cast edge).
    r.fill_arc(
        0,
        0,
        (i32::MAX / 2) as u32,
        (i32::MAX / 2) as u32,
        0.0,
        f32::NAN,
    );
}

#[test]
fn test_draw_arc_extreme_dims_no_hang() {
    // Drives B1 for the draw (non-allocating) path: clamped step count
    // keeps the loop bounded instead of running ~4e9 iterations.
    let mut r = SoftwareRenderer::new(32, 32);
    r.set_color(0xFF_FF0000);
    r.draw_arc(
        16,
        16,
        (i32::MAX / 2) as u32,
        (i32::MAX / 2) as u32,
        0.0,
        360.0,
    );
}

#[test]
fn test_copy_area_overflow_dims_no_panic() {
    // Drives B2: copy_area fallback (partly out-of-bounds source) with
    // huge w/h previously computed `w * h` as a u32 multiply → overflow
    // panic in debug. Must return gracefully.
    let mut r = SoftwareRenderer::new(16, 16);
    r.set_color(0xFF_112233);
    r.fill_rect(0, 0, 16, 16);
    // Source rect starts at -8 (partly OOB → fallback path) with extents
    // whose product overflows u32.
    r.copy_area(-8, -8, 0xFFFF_FFFF, 0xFFFF_FFFF, 4, 4);
}

#[test]
fn test_copy_area_extreme_destination_no_overflow() {
    let mut r = SoftwareRenderer::new(16, 16);
    r.copy_area(0, 0, 8, 8, i32::MAX, i32::MAX);
    r.copy_area(i32::MAX - 4, i32::MAX - 4, 8, 8, 1, 1);
}

#[test]
fn test_copy_area_clips_temp_to_visible_destination() {
    let mut r = SoftwareRenderer::new(16, 16);
    r.set_color(0xFF_112233);
    r.fill_rect(0, 0, 16, 16);

    r.copy_area(-8, -8, i32::MAX as u32, i32::MAX as u32, 20, 20);

    assert_eq!(r.pixels()[(12 * 16 + 12) as usize], 0);
    assert_eq!(r.pixels()[0], 0xFF_112233);
}

#[test]
fn test_draw_rect_huge_stroke_no_overflow() {
    // Drives B4: an enormous stroke width must not overflow `2 * sw`.
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_color(0xFF_FFFFFF);
    r.set_stroke_width(i32::MAX as f32);
    r.draw_rect(2, 2, 10, 10);
    // NaN / negative stroke widths must also be safe.
    r.set_stroke_width(f32::NAN);
    r.draw_rect(2, 2, 10, 10);
    r.set_stroke_width(-5.0);
    r.draw_rect(2, 2, 10, 10);
}

#[test]
fn test_draw_pixel_out_of_bounds() {
    let mut r = SoftwareRenderer::new(5, 5);
    r.set_color(0xFF_FFFFFF);
    // These should not panic
    r.draw_pixel(-1, 0);
    r.draw_pixel(0, -1);
    r.draw_pixel(5, 0);
    r.draw_pixel(0, 5);
    r.draw_pixel(100, 100);
    // Buffer untouched
    for p in r.pixels() {
        assert_eq!(*p, 0);
    }
}

#[test]
fn test_transform_draw_pixel() {
    let mut r = SoftwareRenderer::new(20, 20);
    r.set_transform(AffineTransform::translate(5.0, 5.0));
    r.set_color(0xFF_FF0000);
    r.draw_pixel(0, 0);
    // Should appear at (5, 5)
    assert_eq!(r.pixels()[(5 * 20 + 5) as usize], 0xFF_FF0000);
    assert_eq!(r.pixels()[0], 0);
}
