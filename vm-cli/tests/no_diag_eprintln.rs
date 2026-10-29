// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RJ.1 CI gate: forbid reintroduction of investigative debug-trace
//! `eprintln!` / `println!` markers across the workspace.
//!
//! These markers (`[DIAG-*]`, `[TRACE-*]`, `[SB-TRACE]`, `[INV-TRACE]`,
//! `[IO]`, `[LMF]`, `[FIS.*]`, `[FILE.*]`, `[GC-GUARD]`, `[PANIC]`,
//! `[TRACE-CL]`) were added during KC16 boot-chain investigation. They
//! pollute stderr on every run and confuse users. Structured diagnostics
//! belong in `tracing::{debug,info,warn,error}!`.
//!
//! The test walks the workspace source tree (excluding `target/`, `scripts/`,
//! and `docs/`) and greps for any `eprintln!` / `println!` containing one of
//! the forbidden tag prefixes. Any hit fails the test with the file:line of
//! each offender.
//!
//! COMMENT LINES ARE NOT SCANNED — see [`scan`], which carries the argument and
//! the incident that prompted it. [`the_scanner_sees_code_and_not_prose`] is the
//! positive control for that exclusion; run it and this gate together, because
//! the gate alone asserts an absence and a matcher that matches nothing passes
//! it trivially.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at vm-cli; go up one.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("vm-cli must have a parent")
        .to_path_buf()
}

fn should_skip(path: &Path) -> bool {
    let s = path.to_string_lossy();
    let skip_dirs = [
        "/target",
        "\\target",
        "/target-",
        "\\target-",
        "/scripts",
        "\\scripts",
        "/docs",
        "\\docs",
        "/.git",
        "\\.git",
        "/tests/no_diag_eprintln.rs",
        "\\tests\\no_diag_eprintln.rs",
    ];
    skip_dirs.iter().any(|d| s.contains(d))
}

