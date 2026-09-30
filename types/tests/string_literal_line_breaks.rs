// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A string literal must not carry a line break that lost its `\`.
//!
//! A Rust literal continued over several lines needs a trailing `\` on each
//! line, so the newline and the next line's indentation are dropped. Some
//! authoring step in this repository joins such a literal onto one physical
//! line WITHOUT removing the indentation, and drops the `\` too: the literal
//! then reads `"... that is                          neither ..."` (shortened
//! here), and that run of spaces reaches the user. It reached Java: twenty
//! `Unsafe` `IllegalArgumentException` messages, a `Cipher.doFinal`
//! `IllegalStateException`, three `SignatureException`s and a
//! `KeyStoreException` carried one. `git log -S` shows the literals were
//! committed that way, so the defect recurs rather than decays. The census is
//! in `docs/internal/fixed-bugs/interpreter-L4-string-literals-lost-line-continuations-FIXED-20260924.md`.
//!
//! # What this asserts
//!
//! * **No literal carries a run of [`HARD_RUN`] or more spaces between two
//!   non-space characters.** Nothing legitimate needs one inside a single
//!   literal; column alignment in a table is done with `{:>N}` or with runs
//!   well short of this.
//! * **The number of literals carrying a run of [`SOFT_RUN`] or more spaces
//!   does not grow past [`SOFT_BASELINE`].** Most of them are the same joined
//!   continuation at a shallower indent (test-assertion text, mostly), and a
//!   few are deliberate column alignment, so this is a ratchet rather than a
//!   ban. Lower the baseline when you fix some; never raise it.
//!
//! A run that follows an escape (`"\n        at ..."`) is layout, not a lost
//! break, and is not counted; nor is a run touching either end of the literal.
//!
//! # How to make it green
//!
//! Put the break back: end the line with ` \` and continue on the next line
//! at the literal's indentation (the leading whitespace of a continued line is
//! dropped), or collapse the run to a single space.

use std::path::{Path, PathBuf};

/// A run this long inside one literal is always a joined line break.
const HARD_RUN: usize = 20;

/// The ratchet's run length: joined breaks at a shallow indent, plus a few
/// deliberate alignments.
const SOFT_RUN: usize = 8;

/// Literals with a run of at least [`SOFT_RUN`] spaces, measured 2026-09-25
/// after wave 9 put back every joined break (343 before it). All 30 left are
/// deliberate column alignment: dev's `[GC] tenuring: - age   3:        700
/// bytes, ...` fixture, the `size_of` report in `vm/src/runtime/frame.rs`,
/// the soak-test summary, the `jmap -histo`-style header and metaspace lines
/// in `vm/src/runtime/serviceability.rs`, `vm-cli`'s settings and diff
/// tables, `types/src/error.rs`'s `reason:`/`module:` rows (and the test that
/// pins one), the thread-dump legend, a crypto timing table and a GC test's
/// census line. May only go down.
const SOFT_BASELINE: usize = 30;

/// Directory names never scanned: build output, VCS metadata, the Java
/// application harnesses, and third-party code vendored verbatim.
const SKIPPED_DIRS: &[&str] = &["target", ".git", "apps", "node_modules", "vendor"];

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

/// The contents (between the quotes) of every `"..."` literal that opens and
/// closes on `line`. Escapes are skipped pairwise; a quote with no closing
/// partner on the line is stepped over.
fn literals(line: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < line.len() {
        if line[i] != b'"' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        let mut closed = None;
        while j < line.len() {
            match line[j] {
                b'\\' => j += 2,
                b'"' => {
                    closed = Some(j);
                    break;
                }
                _ => j += 1,
            }
        }
        match closed {
            Some(end) => {
                out.push(&line[i + 1..end]);
                i = end + 1;
            }
            None => i += 1,
        }
    }
    out
}

