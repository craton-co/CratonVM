// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Every file an `include_bytes!` / `include_str!` names must be tracked.
//!
//! # The failure this exists to prevent
//!
//! `include_bytes!` resolves against the working tree at compile time, not
//! against the repository. A fixture that is present on the author's disk and
//! absent from git therefore produces a crate that builds perfectly for the
//! person who added it and **fails to compile** for everyone else — not a
//! failing test, a build error, in a crate half the workspace depends on.
//!
//! That is not hypothetical. `reader/src/class_reader.rs` included
//! `test_classes/HelloWorld.class`, a path covered by `.gitignore`'s
//! `test_classes/**/*.class`. The remediation that added it recorded the
//! fixture as committed; `git add -A` skips ignored paths without a word, so
//! it never was. `cargo test -p cratonvm-reader` then failed to compile in
//! every fresh checkout for two months while passing on any machine where
//! someone had run `javac` in that directory — which is the worst shape a
//! defect can have, because the people most likely to notice are the least
//! likely to be able to reproduce it.
//!
//! # Why the compiler cannot catch it
//!
//! It catches exactly half. The compiler answers "is this file on this disk";
//! this guard answers "will it be on anyone else's". Both halves are needed
//! and only one of them has a compiler.
//!
//! # What is checked, and what is skipped
//!
//! Only string-literal arguments: `include_bytes!("../tests/fixtures/x.class")`.
//! A computed path — `include_bytes!(concat!(env!("OUT_DIR"), "/x"))` — names a
//! build artefact that is *supposed* to be untracked, and this guard cannot
//! resolve it anyway, so it is skipped rather than guessed at.
//!
//! Sources come from `git ls-files` for the same reason
//! [`doc_citation_paths`](doc_citation_paths.rs) does: `apps/` holds suite
//! checkouts and local scratch, so a directory walk passes or fails depending
//! on what happens to be lying around.
//!
//! # Vendored crates
//!
//! `**/vendor/**` is skipped. `native-builtins/vendor/rustls-cbc` is a
//! minimally-patched rustls consumed as a path dependency, not a workspace
//! member, and the vendoring took the library and not the test suite: its
//! `#[cfg(test)]` modules reference ~30 `testdata/` fixtures that were never
//! copied. That is not a latent break here — `cargo check -p rustls
//! --all-targets` fails with 281 errors of which the missing fixtures are a
//! symptom, and nothing in this workspace builds those targets. Reporting them
//! would be a permanent red that no one can act on, which is how a guard stops
//! being read.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// This guard's own path, relative to the workspace root. See the filter in
/// [`every_included_fixture_is_a_tracked_file`].
const SELF: &str = "types/tests/included_fixtures_are_tracked.rs";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("types/ always has a workspace root above it")
        .to_path_buf()
}

/// Every tracked path, as a repo-relative string with forward slashes.
///
/// This is both the set of files to scan and the set of answers: a fixture is
/// acceptable exactly when it appears here.
fn tracked(root: &Path) -> HashSet<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
        .expect("`git ls-files` must run: this guard defines \"in the repo\" as \"tracked\"");
    assert!(
        out.status.success(),
        "`git ls-files` failed in {} — failing rather than checking nothing",
        root.display()
    );
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|rel| !rel.is_empty())
        .map(str::to_string)
        .collect()
}

/// The literal argument of every `include_bytes!` / `include_str!` on `line`.
///
/// Returns nothing for a computed argument, which is the intended skip: the
/// opening `(` must be followed immediately by a quote.
fn included_literals(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    for macro_name in ["include_bytes!", "include_str!"] {
        let mut from = 0;
        while let Some(at) = line[from..].find(macro_name) {
            let after = from + at + macro_name.len();
            from = after;
            let rest = line[after..].trim_start();
            let Some(rest) = rest.strip_prefix('(') else {
                continue;
            };
            let rest = rest.trim_start();
            let Some(rest) = rest.strip_prefix('"') else {
                // A computed path (`concat!`, `env!`) — see the module docs.
                continue;
            };
            if let Some(end) = rest.find('"') {
                out.push(&rest[..end]);
            }
        }
    }
    out
}

