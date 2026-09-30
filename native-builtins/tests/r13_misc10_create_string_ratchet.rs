// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! INTERNED-COMPUTED-TEXT RATCHET (round 13 wave 10, lane misc10; proposal
//! BD2-1 of `docs/internal/jit-proposals/jit-r13-bigdec2-proposals-RETIRED-20260929.md`).
//!
//! `NativeHeapAccess::create_string` interns: on the VM every distinct text it
//! is handed is inserted into `shared.mem.string_pool`, a strong GC root that
//! is never pruned (`r13w8-bigdec-dynamic-strings-interned-into-a-never-pruned-root-FIXED-20260929.md`).
//! Text computed from arguments therefore leaks one `String` (plus a Rust key
//! and a root) per distinct value for the life of the VM, and answers `==` to
//! an equal literal where HotSpot returns a fresh `String`. Rounds 13 waves
//! 8-9 removed the live number-formatting and `toString` families one at a
//! time; this gate stops new ones arriving while the structural fix (design A:
//! hit-or-fresh `create_string` plus `intern_string`) is pending.
//!
//! What it counts, per crate: every `.create_string(` whose first
//! non-whitespace argument character is not `"` -- i.e. every call that is not
//! handed a plain string literal. Literals are bounded (they are the same few
//! texts forever) and are not counted. The count is EXACT: a new computed-text
//! site fails, and so does removing one, so the baseline tightens deliberately.
//!
//! When it fails:
//! * a NEW site: use `create_string_uninterned` (or
//!   `create_string_uninterned_gc_safe`) for computed text; use
//!   `intern_string` only where Java requires identity with literals
//!   (`String.intern()`, class/member/stack-trace names), which is not counted;
//! * a REMOVED site: lower the baseline below to the printed number.
//!
//! Blind spots, stated: a call through a local helper that forwards to
//! `create_string` (count the helper once, not its callers); `create_string`
//! spelled on a new line after the `.` (the dot must be adjacent). Comments
//! that quote a call are counted, deterministically.

use std::path::{Path, PathBuf};

/// `(crate source dir relative to this crate's manifest, exact count)`.
///
/// `src` 1437 -> 1429 (2026-09-29, JIT round 13 wave 13, lane bigdec4): the eight
/// computed-text sites `r13w8-bigdec-dynamic-strings-interned-into-a-never-pruned-root`
/// listed (5 in `lang_math.rs`, 3 in `math_bignum.rs`) moved to `intern_string` or
/// `create_string_uninterned`.
/// `src` 1429 -> 1412 (JIT round 14 waves 1-2, lanes compat and trace): name producers
/// (`Class.getName`, member names, stack-trace and StackWalker names) moved to `intern_string`.
/// `src` 1412 -> 1407 (JIT round 14 wave 3, lane compat3): `Class.getPackageName()` and
/// `Package.getName()` intern their names, as HotSpot does (`Class.java:1156`, `NamedPackage.java:51`).
const BASELINE: &[(&str, usize)] = &[
    ("src", 1405),
    ("../native-collections/src", 9),
    ("../native-io/src", 101),
];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Calls of `.create_string(` in `text` not handed a string literal.
fn computed_sites(text: &str) -> usize {
    const NEEDLE: &str = ".create_string(";
    let mut count = 0;
    let mut rest = text;
    while let Some(at) = rest.find(NEEDLE) {
        rest = &rest[at + NEEDLE.len()..];
        match rest.chars().find(|c| !c.is_whitespace()) {
            Some('"') | None => {}
            Some(_) => count += 1,
        }
    }
    count
}

#[test]
fn computed_create_string_sites_do_not_grow() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for (rel, expected) in BASELINE {
        let root = manifest.join(rel);
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        assert!(!files.is_empty(), "{}: no Rust sources found (gate is blind)", root.display());
        files.sort();
        let mut per_file: Vec<(usize, String)> = Vec::new();
        let mut total = 0usize;
        for file in &files {
            let text = std::fs::read_to_string(file)
                .unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let n = computed_sites(&text);
            if n > 0 {
                per_file.push((n, file.strip_prefix(&root).unwrap_or(file).display().to_string()));
            }
            total += n;
        }
        if total != *expected {
            per_file.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            let top: Vec<String> = per_file
                .iter()
                .take(15)
                .map(|(n, f)| format!("    {n:5}  {f}"))
                .collect();
            failures.push(format!(
                "{rel}: {total} computed-text `create_string` sites, baseline {expected}.\n  \
                 {}\n  largest files:\n{}",
                if total > *expected {
                    "GREW: a new site interns computed text into the never-pruned pool; use \
                     `create_string_uninterned` (or `intern_string` where Java requires identity)."
                } else {
                    "SHRANK: lower the baseline in this file to the number above."
                },
                top.join("\n")
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}

#[test]
fn the_counter_matches_the_rule() {
    assert_eq!(computed_sites(r#"ctx.create_string("lit")"#), 0);
    assert_eq!(computed_sites("ctx.create_string(\n    \"lit\",\n)"), 0);
    assert_eq!(computed_sites("ctx.create_string(&format!(\"{x}\"))"), 1);
    assert_eq!(computed_sites("ctx.create_string(\n    &name,\n)"), 1);
    assert_eq!(computed_sites("ctx.create_string_uninterned(&name)"), 0);
    assert_eq!(computed_sites("ctx.intern_string(&name)"), 0);
    assert_eq!(computed_sites("fn create_string(&mut self, t: &str)"), 0);
    assert_eq!(computed_sites("a.create_string(&x); b.create_string(\"y\"); c.create_string(z)"), 2);
}
