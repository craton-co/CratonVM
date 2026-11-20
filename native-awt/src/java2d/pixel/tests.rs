use super::*;

#[test]
fn mul8_div8_identities() {
    for v in 0..256 {
        assert_eq!(mul8(255, v), v);
        assert_eq!(mul8(v, 255), v);
        assert_eq!(mul8(0, v), 0);
    }
    // 128/255 of 255 is 128.
    assert_eq!(mul8(128, 255), 128);
    // div8 undoes mul8 at full coverage.
    for v in 0..256 {
        assert_eq!(div8(v, 255), v);
    }
    assert_eq!(div8(10, 0), 0);
}

#[test]
fn div8_ties_match_hotspot() {
    // Measured on HotSpot 25: exact halves round either way depending on
    // how the table's increment rounds.
    assert_eq!(div8(7, 14), 128);
    assert_eq!(div8(11, 22), 127);
    assert_eq!(div8(9, 18), 128);
    assert_eq!(div8(3, 18), 43);
    assert_eq!(div8(13, 26), 127);
}

#[test]
fn srcover_argb_over_argb() {
    // 50% red over opaque blue.
    let mut row = vec![0u8; 4];
    store_raw(
        Fmt::IntArgb,
        &mut row,
        0,
        Raw {
            a: 255,
            r: 0,
            g: 0,
            b: 255,
        },
    );
    let (sa, fg) = prepare_fill_color(Fmt::IntArgb, 0x80ff0000);
    alpha_fill_pixel(Fmt::IntArgb, 3, sa, fg, 0xff, &mut row, 0);
    let p = load_raw(Fmt::IntArgb, &row, 0);
    assert_eq!(p.a, 255);
    assert_eq!(p.r, mul8(0x80, 0xff));
    assert_eq!(p.b, mul8(0xff - 0x80, 0xff));
}

#[test]
fn roundtrip_formats() {
    for f in ALL_FMTS {
        let mut row = vec![0u8; 16];
        store_argb(f, &mut row, 1, 0xff, 0x12, 0x34, 0x56);
        let p = load_argb(f, &row, 1);
        assert_eq!(p.a, 0xff);
        if f == Fmt::ByteGray {
            assert_eq!(p.g, gray_of(0x12, 0x34, 0x56));
        } else if f == Fmt::UshortGray {
            assert_eq!(p.g, gray16_of(0x12, 0x34, 0x56) >> 8);
        } else if matches!(f, Fmt::Ushort565Rgb | Fmt::Ushort555Rgb) {
            assert!((p.r as i32 - 0x12).abs() < 8);
        } else {
            assert_eq!((p.r, p.g, p.b), (0x12, 0x34, 0x56), "{f:?}");
        }
    }
}
