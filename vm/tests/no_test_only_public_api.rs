// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! TEST-ONLY-API RATCHET — a one-way ratchet on `pub` items that only their own
//! tests reference.
//!
//! ## Why this exists (ARCH-2026-08-04 A7)
//!
//! `vm/src/runtime/lockfree_resolve.rs` carried 389 lines describing a
//! three-level resolution cache of which one tier was wired. The other two —
//! `ThreadLocalResolveCache`, `SharedResolutionState::resolve_method` /
//! `cache_method` / `resolve_field` / `cache_field`, and an `invalidate_all`
//! that took three write locks to clear two permanently-empty maps — had no
//! production callers at all. They survived for months because **their own unit
//! tests referenced them**, which is exactly the condition under which rustc's
//! `dead_code` lint stays silent.
//!
//! A 2026-07-26 audit found this and documented it accurately in the module
//! header. It was still there on 2026-08-04. Prose in a header does not delete
//! code; a failing build does.
//!
//! ## What it measures
//!
//! For every `pub` / `pub(crate)` / `pub(super)` item declared in `vm/src`
//! production code (outside `#[cfg(test)]`), count references to its bare
//! identifier across every workspace member. An item is an **offender** when it
//! has no production reference beyond its own declaration *and* at least one
//! reference from test code.
//!
//! References are bucketed as:
//!
//!   * *production* — `<member>/src/**.rs`, outside `#[cfg(test)]` regions,
//!     excluding comment lines and `impl` headers (see the two traps below).
//!   * *test* — `#[cfg(test)]` regions in `src`, plus all of `<member>/tests`,
//!     `<member>/benches` and `<member>/examples`.
//!
//! ## Two traps this scanner had to be taught (both cost a real miss)
//!
//! 1. **A doc comment is not a use.** The first version counted every line,
//!    so `lockfree_resolve.rs`'s own header — which *named* all the dead types
//!    while explaining that they were dead — pushed each of them to
//!    `prod >= 2`. The audit's honesty was hiding the corpse. Comment lines are
//!    now skipped.
//! 2. **`impl Foo` is not a use of `Foo`.** It is part of `Foo`'s definition.
//!    Counting it meant `ThreadLocalResolveCache` (three `impl` blocks, zero
//!    call sites outside tests) read as live. `impl` headers are now skipped.
//!
//! Both were found by checking the gate against a known answer — the A7 items —
//! rather than by reading it. A scanner nobody has watched fail is a scanner
//! that passes vacuously.
//!
//! ## Known limitation: cross-crate name collisions make this LENIENT
//!
//! Matching is by bare identifier, so an unrelated same-named item in another
//! crate masks an offender here. `lockfree_resolve.rs`'s `ResolutionKey`,
//! `ResolvedTarget`, `ResolvedField`, `cache_field`, `method_count` and
//! `field_count` were all masked this way by live, unrelated declarations in
//! `cratonvm-classloading` — the gate caught five of the eleven A7 items,
//! not eleven.
//!
//! That error direction is deliberate: this gate must never fail on code that
//! is actually used. It under-reports; it does not over-report. Do not "fix"
//! this by making matching path-aware unless you are prepared to re-verify the
//! whole baseline.
//!
//! ## Working the backlog down
//!
//! [`BASELINE_OFFENDERS`] is the frozen count. Adding a test-only `pub` item
//! pushes past it and fails; removing one requires lowering the baseline in the
//! same change. Run with `--nocapture` to see the full list — each entry is
//! either dead code to delete or an item whose only real caller is a test, in
//! which case it should usually be `#[cfg(test)]` itself.
//!
//! ```text
//! cargo test -p cratonvm-vm --test no_test_only_public_api -- --nocapture
//! ```

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Frozen upper bound on test-only `pub` items in `vm/src`.
///
/// Measured 2026-08-04 immediately after the A7 deletion. Restoring the deleted
/// code takes it to 327, and the five-item difference is exactly
/// `ThreadLocalResolveCache`, `cache_method`, `resolve_method`, `resolve_field`
/// and `invalidate_all` — verified by diffing the two offender lists, not by
/// reading the scanner. The remaining A7 items (`ResolutionKey`,
/// `ResolvedTarget`, `ResolvedField`, `cache_field`, `method_count`,
/// `field_count`) are masked by the cross-crate collision documented above.
///
/// Zero slack, matching `stub_ratchet`'s contract: this is the observed count,
/// not a rounded-up allowance.
const BASELINE_OFFENDERS: usize = 319;

