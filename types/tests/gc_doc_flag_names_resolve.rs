// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Every `CRATONVM_*` name the two GC documents tell an operator to set must be
//! a name the VM actually reads.
//!
//! # Why this is a test and not a review item
//!
//! A misspelt GROUPED token (`CRATONVM_GC=zgc-gen-header-zero`) is fatal: the
//! launcher refuses to start. A misspelt LEGACY variable is claimed by nothing,
//! so nothing expands it, warns about it or refuses it — the run completes as
//! the default configuration and an operator A/B-ing the documented knob
//! measures the default twice. `docs/gc-tuning.md` shipped three such names:
//! `CRATONVM_ZGC_GEN_HEADER_ZERO` and `CRATONVM_ZGC_GEN_DEAD_RUNS` (found
//! 2026-09-20, `gengc-plumbing2-legacy-flag-typos-fail-silently-FIXED-20260923.md`)
//! and `CRATONVM_G1_DBG_ACCESSOR` (found 2026-09-23 by this test's first run;
//! the real name is `CRATONVM_DBG_G1ACCESSOR`). No amount of reading catches
//! the class — the doc, the name and the silent run are all plausible — so the
//! documents are checked against `flag-surface.txt`, the fixture that
//! `flag_surface.rs` already keeps equal to every variable the VM reads.
//!
//! That page's option 2, landed by gengc round 4 lane `plumbing` (2026-09-23).

use std::collections::BTreeSet;
use std::path::Path;

/// The checked-in surface: every `CRATONVM_*` variable the VM reads.
const FIXTURE: &str = include_str!("flag-surface.txt");

/// The documents an operator tunes the collectors from.
const DOCS: &[&str] = &["docs/GC.md", "docs/gc-tuning.md"];

/// Names the documents mention precisely BECAUSE they do not exist — a rename
/// or a typo recorded in prose so a reader holding an old script can find out
/// why it does nothing. Each needs its reason; a name added here without one is
/// a doc error being silenced.
///
/// The names are spelled with `concat!` so this file does not itself read as a
/// use of an undeclared flag to `flag_declaration_guard.rs`, which scans every
/// `CRATONVM_*` literal in the tree.
const MENTIONED_AS_NON_EXISTENT: &[(&str, &str)] = &[
    (
        concat!("CRATONVM_", "ZGC_GEN_HEADER_ZERO"),
        "documented rename: the real knob is CRATONVM_ZGC_SWEEP_HEADER_ZERO",
    ),
    (
        concat!("CRATONVM_", "ZGC_GEN_DEAD_RUNS"),
        "documented rename: the real knob is CRATONVM_ZGC_SWEEP_DEAD_RUNS",
    ),
    (
        concat!("CRATONVM_", "G1_DBG_ACCESSOR"),
        "documented typo: the real knob is CRATONVM_DBG_G1ACCESSOR",
    ),
];

/// Every `CRATONVM_[A-Z0-9_]*[A-Z0-9]` in `text` — the maximal run of name
/// bytes after the prefix, with trailing underscores dropped. A run followed
/// by `*` is a prose wildcard (`CRATONVM_ZGC_*`, `CRATONVM_ZGC_GEN_*`) and a
/// bare `CRATONVM_` is a fragment; neither names a variable.
fn names_in(text: &str) -> BTreeSet<String> {
    const PREFIX: &str = "CRATONVM_";
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(at) = rest.find(PREFIX) {
        let tail = &rest[at + PREFIX.len()..];
        let run = tail
            .bytes()
            .take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_')
            .count();
        let wildcard = tail[run..].starts_with('*');
        let body = tail[..run].trim_end_matches('_');
        if !body.is_empty() && !wildcard {
            out.insert(format!("{PREFIX}{body}"));
        }
        rest = &tail[run..];
    }
    out
}

#[test]
fn every_gc_doc_flag_name_is_read_by_the_vm() {
    let surface: BTreeSet<&str> = FIXTURE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    assert!(
        surface.len() > 500,
        "flag-surface.txt parsed to {} names — the fixture moved or changed \
         shape, and this test would pass by checking nothing",
        surface.len()
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut unknown = Vec::new();
    let mut checked = 0usize;
    for doc in DOCS {
        let path = root.join(doc);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} must be readable: {e}", path.display()));
        for name in names_in(&text) {
            checked += 1;
            if surface.contains(name.as_str())
                || MENTIONED_AS_NON_EXISTENT.iter().any(|(n, _)| *n == name)
            {
                continue;
            }
            unknown.push(format!("{doc}: {name}"));
        }
    }
    assert!(
        checked > 50,
        "only {checked} CRATONVM_* names found across {DOCS:?} — the extractor is \
         not seeing the documents"
    );
    assert!(
        unknown.is_empty(),
        "these CRATONVM_* names are documented but read by nothing, so setting \
         one is silently ignored. Fix the spelling (see flag-surface.txt / \
         types/src/flag_groups.rs), or — only if the prose mentions it AS \
         non-existent — add it to MENTIONED_AS_NON_EXISTENT with a reason:\n  {}",
        unknown.join("\n  ")
    );
}

/// Every allow-list entry must still be mentioned, or it is silencing nothing
/// and should go.
#[test]
fn the_non_existent_allow_list_has_no_stale_entries() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut all = BTreeSet::new();
    for doc in DOCS {
        let text = std::fs::read_to_string(root.join(doc)).unwrap_or_default();
        all.extend(names_in(&text));
    }
    let stale: Vec<&str> = MENTIONED_AS_NON_EXISTENT
        .iter()
        .map(|(n, _)| *n)
        .filter(|n| !all.contains(*n))
        .collect();
    assert!(
        stale.is_empty(),
        "allow-list entries no document mentions any more: {stale:?}"
    );
}

#[test]
fn the_extractor_takes_whole_names_and_skips_wildcards() {
    let names = names_in(
        "set `CRATONVM_GC=par-threads=8` or CRATONVM_DBG_GCPAUSE_, \
         not CRATONVM_ZGC_* or CRATONVM_.",
    );
    let got: Vec<&str> = names.iter().map(String::as_str).collect();
    assert_eq!(got, vec!["CRATONVM_DBG_GCPAUSE", "CRATONVM_GC"]);
}
