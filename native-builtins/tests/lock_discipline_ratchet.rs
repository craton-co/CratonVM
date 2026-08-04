// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! LOCK-DISCIPLINE RATCHET — a one-way ratchet on raw lock constructions in
//! `cratonvm-native-builtins`.
//!
//! ## The finding (ARCH-2026-08-04 A6)
//!
//! The workspace has a designed lock hierarchy: [`LockLevel`] L0..L10 in
//! `types/src/lock_order.rs`, with `OrderedPlMutex` / `OrderedPlRwLock` wrappers
//! that assert acquisition order (strictly decreasing level) and name the
//! violation. Adoption on 2026-08-04:
//!
//! | Crate | Ordered | Raw `Mutex::new` / `RwLock::new` |
//! |-------|--------:|---------------------------------:|
//! | `vm` | 116 | 319 |
//! | `native-builtins` | **0** | **440** |
//!
//! (A plain `grep` over the crate reports 481. This gate counts 440 because it
//! excludes `#[cfg(test)]` regions and comment lines — a lock in a test fixture
//! is not part of the runtime's discipline. 440 is the production figure and is
//! the one to quote.)
//!
//! `native-builtins` is the largest crate in the workspace *and* the one that
//! re-enters the VM: a native callback calls back into Java, which takes heap
//! locks and the L10 class-manager lock. That is precisely the shape a lock
//! hierarchy exists to police, and it had no coverage at all — 440
//! independently-locked pieces of global state with no ordering relation to
//! each other or to the VM's levels. The `ordered` column of this gate's own
//! output is the live proof: it read 0 before this change.
//!
//! Two locks have since been converted (`AOT_CACHE_INPUT_PATH` /
//! `AOT_CACHE_OUTPUT_PATH` in `src/aot.rs`, at `LockLevel::Scratch`), bringing
//! the baseline to 438. They were picked because the claim is *checkable*:
//! every one of their six acquisition sites clones the `Option<String>` and
//! drops the guard before touching `ctx`, so none is held across a re-entry
//! into the VM. That is the standard the rest of the backlog has to meet.
//!
//! ## Why this gate ratchets instead of converting
//!
//! Converting all 440 mechanically would be worse engineering than this gate,
//! not better. `OrderedPlMutex::new` takes a [`LockLevel`], and the level is a
//! *claim*: "no lock at or below this level is ever held when this one is
//! taken". Stamping 440 locks with a level nobody reasoned about makes the
//! checker assert something unverified — it would either fire constantly on
//! correct code or, worse, pass while encoding a false hierarchy. A wrong level
//! is more dangerous than no level, because it reads as audited.
//!
//! The honest unit of work is per-lock: decide whether that lock can be held
//! across a re-entry into the VM, and pick the level from the answer. Most of
//! these are leaf caches that belong at `LockLevel::Scratch`; the interesting
//! minority are the ones held across a `NativeContext` callback, and those are
//! the actual latent deadlocks this program should surface. That audit is
//! tracked in `docs/internal/arch-review-20260804.md` §A6.
//!
//! What this gate does is stop the number growing while that work happens. A
//! new raw lock fails CI; converting one to an ordered wrapper requires
//! lowering [`BASELINE_RAW_LOCKS`] in the same change, which locks in the win.
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test lock_discipline_ratchet -- --nocapture
//! ```
//!
//! [`LockLevel`]: cratonvm_types::lock_order::LockLevel

use std::path::{Path, PathBuf};

/// Frozen upper bound on raw lock constructions in `native-builtins/src`.
///
/// Measured 2026-08-04. Zero slack, matching `stub_ratchet`'s contract.
const BASELINE_RAW_LOCKS: usize = 438;

/// Minimum number of source lines the scan must see before its count means
/// anything.
///
/// A source-scanning gate that stops finding its own source reports zero hits
/// and **passes** — indistinguishable from a clean tree. This crate is ~565k
/// lines; 100k is far below any plausible real shrink and far above anything a
/// broken walk would produce.
const MIN_LINES_SCANNED: usize = 100_000;

/// Sanity floor on the raw-lock count itself.
///
/// Belt-and-braces against a scan that walks the right files but stops matching
/// (a mangled needle, a CRLF split, an over-eager comment filter). A count of
/// zero here would sail past the `<=` assertion below while proving nothing.
const MIN_RAW_LOCKS_FOUND: usize = 400;

