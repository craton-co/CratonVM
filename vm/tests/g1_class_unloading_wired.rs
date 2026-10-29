// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! G1's mark cycle must keep driving class unloading.
//!
//! # Why this is a test and not a comment
//!
//! A 2026-09-02 performance review of `gc/src/g1.rs` reported, as finding F-19,
//! that "no class unloading is wired to the G1 mark cycle" — on the evidence
//! that `grep class_unloading gc/src/g1.rs` returns nothing. **The finding is
//! wrong, and this test is what records why.**
//!
//! What made it look right: the real unload transaction lives in `vm/`, not
//! `gc/`, so no amount of grepping the collector finds it. (An unwired model,
//! `gc/src/class_unloading.rs`, used to invite that grep; it was deleted on
//! 2026-09-23.)
//!
//! What actually happens is that BOTH of G1's reference-processing drivers run
//! it: `g1_remark_process_references` at the final remark — the one point in the
//! cycle where a weak reference to a dead OLD-region referent can be observed
//! dead — and `process_references_after_gc` on the post-pause path. Each calls
//! `gc_reconcile_defining_loaders` to find dead loaders and then
//! `unload_dead_class_metadata` to run the transaction.
//!
//! # What this test pins
//!
//! The half that a refactor could silently drop: G1's remark driver is the ONLY
//! path that can unload on the strength of a completed mark bitmap, and nothing
//! in the type system connects it to the unloader. If that call goes, a
//! long-running application server or dynamic-proxy-heavy framework accumulates
//! metaspace for the life of the process and no test anywhere fails — which is
//! exactly the symptom F-19 described. The review reached the right worry by the
//! wrong route.
//!
//! # What it does NOT claim
//!
//! It is a source-presence check, not a proof that unloading is correct or that
//! any particular class is reclaimed. That is
//! `vm/tests/class_loader_unload_regression.rs`'s job, and the transaction's own
//! invariants are in `docs/architecture/class-loader-unloading.md`. What this
//! pins is existence, on the path a grep of the collector cannot see.

use std::path::{Path, PathBuf};

/// `(file, needle, why it must be there)`.
const REQUIRED: &[(&str, &str, &str)] = &[
    (
        "src/runtime/interpreter/gc_and_alloc.rs",
        "g1_remark_process_references",
        "G1's final-remark reference-processing driver — the only point in the \
         cycle with a completed mark bitmap to decide loader liveness against",
    ),
    (
        "src/runtime/interpreter/gc_and_alloc.rs",
        "gc_reconcile_defining_loaders",
        "the loader-reachability half: without it no loader is ever found dead \
         and the transaction below has nothing to do",
    ),
    (
        "src/runtime/interpreter/gc_and_alloc.rs",
        "unload_dead_class_metadata",
        "the transaction half: prunes statics, class locks, field descriptors, \
         lambda proxies, mirrors, vtables and the JIT code cache for the dead \
         loaders",
    ),
    (
        "src/memory/gc.rs",
        "pub fn unload_dead_class_metadata",
        "the transaction itself, in the crate the architecture doc names",
    ),
];

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn g1s_mark_cycle_still_drives_class_unloading() {
    let root = crate_root();
    let mut missing: Vec<String> = Vec::new();

    for (rel, needle, why) in REQUIRED {
        let path = root.join(rel);
        let Ok(src) = std::fs::read_to_string(&path) else {
            missing.push(format!("  {rel}\n      unreadable — {why}"));
            continue;
        };
        // Ignore comment lines that merely NAME the wiring — several do,
        // including this file's own references to it. Only a real call counts.
        let called = src.lines().any(|line| {
            let t = line.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && t.contains(needle)
        });
        if !called {
            missing.push(format!("  {rel}\n      `{needle}` is absent — {why}"));
        }
    }

    assert!(
        missing.is_empty(),
        "{} link(s) in G1's class-unloading chain are gone:\n\n{}\n\n\
         Losing any of them means a G1 run accumulates class metadata for the \
         life of the process, with no failing test anywhere — the symptom is \
         metaspace growth in an application server, not a crash.\n\n\
         Note that the collector itself contains NO reference to unloading and \
         is not supposed to: the real transaction lives in this crate. See \
         `docs/architecture/class-loader-unloading.md`.",
        missing.len(),
        missing.join("\n"),
    );
}