/// Resolve `literal` against the directory of the including file and normalise
/// it back to a repo-relative path, or `None` if it climbs out of the repo.
///
/// Purely lexical: the file may not exist, and saying so is this guard's job
/// rather than the filesystem's.
fn resolve(including_file: &str, literal: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    let dir = including_file.rsplit_once('/').map_or("", |(d, _)| d);
    if !dir.is_empty() {
        parts.extend(dir.split('/'));
    }
    for seg in literal.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

#[test]
fn every_included_fixture_is_a_tracked_file() {
    let root = workspace_root();
    let tracked = tracked(&root);
    let mut untracked = Vec::new();

    for rel in tracked
        .iter()
        .filter(|r| r.ends_with(".rs"))
        .filter(|r| !r.contains("/vendor/"))
        // This file, and only this file, contains sample macro text as
        // DATA: `the_scanner_finds_literals_and_skips_computed_ones`
        // below feeds the scanner strings that look like includes,
        // because a scanner nobody has watched match anything is a
        // scanner that can be silently broken. Those samples name no
        // real fixture and never should.
        .filter(|r| r.as_str() != SELF)
    {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            for literal in included_literals(line) {
                let Some(target) = resolve(rel, literal) else {
                    untracked.push(format!(
                        "{rel}:{}: include of {literal:?} climbs above the repository root",
                        n + 1
                    ));
                    continue;
                };
                if tracked.contains(&target) {
                    continue;
                }
                // Name the likely cause. "Not tracked" and "tracked-but-
                // ignored" want different fixes, and an ignored path is the
                // one that looks committed to whoever added it.
                let ignored = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(["check-ignore", "-q", &target])
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false);
                let why = if ignored {
                    "it is covered by a .gitignore rule, so `git add -A` skips it silently"
                } else {
                    "it was never added"
                };
                untracked.push(format!("{rel}:{}: includes {target} — {why}", n + 1));
            }
        }
    }

    assert!(
        untracked.is_empty(),
        "these `include_bytes!`/`include_str!` targets are not in the repository, so the \
         crates that include them compile only on a machine that happens to have the file:\n  \
         {}\n\nPut the fixture under the including crate's `tests/fixtures/` — that is where \
         the rest of the workspace keeps binary fixtures and no ignore rule covers it — and \
         commit it.",
        untracked.join("\n  ")
    );
}

/// The guard must be able to fail. A scanner that silently matches nothing
/// passes forever and says nothing, which is the shape of every gate that
/// turned out to have been switched off.
#[test]
fn the_scanner_finds_literals_and_skips_computed_ones() {
    assert_eq!(
        included_literals(r#"let b = include_bytes!("../tests/fixtures/HelloWorld.class");"#),
        vec!["../tests/fixtures/HelloWorld.class"],
    );
    assert_eq!(
        included_literals(r#"include_str!("a.txt"); include_bytes!("b.bin")"#),
        vec!["b.bin", "a.txt"],
    );
    assert!(
        included_literals(r#"include_bytes!(concat!(env!("OUT_DIR"), "/generated.bin"))"#)
            .is_empty(),
        "a computed path names a build artefact and must be skipped, not guessed at"
    );
    assert_eq!(
        resolve("reader/src/class_reader.rs", "../tests/fixtures/x.class").as_deref(),
        Some("reader/tests/fixtures/x.class")
    );
    assert_eq!(resolve("a/b/c.rs", "./d.bin").as_deref(), Some("a/b/d.bin"));
    assert_eq!(
        resolve("a.rs", "../../x").as_deref(),
        None,
        "climbing out must be reported"
    );
}

/// The vendored-crate exemption is scoped to a real vendor tree.
///
/// If `native-builtins/vendor/` ever disappears — the patch upstreamed, the
/// dependency dropped — the `/vendor/` skip above is inherited by whatever
/// takes its place. This fails at that moment so the exemption is re-argued
/// rather than assumed.
#[test]
fn the_vendor_exemption_still_has_something_to_exempt() {
    let root = workspace_root();
    assert!(
        root.join("native-builtins/vendor/rustls-cbc/Cargo.toml").is_file(),
        "native-builtins/vendor/rustls-cbc is gone, so the `/vendor/` skip in          `every_included_fixture_is_a_tracked_file` no longer describes anything.          Delete the skip, or re-state which vendored tree it is now for."
    );
}

/// `SELF` must name this file.
///
/// The exemption above is a path string. Rename this test file and the string
/// stops matching anything — the scan would still pass, still report nothing,
/// and no longer skip the samples it was written to skip. Since the samples
/// would then be reported, the failure is loud in that direction; this covers
/// the other one, where a stale `SELF` quietly widens the scan by one file
/// that happens to be this one.
#[test]
fn the_self_exemption_names_this_file() {
    let path = workspace_root().join(SELF);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "SELF does not name a readable file ({}): {e}",
            path.display()
        )
    });
    assert!(
        text.contains("fn the_self_exemption_names_this_file"),
        "SELF points at {} , which is not this file",
        path.display()
    );
}
