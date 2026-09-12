// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Ratchet: the JIT may not grow new presence-only flag reads.
//!
//! A `runtime_var_os(NAME).is_some()` gate answers "is the variable set", not
//! "is the switch on" — so `NAME=0`, `NAME=false`, `NAME=off` and `NAME=` all
//! turned the switch ON. The JIT had ~360 of them. They were converted to
//! `cratonvm_types::flags::runtime_flag_on`, which applies the
//! `parse::truthy_word` rule to the value; see
//! `jit-presence-only-flag-reads-FIXED.md`.
//!
//! The sites that remain are deliberate: the same name is also read for its
//! VALUE (a number, a mode word, a tri-state), or the read is a test helper
//! that asks about presence on purpose. [`ALLOWED`] is their count, and this
//! test pins it in both directions: a new presence read fails, and converting
//! one of the remaining ones fails too until the number is lowered — so the
//! count can only shrink.
//!
//! The scan is textual: every `runtime_var_os(` outside a whole-line `//`
//! comment, followed — after its balanced argument list and any whitespace,
//! newlines included — by `.is_some()` or `.is_none()`. The needle is assembled
//! at runtime; this file is under `jit/tests/`, outside the scanned tree, but a
//! literal needle would still be one copy-paste away from counting itself.

use std::path::{Path, PathBuf};

/// Presence-only reads left in `jit/src` on 2026-09-12, each on purpose:
///
/// * `ir_verify.rs` `options_from_env_defaults_to_structural_only` — a test
///   helper, `runtime_var_os(n).is_none()`, asking whether the harness left a
///   variable unset before it asserts a default.
/// * `ir_verify.rs` `verify_enabled_matches_the_build_profile_when_unset` —
///   `CRATONVM_JIT_VERIFY_IR` is a tri-state (`env_flag`), and the test is
///   about the UNSET case specifically.
/// * `lib.rs` — `CRATONVM_DBG_JIT_METHOD_STATS`, also parsed by
///   `parse::exactly_one` in the typed configuration.
/// * `tiered.rs` — `CRATONVM_TIER_C2_THRESHOLD`, a number.
/// * `x64/licm.rs` — `CRATONVM_JIT_SAFEPOINT_REG_SPILL`, a mode word
///   (`nostore` / `all`).
const ALLOWED: usize = 5;

fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_sources(&path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// `(line, call text)` for every presence-only read of `needle` in `text`.
///
/// `needle` must end with the opening parenthesis of the call.
fn presence_reads(text: &str, needle: &str) -> Vec<(usize, String)> {
    // Blank whole-line comments but keep their newlines, so line numbers hold.
    let mut code = String::with_capacity(text.len());
    for line in text.lines() {
        if !line.trim_start().starts_with("//") {
            code.push_str(line);
        }
        code.push('\n');
    }
    let bytes = code.as_bytes();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(offset) = code[from..].find(needle) {
        let at = from + offset;
        let mut end = at + needle.len();
        let mut depth = 1usize;
        while end < bytes.len() && depth > 0 {
            match bytes[end] {
                b'(' => depth += 1,
                b')' => depth -= 1,
                _ => {}
            }
            end += 1;
        }
        let rest = code[end..].trim_start();
        if rest.starts_with(".is_some()") || rest.starts_with(".is_none()") {
            let line = code[..at].matches('\n').count() + 1;
            found.push((line, code[at..end].to_string()));
        }
        from = at + needle.len();
    }
    found
}

fn needle() -> String {
    format!("{}(", ["runtime", "var", "os"].join("_"))
}

#[test]
fn the_scanner_sees_split_calls_and_skips_comments() {
    let n = needle();
    let sample = format!(
        "let a = f::{n}\"A\").is_some();\n\
         let b = f::{n}\n    \"B\",\n)\n.is_none();\n\
         // let c = f::{n}\"C\").is_some();\n\
         let d = f::{n}\"D\").and_then(|v| v.into_string().ok());\n\
         let e = f::{n}name).is_none();\n"
    );
    let hits = presence_reads(&sample, &n);
    let lines: Vec<usize> = hits.iter().map(|(line, _)| *line).collect();
    assert_eq!(
        lines,
        vec![1, 2, 8],
        "expected the single-line, the split and the non-literal read, and \
         neither the comment nor the value read; got {hits:?}"
    );
}

#[test]
fn presence_only_flag_reads_in_the_jit_do_not_grow() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rust_sources(&src, &mut files);
    files.sort();
    assert!(
        files.len() > 20,
        "only {} Rust sources under {} — the walk is not reaching the crate, so \
         this ratchet would pass vacuously",
        files.len(),
        src.display()
    );

    let needle = needle();
    let mut sites = Vec::new();
    for path in &files {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) => panic!("cannot read {}: {e}", path.display()),
        };
        let shown = path
            .strip_prefix(&src)
            .unwrap_or(path)
            .display()
            .to_string()
            .replace('\\', "/");
        for (line, call) in presence_reads(&text, &needle) {
            sites.push(format!("jit/src/{shown}:{line}: {call}"));
        }
    }

    assert!(
        sites.len() <= ALLOWED,
        "{} presence-only flag read(s) in jit/src, allowed {ALLOWED}.\n\n\
         `NAME=0` turns a presence-only switch ON. Read a boolean switch with \
         `cratonvm_types::flags::runtime_flag_on(NAME)` (and `!runtime_flag_on` \
         for an opt-out). If this read really must mean \"set at all\", say why \
         at the site and raise ALLOWED here.\n\n  {}",
        sites.len(),
        sites.join("\n  ")
    );
    assert_eq!(
        sites.len(),
        ALLOWED,
        "a presence-only read was removed — lower ALLOWED to {} so the ratchet \
         keeps the ground it gained, and drop its row from the doc comment.\n\n  {}",
        sites.len(),
        sites.join("\n  ")
    );
}
