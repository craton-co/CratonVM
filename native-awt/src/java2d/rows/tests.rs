use super::super::pixel::{prepare_fill_color, store_raw, Raw};
use super::*;

#[test]
fn fill_and_convert() {
    let mut a = vec![0u8; 3 * 4];
    fill_row(Fmt::ThreeByteBgr, &mut a, 4, 0x00112233);
    assert_eq!(&a[..3], &[0x33, 0x22, 0x11]);
    let mut b = vec![0u8; 4 * 4];
    convert_row(Fmt::ThreeByteBgr, Fmt::IntArgb, &a, &mut b, 4);
    assert_eq!(&b[..4], &[0x33, 0x22, 0x11, 0xff]);
}

#[test]
fn mask_fill_zero_mask_is_noop() {
    let mut d = vec![0u8; 4];
    store_raw(
        Fmt::IntArgb,
        &mut d,
        0,
        Raw {
            a: 255,
            r: 1,
            g: 2,
            b: 3,
        },
    );
    let (a, fg) = prepare_fill_color(Fmt::IntArgb, 0xffffffff);
    mask_fill_row(Fmt::IntArgb, 3, a, fg, &mut d, Some(&[0]), 1);
    assert_eq!(d, vec![3, 2, 1, 255]);
}
