// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! gcj j3/x2 (proposal `gcj-j2x1-proposal-collector-scoped-default-flips-20261019`,
//! part B): a row's `off_word` and its reader agree on the default.
//!
//! # Why
//!
//! `flag_groups::E::off_word` is metadata: `flag_groups::apply` reads it only to
//! spell `CRATONVM_<GROUP>=-token`. The default itself lives in the READER:
//! `runtime_flag_on` (unset OFF) or `runtime_flag_default_on` /
//! `runtime_flag_default_on_if` (unset ON where scoped). A flip is two edits
//! that nothing checked against each other, and wave j2 found a flip of the row
//! alone flips nothing (`gcj-j2x1-wave-j2-default-flips-...`, section 1).
//!
//! # The rule
//!
//! For every row with an `on_key` and no `off_key` (one row per key):
//!
//! * `off_word: Some(_)` (default ON) -- at least one reader site of the key is
//!   `runtime_flag_default_on(` or `runtime_flag_default_on_if(`. A
//!   collector-scoped reader may ALSO have a `runtime_flag_on(` arm (the other
//!   collectors' opt-in).
//! * `off_word: None` (opt-in) -- no reader site is default-ON.
//!
//! A reader site is an exact `"CRATONVM_..."` literal whose text right before
//! the opening quote (on its line, or the end of the previous non-blank code
//! line, which is how rustfmt breaks a long call) ends with one of the three
//! calls' opening parenthesis. Comment lines are skipped.
//!
//! # Blind spot (stated)
//!
//! A key read through a `const` (`const SWITCH: &str = "..";
//! runtime_flag_default_on(SWITCH)`), a typed `GcFlags` field, a hand-rolled
//! `runtime_var` match, or a name built at run time has no reader site and is
//! not judged. The test asserts it judges a large share of the inventory and
//! names two wave j2 flips it must see, so it cannot pass vacuously.

use cratonvm_types::flag_groups::INVENTORY;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules"];