fn crate_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // `vendor/` holds third-party sources this crate does not own.
            if path.file_name().is_some_and(|n| n == "vendor") {
                continue;
            }
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Count raw `Mutex::new` / `RwLock::new` constructions outside test code.
///
/// Returns `(raw_locks, ordered_locks, lines_scanned)`.
///
/// Line-oriented on purpose. `str::lines` splits on `\n` and strips a trailing
/// `\r`, so this reads identically on an LF and a CRLF checkout — a gate in
/// this repo has already been red on Windows and green on Linux for splitting
/// on a literal `"\n}\n"`.
fn census() -> (usize, usize, usize, Vec<String>) {
    let mut files = Vec::new();
    rust_files(&crate_src(), &mut files);
    files.sort();

    let mut raw = 0usize;
    let mut ordered = 0usize;
    let mut lines_scanned = 0usize;
    let mut sites = Vec::new();

    for path in &files {
        let Ok(src) = std::fs::read_to_string(path) else {
            continue;
        };
        let rel = path
            .strip_prefix(env!("CARGO_MANIFEST_DIR"))
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();

        let mut pending_test = false;
        let mut depth: i32 = 0;

        for (n, line) in src.lines().enumerate() {
            lines_scanned += 1;
            let t = line.trim_start();

            // Skip `#[cfg(test)]`-gated items: a lock in a test fixture is not
            // part of the runtime's discipline.
            let is_comment = t.starts_with("//") || t.starts_with('*');
            if !is_comment && depth == 0 && !pending_test && t.starts_with("#[cfg(test)]") {
                pending_test = true;
                continue;
            }
            if pending_test {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if line.contains('{') && depth <= 0 {
                    pending_test = false;
                    depth = 0;
                } else if depth > 0 {
                    pending_test = false;
                }
                continue;
            }
            if depth > 0 {
                depth += line.matches('{').count() as i32 - line.matches('}').count() as i32;
                if depth < 0 {
                    depth = 0;
                }
                continue;
            }

            // A mention in a comment is not a construction.
            if is_comment {
                continue;
            }

            if t.contains("OrderedPlMutex") || t.contains("OrderedPlRwLock") || t.contains("OrderedMutex") {
                ordered += 1;
                continue;
            }
            if line.contains("Mutex::new") || line.contains("RwLock::new") {
                raw += 1;
                if sites.len() < 40 {
                    sites.push(format!("{rel}:{}", n + 1));
                }
            }
        }
    }

    (raw, ordered, lines_scanned, sites)
}

#[test]
fn raw_lock_constructions_do_not_grow() {
    let (raw, ordered, lines, sites) = census();

    println!(
        "lock-discipline: {raw} raw lock constructions, {ordered} ordered, \
         across {lines} lines (baseline {BASELINE_RAW_LOCKS})"
    );
    if raw > BASELINE_RAW_LOCKS {
        println!("first sites:");
        for s in &sites {
            println!("  {s}");
        }
    }

    assert!(
        lines >= MIN_LINES_SCANNED,
        "only {lines} lines scanned (floor {MIN_LINES_SCANNED}). The walk broke \
         rather than the crate shrinking — fix the scan before touching the \
         baseline."
    );
    assert!(
        raw >= MIN_RAW_LOCKS_FOUND,
        "only {raw} raw locks found (floor {MIN_RAW_LOCKS_FOUND}). The needle \
         stopped matching; a zero here would pass the ceiling assertion below \
         while proving nothing."
    );

    assert!(
        raw <= BASELINE_RAW_LOCKS,
        "raw lock constructions in native-builtins rose to {raw} (baseline \
         {BASELINE_RAW_LOCKS}).\n\
         This crate re-enters the VM — a native callback calls back into Java, \
         which takes the heap and L10 class-manager locks — so a new global \
         lock here with no LockLevel is a new deadlock the checker cannot see.\n\
         Use OrderedPlMutex / OrderedPlRwLock with a level you can justify \
         (see types/src/lock_order.rs), or explain in review why this lock \
         cannot participate in a cycle. Do NOT raise the baseline."
    );

    // Ratchet: a conversion must lower the baseline in the same change,
    // otherwise the win leaks back the next time someone adds a lock.
    assert_eq!(
        raw, BASELINE_RAW_LOCKS,
        "raw lock constructions fell to {raw} (baseline {BASELINE_RAW_LOCKS}). \
         Good — now set BASELINE_RAW_LOCKS to {raw} in this same change."
    );
}
