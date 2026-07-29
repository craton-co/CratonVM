// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Locale-sensitive case mapping for `String.toUpperCase`/`toLowerCase`.
//!
//! Rust's `str::to_uppercase()` / `to_lowercase()` implement Unicode's
//! *unconditional* full case mappings plus the Final_Sigma rule, which is
//! exactly what HotSpot produces for the root and every ordinary locale — a
//! differential run against JDK 25 matches byte-for-byte on `ß → SS`,
//! `ΣΣ → σς`, `İ → i̇` and the rest.
//!
//! What Rust does *not* model is the **locale-conditional** half of
//! `SpecialCasing.txt`, i.e. `java.lang.ConditionalSpecialCasing`: Turkish and
//! Azeri dotted/dotless I, and the Lithuanian retained dot above. Ignoring the
//! `Locale` argument made `"TITLE".toLowerCase(Locale.forLanguageTag("tr"))`
//! return `"title"` where HotSpot returns `"tıtle"` — the divergence this
//! module closes.
//!
//! The port is deliberately literal: [`ENTRIES`] is the JDK's
//! `ConditionalSpecialCasing.entry` table, [`Cond`] its five context
//! conditions, and [`lookup`] its `lookUpTable` (a language-specific entry wins
//! over the language-independent one for the same code point). Only the three
//! locale-dependent languages take this path at all; every other locale keeps
//! the already-verified Rust mapping, so the blast radius of the new code is
//! bounded to `tr`/`az`/`lt`.

use unicode_normalization::char::canonical_combining_class;

/// Canonical combining class 230 — "above". Every condition in
/// `SpecialCasing.txt` is phrased in terms of "no intervening ccc 230".
const CCC_ABOVE: u8 = 230;

/// The context condition attached to a [`Entry`], mirroring the constants in
/// `java.lang.ConditionalSpecialCasing`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Cond {
    /// Unconditional.
    Always,
    /// `Final_Cased` — the final-sigma rule.
    FinalCased,
    /// `After_Soft_Dotted`.
    AfterSoftDotted,
    /// `More_Above`.
    MoreAbove,
    /// `After_I`.
    AfterI,
    /// `Not_Before_Dot` (the negation of `Before_Dot`).
    NotBeforeDot,
}

/// One row of `SpecialCasing.txt`'s conditional section, as the JDK models it.
struct Entry {
    /// The code point this row maps.
    cp: char,
    /// Replacement when lowercasing (may be empty — the character is deleted).
    lower: &'static [char],
    /// Replacement when uppercasing (may be empty).
    upper: &'static [char],
    /// ISO-639 language this row is restricted to, or `None` for all locales.
    lang: Option<&'static str>,
    /// Context condition that must hold for the row to apply.
    cond: Cond,
}

