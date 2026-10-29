// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: no NEW direct `eprintln!("[cratonvm] ...")` site.
//!
//! HotSpot 25 prints nothing of its own on stderr when a program ends, and both
//! CratonVM modes must match it byte for byte. The `[cratonvm]` exit narration
//! (census rows, summaries, warnings) therefore goes through ONE policy,
//! `cratonvm_types::exit_report!` (silent unless `CRATONVM_DBG=exit-census` or
//! `CRATONVM_EXIT_REPORT_FILE`), proposal
//! `docs/feature-designs/jit-ir-optimizations.md` item 1.
//!
//! Which direct prints are "on an exit path" and "unconditional" is not
//! decidable from the text, so this gate freezes the population instead: the
//! number of direct `eprintln!` / `eprint!` statements whose format text carries
//! a `[cratonvm]` tag, per file (`exit-report-baseline.txt`). A file may not
//! grow. A new line either goes through `exit_report!` (not counted) or, when it
//! is a launcher error HotSpot also prints in its own words, raises the file's
//! number in the baseline in the same commit, with the reason in the commit.
//! A file that shrank should lower its number (the test says so, without
//! failing, so concurrent lanes removing lines do not race each other).
//!
//! Matching rule, shared with `no_diag_eprintln.rs`: a non-comment line that
//! contains `eprintln!(` or `eprint!(`, joined with the next three non-comment
//! lines, contains `"[cratonvm]`. [`the_counter_sees_code_and_not_prose`] is
//! the positive control, so the gate cannot pass on a matcher that matches
//! nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

const BASELINE: &str = include_str!("exit-report-baseline.txt");

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm-cli must have a parent")
        .to_path_buf()
}

fn should_skip(rel: &str) -> bool {
    let first = rel.split('/').next().unwrap_or("");
    first.starts_with("target")
        || matches!(first, "scripts" | "docs" | ".git" | ".claude" | "node_modules")
        || rel == "vm-cli/tests/exit_report_ratchet.rs"
        || rel == "vm-cli/tests/no_diag_eprintln.rs"
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if should_skip(&rel) {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push((rel, path));
        }
    }
}

fn is_line_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// The number of direct `[cratonvm]` prints in `contents`.
fn count(contents: &str) -> usize {
    let lines: Vec<&str> = contents.lines().collect();
    let mut n = 0;
    for (i, line) in lines.iter().enumerate() {
        if is_line_comment(line) {
            continue;
        }
        if !(line.contains("eprintln!(") || line.contains("eprint!(")) {
            continue;
        }
        let end = (i + 4).min(lines.len());
        let window = lines[i..end]
            .iter()
            .filter(|l| !is_line_comment(l))
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        if window.contains("\"[cratonvm]") {
            n += 1;
        }
    }
    n
}

fn baseline() -> BTreeMap<&'static str, usize> {
    BASELINE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (path, n) = l.rsplit_once(' ').expect("`<path> <count>`");
            (path, n.parse().expect("count"))
        })
        .collect()
}

#[test]
fn the_counter_sees_code_and_not_prose() {
    assert_eq!(count("fn f() {\n    eprintln!(\"[cratonvm] x\");\n}\n"), 1);
    assert_eq!(count("fn f() {\n    eprintln!(\n        \"[cratonvm] wrapped {}\",\n        1\n    );\n}\n"), 1);
    assert_eq!(count("fn f() {\n    eprint!(\"[cratonvm] no newline\");\n}\n"), 1);
    // Prose and the policy macro are not counted.
    assert_eq!(count("// eprintln!(\"[cratonvm] in a comment\");\n"), 0);
    assert_eq!(
        count("fn f() {\n    cratonvm_types::exit_report!(\"[cratonvm] gated\");\n}\n"),
        0
    );
    // Another tag is not this gate's business.
    assert_eq!(count("fn f() {\n    eprintln!(\"[JIT] x\");\n}\n"), 0);
}

#[test]
fn the_baseline_is_sorted_and_positive() {
    let rows: Vec<&str> = BASELINE
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    for pair in rows.windows(2) {
        assert!(pair[0] < pair[1], "baseline out of order: {} / {}", pair[0], pair[1]);
    }
    for (path, n) in baseline() {
        assert!(n > 0, "{path}: a zero row is a deleted row");
    }
}

#[test]
fn no_new_direct_cratonvm_print() {
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root, &root, &mut files);
    assert!(files.len() > 100, "workspace walk found {} files", files.len());
    let frozen = baseline();
    let mut grew = Vec::new();
    let mut total = 0usize;
    for (rel, path) in &files {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        let n = count(&text);
        total += n;
        let allowed = frozen.get(rel.as_str()).copied().unwrap_or(0);
        if n > allowed {
            grew.push(format!("{rel}: {n} direct [cratonvm] print(s), baseline {allowed}"));
        } else if n < allowed {
            eprintln!(
                "note: {rel} has {n} direct [cratonvm] print(s), baseline {allowed}: lower it in \
                 vm-cli/tests/exit-report-baseline.txt"
            );
        }
    }
    // The walk must see the population it freezes, or the gate is vacuous.
    assert!(total > 100, "only {total} direct [cratonvm] prints seen: matcher or walk broken");
    assert!(
        grew.is_empty(),
        "new direct `[cratonvm]` print(s) on stderr; HotSpot prints none of them. Route exit \
         narration through `cratonvm_types::exit_report!` (CRATONVM_DBG=exit-census), or, for a \
         launcher error HotSpot also reports, raise the file's number in \
         vm-cli/tests/exit-report-baseline.txt with the reason:\n{}",
        grew.join("\n")
    );
}
