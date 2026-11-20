use super::{bi_java_digit, bi_number_format_message, bi_parse_java, BMP_ND_ZEROS};

fn on() -> bool {
    true
}

#[test]
fn ascii_digits_and_letters_follow_character_digit() {
    assert_eq!(bi_java_digit(u16::from(b'7'), 10), Some(7));
    assert_eq!(bi_java_digit(u16::from(b'z'), 36), Some(35));
    assert_eq!(bi_java_digit(u16::from(b'Z'), 36), Some(35));
    assert_eq!(bi_java_digit(u16::from(b'a'), 10), None);
    assert_eq!(bi_java_digit(u16::from(b'_'), 36), None);
    assert_eq!(bi_parse_java("0012", 10).ok(), Some("12".to_string()));
}

#[test]
fn unicode_decimal_digits_are_digits() {
    if !on() {
        return;
    }
    // Every block: ten consecutive digits valued 0..9, nothing past them.
    for (i, &z) in BMP_ND_ZEROS.iter().enumerate() {
        for k in 0..10u16 {
            assert_eq!(
                bi_java_digit(z + k, 10),
                Some(u32::from(k)),
                "U+{:04X}",
                z + k
            );
        }
        let next = BMP_ND_ZEROS.get(i + 1).copied().unwrap_or(u16::MAX);
        if z + 10 < next {
            assert_eq!(bi_java_digit(z + 10, 36), None, "U+{:04X}", z + 10);
        }
    }
    assert_eq!(
        bi_parse_java("\u{661}\u{662}\u{663}", 10).ok(),
        Some("123".to_string())
    );
    assert_eq!(
        bi_parse_java("-\u{FF10}\u{FF14}\u{FF12}", 10).ok(),
        Some("-42".to_string())
    );
    assert_eq!(
        bi_parse_java("\u{FF11}\u{FF10}", 16).ok(),
        Some("16".to_string())
    );
    // Fullwidth letters are digits 10..35.
    assert_eq!(
        bi_parse_java("\u{FF26}\u{FF46}", 16).ok(),
        Some("255".to_string())
    );
    assert_eq!(bi_java_digit(0xFF26, 15), None);
}

#[test]
fn other_numerics_and_surrogates_are_not_digits() {
    // VULGAR FRACTION ONE FIFTH (No), ROMAN NUMERAL ONE (Nl), SUPERSCRIPT
    // TWO (No), NEW TAI LUE THAM DIGIT ONE (No), a surrogate.
    for c in [0x2155u16, 0x2160, 0x00B2, 0x19DA, 0xD835] {
        assert_eq!(bi_java_digit(c, 36), None, "U+{c:04X}");
    }
    assert!(bi_parse_java("\u{2155}", 10).is_err());
    // MATHEMATICAL BOLD DIGIT ZERO is `Nd` but supplementary: two chars,
    // neither a digit, as in the JDK.
    assert!(bi_parse_java("\u{1D7CE}", 10).is_err());
}

#[test]
fn the_message_group_counts_utf16_units() {
    let digits: Vec<u16> = "1\u{1D7CE}".encode_utf16().collect();
    assert_eq!(digits.len(), 3);
    let m = bi_number_format_message(&digits, 1, 10);
    assert_eq!(m, "For input string: \"1\u{1D7CE}\"");
}
