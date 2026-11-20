use super::*;

// Every expectation below is the verbatim output of HotSpot JDK 25
// (`String.to{Lower,Upper}Case(Locale)`), captured by the `DivLocale`
// differential probe.

#[test]
fn root_locale_is_unchanged_by_the_new_path() {
    for lang in ["", "en", "de", "el", "fr"] {
        assert!(!is_locale_dependent(lang));
        assert_eq!(to_upper_case("straße", lang), "STRASSE");
        assert_eq!(to_lower_case("ΣΣ", lang), "σς");
        assert_eq!(to_upper_case("title", lang), "TITLE");
        assert_eq!(to_lower_case("TITLE", lang), "title");
        assert_eq!(to_lower_case("\u{0130}", lang), "i\u{0307}");
    }
}

#[test]
fn turkish_dotted_and_dotless_i() {
    for lang in ["tr", "az"] {
        assert_eq!(to_upper_case("title", lang), "T\u{0130}TLE");
        assert_eq!(to_lower_case("TITLE", lang), "t\u{0131}tle");
        assert_eq!(to_upper_case("istanbul", lang), "\u{0130}STANBUL");
        assert_eq!(to_lower_case("ISTANBUL", lang), "\u{0131}stanbul");
        assert_eq!(to_upper_case("i", lang), "\u{0130}");
        assert_eq!(to_lower_case("I", lang), "\u{0131}");
        assert_eq!(to_upper_case("I", lang), "I");
        assert_eq!(to_lower_case("i", lang), "i");
        assert_eq!(to_lower_case("\u{0131}", lang), "\u{0131}");
        assert_eq!(to_upper_case("\u{0131}", lang), "I");
    }
}

#[test]
fn turkish_dot_above_context() {
    for lang in ["tr", "az"] {
        // `İ` lowercases to a bare `i` (the JDK's tr/az row wins over the
        // language-independent `i` + combining dot).
        assert_eq!(to_lower_case("\u{0130}", lang), "i");
        assert_eq!(to_upper_case("\u{0130}", lang), "\u{0130}");
        // `I` + COMBINING DOT ABOVE: Not_Before_Dot fails, so `I` takes the
        // ordinary `i`, and the dot is then dropped by After_I.
        assert_eq!(to_lower_case("I\u{0307}", lang), "i");
        assert_eq!(to_upper_case("I\u{0307}", lang), "I\u{0307}");
        // `i` + dot: After_I does NOT hold (the base is lowercase `i`).
        assert_eq!(to_upper_case("i\u{0307}", lang), "\u{0130}\u{0307}");
        assert_eq!(to_lower_case("i\u{0307}", lang), "i\u{0307}");
        // A non-dot accent above keeps `I` dotless.
        assert_eq!(to_lower_case("I\u{0301}", lang), "\u{0131}\u{0301}");
    }
}

#[test]
fn lithuanian_retained_dot() {
    assert_eq!(to_lower_case("\u{00CC}", "lt"), "i\u{0307}\u{0300}");
    assert_eq!(to_lower_case("I\u{0301}", "lt"), "i\u{0307}\u{0301}");
    assert_eq!(to_lower_case("I\u{0307}", "lt"), "i\u{0307}\u{0307}");
    assert_eq!(to_upper_case("i\u{0307}", "lt"), "I");
    // Unconditional cases stay unconditional under `lt`.
    assert_eq!(to_upper_case("title", "lt"), "TITLE");
    assert_eq!(to_lower_case("TITLE", "lt"), "title");
    assert_eq!(to_lower_case("\u{0130}", "lt"), "i\u{0307}");
    assert_eq!(to_lower_case("\u{012E}", "lt"), "\u{012F}");
    assert_eq!(to_upper_case("J", "lt"), "J");
}

#[test]
fn unconditional_mappings_survive_the_locale_path() {
    for lang in ["tr", "az", "lt"] {
        assert_eq!(to_upper_case("straße", lang), "STRASSE");
        assert_eq!(to_lower_case("straße", lang), "straße");
        // The language-independent Final_Cased row still applies.
        assert_eq!(to_lower_case("ΣΣ", lang), "σς");
        assert_eq!(to_upper_case("ΣΣ", lang), "ΣΣ");
    }
}

