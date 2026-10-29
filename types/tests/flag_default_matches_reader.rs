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

/// Rows whose reader and `off_word` disagree, each with what it is. The six
/// of the tree this guard was written on (2026-10-19) are all fixed
/// (`docs/internal/gc/gcj-j3x2-flag-rows-whose-off-word-disagrees-with-the-reader-FIXED-20261025.md`);
/// a new one is fixed, not listed. A row here that stops disagreeing fails
/// `the_known_mismatches_have_no_dead_rows`: delete it then.
const KNOWN_MISMATCHES: &[(&str, &str)] = &[];

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

// ── The reader's DOC agrees with its default (JIT round 25 review) ──────
//
// `docs/feature-designs/jdk-only-mode.md`:
// the R22/R23 A/B flips changed 32 readers to `runtime_flag_default_on` and
// left their `///` comments saying "opt-in, default OFF". The doc is what a
// reader of the code (or a bisect) believes. Judged: a `fn name() -> bool {`
// whose whole body is ONE `cratonvm_types::flags::runtime_flag_on("KEY")` or
// `runtime_flag_default_on("KEY")` call (no negation, no memo, no second arm),
// and whose `///` block says "default off" / "opt-in" with no "default on"
// over a default-ON body, or the reverse. A doc that names both (history: "was
// opt-in until ...") is not judged.

/// Docs that disagree with their reader on the tree this guard was written on
/// (2026-10-24), outside the JIT lane's files or historical wording, by
/// `(path suffix, function)`. A row that stops disagreeing fails
/// `the_known_doc_mismatches_have_no_dead_rows`: delete it then.
const KNOWN_DOC_MISMATCHES: &[(&str, &str)] = &[
    ("classloading/src/class_manager.rs", "string_builder_compact_layout_enabled"),
    ("gc/src/gen_heap.rs", "pinned_finalizable_unnamed_not_roots_enabled"),
    ("gc/src/gen_heap.rs", "gen_serial_eden_age_clock_enabled"),
    ("gc/src/gen_heap.rs", "gen_serial_eden_age_clock_old_direct_enabled"),
    ("gc/src/gen_heap.rs", "full_gc_true_roots_all_majors_enabled"),
    // "it was inert while they were opt-in": history, not a default claim.
    ("jit/src/osr_exit.rs", "guard_exits_per_artifact_enabled"),
    ("native-builtins/src/intrinsics/mod.rs", "jdk_only_sb_bytecode_enabled"),
    ("vm/src/jit/conservative_roots.rs", "xt_jit_coverage_assume"),
    ("vm/src/runtime/invokedynamic.rs", "jit_indy_record_fast_enabled"),
    ("vm/src/runtime/interpreter/gc_and_alloc.rs", "no_roots_lazy_claim_enabled"),
    ("vm/src/runtime/interpreter/jit_bridge.rs", "ir_field_site_trap_enabled"),
    ("vm/src/runtime/interpreter/jit_bridge.rs", "osr_deferred_field_site_retier_enabled"),
    ("vm/src/runtime/interpreter/jit_bridge.rs", "self_lock_eager_bind_enabled"),
];

/// `name` when `line` is `[pub[(..)] ]fn name() -> bool {`.
fn bool_reader_fn_name(line: &str) -> Option<&str> {
    let t = line.trim();
    let at = t.find("fn ")?;
    let prefix = t[..at].trim_end();
    if !(prefix.is_empty()
        || prefix == "pub"
        || (prefix.starts_with("pub(") && prefix.ends_with(')')))
    {
        return None;
    }
    let rest = &t[at + 3..];
    let paren = rest.find("()")?;
    let name = &rest[..paren];
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let tail: String = rest[paren + 2..]
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (tail == "-> bool {").then_some(name)
}

/// `Some(true)` for a body that is exactly one `runtime_flag_default_on` call
/// on a `"CRATONVM_.."` literal, `Some(false)` for one `runtime_flag_on` call.
fn sole_reader(body: &str) -> Option<bool> {
    let b = body.trim().strip_prefix("cratonvm_types::flags::")?;
    let (default_on, rest) = if let Some(r) = b.strip_prefix("runtime_flag_default_on(") {
        (true, r)
    } else if let Some(r) = b.strip_prefix("runtime_flag_on(") {
        (false, r)
    } else {
        return None;
    };
    let rest = rest
        .trim_start()
        .strip_prefix("\"CRATON")?
        .strip_prefix("VM_")?;
    let q = rest.find('"')?;
    if !rest[..q]
        .chars()
        .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return None;
    }
    let tail = rest[q + 1..].trim_start();
    let tail = tail.strip_prefix(',').unwrap_or(tail).trim_start();
    (tail == ")").then_some(default_on)
}

