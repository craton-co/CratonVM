use super::*;

#[test]
fn every_rule_neutralizes_its_own_probe() {
    for r in ORDER {
        assert_eq!(
            r.apply(r.probe),
            r.probe_expected,
            "rule {} did not rewrite its own probe",
            r.id
        );
    }
}

/// The other half of the contract: a rule must touch **only** its target.
///
/// `line-endings` is excluded as a *victim* (never as an actor): its probe
/// is CR/LF-shaped, and `str::lines` legitimately drops the `\r`, so
/// `sort-lines` rewrites it for a reason that is not over-normalization.
#[test]
fn no_rule_touches_another_rules_probe() {
    for actor in ORDER {
        for victim in ORDER {
            if actor.id == victim.id || victim.id == LINE_ENDINGS.id {
                continue;
            }
            assert_eq!(
                actor.apply(victim.probe),
                victim.probe,
                "rule {} must not rewrite {}'s probe {:?}",
                actor.id,
                victim.id,
                victim.probe
            );
        }
    }
}

#[test]
fn every_rule_has_a_stated_risk_and_a_unique_id() {
    let mut ids: Vec<&str> = ORDER.iter().map(|r| r.id).collect();
    let count = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), count, "duplicate rule id");
    for r in ORDER {
        assert!(!r.target.is_empty(), "{} has no target", r.id);
        assert!(!r.justification.is_empty(), "{} has no justification", r.id);
        assert!(
            r.risk.len() > 40,
            "{} states no meaningful risk — every rule has one",
            r.id
        );
        assert_eq!(rule(r.id).map(|f| f.id), Some(r.id));
    }
    assert!(rule("no-such-rule").is_none());
}

// -- individual rule edge cases -----------------------------------------

#[test]
fn identity_hash_needs_four_hex_digits_and_a_name() {
    let f = IDENTITY_HASH;
    assert_eq!(f.apply("Foo@1b6d3586"), "Foo@<idhash>");
    // Annotation-looking text and short suffixes are left alone.
    assert_eq!(f.apply("@Override"), "@Override");
    assert_eq!(f.apply("Foo@ab"), "Foo@ab");
    // A bare `@` with no preceding identifier is not an identity hash.
    assert_eq!(f.apply(" @1b6d3586"), " @1b6d3586");
    // Glued identifier text after the hex run is not a hash either.
    assert_eq!(f.apply("Foo@deadzz"), "Foo@deadzz");
}

#[test]
fn hex_address_ignores_identifier_glued_zero_x() {
    let f = HEX_ADDRESS;
    assert_eq!(f.apply("0xff"), "0x<addr>");
    assert_eq!(f.apply("at 0x7f9c1a2b lives"), "at 0x<addr> lives");
    // `a0x1` is not an address literal.
    assert_eq!(f.apply("a0x1"), "a0x1");
    // A lone `0x` with no digits is not one either.
    assert_eq!(f.apply("0x"), "0x");
}

#[test]
fn timestamp_accepts_date_only_and_rejects_longer_numbers() {
    let f = TIMESTAMP;
    assert_eq!(f.apply("2026-07-31"), "<timestamp>");
    assert_eq!(f.apply("2026-07-31 12:00:00"), "<timestamp>");
    assert_eq!(f.apply("12026-07-31"), "12026-07-31");
}

#[test]
fn absolute_path_keeps_the_basename_and_trailing_punctuation() {
    let f = ABSOLUTE_PATH;
    assert_eq!(
        f.apply("at Foo.main(C:\\work\\repo\\Foo.java:12)"),
        "at Foo.main(<path>/Foo.java:12)"
    );
    assert_eq!(f.apply("/tmp/x/y.txt"), "<path>/y.txt");
    // A relative path is not touched.
    assert_eq!(f.apply("src/main/Foo.java"), "src/main/Foo.java");
    // A URL authority is not a drive letter.
    assert_eq!(f.apply("http://host/p"), "http://host/p");
}

#[test]
fn path_separator_only_rewrites_path_shaped_tokens() {
    let f = PATH_SEPARATOR;
    assert_eq!(f.apply("C:\\a\\b"), "C:/a/b");
    assert_eq!(f.apply("x/y\\z"), "x/y/z");
    // A backslash in ordinary prose or a regex without a slash is kept.
    assert_eq!(f.apply("escaped\\n"), "escaped\\n");
}

#[test]
fn frame_line_numbers_needs_digits() {
    let f = FRAME_LINE_NUMBERS;
    assert_eq!(f.apply("Foo.java:1"), "Foo.java:<line>");
    assert_eq!(f.apply("Foo.java:"), "Foo.java:");
    assert_eq!(f.apply("Foo.java"), "Foo.java");
}

#[test]
fn strip_ansi_removes_csi_sequences_only() {
    assert_eq!(strip_ansi("\u{1b}[2mdim\u{1b}[0m"), "dim");
    assert_eq!(strip_ansi("plain"), "plain");
    assert_eq!(strip_ansi("\u{1b}[38;5;196mred\u{1b}[0m!"), "red!");
    // Brackets that aren't escapes stay put.
    assert_eq!(strip_ansi("[cratonvm] ok"), "[cratonvm] ok");
}

#[test]
fn line_endings_normalized() {
    assert_eq!(normalize_line_endings("a\r\nb\r\n"), "a\nb");
    assert_eq!(normalize_line_endings("a\nb\n\n  "), "a\nb");
}