#[test]
fn language_is_normalized_case_insensitively() {
    assert!(is_locale_dependent(&normalize_language("TR")));
    assert!(is_locale_dependent(&normalize_language("Az")));
    assert!(!is_locale_dependent(&normalize_language("EN")));
}

/// Every member of [`JDK_UNMAPPED_CASE_RUNS`], through the LOCALE path.
///
/// This is the site the earlier fix in `lang_string.rs` could not reach:
/// `string_case_impl` tests `is_locale_dependent` first and hands `tr`,
/// `az` and `lt` to this module, whose fallback arms called Rust's mapping
/// directly. Every expectation is the verbatim OpenJDK 25.0.3+9 answer.
///
/// The loop is over the runs rather than over six literals since G9-1: the
/// measured set is 56 code points and 50 of them are supplementary, so a
/// `u16`-shaped test would go on passing while proving nothing about them.
#[test]
fn the_jdk_unmapped_code_points_are_identity_in_every_locale() {
    let mut seen = 0usize;
    for (lo, hi) in JDK_UNMAPPED_CASE_RUNS {
        for cp in lo..=hi {
            seen += 1;
            let c = char::from_u32(cp).expect("scalar value");
            let s = c.to_string();
            for lang in ["tr", "az", "lt", "en", "el", ""] {
                assert_eq!(
                    to_upper_case(&s, lang),
                    s,
                    "toUpperCase(U+{cp:04X}, {lang})"
                );
                assert_eq!(
                    to_lower_case(&s, lang),
                    s,
                    "toLowerCase(U+{cp:04X}, {lang})"
                );
            }
            // ... and embedded in a string the locale path actually rewrites.
            for lang in ["tr", "az"] {
                assert_eq!(
                    to_upper_case(&format!("i{c}i"), lang),
                    format!("\u{0130}{c}\u{0130}")
                );
                assert_eq!(
                    to_lower_case(&format!("I{c}I"), lang),
                    format!("\u{0131}{c}\u{0131}")
                );
            }
        }
    }
    assert_eq!(seen, 56, "the measured skew set is 56 code points");
}

/// The same 56, through the LOCALE-INDEPENDENT entry points — which is the
/// path `String.toUpperCase()` with no argument takes.
///
/// Each expectation is HotSpot's: identity. Before G9-1 the 50
/// supplementary members answered Rust's pairing instead — measured
/// `("\u{16EBB}").toUpperCase()` was `U+16EA0` where OpenJDK 25.0.3+9
/// returns `U+16EBB` unchanged.
#[test]
fn the_supplementary_half_of_the_skew_is_identity_without_a_locale() {
    for (lo, hi) in JDK_UNMAPPED_CASE_RUNS {
        for cp in lo..=hi {
            let s = char::from_u32(cp).unwrap().to_string();
            assert_eq!(jdk_to_uppercase(&s), s, "jdk_to_uppercase(U+{cp:04X})");
            assert_eq!(jdk_to_lowercase(&s), s, "jdk_to_lowercase(U+{cp:04X})");
            assert!(is_jdk_unmapped_case_code_point(cp), "U+{cp:04X}");
        }
    }
    // The block's own neighbours are NOT in it — the runs are measured
    // boundaries, not a bracket around the interesting region.
    for cp in [
        0xA7CDu32, 0xA7D0, 0xA7D1, 0xA7D6, 0x16E9F, 0x16EB9, 0x16EBA, 0x16ED4,
    ] {
        assert!(
            !is_jdk_unmapped_case_code_point(cp),
            "U+{cp:04X} is not skewed"
        );
    }
}