/// Rows whose reader and `off_word` disagree on the tree this guard was
/// written on (2026-10-19), each with what it is. None is Generational or
/// common GC code; each is filed on
/// `docs/known-issues/gc/gcj-j3x2-flag-rows-whose-off-word-disagrees-with-the-reader-20261019.md`
/// for its owner. A row here that stops disagreeing fails
/// `the_known_mismatches_have_no_dead_rows`: delete it then.
const KNOWN_MISMATCHES: &[(&str, &str)] = &[
    (
        "CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE",
        "off_word Some, reader runtime_flag_on (vm/src/config.rs): the row says default ON",
    ),
    (
        "CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT",
        "off_word Some, reader runtime_flag_on (jit/src/aarch64_backend.rs)",
    ),
    (
        "CRATONVM_JIT_IR_VH_READ_BINDS",
        "off_word None, but one reader arm is runtime_flag_default_on (jit/src/lib.rs, \
         ByteBuffer descriptors): a mixed default",
    ),
    (
        "CRATONVM_ZGC_SOFT_MAX_MANAGEABLE",
        "off_word Some, reader runtime_flag_on (native-builtins/src/jmx.rs); ZGC-owned",
    ),
    (
        "CRATONVM_ZGC_JIT_SHORT_ANNOUNCE",
        "off_word Some, reader runtime_flag_on (jit/src/x64/objects.rs); ZGC-owned",
    ),
    (
        "CRATONVM_ZGC_JIT_ANNOUNCE_EPOCH_GATE",
        "off_word Some, readers runtime_flag_on (gc/src/zgc/vm_tlab.rs, \
         jit/src/x64/objects.rs); ZGC-owned",
    ),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Reader {
    DefaultOn,
    OptIn,
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

fn rust_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if path.is_dir() {
            if !SKIPPED_DIRS.contains(&name) && !name.starts_with('.') {
                rust_sources(&path, out);
            }
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

/// The comment rule of `flag_declaration_guard.rs` (`is_comment_line`).
fn is_comment_line(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("//") || t == "*" || t.starts_with("* ") || t.starts_with("*/")
}

/// Every exact `"CRATONVM_..."` literal in `line`: (name, index of its
/// opening quote).
fn exact_literals(line: &str) -> Vec<(&str, usize)> {
    const OPEN: &str = "\"CRATONVM_";
    let mut found = Vec::new();
    let mut from = 0usize;
    while let Some(rel) = line[from..].find(OPEN) {
        let quote = from + rel;
        let tail = &line[quote + 1..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        if tail[end..].starts_with('"') {
            found.push((&tail[..end], quote));
        }
        from = quote + 1;
    }
    found
}

/// The reader whose call opens right before a literal, given the code text
/// in front of its opening quote (previous code line + this line's prefix).
fn reader_before(context: &str) -> Option<Reader> {
    let c = context.trim_end();
    if c.ends_with("runtime_flag_default_on_if(") || c.ends_with("runtime_flag_default_on(") {
        Some(Reader::DefaultOn)
    } else if c.ends_with("runtime_flag_on(") {
        Some(Reader::OptIn)
    } else {
        None
    }
}

/// Every reader site of every literal in `text`, as (name, reader, line).
fn reader_sites(text: &str) -> Vec<(String, Reader, usize)> {
    let mut out = Vec::new();
    let mut prev = String::new();
    for (index, line) in text.lines().enumerate() {
        if is_comment_line(line) {
            continue;
        }
        for (name, quote) in exact_literals(line) {
            let context = format!("{prev} {}", &line[..quote]);
            if let Some(reader) = reader_before(&context) {
                out.push((name.to_string(), reader, index + 1));
            }
        }
        if !line.trim().is_empty() {
            prev = line.to_string();
        }
    }
    out
}

/// key -> its reader sites (`path:line`, reader), over the workspace.
fn scan() -> BTreeMap<String, Vec<(String, Reader)>> {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(sources.len() > 100, "the walk is not reaching the workspace");
    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut sites: BTreeMap<String, Vec<(String, Reader)>> = BTreeMap::new();
    for path in &sources {
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("CRATONVM_") {
            continue;
        }
        let shown = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/");
        for (name, reader, line) in reader_sites(&text) {
            sites
                .entry(name)
                .or_default()
                .push((format!("{shown}:{line}"), reader));
        }
    }
    sites
}

/// key -> declared default ON (`off_word` is `Some`), for the rows the rule
/// judges: an `on_key`, no `off_key`, and the only row naming that key.
fn judged_rows() -> BTreeMap<&'static str, bool> {
    let mut count: BTreeMap<&'static str, usize> = BTreeMap::new();
    for e in INVENTORY {
        if let (Some(on), None) = (e.on_key, e.off_key) {
            *count.entry(on).or_default() += 1;
        }
    }
    INVENTORY
        .iter()
        .filter_map(|e| match (e.on_key, e.off_key) {
            (Some(on), None) if count[on] == 1 => Some((on, e.off_word.is_some())),
            _ => None,
        })
        .collect()
}

/// The mismatches on today's tree: (key, what is wrong, the sites).
fn mismatches() -> Vec<(&'static str, &'static str, Vec<String>)> {
    let sites = scan();
    let mut out: Vec<(&'static str, &'static str, Vec<String>)> = Vec::new();
    for (key, default_on) in judged_rows() {
        let Some(found) = sites.get(key) else {
            continue;
        };
        let any_default_on = found.iter().any(|(_, r)| *r == Reader::DefaultOn);
        let shown = || -> Vec<String> {
            found.iter().map(|(s, r)| format!("{s} ({r:?})")).collect()
        };
        if default_on && !any_default_on {
            out.push((
                key,
                "off_word is Some (default ON) but every reader is runtime_flag_on",
                shown(),
            ));
        }
        if !default_on && any_default_on {
            out.push((key, "off_word is None (opt-in) but a reader is default-ON", shown()));
        }
    }
    out
}

#[test]
fn every_off_word_row_agrees_with_its_reader() {
    let unexpected: Vec<String> = mismatches()
        .into_iter()
        .filter(|(key, _, _)| !KNOWN_MISMATCHES.iter().any(|(k, _)| k == key))
        .map(|(key, what, sites)| {
            format!("{key}: {what}\n      {}", sites.join("\n      "))
        })
        .collect();
    assert!(
        unexpected.is_empty(),
        "{} flag row(s) whose `off_word` disagrees with the reader's default.\n\n\
         `off_word` is metadata (the `-token` spelling); the default lives in the \
         reader. A flip changes BOTH: the reader to `runtime_flag_default_on` (or \
         `runtime_flag_default_on_if(key, heap.is_generational())` for a \
         collector-scoped flip) AND the row to `off_word: Some(\"0\")`; an opt-in \
         keeps `runtime_flag_on` and `off_word: None`.\n\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}

#[test]
fn the_known_mismatches_have_no_dead_rows() {
    let now: Vec<&str> = mismatches().into_iter().map(|(k, _, _)| k).collect();
    let dead: Vec<&str> = KNOWN_MISMATCHES
        .iter()
        .map(|(k, _)| *k)
        .filter(|k| !now.contains(k))
        .collect();
    assert!(dead.is_empty(), "KNOWN_MISMATCHES rows that agree now (delete them): {dead:?}");
}

/// Not vacuous: the scan judges most of the inventory and sees the readers of
/// two wave j2 rows by their shape -- a plain default-ON reader, and a
/// collector-scoped one with both arms.
#[test]
fn the_guard_sees_the_wave_j2_readers() {
    let sites = scan();
    let judged = judged_rows();
    let seen = judged.keys().filter(|k| sites.contains_key(**k)).count();
    assert!(seen > 1000, "only {seen} of {} rows have a reader site", judged.len());
    let readers = |key: &str| -> Vec<Reader> {
        sites
            .get(key)
            .map(|v| v.iter().map(|(_, r)| *r).collect())
            .unwrap_or_default()
    };
    assert!(judged["CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS"]);
    assert!(readers("CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS").contains(&Reader::DefaultOn));
    let scoped = readers("CRATONVM_GC_DEDUPE_INITIATOR_SNAPSHOT");
    assert!(scoped.contains(&Reader::DefaultOn) && scoped.contains(&Reader::OptIn));
    assert!(!judged["CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE"]);
    assert_eq!(readers("CRATONVM_GC_WEAK_STRING_TABLE_HIT_GRACE"), vec![Reader::OptIn]);
}

/// The site rule on fixed text: a call on the same line, a call broken by
/// rustfmt onto the previous line, a literal that is only an argument of a
/// different call, and a comment.
#[test]
fn the_reader_site_rule() {
    // The fixture names are spelled through `PFX_` so the declaration guard,
    // which scans this file, never sees an undeclared literal.
    const P: &str = concat!("CRATON", "VM_");
    let text = concat!(
        "    runtime_flag_default_on(\"PFX_A\")\n",
        "    cratonvm_types::flags::runtime_flag_on(\n",
        "        \"PFX_B\",\n",
        "    )\n",
        "    with_thread_overrides(&[(\"PFX_C\", None)], || runtime_flag_on(X));\n",
        "    // runtime_flag_on(\"PFX_D\")\n",
        "    flags::runtime_flag_default_on_if(\"PFX_E\", heap.is_generational())\n",
    )
    .replace("PFX_", P);
    let got: Vec<(String, Reader, usize)> = reader_sites(&text);
    assert_eq!(
        got,
        vec![
            (format!("{P}A"), Reader::DefaultOn, 1),
            (format!("{P}B"), Reader::OptIn, 3),
            (format!("{P}E"), Reader::DefaultOn, 7),
        ]
    );
}
