// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//!
//! # "Unicode's rules" is the wrong target — the JDK's are
//!
//! Every sentence above that says this module implements *Unicode's*
//! unconditional mappings names the defect it shipped with. The VM must answer
//! what **the JDK on this image** answers, and Rust's `char` tables are a
//! DIFFERENT, NEWER Unicode than the JDK's. Where the two disagree, Unicode is
//! not the oracle. [`JDK_UNMAPPED_CASE_RUNS`] below is the measured
//! disagreement, and it is now the single definition shared with
//! `lang_string.rs` rather than a copy per call site — this module had three
//! sites (both entry points and the `map_locale_dependent` fallback arms) plus
//! a fourth in [`is_cased`], all of which bypassed the fix that already existed
//! next door.

use unicode_normalization::char::canonical_combining_class;

/// Canonical combining class 230 — "above". Every condition in
/// `SpecialCasing.txt` is phrased in terms of "no intervening ccc 230".
const CCC_ABOVE: u8 = 230;

// ---------------------------------------------------------------------------
// The JDK-vs-Rust Unicode version skew. ONE definition, used by this module and
// by `lang_string.rs`.
// ---------------------------------------------------------------------------

/// Code points whose case mapping **Rust has and the JDK does not**.
///
/// Not a Unicode subtlety — a VERSION SKEW, and the reason neither this module
/// nor `lang_string.rs` derives a case answer from Rust's `char` methods
/// without going through [`is_jdk_unmapped_case_code_point`] first.
///
/// Measured on Microsoft OpenJDK 25.0.3+9, which is on Unicode 16:
///
/// ```text
///          Character.toUpperCase  toLowerCase  isDefined  getType
/// U+A7CE   U+A7CE                 U+A7CE       false      0 (UNASSIGNED)
/// U+A7CF   U+A7CF                 U+A7CF       false      0
/// U+A7D2   U+A7D2                 U+A7D2       false      0
/// U+A7D3   U+A7D3                 U+A7D3       true       2 (LOWERCASE_LETTER)
/// U+A7D4   U+A7D4                 U+A7D4       false      0
/// U+A7D5   U+A7D5                 U+A7D5       true       2 (LOWERCASE_LETTER)
/// ```
///
/// All six map to THEMSELVES, for `Character.toUpperCase`/`toLowerCase`, for
/// `String.toUpperCase`/`toLowerCase` in **every** locale — the `tr`, `az` and
/// `lt` columns were measured separately and agree — and for
/// `String.regionMatches(true,…)` / `equalsIgnoreCase`. Rust's tables are newer
/// and pair them (`A7CF`/`A7CE`, `A7D3`/`A7D2`, `A7D5`/`A7D4`), so every one of
/// those answers came back wrong.
///
/// Note the two shapes, because the *mapping* fix is the same arm for both and
/// the *property* fix is not: four of those six are UNASSIGNED in the JDK's
/// Unicode version, while `A7D3` (LATIN SMALL LETTER DOUBLE THORN) and `A7D5`
/// (LATIN SMALL LETTER DOUBLE WYNN) are assigned lowercase letters that simply
/// have no uppercase partner yet — a newer Unicode added the capitals. See
/// [`JDK_UNASSIGNED_CASE_RUNS`] for where the two shapes part company.
///
/// How this was missed the first time, since the method looked exhaustive: the
/// original exception table was derived by dumping all 65,536 BMP code units
/// from the JDK and diffing them against **Python's** `str.upper()`/`lower()`
/// as a stand-in for Rust's. Python here is on UCD 16.0.0 and agrees with the
/// JDK at all six, so the diff was empty and reported success. The Java side of
/// that measurement was real; the Rust side was a proxy that was never
/// validated as one. `[setup lies]` — an exhaustive sweep against the wrong
/// oracle is still exhaustive.
///
/// # The list was six code points and the measured set is fifty-six
///
/// The paragraph above says the Rust side had never been executed. G9-1
/// re-derived it by executing Rust's own `char::to_{upper,lower}case` (rustc
/// 1.97.1, the toolchain this crate builds with) and
/// `String.to{Upper,Lower}Case(Locale.ROOT)` on OpenJDK 25.0.3+9 over **all
/// 1,112,064 Unicode scalar values** and classifying every disagreement:
///
/// ```text
///   Rust has a mapping, JDK 25 has NONE   56   <- this table
///   JDK has a mapping, Rust has none       0
///   both map, to different things          0
/// ```
///
/// The previous form could not have found the other fifty: it was `[u16; 6]`
/// behind a `cp <= 0xFFFF` guard, so every supplementary code point was
/// STRUCTURALLY out of reach. `U+16EA0..U+16EB8` and `U+16EBB..U+16ED3` are a
/// case pair block that Rust's Unicode knows (delta `+0x1B`) and JDK 25 does
/// not assign at all — measured `getType == 0`, `isDefined == false` for the
/// whole of `U+16E9B..U+16EDF`. Same shape as the `A7Cx` rows, one plane up.
///
/// Runs, not a flat list, because these are now contiguous spans — but the
/// spans are the MEASURED ones and are still not bracketable by hand:
/// `U+A7D0`/`U+A7D1` and `U+A7D6`/`U+A7D7` sit inside the same `A7Cx` region
/// and ARE case pairs in the JDK, and `U+16EB9`/`U+16EBA` split the
/// supplementary block in two.
pub const JDK_UNMAPPED_CASE_RUNS: [(u32, u32); 4] = [
    (0xA7CE, 0xA7CF),
    (0xA7D2, 0xA7D5),
    (0x16EA0, 0x16EB8),
    (0x16EBB, 0x16ED3),
];

