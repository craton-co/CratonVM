// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Turn named rows of the retirement tables back OFF at runtime, so bisecting a
//! wave costs a RUN instead of a BUILD.
//!
//! # The problem this solves
//!
//! [`crate::retired_shadow`] is a compile-time constant. When a retirement wave
//! lands and one corpus vector goes red, the only way to find which of the
//! wave's triples did it has been to edit the table, rebuild the VM (~20-26
//! minutes on the shared host) and re-run — once per hypothesis. Lane 4 wave 1
//! spent three of its seven builds on exactly that, and still could not
//! attribute `RJdkSecurity` per-triple: eighteen retired triples were reachable
//! from that vector and the file-handle group among them had to be reported as
//! a GROUP, un-attributed, in the lane page.
//!
//! With this switch the same bisection is a sequence of runs:
//!
//! ```text
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=all              does un-retiring fix it?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/         which package?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File     which class?
//!   CRATONVM_UNRETIRE_NATIVE_SHADOW=java/io/File.isAbsolute()Z   which row?
//! ```
//!
//! # It is a DIAGNOSTIC, and the default is provably inert
//!
//! Unset — which is every shipping configuration and every CI arm — parses to
//! `None` and [`is_excluded`] returns `false` for every input without
//! consulting anything. `the_default_is_inert` asserts that against the whole
//! of `RETIRED_SHADOW_TABLES` rather than against a sample, because "a guard's
//! default must be chosen by measuring the inert one" and a switch that
//! *nearly* does nothing by default is a correctness bug in every lane at once.
//!
//! # Why it reports at ARM time, and why a rule matching nothing is loud
//!
//! The failure this instrument would otherwise invite is the one that makes
//! bisection worse than useless: a mistyped rule matches no row, the vector
//! still fails, and the reader concludes the triple is exonerated. That is a
//! false NEGATIVE produced by the tool itself.
//!
//! So the rules are resolved against the tables **once, at parse time, before
//! any dispatch**, and the row count for each is printed. A rule matching zero
//! rows is called out by name. You learn your list is wrong in the first
//! milliseconds of the run rather than from a clean result an hour later.
//!
//! ```text
//!   [cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW armed, 2 rule(s):
//!       java/io/File                              51 row(s)
//!       java/io/Reader.close                       0 row(s)   <-- MATCHES NOTHING
//!   [cratonvm] 1 rule matches no retired row. A rule that matches nothing
//!              cannot exonerate anything: the run below is NOT a test of it.
//! ```
//!
//! The count is TABLE ROWS -- distinct triples in
//! `RETIRED_SHADOW_TABLES` -- not registrations. A census taken either side of
//! the switch moves by more: `java/io/File` reports 51 rows and returns 71
//! registrations to `Bridge`, because 54 distinct `java/io/File` triples are
//! registered at 74 ordinals and the re-tag flips each one. The stub ratchet
//! documents the same distinction for the same reason.
//!
//! # What it does NOT do
//!
//! It un-retires; it does not re-retire, and it cannot make a triple retired
//! that no table carries. It is therefore incapable of turning a green run red
//! by itself — the worst it can do is restore the pre-wave behaviour, which is
//! the behaviour the wave was measured against.
//!
//! Nor is it the dial. `CRATONVM_ENFORCE_NATIVE_SHADOW` declines a native at
//! DISPATCH and its decline is conditional — when the receiver has no concrete
//! body it runs the native anyway. This switch edits the TABLE, so what it
//! turns off is off unconditionally. That asymmetry is the whole reason the
//! dial cannot certify a wave and this can bisect one.

use std::sync::OnceLock;

/// One parsed entry of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rule {
    /// `all` — every row of every table.
    All,
    /// `java/io/` — every class under a package prefix (trailing `/`).
    Prefix(String),
    /// `java/io/File` — every row of one class.
    Class(String),
    /// `java/io/File.isAbsolute` — every overload of one method.
    ClassMethod(String, String),
    /// `java/io/File.isAbsolute()Z` — exactly one row.
    Triple(String, String, String),
}

impl Rule {
    fn matches(&self, class: &str, method: &str, descriptor: &str) -> bool {
        match self {
            Rule::All => true,
            Rule::Prefix(p) => class.starts_with(p.as_str()),
            Rule::Class(c) => class == c,
            Rule::ClassMethod(c, m) => class == c && method == m,
            Rule::Triple(c, m, d) => class == c && method == m && descriptor == d,
        }
    }
}