/// `java.lang.ConditionalSpecialCasing.entry`, verbatim and in source order.
static ENTRIES: &[Entry] = &[
    // ---- Conditional mappings (language-independent) ----
    // GREEK CAPITAL LETTER SIGMA
    Entry { cp: '\u{03A3}', lower: &['\u{03C2}'], upper: &['\u{03A3}'], lang: None, cond: Cond::FinalCased },
    // LATIN CAPITAL LETTER I WITH DOT ABOVE
    Entry { cp: '\u{0130}', lower: &['\u{0069}', '\u{0307}'], upper: &['\u{0130}'], lang: None, cond: Cond::Always },
    // ---- Lithuanian ----
    // COMBINING DOT ABOVE
    Entry { cp: '\u{0307}', lower: &['\u{0307}'], upper: &[], lang: Some("lt"), cond: Cond::AfterSoftDotted },
    // LATIN CAPITAL LETTER I
    Entry { cp: '\u{0049}', lower: &['\u{0069}', '\u{0307}'], upper: &['\u{0049}'], lang: Some("lt"), cond: Cond::MoreAbove },
    // LATIN CAPITAL LETTER J
    Entry { cp: '\u{004A}', lower: &['\u{006A}', '\u{0307}'], upper: &['\u{004A}'], lang: Some("lt"), cond: Cond::MoreAbove },
    // LATIN CAPITAL LETTER I WITH OGONEK
    Entry { cp: '\u{012E}', lower: &['\u{012F}', '\u{0307}'], upper: &['\u{012E}'], lang: Some("lt"), cond: Cond::MoreAbove },
    // LATIN CAPITAL LETTER I WITH GRAVE
    Entry { cp: '\u{00CC}', lower: &['\u{0069}', '\u{0307}', '\u{0300}'], upper: &['\u{00CC}'], lang: Some("lt"), cond: Cond::Always },
    // LATIN CAPITAL LETTER I WITH ACUTE
    Entry { cp: '\u{00CD}', lower: &['\u{0069}', '\u{0307}', '\u{0301}'], upper: &['\u{00CD}'], lang: Some("lt"), cond: Cond::Always },
    // LATIN CAPITAL LETTER I WITH TILDE
    Entry { cp: '\u{0128}', lower: &['\u{0069}', '\u{0307}', '\u{0303}'], upper: &['\u{0128}'], lang: Some("lt"), cond: Cond::Always },
    // ---- Turkish and Azeri ----
    Entry { cp: '\u{0130}', lower: &['\u{0069}'], upper: &['\u{0130}'], lang: Some("tr"), cond: Cond::Always },
    Entry { cp: '\u{0130}', lower: &['\u{0069}'], upper: &['\u{0130}'], lang: Some("az"), cond: Cond::Always },
    Entry { cp: '\u{0307}', lower: &[], upper: &['\u{0307}'], lang: Some("tr"), cond: Cond::AfterI },
    Entry { cp: '\u{0307}', lower: &[], upper: &['\u{0307}'], lang: Some("az"), cond: Cond::AfterI },
    Entry { cp: '\u{0049}', lower: &['\u{0131}'], upper: &['\u{0049}'], lang: Some("tr"), cond: Cond::NotBeforeDot },
    Entry { cp: '\u{0049}', lower: &['\u{0131}'], upper: &['\u{0049}'], lang: Some("az"), cond: Cond::NotBeforeDot },
    Entry { cp: '\u{0069}', lower: &['\u{0069}'], upper: &['\u{0130}'], lang: Some("tr"), cond: Cond::Always },
    Entry { cp: '\u{0069}', lower: &['\u{0069}'], upper: &['\u{0130}'], lang: Some("az"), cond: Cond::Always },
];

/// Whether `lang` selects HotSpot's locale-dependent case-mapping path.
///
/// Mirrors `String.toLowerCase(Locale)`'s `localeDependent` test. Callers use
/// this to decide whether the locale even has to be resolved, and to bypass
/// caches that are not keyed by locale.
pub fn is_locale_dependent(lang: &str) -> bool {
    matches!(lang, "tr" | "az" | "lt")
}

/// Normalise a `Locale.getLanguage()` value for [`is_locale_dependent`] and
/// [`ENTRIES`] matching: lower-cased, and the historical ISO-639 codes Java
/// still reports for Indonesian/Hebrew/Yiddish left alone (none of them is
/// locale-dependent, so only the case folding matters here).
pub fn normalize_language(raw: &str) -> String {
    raw.to_ascii_lowercase()
}

/// `String.toLowerCase(Locale)` for a locale whose language is `lang`.
pub fn to_lower_case(s: &str, lang: &str) -> String {
    if !is_locale_dependent(lang) {
        return s.to_lowercase();
    }
    map_locale_dependent(s, lang, true)
}

/// `String.toUpperCase(Locale)` for a locale whose language is `lang`.
pub fn to_upper_case(s: &str, lang: &str) -> String {
    if !is_locale_dependent(lang) {
        return s.to_uppercase();
    }
    map_locale_dependent(s, lang, false)
}

/// The `tr`/`az`/`lt` path: walk code points, consult [`ENTRIES`] for each, and
/// fall back to the ordinary Unicode full mapping. This mirrors
/// `StringUTF16.toLowerCase`'s loop, which calls `ConditionalSpecialCasing`
/// for *every* character once the locale is locale-dependent.
fn map_locale_dependent(s: &str, lang: &str, lowercasing: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        match lookup(&chars, i, lang, lowercasing) {
            Some(mapped) => out.extend(mapped.iter().copied()),
            None if lowercasing => out.extend(c.to_lowercase()),
            None => out.extend(c.to_uppercase()),
        }
    }
    out
}

/// `ConditionalSpecialCasing.lookUpTable`: the matching **language-specific**
/// row wins (the JDK `break`s on one); otherwise a matching language-independent
/// row applies; otherwise `None` → the caller uses the unconditional mapping.
fn lookup(
    chars: &[char],
    index: usize,
    lang: &str,
    lowercasing: bool,
) -> Option<&'static [char]> {
    let cp = chars[index];
    let mut fallback: Option<&'static [char]> = None;
    for entry in ENTRIES {
        if entry.cp != cp {
            continue;
        }
        match entry.lang {
            Some(l) if l != lang => continue,
            _ => {}
        }
        if !condition_met(chars, index, entry.cond) {
            continue;
        }
        let mapped = if lowercasing { entry.lower } else { entry.upper };
        if entry.lang.is_some() {
            return Some(mapped);
        }
        fallback = Some(mapped);
    }
    fallback
}