/// The subset of [`JDK_UNMAPPED_CASE_RUNS`] that the JDK does not assign at all
/// (`Character.isDefined == false`, `getType == 0`).
///
/// These differ from `A7D3`/`A7D5` in every property, not just in the mapping:
/// the JDK reports them as uncased non-letters where Rust's newer tables report
/// a cased letter. That distinction is observable through `Final_Cased`, and it
/// was measured rather than reasoned — lowercasing `"A" + U+03A3 + X` on
/// OpenJDK 25.0.3+9 gives the FINAL sigma `U+03C2` when `X` is one of these (so
/// the JDK sees no cased letter after the sigma) and the medial `U+03C3` when
/// `X` is `A7D3` or `A7D5`. Identical rows for `Locale.ROOT`, `tr` and `lt`.
///
/// G9-1 extended this the same way as its parent: the whole of
/// `U+16EA0..U+16EB8` and `U+16EBB..U+16ED3` is `getType == 0` on JDK 25, so
/// all fifty join the four. Only `A7D3` and `A7D5` are unmapped-but-assigned.
///
/// This is deliberately NOT a general "is this code point assigned in the JDK"
/// predicate — that question is thousands of code points wide and cannot be
/// answered from Rust's tables at all. It is exactly the set this file already
/// had to enumerate for the mapping, minus the two the JDK does assign.
const JDK_UNASSIGNED_CASE_RUNS: [(u32, u32); 4] = [
    (0xA7CE, 0xA7CF),
    (0xA7D2, 0xA7D2),
    (0xA7D4, 0xA7D4),
    (0x16EA0, 0x16ED3),
];

