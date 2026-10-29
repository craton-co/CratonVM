// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! RAW-THROWABLE-MINT RATCHET (JIT round 21 wave 19, lane jnilazy; proposal
//! JO4-1 of `docs/internal/jit-proposals/jit-r15w4-jdkonly4-proposals-RETIRED-20261003.md`,
//! page `docs/internal/fixed-bugs/jr21-jnilazy-throwable-builder-and-ratchet-FIXED-20261027.md`).
//!
//! Natively thrown exceptions have ONE builder:
//! `cratonvm_native_api::throwable_builder` (`throw_new`, `construct_new`).
//! It holds the rule rounds 15 waves 3-4 learned: build through the
//! constructor with the arguments rooted across `<init>` and the forwarded
//! reference answered, and propagate a constructor that THREW instead of
//! raising a fallback. The shapes that break it:
//!
//! * `.new_object("<throwable class>")` -- the bare allocation: no `<init>`,
//!   or a separate `invoke("<init>")` that answers the pre-constructor
//!   address (stale after a moving collection inside the constructor) and
//!   throws a half-built object when the constructor fails;
//! * `.new_object_initialized("<throwable class>", ..)` -- rooted, but it
//!   answers `Err` both for "no such class" and for "the constructor threw",
//!   so the site raises its fallback where HotSpot propagates;
//! * `.construct_throwable(` called directly -- the right primitive, but a
//!   second copy of the three-way rule outside the builder (the shape the two
//!   `construct_thrown` twins had before they became forwards).
//!
//! What it counts, per crate source tree: every `.new_object(` and
//! `.new_object_initialized(` whose first argument is a string literal naming
//! a class that ends in `Exception`, `Error` or `Throwable`, and every
//! `.construct_throwable(` -- outside `native-api/src/throwable_builder.rs`.
//! The count is EXACT, frozen at the tree of 2026-10-14 with zero slack: a new
//! site fails, and so does a converted one, so the baseline tightens
//! deliberately.
//!
//! When it fails:
//! * GREW: build the exception with `cratonvm_native_api::throw_new` (or
//!   `throwable_builder::construct_new` when the site has a mint to fall back
//!   on, as `throwable_mint::construct_or_mint` does);
//! * SHRANK: lower the baseline below to the printed number, and say which
//!   site moved in the comment.
//!
//! Blind spots, stated: a class name held in a variable (`new_object(cls)`
//! followed by `invoke(cls, "<init>", ..)`) is not seen; nor is a call
//! spelled with the `.` on a previous line, nor a literal that is a raw
//! string. Comments that quote a call are counted, deterministically. The
//! VM crate (`vm/src`) builds its own exceptions through
//! `runtime::exceptions` and is out of this gate's scope.

use std::path::{Path, PathBuf};

/// `(crate source dir relative to this crate's manifest, exact count)`.
///
/// Frozen 2026-10-14 (JIT round 21 wave 19, lane jnilazy) after the two
/// `construct_thrown` twins (`throwable_mint.rs`, `native-io`'s
/// `nio_native.rs`) and `native-api`'s `refusal_to_java_failure` moved onto
/// the builder. Re-frozen 2026-10-26 when the ratchet was re-landed on dev
/// (dev had grown `src` 104 -> 110 and `native-awt` 1 -> 2 raw mints in the
/// ten days the round's branch was off it).
const BASELINE: &[(&str, usize)] = &[
    ("../native-api/src", 0),
    ("src", 110),
    ("../native-io/src", 23),
    ("../native-collections/src", 4),
    ("../native-builtins-crypto/src", 0),
    ("../native-builtins-security/src", 0),
    ("../native-awt/src", 2),
];

/// The builder itself, relative to its crate's source dir.
const EXEMPT: &[(&str, &str)] = &[("../native-api/src", "throwable_builder.rs")];

/// The call needles a raw mint is spelled with.
const NEEDLES: &[&str] = &[
    ".new_object(",
    ".new_object_initialized(",
    ".construct_throwable(",
];

/// Class-name suffixes taken as a throwable.
const THROWABLE_SUFFIXES: &[&str] = &["Exception", "Error", "Throwable"];

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

/// The raw throwable mints in `text` (see the module docs).
fn raw_mints(text: &str) -> usize {
    let mut count = 0;
    for needle in NEEDLES {
        let mut rest = text;
        while let Some(at) = rest.find(needle) {
            rest = &rest[at + needle.len()..];
            if *needle == ".construct_throwable(" {
                count += 1;
                continue;
            }
            let arg = rest.trim_start();
            let Some(lit) = arg.strip_prefix('"') else {
                continue;
            };
            let Some(end) = lit.find('"') else {
                continue;
            };
            let class = &lit[..end];
            if THROWABLE_SUFFIXES.iter().any(|s| class.ends_with(s)) {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn raw_throwable_mints_do_not_grow() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut failures = Vec::new();
    for (rel, expected) in BASELINE {
        let root = manifest.join(rel);
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        assert!(
            !files.is_empty(),
            "{}: no Rust sources found (gate is blind)",
            root.display()
        );
        files.sort();
        let mut per_file: Vec<(usize, String)> = Vec::new();
        let mut total = 0usize;
        for file in &files {
            let name = file
                .strip_prefix(&root)
                .unwrap_or(file)
                .display()
                .to_string()
                .replace('\\', "/");
            if EXEMPT.iter().any(|(dir, f)| dir == rel && *f == name) {
                continue;
            }
            let text =
                std::fs::read_to_string(file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
            let n = raw_mints(&text);
            if n > 0 {
                per_file.push((n, name));
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
                "{rel}: {total} raw throwable mints, baseline {expected}.\n  {}\n  largest files:\n{}",
                if total > *expected {
                    "GREW: build the exception with `cratonvm_native_api::throw_new` \
                     (`throwable_builder::construct_new` for a site with a mint fallback)."
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
    assert_eq!(
        raw_mints(r#"ctx.new_object("java/lang/IllegalStateException")"#),
        1
    );
    assert_eq!(raw_mints(r#"ctx.new_object("java/lang/StringBuilder")"#), 0);
    assert_eq!(
        raw_mints("ctx.new_object_initialized(\n    \"java/lang/NoSuchFieldError\",\n    d, a)"),
        1
    );
    assert_eq!(
        raw_mints(r#"ctx.new_object_initialized("java/lang/Throwable", d, a)"#),
        1
    );
    assert_eq!(raw_mints(r#"ctx.new_object_initialized(class, d, a)"#), 0);
    assert_eq!(raw_mints(r#"ctx.construct_throwable(class, d, a)"#), 1);
    assert_eq!(raw_mints("fn construct_throwable(&mut self)"), 0);
    assert_eq!(raw_mints("fn new_object(&mut self, c: &str)"), 0);
    assert_eq!(
        raw_mints(r#"cratonvm_native_api::throw_new(ctx, "java/lang/Error", d, a, f)"#),
        0
    );
    assert_eq!(
        raw_mints(r#"a.new_object("x/AException"); b.new_object("x/B"); c.construct_throwable(z)"#),
        2
    );
}
