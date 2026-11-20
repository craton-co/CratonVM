use super::CODE_POINT_PAIR_OFFSET;

/// The folded constant, the surrogate split and the screens of both bodies,
/// computed in Rust over the boundary values and compared with `char`'s own
/// UTF-16 encoding -- the arithmetic the emitted instructions perform.
#[test]
fn r23_l5_code_point_arithmetic_matches_utf16() {
    let to_code_point = |hi: u32, lo: u32| {
        ((hi << 10).wrapping_add(lo) as i32).wrapping_add(CODE_POINT_PAIR_OFFSET)
    };
    for cp in [0x1_0000u32, 0x1_F600, 0x2_0000, 0xF_FFFF, 0x10_FFFF] {
        let c = char::from_u32(cp).expect("a valid supplementary code point");
        let mut units = [0u16; 2];
        let enc = c.encode_utf16(&mut units);
        assert_eq!(enc.len(), 2);
        let hi = (cp >> 10).wrapping_add(super::super::concat_emit::HIGH_SURROGATE_BIAS as u32);
        let lo = (cp & 0x3FF) + super::super::concat_emit::LOW_SURROGATE_BASE as u32;
        assert_eq!(
            (hi as u16, lo as u16),
            (units[0], units[1]),
            "split {cp:#x}"
        );
        assert_eq!(to_code_point(hi, lo), cp as i32, "join {cp:#x}");
        // The supplementary screen admits it ...
        assert!(cp.wrapping_sub(0x1_0000) < 0x10_0000);
    }
    // ... and refuses everything else a `>>> 16 != 0` code point can be.
    for bad in [0x11_0000u32, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF] {
        assert!(
            bad >> 16 != 0 && bad.wrapping_sub(0x1_0000) >= 0x10_0000,
            "{bad:#x}"
        );
    }
    // The BMP arm takes every `cp >>> 16 == 0`, lone surrogates included
    // (`Character.isBmpCodePoint`), and the LATIN1 screen is `<= 0xFF`
    // unsigned, so a negative code point declines there too.
    for bmp in [0u32, 0xFF, 0x100, 0xD800, 0xDBFF, 0xDC00, 0xFFFF] {
        assert_eq!(bmp >> 16, 0);
    }
    assert!((-1i32) as u32 > 0xFF);
    // The high-surrogate and low-surrogate masks.
    for u in 0u32..=0xFFFF {
        assert_eq!(u & 0xFC00 == 0xD800, (0xD800..=0xDBFF).contains(&u));
        assert_eq!(u & 0xFC00 == 0xDC00, (0xDC00..=0xDFFF).contains(&u));
    }
}

/// The disp8 encodings the bodies use for the array header must fit.
#[test]
fn r23_l5_array_header_offsets_fit_a_disp8() {
    assert!(cratonvm_types::ARRAY_LENGTH_OFFSET <= 127);
    assert!(cratonvm_types::ARRAY_DATA_OFFSET <= 125);
}