/// Whether a code point falls in one of [`JDK_UNMAPPED_CASE_RUNS`].
///
/// Takes the FULL `u32` code-point domain. The `cp <= 0xFFFF` guard this
/// replaced was not a fast path, it was the bug: fifty of the fifty-six
/// members are supplementary.
#[inline]
pub fn is_jdk_unmapped_case_code_point(cp: u32) -> bool {
    JDK_UNMAPPED_CASE_RUNS
        .iter()
        .any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// Whether `c` falls in one of [`JDK_UNASSIGNED_CASE_RUNS`] — a character
/// Rust's tables know as a cased letter and the JDK does not know at all.
#[inline]
fn is_jdk_unassigned(c: char) -> bool {
    let cp = u32::from(c);
    JDK_UNASSIGNED_CASE_RUNS
        .iter()
        .any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// One character's **full** uppercase mapping as the JDK produces it, appended
/// to `out`.
///
/// `char::to_uppercase` is the right primitive — `String.toUpperCase()` really
/// does answer `"SS"` for a sharp s, and only `Character.toUpperCase(char)` is
/// the 1:1 mapping — with the version-skew arm in front of it.
#[inline]
pub fn push_jdk_upper(out: &mut String, c: char) {
    if is_jdk_unmapped_case_code_point(u32::from(c)) {
        out.push(c);
    } else {
        out.extend(c.to_uppercase());
    }
}

/// [`push_jdk_upper`]'s lowercase twin.
#[inline]
pub fn push_jdk_lower(out: &mut String, c: char) {
    if is_jdk_unmapped_case_code_point(u32::from(c)) {
        out.push(c);
    } else {
        out.extend(c.to_lowercase());
    }
}

/// `String.toUpperCase()`'s locale-independent full mapping, JDK-correct.
///
/// The scan for a skewed code point is a cheap early-out: every entry in
/// [`JDK_UNMAPPED_CASE_RUNS`] is above `U+A7CD`, so essentially all real text
/// keeps the single bulk `str::to_uppercase` call.
///
/// Uppercasing has no context-dependent rule on either side, so walking
/// character by character here is exactly `str::to_uppercase` with the skew arm
/// spliced in. Its lowercase twin below is NOT in that position — see there.
pub fn jdk_to_uppercase(s: &str) -> String {
    if !s
        .chars()
        .any(|c| is_jdk_unmapped_case_code_point(u32::from(c)))
    {
        return s.to_uppercase();
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        push_jdk_upper(&mut out, c);
    }
    out
}

/// [`jdk_to_uppercase`]'s lowercase twin — and NOT its mirror image, because
/// lowercasing has one context-dependent rule and uppercasing has none.
///
/// `str::to_lowercase` implements Final_Sigma; `char::to_lowercase` cannot,
/// because a lone `char` has no context. So the character-wise arm below was a
/// *silent downgrade*: any string that merely CONTAINED a skewed code point
/// lost the final sigma everywhere else in it. Measured on OpenJDK 25.0.3+9,
/// printed as code units:
///
/// ```text
///   ("A" + U+03A3 + U+A7CE).toLowerCase()   HotSpot [97, 962, 42958]
///                                           was     [97, 963, 42958]
/// ```
///
/// `962` is `U+03C2` FINAL SIGMA and `963` is the medial `U+03C3`. The bug was
/// invisible while the trigger set was six code points nobody types; G9-1's
/// measurement grew that set to fifty-six, which is what made it worth closing
/// rather than widening.
///
/// [`is_final_cased`] is the JDK's own `ConditionalSpecialCasing` rule, already
/// in this file for the `tr`/`az`/`lt` path — so this arm now answers the same
/// way the locale-dependent arm does, which it did not before. It is not
/// Rust's rule and does not claim to be: Rust skips Case_Ignorable characters
/// and consults the live `Cased` property, while the JDK stops at a word
/// boundary and consults a frozen list. Those two disagree on **259** code
/// points (measured, G9-1); the fast path above still takes Rust's answer for
/// every string without a skewed code point in it, so this change moves no row
/// that was previously right.
pub fn jdk_to_lowercase(s: &str) -> String {
    if !s
        .chars()
        .any(|c| is_jdk_unmapped_case_code_point(u32::from(c)))
    {
        return s.to_lowercase();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == '\u{03A3}' && is_final_cased(&chars, i) {
            out.push('\u{03C2}');
        } else {
            push_jdk_lower(&mut out, c);
        }
    }
    out
}

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
    Entry {
        cp: '\u{03A3}',
        lower: &['\u{03C2}'],
        upper: &['\u{03A3}'],
        lang: None,
        cond: Cond::FinalCased,
    },
    // LATIN CAPITAL LETTER I WITH DOT ABOVE
    Entry {
        cp: '\u{0130}',
        lower: &['\u{0069}', '\u{0307}'],
        upper: &['\u{0130}'],
        lang: None,
        cond: Cond::Always,
    },
    // ---- Lithuanian ----
    // COMBINING DOT ABOVE
    Entry {
        cp: '\u{0307}',
        lower: &['\u{0307}'],
        upper: &[],
        lang: Some("lt"),
        cond: Cond::AfterSoftDotted,
    },
    // LATIN CAPITAL LETTER I
    Entry {
        cp: '\u{0049}',
        lower: &['\u{0069}', '\u{0307}'],
        upper: &['\u{0049}'],
        lang: Some("lt"),
        cond: Cond::MoreAbove,
    },
    // LATIN CAPITAL LETTER J
    Entry {
        cp: '\u{004A}',
        lower: &['\u{006A}', '\u{0307}'],
        upper: &['\u{004A}'],
        lang: Some("lt"),
        cond: Cond::MoreAbove,
    },
    // LATIN CAPITAL LETTER I WITH OGONEK
    Entry {
        cp: '\u{012E}',
        lower: &['\u{012F}', '\u{0307}'],
        upper: &['\u{012E}'],
        lang: Some("lt"),
        cond: Cond::MoreAbove,
    },
    // LATIN CAPITAL LETTER I WITH GRAVE
    Entry {
        cp: '\u{00CC}',
        lower: &['\u{0069}', '\u{0307}', '\u{0300}'],
        upper: &['\u{00CC}'],
        lang: Some("lt"),
        cond: Cond::Always,
    },
    // LATIN CAPITAL LETTER I WITH ACUTE
    Entry {
        cp: '\u{00CD}',
        lower: &['\u{0069}', '\u{0307}', '\u{0301}'],
        upper: &['\u{00CD}'],
        lang: Some("lt"),
        cond: Cond::Always,
    },
    // LATIN CAPITAL LETTER I WITH TILDE
    Entry {
        cp: '\u{0128}',
        lower: &['\u{0069}', '\u{0307}', '\u{0303}'],
        upper: &['\u{0128}'],
        lang: Some("lt"),
        cond: Cond::Always,
    },
    // ---- Turkish and Azeri ----
    Entry {
        cp: '\u{0130}',
        lower: &['\u{0069}'],
        upper: &['\u{0130}'],
        lang: Some("tr"),
        cond: Cond::Always,
    },
    Entry {
        cp: '\u{0130}',
        lower: &['\u{0069}'],
        upper: &['\u{0130}'],
        lang: Some("az"),
        cond: Cond::Always,
    },
    Entry {
        cp: '\u{0307}',
        lower: &[],
        upper: &['\u{0307}'],
        lang: Some("tr"),
        cond: Cond::AfterI,
    },
    Entry {
        cp: '\u{0307}',
        lower: &[],
        upper: &['\u{0307}'],
        lang: Some("az"),
        cond: Cond::AfterI,
    },
    Entry {
        cp: '\u{0049}',
        lower: &['\u{0131}'],
        upper: &['\u{0049}'],
        lang: Some("tr"),
        cond: Cond::NotBeforeDot,
    },
    Entry {
        cp: '\u{0049}',
        lower: &['\u{0131}'],
        upper: &['\u{0049}'],
        lang: Some("az"),
        cond: Cond::NotBeforeDot,
    },
    Entry {
        cp: '\u{0069}',
        lower: &['\u{0069}'],
        upper: &['\u{0130}'],
        lang: Some("tr"),
        cond: Cond::Always,
    },
    Entry {
        cp: '\u{0069}',
        lower: &['\u{0069}'],
        upper: &['\u{0130}'],
        lang: Some("az"),
        cond: Cond::Always,
    },
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
        return jdk_to_lowercase(s);
    }
    map_locale_dependent(s, lang, true)
}

