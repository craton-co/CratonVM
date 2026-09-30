// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `vm` crate twin of the `gc` crate's collapsed-literal guard
//! (`gc/src/vm_heap.rs::gc_summary_key_tests::no_gc_crate_literal_carries_a_collapsed_line_continuation`),
//! over the VM's GC files: `vm/src/memory/`, the GC files of
//! `vm/src/runtime/interpreter/` (`gc_and_alloc.rs` and its test directory,
//! `gc_events.rs`) and the STW protocol files of `vm/src/threading/`
//! (`thread_registry.rs`, `gc_barrier.rs`).
//!
//! gc-common round 2026-09-23, wave 6, lane F6 — the last half of
//! `common-w3f-collapsed-string-continuations-in-gc-messages` (handoff
//! `handoff-w5f-collapsed-literals-outside-f5.md` §4). The orchestrator's
//! wave-5 merge collapsed the last 26 sites in these files; this keeps them
//! at zero.
//!
//! Two shapes of the same authoring defect, both with an EMPTY baseline:
//!
//! * a `\` line continuation that lost its backslash — the next line's
//!   indentation stays in the text as a run of 12+ spaces between two words;
//! * a `\` continuation typed as a `\n` escape — the message prints a line
//!   break followed by that indentation.
//!
//! The lexer is `gc`'s `literal_space_run_lines`, copied verbatim: comments,
//! char literals and raw strings are skipped. A pure source scan — it links
//! nothing from the `vm` crate.

use std::path::{Path, PathBuf};

const MIN_RUN: usize = 12;

fn literal_space_run_lines(src: &str, after_newline_escape: bool) -> Vec<usize> {
    let b = src.as_bytes();
    let n = b.len();
    let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80;
    let blank = |c: u8| c == b' ' || c == b'\n' || c == b'\t';
    let mut out: Vec<usize> = Vec::new();
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < n && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            i += 2;
            while i < n && depth > 0 {
                if b[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if b[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            continue;
        }
        if c == b'r'
            && (i == 0 || !ident(b[i - 1]) || (b[i - 1] == b'b' && (i < 2 || !ident(b[i - 2]))))
        {
            let mut j = i + 1;
            while j < n && b[j] == b'#' {
                j += 1;
            }
            if j < n && b[j] == b'"' {
                // `"` then as many `#` as opened it.
                let mut close = vec![b'"'];
                close.resize(j - i, b'#');
                let mut k = j + 1;
                while k < n && !b[k..].starts_with(&close) {
                    k += 1;
                }
                i = (k + close.len()).min(n);
                continue;
            }
        }
        if c == b'\'' {
            if b.get(i + 1) == Some(&b'\\') {
                let mut k = i + 3;
                while k < n && b[k] != b'\'' {
                    k += 1;
                }
                i = (k + 1).min(n);
                continue;
            }
            if i + 2 < n && b[i + 1] < 0x80 && b[i + 2] == b'\'' {
                i += 3;
                continue;
            }
            if i + 1 < n && b[i + 1] >= 0xc0 {
                let mut j = i + 2;
                while j < n && (0x80..=0xbf).contains(&b[j]) {
                    j += 1;
                }
                if j > i + 2 && j < n && b[j] == b'\'' && j - i <= 5 {
                    i = j + 1;
                    continue;
                }
            }
            i += 1; // a lifetime
            continue;
        }
        if c == b'"' {
            let start = i + 1;
            let mut j = start;
            while j < n && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = j.min(n);
            let mut k = start;
            while k < end {
                if b[k] != b' ' {
                    k += 1;
                    continue;
                }
                let run_start = k;
                while k < end && b[k] == b' ' {
                    k += 1;
                }
                let escaped_break = run_start >= start + 2
                    && (b[run_start - 2..run_start] == *b"\\n"
                        || b[run_start - 2..run_start] == *b"\\t");
                let escaped_newline =
                    run_start >= start + 2 && b[run_start - 2..run_start] == *b"\\n";
                let counted = if after_newline_escape {
                    escaped_newline
                } else {
                    run_start > start && !blank(b[run_start - 1]) && !escaped_break
                };
                if k - run_start >= MIN_RUN && counted && k < end && !blank(b[k]) {
                    let line = 1 + b[..run_start].iter().filter(|&&x| x == b'\n').count();
                    if out.last() != Some(&line) {
                        out.push(line);
                    }
                }
            }
            i = end + 1;
            continue;
        }
        i += 1;
    }
    out.dedup();
    out
}

fn collect(path: &Path, out: &mut Vec<PathBuf>) {
    if path.is_file() {
        out.push(path.to_path_buf());
        return;
    }
    for entry in std::fs::read_dir(path).unwrap_or_else(|e| panic!("read {path:?}: {e}")) {
        let p = entry.expect("dir entry").path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|e| e == "rs") {
            out.push(p);
        }
    }
}

/// The scanned set, relative to `vm/`.
const SCANNED: &[&str] = &[
    "src/memory",
    "src/runtime/interpreter/gc_and_alloc.rs",
    "src/runtime/interpreter/gc_and_alloc",
    "src/runtime/interpreter/gc_events.rs",
    "src/threading/thread_registry.rs",
    "src/threading/gc_barrier.rs",
];

#[test]
fn no_vm_gc_literal_carries_a_collapsed_line_continuation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for rel in SCANNED {
        let p = root.join(rel);
        assert!(p.exists(), "fixture: {p:?} must exist (a scanned file moved?)");
        collect(&p, &mut files);
    }
    assert!(files.len() >= 10, "fixture: the scanned set must be found ({files:?})");
    let mut bad: Vec<String> = Vec::new();
    for path in files {
        let src = std::fs::read_to_string(&path).expect("read a vm source file");
        let rel = path
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        let lost = literal_space_run_lines(&src, false);
        if !lost.is_empty() {
            bad.push(format!("{rel}: lost `\\` continuation on lines {lost:?}"));
        }
        let typed = literal_space_run_lines(&src, true);
        if !typed.is_empty() {
            bad.push(format!("{rel}: `\\n`-typed continuation on lines {typed:?}"));
        }
    }
    assert!(
        bad.is_empty(),
        "string literal(s) with a run of 12+ spaces mid-message -- a `\\` line \
         continuation lost its backslash or was typed as `\\n` (end the line with \
         `\\` or use one space): {bad:#?}"
    );
}

#[test]
fn the_scanner_sees_only_literals() {
    let q = '"';
    let gap = " ".repeat(14);
    let lost = format!("let a = {q}first{gap}second{q};\n");
    let comment = format!("// first{gap}second\n");
    let escaped = format!("let b = {q}one\\n{gap}two{q};\n");
    let raw = format!("let c = r#{q}first{gap}second{q}#;\n");
    let src = format!("{comment}{escaped}{raw}{lost}");
    assert_eq!(literal_space_run_lines(&src, false), vec![4]);
    assert_eq!(literal_space_run_lines(&src, true), vec![2]);
    let short = format!("let d = {q}one\\n    two{q};\nlet e = {q}one\\t{gap}two{q};\n");
    assert!(literal_space_run_lines(&short, true).is_empty());
    assert!(literal_space_run_lines(&short, false).is_empty());
}
