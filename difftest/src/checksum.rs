// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The **program-computed checksum** channel.
//!
//! The report's P0 asks the differential run to compare "success, stdout/stderr,
//! exit status, exception type/message, and **checksum**". The first five are
//! things the harness observes; the checksum is different in kind — it is
//! something the *program under test* computes and declares, and that
//! difference is the point:
//!
//! * A stdout diff says "line 37 differs". A checksum diff says **which
//!   computed quantity** is wrong, by name, which is what a bisection needs.
//! * A checksum survives normalization. Every rule in [`crate::normalize`] is a
//!   potential over-normalizer; the checksum line is extracted from the
//!   *un-normalized* stdout and compared verbatim, so a rule can never launder
//!   it. If stdout agrees only after normalization but the checksums disagree,
//!   the normalization hid a real bug and the harness says so.
//! * Conversely, if the checksums agree while stdout diverges, the divergence
//!   is in formatting or in noise the program did not intend as a result — a
//!   different, cheaper class of finding.
//!
//! ## The contract
//!
//! A program declares a checksum by printing, on its own line:
//!
//! ```text
//! ##DIFFTEST-CHECKSUM## <name> <value>
//! ```
//!
//! `<name>` is any run of non-whitespace (the quantity being summarized, e.g.
//! `arith` or `hash-order`); `<value>` is the rest of the line, trimmed. A
//! program may declare any number of them; a program that declares none simply
//! has no checksum channel, and the dimension reports nothing rather than
//! reporting a vacuous match.
//!
//! The value is deliberately opaque to the harness: an FNV-1a of a `long`
//! accumulator, a `String.hashCode`, a `MessageDigest` hex string — whatever the
//! seed can compute deterministically under both VMs. [`fnv1a64_hex`] is
//! provided for seeds that want the harness's own spelling, and for the
//! harness's compact `digest` of a whole stream.

use std::collections::BTreeMap;

/// The line marker a program prints to declare a checksum.
pub const MARKER: &str = "##DIFFTEST-CHECKSUM##";

/// The declared checksums found in one captured stdout, keyed by name.
///
/// A `BTreeMap` so the rendering is stable regardless of the order the program
/// printed them in — the *set* of declared quantities is the observable, not
/// the sequence (the sequence is already covered by the stdout channel).
pub type Checksums = BTreeMap<String, String>;

/// Extract every `##DIFFTEST-CHECKSUM## <name> <value>` declaration from a
/// captured stream.
///
/// Leading whitespace before the marker is tolerated (a seed may indent), and a
/// marker line with no name is ignored rather than recorded under an empty key.
/// A repeated name keeps the **last** declaration, so a program that recomputes
/// a quantity in a loop reports its final value.
pub fn extract(stream: &str) -> Checksums {
    let mut out = Checksums::new();
    for line in stream.lines() {
        let Some(rest) = line.trim_start().strip_prefix(MARKER) else {
            continue;
        };
        let rest = rest.trim();
        let mut parts = rest.splitn(2, char::is_whitespace);
        let Some(name) = parts.next().filter(|n| !n.is_empty()) else {
            continue;
        };
        let value = parts.next().unwrap_or("").trim().to_string();
        out.insert(name.to_string(), value);
    }
    out
}

/// Render a checksum set for a divergence report: `name=value` pairs, one per
/// line, in name order. `<none declared>` when the program declared nothing, so
/// a report never shows an ambiguous empty string.
pub fn render(sums: &Checksums) -> String {
    if sums.is_empty() {
        return "<none declared>".to_string();
    }
    sums.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<String>>()
        .join("\n")
}

/// The names whose values differ between two checksum sets, plus the names
/// present on only one side.
///
/// This is what makes the dimension *actionable*: a divergence report names the
/// quantity, not the byte offset.
pub fn differing_names(a: &Checksums, b: &Checksums) -> Vec<String> {
    let mut names: Vec<String> = a.keys().chain(b.keys()).cloned().collect();
    names.sort_unstable();
    names.dedup();
    names.retain(|n| a.get(n) != b.get(n));
    names
}

/// FNV-1a 64, rendered as 16 lowercase hex digits.
///
/// Used for the harness's compact stream digest and offered to seeds that want
/// a checksum spelling the harness can reproduce. FNV is chosen over anything
/// stronger on purpose: it is trivially reimplementable in a five-line Java
/// helper inside a seed, with no dependency on `java.security` (whose provider
/// stack is itself one of the things under test).
pub fn fnv1a64_hex(data: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in data {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// A compact digest of a whole captured stream, for reports and ledger
/// summaries. Never used as a comparison dimension on its own — a digest
/// mismatch says nothing a stdout diff does not already say, and says it less
/// usefully.
pub fn digest(stream: &str) -> String {
    fnv1a64_hex(stream.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_declared_checksums() {
        let out = "hello\n\
                   ##DIFFTEST-CHECKSUM## arith 0x1234\n\
                   world\n\
                   ##DIFFTEST-CHECKSUM## strings deadbeefdeadbeef\n";
        let sums = extract(out);
        assert_eq!(sums.len(), 2);
        assert_eq!(sums["arith"], "0x1234");
        assert_eq!(sums["strings"], "deadbeefdeadbeef");
    }

    #[test]
    fn a_program_that_declares_nothing_has_no_checksum_channel() {
        assert!(extract("plain output\nno markers here\n").is_empty());
        assert_eq!(render(&Checksums::new()), "<none declared>");
    }

    #[test]
    fn indented_and_malformed_declarations() {
        // Indentation is tolerated; a marker with no name is ignored rather
        // than recorded under an empty key.
        let sums = extract("    ##DIFFTEST-CHECKSUM## indented v\n##DIFFTEST-CHECKSUM##\n");
        assert_eq!(sums.len(), 1);
        assert_eq!(sums["indented"], "v");
    }

    #[test]
    fn a_repeated_name_keeps_the_last_value() {
        let sums = extract("##DIFFTEST-CHECKSUM## n 1\n##DIFFTEST-CHECKSUM## n 2\n");
        assert_eq!(sums["n"], "2");
    }

    #[test]
    fn differing_names_reports_value_and_presence_splits() {
        let a = extract("##DIFFTEST-CHECKSUM## same 1\n##DIFFTEST-CHECKSUM## diff a\n");
        let b = extract(
            "##DIFFTEST-CHECKSUM## same 1\n##DIFFTEST-CHECKSUM## diff b\n\
                         ##DIFFTEST-CHECKSUM## only-b x\n",
        );
        assert_eq!(differing_names(&a, &b), vec!["diff", "only-b"]);
        assert!(differing_names(&a, &a).is_empty());
    }

    #[test]
    fn fnv1a_matches_the_published_vectors() {
        // The reference FNV-1a 64 test vectors, so a seed reimplementing this
        // in Java can check itself against the same numbers.
        assert_eq!(fnv1a64_hex(b""), "cbf29ce484222325");
        assert_eq!(fnv1a64_hex(b"a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64_hex(b"foobar"), "85944171f73967e8");
        assert_eq!(digest("a"), fnv1a64_hex(b"a"));
    }

    #[test]
    fn render_is_stable_regardless_of_declaration_order() {
        let a = extract("##DIFFTEST-CHECKSUM## b 2\n##DIFFTEST-CHECKSUM## a 1\n");
        let b = extract("##DIFFTEST-CHECKSUM## a 1\n##DIFFTEST-CHECKSUM## b 2\n");
        assert_eq!(render(&a), render(&b));
        assert_eq!(render(&a), "a=1\nb=2");
    }
}