fn walk_rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if should_skip(dir) {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if should_skip(&path) {
            continue;
        }
        if path.is_dir() {
            walk_rust_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

/// Tag prefixes that must not appear inside a println!/eprintln! literal.
/// Each entry is matched as a substring starting with `[`.
const FORBIDDEN_TAGS: &[&str] = &[
    "[DIAG-",
    "[DIAG ",
    "[TRACE-",
    "[SB-TRACE",
    "[SB-REG",
    "[INV-TRACE",
    "[IO]",
    "[LMF]",
    "[FIS.",
    "[FILE.",
    "[GC-GUARD]",
    "[PANIC]",
    "[TRACE-CL",
];

/// Is this line Rust prose rather than Rust code?
///
/// Only the `//` form is recognised, and only when it starts the line. That is
/// deliberately narrow: a `/* ... */` block or a trailing `// ...` on a real
/// statement leaves the code part of the line intact, and the code part is what
/// this gate is about. Being narrow here costs nothing — a forbidden tag inside
/// a block comment is still prose — and it keeps the predicate from ever
/// swallowing a line that has executable content on it.
fn is_line_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// One forbidden print, as `(line number, tag)`. Line numbers are 1-based.
///
/// Split out of the gate test so it can be driven by
/// [`the_scanner_sees_code_and_not_prose`] over inputs whose right answer is
/// known. The gate walks thousands of files and asserts an ABSENCE; on its own
/// it cannot tell "nothing is wrong" from "the matcher stopped matching", and
/// this function was made more permissive on 2026-09-22 (see below), which is
/// exactly the change that makes that distinction worth a test. Same argument
/// `scripts/check-no-diag-prints.sh` makes at length for its own `SENTINEL`.
///
/// The heuristic: for every line that textually contains `eprintln!(` or
/// `println!(`, join that line and the next three and look for a forbidden tag
/// prefix. The window exists because the format string may start on a later
/// line —
///
/// ```text
/// eprintln!(
///     "[DIAG-x] ..."
/// );
/// ```
///
/// — which is the one shape `scripts/check-no-diag-prints.sh`'s single-line
/// regex cannot see. The two gates also carry different tag lists, so neither
/// replaces the other.
///
/// COMMENT LINES ARE EXCLUDED, both as a window's first line and inside a
/// window. Without that this gate matched its own documentation: a block comment
/// in `vm-cli/src/main.rs` mentions `eprintln!(...)` inline and, three lines
/// later, names the forbidden bring-up tags so a reader knows which ones they
/// are — so the window opened on prose and found a tag in prose. The test had
/// been red on `dev` since the commit that wrote that comment (afd6378fc), while
/// `scripts/check-no-diag-prints.sh` stayed green on the same tree because its
/// regex anchors the tag to the macro's own opening paren. A gate that cannot
/// survive being described is not enforceable: the only way to keep this one
/// green was to stop writing down what it forbids.
///
/// What this gives up is a forbidden tag that reaches a print ONLY through a
/// commented-out line — which is not a print. What it keeps is every tag in live
/// code, single-line or wrapped, which is the whole population the gate exists
/// for.
fn scan(contents: &str) -> Vec<(usize, &'static str)> {
    let lines: Vec<&str> = contents.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if is_line_comment(line) {
            continue;
        }
        if !(line.contains("eprintln!(") || line.contains("println!(")) {
            continue;
        }
        let window_end = (i + 4).min(lines.len());
        let window = lines[i..window_end]
            .iter()
            .filter(|l| !is_line_comment(l))
            .copied()
            .collect::<Vec<&str>>()
            .join("\n");
        for tag in FORBIDDEN_TAGS {
            if window.contains(tag) {
                hits.push((i + 1, *tag));
                break;
            }
        }
    }
    hits
}

/// POSITIVE CONTROL for [`scan`] itself.
///
/// The gate below asserts that a large tree contains none of these tags, and a
/// broken matcher passes that assertion trivially. This drives the matcher over
/// inputs whose right answer is known, including both halves of the 2026-09-22
/// comment-exclusion change: the two cases it must now IGNORE, and the ones it
/// must still CATCH.
#[test]
fn the_scanner_sees_code_and_not_prose() {
    // 1. A live single-line print. The ordinary case, and it must be caught.
    let hits = scan("fn f() {\n    eprintln!(\"[DIAG-boot] x\");\n}\n");
    assert_eq!(
        hits,
        vec![(2, "[DIAG-")],
        "a forbidden tag in a live single-line print must be caught"
    );

    // 2. A live print whose format string is on a LATER line. This is the only
    //    shape `scripts/check-no-diag-prints.sh` cannot see, so it is the whole
    //    reason this gate carries a window at all. Losing it would make the two
    //    gates redundant instead of complementary.
    let hits = scan("fn f() {\n    eprintln!(\n        \"[TRACE-cl] x\",\n    );\n}\n");
    assert_eq!(
        hits,
        vec![(2, "[TRACE-")],
        "a wrapped print must still be caught through the window"
    );

    // 3. A COMMENTED print whose window names a tag. This is the false positive
    //    that had `dev` red: prose describing the gate, not a print.
    let hits = scan(
        "// A bare `eprintln!(\"{}\", report())` is the idiom here, and it is\n\
         // clean under the gate, which forbids the bring-up tags\n\
         // `[DIAG-*]`/`[SB-TRACE]` and nothing else.\n",
    );
    assert!(
        hits.is_empty(),
        "a commented-out print must not open a window: got {hits:?}"
    );

    // 4. A LIVE print with a tag named in a comment BELOW it. The window must
    //    not reach into prose either, or the same false positive returns
    //    wearing a different hat — and this is the half case 3 does not cover.
    let hits = scan(
        "fn f() {\n    eprintln!(\"[cratonvm] fine\");\n    // never write [DIAG-x] here\n}\n",
    );
    assert!(
        hits.is_empty(),
        "a tag named only in a comment inside the window is prose: got {hits:?}"
    );

    // 5. A trailing comment does NOT excuse the code on the same line: the
    //    predicate is `starts_with`, so this line is still scanned.
    let hits = scan("fn f() {\n    eprintln!(\"[DIAG-boot] x\"); // why\n}\n");
    assert_eq!(
        hits,
        vec![(2, "[DIAG-")],
        "a trailing comment must not exempt the statement it follows"
    );

    // 6. An ordinary user-visible line is not a hit. Without this the other
    //    cases could all pass on a matcher that flags every print.
    let hits = scan("fn f() {\n    eprintln!(\"[cratonvm] JIT method stats: ...\");\n}\n");
    assert!(hits.is_empty(), "`[cratonvm]` is allowed: got {hits:?}");
}

#[test]
fn no_unconditional_diag_eprintln() {
    let root = workspace_root();
    let mut files = Vec::new();
    walk_rust_files(&root, &mut files);
    assert!(
        !files.is_empty(),
        "workspace walk found zero .rs files — walker is broken"
    );

    let mut hits: Vec<String> = Vec::new();
    for file in &files {
        let Ok(contents) = fs::read_to_string(file) else {
            continue;
        };
        for (line_no, tag) in scan(&contents) {
            hits.push(format!(
                "{}:{}: forbidden tag {}",
                file.display(),
                line_no,
                tag
            ));
        }
    }

    assert!(
        hits.is_empty(),
        "RJ.1 CI gate: found {} forbidden debug-trace print(s):\n{}\n\
         Remove them or convert to tracing::debug!(target: \"cratonvm::...\", ...).",
        hits.len(),
        hits.join("\n")
    );
}
