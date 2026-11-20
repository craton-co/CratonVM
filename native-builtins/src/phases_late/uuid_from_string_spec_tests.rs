use super::{uuid_from_string_bits, uuid_parse_long_hex};

/// Every expectation is a line of `scratchpad/c15/P4Uuid` / `P4b` output on
/// Microsoft OpenJDK 25.0.3+9, transcribed, not remembered.
#[test]
fn lenient_forms_hotspot_accepts() {
    assert_eq!(
        uuid_from_string_bits("1-2-3-4-5").expect("HotSpot accepts this"),
        (0x0000_0001_0002_0003, 0x0004_0000_0000_0005)
    );
    assert_eq!(
        uuid_from_string_bits("0112233-4455-6677-8899-00aabbccddeef").expect("35-char form"),
        (0x0011_2233_4455_6677, 0x8899_0aab_bccd_deefu64 as i64)
    );
    // Groups WIDER than their field are masked, not rejected.
    assert_eq!(
        uuid_from_string_bits("fffffffff-2-3-4-5").expect("9 f's mask to 8"),
        (0xffff_ffff_0002_0003u64 as i64, 0x0004_0000_0000_0005)
    );
    // `Long.parseLong` accepts a leading sign, so `UUID.fromString` does.
    assert_eq!(
        uuid_from_string_bits("+1-2-3-4-5").expect("leading plus"),
        (0x0000_0001_0002_0003, 0x0004_0000_0000_0005)
    );
    // Dash POSITIONS are not checked, only that there are exactly four.
    assert_eq!(
        uuid_from_string_bits("001122334455-6677-8899-0aab-bccddeef")
            .expect("36 chars, misplaced dashes"),
        (0x2233_4455_6677_8899u64 as i64, 0x0aab_0000_bccd_deef)
    );
}

#[test]
fn strict_forms_hotspot_rejects() {
    // The over-LAX half of the same missing spec: the old parser stripped
    // dashes and counted to 32, so both of these were ACCEPTED.
    for bad in [
        "00112233445566778899aabbccddeeff",
        "1-2-3-4-5-6",
        "-1-2-3-4-5",
        "1-2-3-4",
        "",
    ] {
        assert!(
            uuid_from_string_bits(bad).is_err(),
            "HotSpot rejects {bad:?}"
        );
    }
    assert!(uuid_from_string_bits(&"a".repeat(37)).is_err(), "too large");
    // Not silently zero: a non-hex group is a NumberFormatException.
    assert!(uuid_from_string_bits("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz").is_err());
    // An empty group is a NumberFormatException, not an IAE.
    assert!(uuid_from_string_bits("1-2-3-4-").is_err());
}

#[test]
fn parse_long_hex_matches_hotspot_messages() {
    let e = uuid_parse_long_hex(&"0x1".chars().collect::<Vec<_>>()).unwrap_err();
    assert!(
        format!("{e:?}").contains("Error at index 1"),
        "bad digit index, got {e:?}"
    );
    // Overflow is reported one digit EARLY, at 15 of 17.
    let e = uuid_parse_long_hex(&"f".repeat(17).chars().collect::<Vec<_>>()).unwrap_err();
    assert!(
        format!("{e:?}").contains("Error at index 15"),
        "overflow index, got {e:?}"
    );
    // A 16-digit group is fine and wraps to a negative i64.
    assert_eq!(
        uuid_parse_long_hex(&"ffffffffffffffff".chars().collect::<Vec<_>>()).ok(),
        None,
        "16 f's overflow a signed long — HotSpot masks AFTER parseLong, \
         and parseLong itself refuses; UUID's own groups are never that wide \
         except the 12-digit one"
    );
}

/// The mutation check the standing lesson asks for: if the fifth-dash
/// rejection is removed, `strict_forms_hotspot_rejects` must go red. Kept
/// as an assertion on the boundary rather than a comment.
#[test]
fn fifth_dash_is_the_rejecting_rule() {
    assert!(uuid_from_string_bits("1-2-3-4-5").is_ok());
    assert!(uuid_from_string_bits("1-2-3-4-5-6").is_err());
}