/// The G1 remark driver must reach the unloader, not merely coexist with it in
/// one file.
///
/// The check above is per-file, so a refactor that left
/// `unload_dead_class_metadata` called only from the post-pause path would
/// satisfy it while removing the only unload opportunity a mark cycle has. This
/// reads the remark function's own body.
#[test]
fn the_remark_driver_body_reaches_the_unloader() {
    let src = std::fs::read_to_string(crate_root().join("src/runtime/interpreter/gc_and_alloc.rs"))
        .expect("gc_and_alloc.rs must be readable");

    let start = src
        .find("fn g1_remark_process_references")
        .expect("G1's final-remark reference-processing driver is gone entirely");
    // The next top-level item after it bounds the body. `\n}\n` closes the
    // function at column zero; nothing inside it is indented that way.
    let end = src[start..]
        .find("\n}\n")
        .map(|off| start + off)
        .unwrap_or(src.len());
    let body = &src[start..end];

    for needle in [
        "gc_reconcile_defining_loaders",
        "unload_dead_class_metadata",
    ] {
        assert!(
            body.contains(needle),
            "`g1_remark_process_references` no longer calls `{needle}`. The \
             final remark is the ONLY point in a G1 cycle where loader liveness \
             can be decided against a completed mark bitmap — an evacuation \
             pause only ever sees collection-set deaths, so moving the call to \
             the post-pause path would silently stop unloading anything that \
             dies in the old generation."
        );
    }
}

/// gch h4/g1 (`gch-h3os-g1-remark-unloads-classes-before-it-resurrects-soft-retained-referents-FIXED-20261012`):
/// the remark driver runs the reference rows and marks their keep closure
/// BEFORE it judges any class mirror or defining loader. The old order (class
/// verdicts on the raw bitmap, reference rows last) unloaded a class whose
/// mirror only a policy-kept soft referent reached, a moment before the heap
/// resurrected that referent. The order is source text, so it is pinned the way
/// the wiring above is: over the non-comment lines of the driver's own body.
#[test]
fn gch_h4g1_the_remark_judges_classes_after_its_keep_closure_is_marked() {
    let src = std::fs::read_to_string(crate_root().join("src/runtime/interpreter/gc_and_alloc.rs"))
        .expect("gc_and_alloc.rs must be readable");
    let start = src
        .find("fn g1_remark_process_references")
        .expect("G1's final-remark reference-processing driver is gone entirely");
    let end = src[start..]
        .find("\n}\n")
        .map(|off| start + off)
        .unwrap_or(src.len());
    let code: Vec<&str> = src[start..end]
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !t.starts_with("//")
        })
        .collect();
    let first = |needle: &str| -> usize {
        code.iter()
            .position(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("`g1_remark_process_references` no longer calls `{needle}`"))
    };
    let order = [
        "remark_reference_rows(",
        "g1_remark_mark_kept_closure(",
        "reconcile_class_mirrors(",
        "gc_reconcile_defining_loaders(",
        "unload_dead_class_metadata(",
    ];
    let lines: Vec<usize> = order.iter().map(|n| first(*n)).collect();
    for pair in order
        .iter()
        .zip(lines.iter())
        .collect::<Vec<_>>()
        .windows(2)
    {
        let ((a, la), (b, lb)) = (pair[0], pair[1]);
        assert!(
            la < lb,
            "`{a}` (body line {la}) must come before `{b}` (body line {lb}): the class \
             and loader verdicts must read the bitmap AFTER the keep closure is marked"
        );
    }
}