/// Every judged reader of `text` whose doc disagrees: `(path, fn, what)`.
fn doc_mismatches_in(shown: &str, text: &str, out: &mut Vec<(String, String, &'static str)>) {
    let lines: Vec<&str> = text.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(name) = bool_reader_fn_name(line) else {
            continue;
        };
        let mut stmts: Vec<&str> = Vec::new();
        for l in lines.iter().skip(i + 1) {
            let s = l.trim();
            if s.starts_with('}') {
                break;
            }
            if !s.is_empty() && !s.starts_with("//") {
                stmts.push(s);
            }
        }
        let Some(default_on) = sole_reader(&stmts.join(" ")) else {
            continue;
        };
        let mut doc: Vec<&str> = Vec::new();
        for l in lines[..i].iter().rev() {
            let s = l.trim();
            if let Some(d) = s.strip_prefix("///") {
                doc.push(d);
            } else if !s.starts_with("#[") {
                break;
            }
        }
        if doc.is_empty() {
            continue;
        }
        doc.reverse();
        let low = doc.join(" ").to_ascii_lowercase().replace('*', "");
        let says_on = low.contains("default on") || low.contains("default-on");
        let says_off = low.contains("default off") || low.contains("opt-in");
        if default_on && says_off && !says_on {
            out.push((
                shown.to_string(),
                name.to_string(),
                "default-ON reader, doc says opt-in / default OFF",
            ));
        }
        if !default_on && says_on && !says_off {
            out.push((
                shown.to_string(),
                name.to_string(),
                "opt-in reader, doc says default ON",
            ));
        }
    }
}

/// `(bool readers seen, mismatches)` over the workspace.
fn doc_mismatches() -> (usize, Vec<(String, String, &'static str)>) {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut out = Vec::new();
    let mut readers = 0usize;
    for path in &sources {
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        if !text.contains("-> bool {") {
            continue;
        }
        let shown = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/");
        readers += text
            .lines()
            .filter(|l| bool_reader_fn_name(l).is_some())
            .count();
        doc_mismatches_in(&shown, &text, &mut out);
    }
    (readers, out)
}

#[test]
fn every_judged_reader_doc_states_its_default() {
    let (readers, found) = doc_mismatches();
    assert!(
        readers > 1000,
        "only {readers} bool readers seen: the walk is not reaching the workspace"
    );
    let unexpected: Vec<String> = found
        .iter()
        .filter(|(path, name, _)| {
            !KNOWN_DOC_MISMATCHES
                .iter()
                .any(|(p, n)| path.ends_with(p) && name == n)
        })
        .map(|(path, name, what)| format!("{path}: {name}: {what}"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{} reader doc(s) contradict the reader's default. A flip changes the reader, \
         the flag row's `off_word` AND the `///` polarity clause.\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
}

#[test]
fn the_known_doc_mismatches_have_no_dead_rows() {
    let (_, found) = doc_mismatches();
    let dead: Vec<String> = KNOWN_DOC_MISMATCHES
        .iter()
        .filter(|(p, n)| {
            !found
                .iter()
                .any(|(path, name, _)| path.ends_with(p) && name == n)
        })
        .map(|(p, n)| format!("{p}: {n}"))
        .collect();
    assert!(
        dead.is_empty(),
        "KNOWN_DOC_MISMATCHES rows that agree now (delete them): {dead:?}"
    );
}

/// The doc rule on fixed text: a default-ON body under an "opt-in" doc is
/// caught, a doc naming both is not judged, a negated body is not judged, and
/// a call rustfmt broke over three lines is.
#[test]
fn the_reader_doc_rule() {
    let k = concat!("\"CRATON", "VM_X\"");
    let text = [
        "/// Opt-in (perf).".to_string(),
        "fn a_enabled() -> bool {".to_string(),
        format!("    cratonvm_types::flags::runtime_flag_default_on({k})"),
        "}".to_string(),
        "/// Was opt-in; default ON since the flip.".to_string(),
        "pub(crate) fn b_enabled() -> bool {".to_string(),
        format!("    cratonvm_types::flags::runtime_flag_default_on({k})"),
        "}".to_string(),
        "/// Default ON.".to_string(),
        "fn c_enabled() -> bool {".to_string(),
        format!("    !cratonvm_types::flags::runtime_flag_on({k})"),
        "}".to_string(),
        "/// **default ON**".to_string(),
        "pub fn d_enabled() -> bool {".to_string(),
        "    cratonvm_types::flags::runtime_flag_on(".to_string(),
        format!("        {k},"),
        "    )".to_string(),
        "}".to_string(),
    ]
    .join("\n");
    let mut out = Vec::new();
    doc_mismatches_in("fixture.rs", &text, &mut out);
    let names: Vec<&str> = out.iter().map(|(_, n, _)| n.as_str()).collect();
    assert_eq!(names, vec!["a_enabled", "d_enabled"]);
}