/// The longest run of spaces in `content` that has a non-space byte on both
/// sides and does not follow an escape such as `\n`.
fn longest_joined_run(content: &[u8]) -> usize {
    let mut best = 0;
    let mut i = 0;
    while i < content.len() {
        if content[i] != b' ' {
            i += 1;
            continue;
        }
        let mut j = i;
        while j < content.len() && content[j] == b' ' {
            j += 1;
        }
        let after_escape = i >= 2 && content[i - 2] == b'\\';
        if i > 0 && j < content.len() && !after_escape {
            best = best.max(j - i);
        }
        i = j;
    }
    best
}

/// `(site, longest run, excerpt)` for every literal in `text` whose longest
/// joined run is at least `min`.
fn offending_literals(shown: &str, text: &str, min: usize) -> Vec<(String, usize, String)> {
    let mut out = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        if line
            .trim_start_matches(|c: char| c == ' ' || c == '\t')
            .starts_with("//")
        {
            continue;
        }
        for content in literals(line.as_bytes()) {
            let run = longest_joined_run(content);
            if run >= min {
                let excerpt = String::from_utf8_lossy(&content[..content.len().min(100)]);
                out.push((format!("{shown}:{}", index + 1), run, excerpt.into_owned()));
            }
        }
    }
    out
}

#[test]
fn the_scanner_flags_a_joined_break_and_nothing_else() {
    let joined = format!("let m = \"that is{}neither\";", " ".repeat(HARD_RUN + 6));
    assert_eq!(
        offending_literals("x", &joined, HARD_RUN).len(),
        1,
        "{joined}"
    );

    let continued = "let m = \"that is \\\n         neither\";";
    assert!(
        offending_literals("x", continued, SOFT_RUN).is_empty(),
        "{continued}"
    );

    let layout = format!("let m = \"\\n{}at frame\";", " ".repeat(HARD_RUN + 4));
    assert!(
        offending_literals("x", &layout, SOFT_RUN).is_empty(),
        "{layout}"
    );

    let commented = format!("// \"that is{}neither\"", " ".repeat(HARD_RUN + 6));
    assert!(
        offending_literals("x", &commented, SOFT_RUN).is_empty(),
        "{commented}"
    );

    let between = format!("(\"a\",{}\"b\")", " ".repeat(HARD_RUN + 6));
    assert!(
        offending_literals("x", &between, SOFT_RUN).is_empty(),
        "a run BETWEEN two literals is table alignment: {between}"
    );
}

#[test]
fn no_string_literal_carries_a_joined_line_break() {
    let root = workspace_root();
    let mut sources = Vec::new();
    rust_sources(&root, &mut sources);
    assert!(
        sources.len() > 500,
        "only found {} Rust sources under {} — the walk is not reaching the \
         workspace, so this guard would pass vacuously",
        sources.len(),
        root.display()
    );

    let this_file = Path::new(file!()).file_name().expect("test file name");
    let mut hard = Vec::new();
    let mut soft = 0usize;
    for path in &sources {
        if path.file_name() == Some(this_file) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let relative = path.strip_prefix(&root).unwrap_or(path);
        let shown = relative.display().to_string().replace('\\', "/");
        for (site, run, excerpt) in offending_literals(&shown, &text, SOFT_RUN) {
            soft += 1;
            if run >= HARD_RUN {
                hard.push(format!("  {site}: {run} spaces in \"{excerpt}\""));
            }
        }
    }

    assert!(
        hard.is_empty(),
        "{} string literal(s) carry a run of {HARD_RUN}+ spaces — a line break whose \
         `\\` was lost when the literal was joined onto one line. End the line with \
         ` \\` and continue on the next, or collapse the run to one space:\n{}",
        hard.len(),
        hard.join("\n")
    );
    assert!(
        soft <= SOFT_BASELINE,
        "{soft} string literals carry a run of {SOFT_RUN}+ spaces, above the ratchet of \
         {SOFT_BASELINE}: a new literal joined a line break without its `\\`. Put the \
         break back (list them with this file's scanner at SOFT_RUN)."
    );
    if soft < SOFT_BASELINE {
        eprintln!(
            "note: {soft} literals with a {SOFT_RUN}+ space run, below the ratchet of \
             {SOFT_BASELINE}; lower SOFT_BASELINE to lock the gain in"
        );
    }
}