/// `String.toUpperCase(Locale)` for a locale whose language is `lang`.
pub fn to_upper_case(s: &str, lang: &str) -> String {
    if !is_locale_dependent(lang) {
        return jdk_to_uppercase(s);
    }
    map_locale_dependent(s, lang, false)
}

/// The `tr`/`az`/`lt` path: walk code points, consult [`ENTRIES`] for each, and
/// fall back to the ordinary full mapping. This mirrors
/// `StringUTF16.toLowerCase`'s loop, which calls `ConditionalSpecialCasing`
/// for *every* character once the locale is locale-dependent.
///
/// The fallback arms go through [`push_jdk_lower`]/[`push_jdk_upper`], not
/// `char::to_{lower,upper}case` directly. `lang_string.rs` guards its own
/// non-locale path against [`JDK_UNMAPPED_CASE_RUNS`] and then calls
/// straight into here for `tr`/`az`/`lt`, so a bare Rust mapping on this line
/// meant `"ꟓ".toUpperCase(Locale.forLanguageTag("tr"))` was still wrong while
/// the same string with `Locale.ROOT` was right — the fix next door was
/// reachable only by the locales that did not take this branch.
fn map_locale_dependent(s: &str, lang: &str, lowercasing: bool) -> String {
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    for (i, &c) in chars.iter().enumerate() {
        match lookup(&chars, i, lang, lowercasing) {
            Some(mapped) => out.extend(mapped.iter().copied()),
            None if lowercasing => push_jdk_lower(&mut out, c),
            None => push_jdk_upper(&mut out, c),
        }
    }
    out
}