/// Parse one entry.
///
/// The grammar is unambiguous without lookahead because of what JVM names can
/// contain: a descriptor always starts at the first `(`, a class name never
/// contains `.` (it is internal form, `java/io/File`), and a method name
/// contains neither `.` nor `(`. So the first `(` splits off the descriptor and
/// the last `.` before it splits class from method.
fn parse_rule(entry: &str) -> Option<Rule> {
    let entry = entry.trim();
    if entry.is_empty() {
        return None;
    }
    if entry.eq_ignore_ascii_case("all") {
        return Some(Rule::All);
    }
    if entry.ends_with('/') {
        return Some(Rule::Prefix(entry.to_string()));
    }
    let (head, descriptor) = match entry.find('(') {
        Some(i) => (&entry[..i], Some(entry[i..].to_string())),
        None => (entry, None),
    };
    match head.rfind('.') {
        Some(i) => {
            let class = head[..i].to_string();
            let method = head[i + 1..].to_string();
            if class.is_empty() || method.is_empty() {
                return None;
            }
            Some(match descriptor {
                Some(d) => Rule::Triple(class, method, d),
                None => Rule::ClassMethod(class, method),
            })
        }
        // No `.` at all: a bare class name. A descriptor without a method is
        // meaningless, so reject it rather than silently matching the class.
        None => {
            if descriptor.is_some() {
                None
            } else {
                Some(Rule::Class(head.to_string()))
            }
        }
    }
}

/// The parsed list and, beside each rule, how many retired rows it reaches.
struct Armed {
    rules: Vec<Rule>,
    /// `(source text, rows matched)`, in the order given.
    report: Vec<(String, usize)>,
}

fn armed() -> Option<&'static Armed> {
    static ARMED: OnceLock<Option<Armed>> = OnceLock::new();
    ARMED.get_or_init(parse_env).as_ref()
}

fn parse_env() -> Option<Armed> {
    let raw = cratonvm_types::flags::runtime_var("CRATONVM_UNRETIRE_NATIVE_SHADOW").ok()?;
    build(&raw, true)
}

/// Split, parse and resolve a list. `announce` is false in tests so the suite
/// does not write to stderr.
fn build(raw: &str, announce: bool) -> Option<Armed> {
    let mut rules = Vec::new();
    let mut report = Vec::new();
    let mut rejected = Vec::new();
    for entry in raw.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        match parse_rule(entry) {
            Some(rule) => {
                let rows = rows_matched(&rule);
                report.push((entry.to_string(), rows));
                rules.push(rule);
            }
            None => rejected.push(entry.to_string()),
        }
    }
    if rules.is_empty() && rejected.is_empty() {
        return None;
    }
    if announce {
        announce_arm(&report, &rejected);
    }
    Some(Armed { rules, report })
}

/// How many rows of the retirement tables a rule reaches.
///
/// Resolved once, at arm time. This is the number that makes a typo visible
/// BEFORE the run rather than after it.
fn rows_matched(rule: &Rule) -> usize {
    crate::retired_shadow::RETIRED_SHADOW_TABLES
        .iter()
        .flat_map(|table| table.iter())
        .filter(|(c, m, d)| rule.matches(c, m, d))
        .count()
}

fn announce_arm(report: &[(String, usize)], rejected: &[String]) {
    eprintln!(
        "[cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW armed, {} rule(s):",
        report.len()
    );
    for (text, rows) in report {
        let flag = if *rows == 0 {
            "   <-- MATCHES NOTHING"
        } else {
            ""
        };
        eprintln!("    {text:<48} {rows:>5} table row(s){flag}");
    }
    let empty = report.iter().filter(|(_, n)| *n == 0).count();
    if empty > 0 {
        eprintln!(
            "[cratonvm] {empty} rule(s) match no retired row. A rule that matches \
             nothing cannot\n           exonerate anything: this run is NOT a test \
             of it. Check the spelling\n           against --dump-native-registry \
             (class names are internal form, `java/io/File`)."
        );
    }
    for text in rejected {
        eprintln!("[cratonvm] CRATONVM_UNRETIRE_NATIVE_SHADOW: cannot parse {text:?} — ignored.");
    }
}

/// Is this triple excluded from retirement by the switch?
///
/// `false` for every input when the variable is unset, which is every shipping
/// configuration. See the module header on why the default is asserted inert
/// against the whole table rather than a sample.
#[must_use]
pub fn is_excluded(class_name: &str, method_name: &str, descriptor: &str) -> bool {
    match armed() {
        None => false,
        Some(a) => a
            .rules
            .iter()
            .any(|r| r.matches(class_name, method_name, descriptor)),
    }
}

