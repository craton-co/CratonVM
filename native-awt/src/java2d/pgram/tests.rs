use super::*;

#[test]
fn half_covered_pixel_is_127_and_full_is_255() {
    let q = pgram(0.0, 0.0, 1.0, 0.0, 0.0, 0.5);
    assert_eq!(pixel_cov(&q, None, 0, 0), 127);
    let full = pgram(-1.0, -1.0, 3.0, 0.0, 0.0, 3.0);
    assert_eq!(pixel_cov(&full, None, 0, 0), 255);
    assert_eq!(pixel_cov(&full, None, 5, 5), 0);
}

#[test]
fn quarter_corner_matches_hotspot_63() {
    // translate(0.5, 0.5); fillRect(2, 2, ...): the corner pixel is a quarter covered.
    let q = pgram(2.5, 2.5, 7.0, 0.0, 0.0, 4.0);
    assert_eq!(pixel_cov(&q, None, 2, 2), 63);
    assert_eq!(pixel_cov(&q, None, 3, 2), 127);
    assert_eq!(pixel_cov(&q, None, 3, 3), 255);
}
