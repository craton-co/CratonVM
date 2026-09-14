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
        // Track whether we are inside an eprintln!/println! invocation.
        // Simple heuristic: for every line, if it textually contains
        // `eprintln!(` or `println!(`, scan a small window (that line plus
        // the next 3 lines joined) for any forbidden tag prefix.
        let lines: Vec<&str> = contents.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !(line.contains("eprintln!(") || line.contains("println!(")) {
                continue;
            }
            let window_end = (i + 4).min(lines.len());
            let window = lines[i..window_end].join("\n");
            for tag in FORBIDDEN_TAGS {
                if window.contains(tag) {
                    hits.push(format!(
                        "{}:{}: forbidden tag {} in {}!",
                        file.display(),
                        i + 1,
                        tag,
                        if line.contains("eprintln!(") {
                            "eprintln"
                        } else {
                            "println"
                        }
                    ));
                    break;
                }
            }
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