/// The character-wise arm of [`jdk_to_lowercase`] must keep Final_Sigma.
///
/// Regression for the downgrade G9-1 found: the arm runs whenever the
/// string contains ANY skewed code point, and `char::to_lowercase` has no
/// context, so every sigma in such a string came back medial. Expectations
/// measured on OpenJDK 25.0.3+9, printed as code units.
#[test]
fn the_character_wise_arm_still_finds_the_final_sigma() {
    // [97, 962, 42958] on HotSpot; this arm used to answer 963 in the middle.
    assert_eq!(jdk_to_lowercase("A\u{03A3}\u{A7CE}"), "a\u{03C2}\u{A7CE}");
    assert_eq!(jdk_to_lowercase("A\u{03A3}\u{16EA0}"), "a\u{03C2}\u{16EA0}");
    // A7D3 IS a cased letter on JDK 25, so the sigma stays MEDIAL — the
    // control that says this is Final_Cased and not "always final".
    assert_eq!(jdk_to_lowercase("A\u{03A3}\u{A7D3}"), "a\u{03C3}\u{A7D3}");
    // A cased letter after the sigma keeps it medial even with a skewed
    // code point elsewhere in the string.
    assert_eq!(jdk_to_lowercase("\u{A7CE}A\u{03A3}A"), "\u{A7CE}a\u{03C3}a");
    // No skewed code point at all: the bulk path, unchanged.
    assert_eq!(jdk_to_lowercase("A\u{03A3}"), "a\u{03C2}");
    assert_eq!(jdk_to_lowercase("\u{03A3}\u{03A3}"), "\u{03C3}\u{03C2}");
}

/// The neighbours that ARE case pairs on OpenJDK 25 — the control that says
/// the arm is an enumeration and not a range.
#[test]
fn the_neighbouring_code_points_still_case_pair() {
    for lang in ["tr", "az", "lt", "en"] {
        assert_eq!(to_upper_case("\u{A7D1}", lang), "\u{A7D0}");
        assert_eq!(to_lower_case("\u{A7D0}", lang), "\u{A7D1}");
        assert_eq!(to_upper_case("\u{A7D7}", lang), "\u{A7D6}");
        assert_eq!(to_lower_case("\u{A7D6}", lang), "\u{A7D7}");
    }
}

/// `Final_Cased` is where the four UNASSIGNED code points and the two
/// assigned ones part company, and it is the only observable that reaches
/// [`is_cased`]. Measured: `"A" + U+03A3 + X` lowercased.
#[test]
fn final_sigma_sees_a7d3_and_a7d5_as_cased_and_the_other_four_as_not() {
    for lang in ["tr", "az", "lt"] {
        // Unassigned in the JDK → the sigma is FINAL.
        for (lo, hi) in JDK_UNASSIGNED_CASE_RUNS {
            for cp in lo..=hi {
                let c = char::from_u32(cp).unwrap();
                assert_eq!(
                    to_lower_case(&format!("A\u{03A3}{c}"), lang),
                    format!("a\u{03C2}{c}"),
                    "U+{cp:04X} must not count as a cased letter after the sigma"
                );
            }
        }
        // Assigned lowercase letters in the JDK → the sigma is MEDIAL.
        for c in ['\u{A7D3}', '\u{A7D5}'] {
            assert_eq!(
                to_lower_case(&format!("A\u{03A3}{c}"), lang),
                format!("a\u{03C3}{c}"),
                "{c:?} IS cased on OpenJDK 25 and must keep the sigma medial"
            );
        }
        // Controls at both ends.
        assert_eq!(to_lower_case(&format!("A\u{03A3}a"), lang), "a\u{03C3}a");
        assert_eq!(to_lower_case(&format!("A\u{03A3}."), lang), "a\u{03C2}.");
        assert_eq!(
            to_lower_case(&format!("A\u{03A3}\u{A7D1}"), lang),
            "a\u{03C3}\u{A7D1}"
        );
    }
}

#[test]
fn empty_and_ascii_only_inputs() {
    for lang in ["tr", "az", "lt", "en"] {
        assert_eq!(to_lower_case("", lang), "");
        assert_eq!(to_upper_case("", lang), "");
        assert_eq!(to_upper_case("abc-123", lang), "ABC-123");
    }
}