/// `ConditionalSpecialCasing.lookUpTable`: the matching **language-specific**
/// row wins (the JDK `break`s on one); otherwise a matching language-independent
/// row applies; otherwise `None` → the caller uses the unconditional mapping.
fn lookup(chars: &[char], index: usize, lang: &str, lowercasing: bool) -> Option<&'static [char]> {
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
        let mapped = if lowercasing {
            entry.lower
        } else {
            entry.upper
        };
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
///
/// The JDK asks a `BreakIterator`, whose word characters are the ones
/// `Character.isLetterOrDigit` accepts. Measured on OpenJDK 25.0.3+9, that is
/// `false` for every code point in [`JDK_UNASSIGNED_CASE_RUNS`] and
/// `true` for `A7D3`/`A7D5`; Rust's `is_alphanumeric` says `true` for all six.
fn in_word(c: char) -> bool {
    if is_jdk_unassigned(c) {
        return false;
    }
    c.is_alphanumeric() || canonical_combining_class(c) != 0
}

/// `isCased` — Unicode `Uppercase` or `Lowercase` (Rust's `is_uppercase` /
/// `is_lowercase` are those derived properties, so `Other_Uppercase` /
/// `Other_Lowercase` are already included), plus the titlecase letters, which
/// Rust exposes only indirectly: a `Lt` character is a cased letter that has
/// *both* an upper- and a lower-case mapping away from itself.
///
/// The version-skew arm comes FIRST, and it is
/// [`JDK_UNASSIGNED_CASE_RUNS`] rather than the wider
/// [`JDK_UNMAPPED_CASE_RUNS`]: `A7D3` and `A7D5` ARE cased on
/// OpenJDK 25 (`getType == LOWERCASE_LETTER`), so blanket-excluding all six
/// here would trade one wrong answer for another. The two shapes need the same
/// arm for the *mapping* and different arms for the *property* — measured, not
/// reasoned, via the Final_Sigma rows in
/// [`JDK_UNASSIGNED_CASE_RUNS`]'s doc.
///
/// Without this arm the first line already answered `true` for the four:
/// Rust classifies `A7CE`/`A7D2`/`A7D4` as uppercase letters and `A7CF` as a
/// lowercase one, so the `is_alphabetic()` clause below was never even
/// consulted for them.
fn is_cased(c: char) -> bool {
    if is_jdk_unassigned(c) {
        return false;
    }
    if c.is_uppercase() || c.is_lowercase() {
        return true;
    }
    c.is_alphabetic() && c.to_uppercase().next() != Some(c) && c.to_lowercase().next() != Some(c)
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
mod tests;