/// Minimum number of declarations the scan must find before its result means
/// anything.
///
/// Without this, any change that broke the declaration regex, mis-rooted the
/// workspace walk, or pointed the scan at an empty directory would report zero
/// offenders and **pass**. A gate that cannot distinguish "clean" from "did not
/// run" is not a gate. The real count is ~2,666; 2,000 leaves room for genuine
/// deletion without leaving room for a silent no-op.
const MIN_DECLARATIONS_SCANNED: usize = 2_000;

/// Workspace members to search for references. Mirrors the `[workspace]
/// members` list in the root `Cargo.toml`.
const MEMBERS: &[&str] = &[
    "reader",
    "types",
    "native-api",
    "native-collections",
    "native-io",
    "native-builtins",
    "native-builtins-crypto",
    "native-builtins-security",
    "native-awt",
    "jit-api",
    "jit",
    "jit-cuda",
    "cuda-bridge",
    "classloading",
    "craton-gpu",
    "gc",
    "vm",
    "vm-cli",
    "jfr",
    "libcratonvm",
    "cratonvm-embed",
    "difftest",
];

/// Repository root — `vm/`'s parent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("vm/ must have a parent")
        .to_path_buf()
}

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

/// Split one file into (production lines, test lines).
///
/// Skips the body of any genuine `#[cfg(test)]`-gated item — a real attribute
/// line, not a comment mentioning one — tracking brace depth so both a trailing
/// `mod tests { .. }` and a mid-file `#[cfg(test)] fn helper() { .. }` are
/// excluded. This is the same shape as `scan_production_section` in
/// `vm/src/runtime/interpreter/tests.rs`, which learned the "the first literal
/// `#[cfg(test)]` is inside a doc comment" lesson the expensive way.
fn split_regions(src: &str) -> (Vec<&str>, Vec<&str>) {
    let mut prod = Vec::new();
    let mut test = Vec::new();
    let mut pending = false;
    let mut depth: i32 = 0;

    for line in src.lines() {
        let t = line.trim_start();
        let is_comment = t.starts_with("//") || t.starts_with('*');

        if !is_comment && depth == 0 && !pending && t.starts_with("#[cfg(test)]") {
            pending = true;
            test.push(line);
            continue;
        }
        if pending {
            test.push(line);
            depth += brace_delta(line);
            if line.contains('{') && depth <= 0 {
                pending = false;
                depth = 0;
            } else if depth > 0 {
                pending = false;
            }
            continue;
        }
        if depth > 0 {
            test.push(line);
            depth += brace_delta(line);
            if depth < 0 {
                depth = 0;
            }
            continue;
        }
        prod.push(line);
    }
    (prod, test)
}

fn brace_delta(line: &str) -> i32 {
    let open = line.matches('{').count() as i32;
    let close = line.matches('}').count() as i32;
    open - close
}

/// Extract the declared name from a `pub` item line, if it is one.
fn decl_name(line: &str) -> Option<(&'static str, String)> {
    let t = line.trim_start();
    let rest = t
        .strip_prefix("pub(crate) ")
        .or_else(|| t.strip_prefix("pub(super) "))
        .or_else(|| t.strip_prefix("pub "))?;
    // Strip modifiers that may appear between `pub` and the item keyword.
    let mut rest = rest.trim_start();
    for m in ["async ", "unsafe ", "extern \"C\" ", "const "] {
        if let Some(r) = rest.strip_prefix(m) {
            rest = r.trim_start();
        }
    }
    for kw in ["fn ", "struct ", "enum ", "trait ", "type ", "const ", "static "] {
        if let Some(after) = rest.strip_prefix(kw) {
            let name: String = after
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if name.is_empty() || !name.starts_with(|c: char| c.is_alphabetic() || c == '_') {
                return None;
            }
            let kind: &'static str = match kw {
                "fn " => "fn",
                "struct " => "struct",
                "enum " => "enum",
                "trait " => "trait",
                "type " => "type",
                "const " => "const",
                _ => "static",
            };
            return Some((kind, name));
        }
    }
    None
}

/// Count identifier occurrences in `line` that are in `names`.
///
/// Rust identifiers in this tree are ASCII, but the *lines* are not — comments
/// carry `§`, `→`, `—` and friends. Walking by byte index and slicing on it
/// panics mid-codepoint, so this walks `char_indices` and only ever slices at a
/// boundary it was handed.
fn tally(line: &str, names: &HashSet<String>, counts: &mut HashMap<String, usize>) {
    let mut start: Option<usize> = None;
    for (idx, ch) in line.char_indices() {
        let is_start = ch.is_ascii_alphabetic() || ch == '_';
        let is_cont = ch.is_ascii_alphanumeric() || ch == '_';
        match start {
            None => {
                if is_start {
                    start = Some(idx);
                }
            }
            Some(s) => {
                if !is_cont {
                    let word = &line[s..idx];
                    if names.contains(word) {
                        *counts.entry(word.to_string()).or_insert(0) += 1;
                    }
                    start = if is_start { Some(idx) } else { None };
                }
            }
        }
    }
    if let Some(s) = start {
        let word = &line[s..];
        if names.contains(word) {
            *counts.entry(word.to_string()).or_insert(0) += 1;
        }
    }
}