/// `(rule as written, rows of the retirement tables it reaches)`, empty when
/// the switch is not armed.
///
/// Exposed so a caller can fold the same account into a report rather than
/// re-deriving it, and so the zero-match case is testable without capturing
/// stderr.
#[must_use]
pub fn report() -> Vec<(String, usize)> {
    armed().map(|a| a.report.clone()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::retired_shadow::{triple_is_retired_shadow, RETIRED_SHADOW_TABLES};

    fn rules(raw: &str) -> Vec<Rule> {
        build(raw, false).map(|a| a.rules).unwrap_or_default()
    }

    #[test]
    fn the_default_is_inert_across_every_retired_row() {
        // Not a sample. The whole population, because this switch is compiled
        // into every shipping binary and a default that is *nearly* inert is a
        // defect in every lane at once.
        let mut rows = 0;
        for table in RETIRED_SHADOW_TABLES {
            for (c, m, d) in table.iter() {
                assert!(
                    !is_excluded(c, m, d),
                    "{c}.{m}{d} is excluded with the variable unset — the \
                     default is not inert"
                );
                assert!(
                    triple_is_retired_shadow(c, m, d),
                    "{c}.{m}{d} stopped being retired with the variable unset"
                );
                rows += 1;
            }
        }
        assert!(
            rows > 500,
            "expected the tables to be populated, saw {rows}"
        );
        assert!(report().is_empty());
    }

    #[test]
    fn each_shape_parses_to_the_rule_it_reads_as() {
        assert_eq!(rules("all"), vec![Rule::All]);
        assert_eq!(rules("ALL"), vec![Rule::All]);
        assert_eq!(rules("java/io/"), vec![Rule::Prefix("java/io/".into())]);
        assert_eq!(
            rules("java/io/File"),
            vec![Rule::Class("java/io/File".into())]
        );
        assert_eq!(
            rules("java/io/File.isAbsolute"),
            vec![Rule::ClassMethod(
                "java/io/File".into(),
                "isAbsolute".into()
            )]
        );
        assert_eq!(
            rules("java/io/File.isAbsolute()Z"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "isAbsolute".into(),
                "()Z".into()
            )]
        );
    }

    #[test]
    fn a_descriptor_with_slashes_and_parens_survives_parsing() {
        // The grammar claim in `parse_rule` is that the first `(` ends the
        // head and the last `.` before it splits class from method. A
        // descriptor full of `/` and `;` is where that claim earns its keep.
        assert_eq!(
            rules("java/io/File.listFiles()[Ljava/io/File;"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "listFiles".into(),
                "()[Ljava/io/File;".into()
            )]
        );
        assert_eq!(
            rules("java/io/File.<init>(Ljava/lang/String;)V"),
            vec![Rule::Triple(
                "java/io/File".into(),
                "<init>".into(),
                "(Ljava/lang/String;)V".into()
            )]
        );
    }

    #[test]
    fn a_list_is_split_and_blanks_are_dropped() {
        assert_eq!(
            rules(" java/io/ , , java/nio/ "),
            vec![
                Rule::Prefix("java/io/".into()),
                Rule::Prefix("java/nio/".into())
            ]
        );
    }

    #[test]
    fn an_unparseable_entry_is_rejected_rather_than_guessed() {
        assert_eq!(parse_rule("()V"), None);
        assert_eq!(parse_rule(".foo"), None);
        assert_eq!(parse_rule("java/io/File."), None);
        assert_eq!(parse_rule(""), None);
    }

    #[test]
    fn a_rule_reports_the_rows_it_reaches_and_zero_is_visible() {
        // The whole point of resolving at arm time: a plausible-looking rule
        // that reaches nothing is reported as 0 rather than silently
        // exonerating whatever it was meant to name.
        let armed = build("java/io/File,java/io/Reader.close,all", false).unwrap();
        let by_text: Vec<(String, usize)> = armed.report;
        assert_eq!(by_text[0].0, "java/io/File");
        assert!(
            by_text[0].1 >= 50,
            "java/io/File should reach its retired rows, got {}",
            by_text[0].1
        );
        assert_eq!(
            by_text[1].1, 0,
            "java/io/Reader.close is in no table; it must report 0"
        );
        assert!(by_text[2].1 > by_text[0].1, "`all` must reach every row");
    }

    #[test]
    fn matching_is_scoped_exactly_as_written() {
        let c = Rule::Class("java/io/File".into());
        assert!(c.matches("java/io/File", "isAbsolute", "()Z"));
        assert!(!c.matches("java/io/FileWriter", "close", "()V"));

        let p = Rule::Prefix("java/io/".into());
        assert!(p.matches("java/io/FileWriter", "close", "()V"));
        assert!(!p.matches("java/nio/ByteBuffer", "limit", "()I"));

        let t = Rule::Triple("java/io/File".into(), "isAbsolute".into(), "()Z".into());
        assert!(t.matches("java/io/File", "isAbsolute", "()Z"));
        assert!(!t.matches("java/io/File", "isAbsolute", "()I"));
        assert!(!t.matches("java/io/File", "isDirectory", "()Z"));
    }
}