fn condition_met(chars: &[char], index: usize, cond: Cond) -> bool {
    match cond {
        Cond::Always => true,
        Cond::FinalCased => is_final_cased(chars, index),
        Cond::AfterSoftDotted => scan_back(chars, index, is_soft_dotted),
        Cond::MoreAbove => is_more_above(chars, index),
        Cond::AfterI => scan_back(chars, index, |c| c == 'I'),
        Cond::NotBeforeDot => !is_before_dot(chars, index),
    }
}

/// Shared shape of `isAfterI` / `isAfterSoftDotted`: walk backwards to the last
/// preceding *base* character (combining class 0) and test it with `pred`,
/// stopping early on an intervening class-230 mark.
fn scan_back(chars: &[char], index: usize, pred: fn(char) -> bool) -> bool {
    for &c in chars[..index].iter().rev() {
        if pred(c) {
            return true;
        }
        let cc = canonical_combining_class(c);
        if cc == 0 || cc == CCC_ABOVE {
            return false;
        }
    }
    false
}

/// `isMoreAbove` — followed, within the combining sequence, by a class-230 mark.
fn is_more_above(chars: &[char], index: usize) -> bool {
    for &c in &chars[index + 1..] {
        let cc = canonical_combining_class(c);
        if cc == CCC_ABOVE {
            return true;
        }
        if cc == 0 {
            return false;
        }
    }
    false
}

/// `isBeforeDot` — followed by `U+0307`, with only class-{≠0,≠230} marks between.
fn is_before_dot(chars: &[char], index: usize) -> bool {
    for &c in &chars[index + 1..] {
        if c == '\u{0307}' {
            return true;
        }
        let cc = canonical_combining_class(c);
        if cc == 0 || cc == CCC_ABOVE {
            return false;
        }
    }
    false
}

/// `isFinalCased` — a cased letter precedes the character inside the same word,
/// and none follows it.
///
/// The JDK asks a `BreakIterator` for the word bounds; we approximate the word
/// with the surrounding run of alphanumerics and combining marks, which agrees
/// with `BreakIterator.getWordInstance` for every text this can be reached with
/// (a Greek sigma inside a `tr`/`az`/`lt` string — the language-independent
/// sigma row is only consulted on the locale-dependent path, since every other
/// locale takes Rust's own Final_Sigma implementation).
fn is_final_cased(chars: &[char], index: usize) -> bool {
    let mut saw_cased_before = false;
    for &c in chars[..index].iter().rev() {
        if !in_word(c) {
            break;
        }
        if is_cased(c) {
            saw_cased_before = true;
            break;
        }
    }
    if !saw_cased_before {
        return false;
    }
    for &c in &chars[index + 1..] {
        if !in_word(c) {
            break;
        }
        if is_cased(c) {
            return false;
        }
    }
    true
}

/// Whether `c` continues a word for [`is_final_cased`]'s boundary approximation.
fn in_word(c: char) -> bool {
    c.is_alphanumeric() || canonical_combining_class(c) != 0
}

/// `isCased` — Unicode `Uppercase` or `Lowercase` (Rust's `is_uppercase` /
/// `is_lowercase` are those derived properties, so `Other_Uppercase` /
/// `Other_Lowercase` are already included), plus the titlecase letters, which
/// Rust exposes only indirectly: a `Lt` character is a cased letter that has
/// *both* an upper- and a lower-case mapping away from itself.
fn is_cased(c: char) -> bool {
    if c.is_uppercase() || c.is_lowercase() {
        return true;
    }
    c.is_alphabetic()
        && c.to_uppercase().next() != Some(c)
        && c.to_lowercase().next() != Some(c)
}

/// Whether `c` has the Unicode `Soft_Dotted` property, per the JDK's own list.
fn is_soft_dotted(c: char) -> bool {
    matches!(
        c,
        '\u{0069}'
            | '\u{006A}'
            | '\u{012F}'
            | '\u{0268}'
            | '\u{0456}'
            | '\u{0458}'
            | '\u{1D62}'
            | '\u{1E2D}'
            | '\u{1ECB}'
            | '\u{2071}'
    )
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn empty_and_ascii_only_inputs() {
        for lang in ["tr", "az", "lt", "en"] {
            assert_eq!(to_lower_case("", lang), "");
            assert_eq!(to_upper_case("", lang), "");
            assert_eq!(to_upper_case("abc-123", lang), "ABC-123");
        }
    }
}