#[test]
fn no_new_test_only_public_api() {
    let root = repo_root();

    // ---- 1. Declarations in vm/src production code ------------------------
    let mut vm_files = Vec::new();
    rust_files(&root.join("vm").join("src"), &mut vm_files);
    assert!(
        !vm_files.is_empty(),
        "found no .rs files under vm/src — the scan is mis-rooted at {}",
        root.display()
    );

    // name -> (kind, relative path)
    let mut decls: HashMap<String, (&'static str, String)> = HashMap::new();
    for path in &vm_files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let (prod, _) = split_regions(&src);
        for line in prod {
            if let Some((kind, name)) = decl_name(line) {
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                decls.entry(name).or_insert((kind, rel));
            }
        }
    }

    assert!(
        decls.len() >= MIN_DECLARATIONS_SCANNED,
        "only {} declarations scanned (floor {}). The scan found almost nothing, \
         which means it broke rather than that the tree got clean — check \
         decl_name() and the vm/src walk before touching the baseline.",
        decls.len(),
        MIN_DECLARATIONS_SCANNED
    );

    let names: HashSet<String> = decls.keys().cloned().collect();

    // ---- 2. Reference census across the workspace -------------------------
    let mut prod_refs: HashMap<String, usize> = HashMap::new();
    let mut test_refs: HashMap<String, usize> = HashMap::new();

    for member in MEMBERS {
        let base = root.join(member);
        // src: split into production / test regions.
        let mut files = Vec::new();
        rust_files(&base.join("src"), &mut files);
        for path in &files {
            let Ok(src) = std::fs::read_to_string(path) else {
                continue;
            };
            let (prod, test) = split_regions(&src);
            for line in prod {
                let t = line.trim_start();
                // See the module header: a comment is not a use, and an `impl`
                // header is part of the type's own definition.
                if t.starts_with("//") || t.starts_with('*') || t.starts_with("impl") {
                    continue;
                }
                tally(line, &names, &mut prod_refs);
            }
            for line in test {
                tally(line, &names, &mut test_refs);
            }
        }
        // tests / benches / examples: entirely test code.
        for sub in ["tests", "benches", "examples"] {
            let mut files = Vec::new();
            rust_files(&base.join(sub), &mut files);
            for path in &files {
                let Ok(src) = std::fs::read_to_string(path) else {
                    continue;
                };
                for line in src.lines() {
                    tally(line, &names, &mut test_refs);
                }
            }
        }
    }

    // ---- 3. Offenders -----------------------------------------------------
    // The declaration line itself counts as one production reference, so a
    // genuinely-unused item sits at exactly 1.
    let mut offenders: Vec<(String, &'static str, String, usize, usize)> = decls
        .iter()
        .filter_map(|(name, (kind, path))| {
            let p = prod_refs.get(name).copied().unwrap_or(0);
            let t = test_refs.get(name).copied().unwrap_or(0);
            (p <= 1 && t >= 1).then(|| (name.clone(), *kind, path.clone(), p, t))
        })
        .collect();
    offenders.sort();

    println!(
        "test-only-api: {} offenders across {} declarations (baseline {})",
        offenders.len(),
        decls.len(),
        BASELINE_OFFENDERS
    );
    for (name, kind, path, p, t) in &offenders {
        println!("  {kind:7} {name:55} prod={p} test={t}  {path}");
    }

    assert!(
        offenders.len() <= BASELINE_OFFENDERS,
        "test-only public API rose to {} (baseline {}).\n\
         A `pub` item in vm/src is now referenced only by tests — it is either \
         dead code to delete, or an item whose sole real caller is a test and \
         which should therefore be `#[cfg(test)]` itself.\n\
         Run with --nocapture for the full list. Do NOT raise the baseline to \
         make this pass; that is the exact failure mode this gate exists to \
         stop (see ARCH-2026-08-04 A7).",
        offenders.len(),
        BASELINE_OFFENDERS
    );

    // Ratchet: if the count dropped, the baseline must drop with it, otherwise
    // the win silently leaks back.
    assert_eq!(
        offenders.len(),
        BASELINE_OFFENDERS,
        "test-only public API fell to {} (baseline {}). Good — now lower \
         BASELINE_OFFENDERS to {} in this same change to lock it in.",
        offenders.len(),
        BASELINE_OFFENDERS,
        offenders.len()
    );
}
