use super::*;

/// Regression test for the C30 sign-extension fix in `lparam_xy`. The
/// old implementation masked LOWORD/HIWORD with `0xFFFF` and produced
/// `(0, 65534)` for an LPARAM packing `(0, -2)`; mouse drags outside
/// the client rect were therefore mis-reported as positions in the
/// 32_768..65_535 range. The fixed version widens through `i16` so the
/// sign bit reaches `i32`.
#[test]
fn lparam_xy_sign_extends_negative_coords() {
    // LPARAM packing x = 0, y = -2 (i16) → bits 0xFFFE0000.
    let lp = LPARAM(0xFFFE0000u32 as isize);
    assert_eq!(Win32Backend::lparam_xy(lp), (0, -2));

    // LPARAM packing x = -1, y = -1 → bits 0xFFFFFFFF.
    let lp = LPARAM(0xFFFFFFFFu32 as isize);
    assert_eq!(Win32Backend::lparam_xy(lp), (-1, -1));

    // Positive coordinates should be unchanged.
    let lp = LPARAM(((100i32 as u32) | ((200u32) << 16)) as isize);
    assert_eq!(Win32Backend::lparam_xy(lp), (100, 200));

    // The most-negative coordinate the i16 encoding can carry.
    let lp = LPARAM(0x80008000u32 as isize);
    assert_eq!(Win32Backend::lparam_xy(lp), (-32768, -32768));
}
