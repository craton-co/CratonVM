// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JIT's deopt stashes must stay wired as GC roots, in BOTH halves.
//!
//! # Why this is a test and not a comment
//!
//! `LAST_DEOPT` and `LAST_EXCEPTIONAL` (`jit/src/deopt.rs`) hold
//! `ReconstructedFrame`s whose slots carry raw Java heap addresses. They are
//! only safe to hold across an allocation because `vm/` scans them during root
//! enumeration and rewrites them during the pointer-map update. `jit/` cannot
//! depend on `vm/`, so nothing in the type system connects the two: the wiring
//! is four call sites in this crate and a debug assertion that fires only if a
//! remap runs on a thread that never scanned.
//!
//! On 2026-08-18 that gap cost a real miscompile. `route_implicit_exc_through_callee`
//! carried a `clear_exceptional_frame()` immediately before
//! `create_exception_object`, justified by "a `ReconstructedFrame` is not a GC
//! root". By then it WAS one — but `docs/jit/deopt-thread-local-roots.md` still
//! said the `vm/` half was "not wired", so the stale defence read as correct.
//! It discarded the reason-9 frame the compiled body had just published, and
//! RBC.6's `getfield`/`putfield` admission let a NullPointerException escape a
//! handler that catches it — every compiled `try`-wrapped field access in the
//! tree. See `rbc6-getfield-putfield-npe-escape-FIXED-20260818.md`.
//!
//! The fix removed that clear and now *relies* on the wiring. This test is what
//! makes that reliance checkable: unwire either half and the frame the handler
//! recovers its locals from becomes a set of stale addresses across the very
//! allocation the fix keeps it alive for. A silent revert of one half would
//! otherwise reproduce the same class of bug with no failing test anywhere.
//!
//! # What it does NOT claim
//!
//! It is a source-presence check, not a proof that the visitors are correct or
//! that every collector reaches them. Those are the visitors' own unit tests in
//! `jit/src/deopt.rs` and the debug assertion that pairs the halves at runtime.
//! What this pins is the half that has already been lost once: existence.

use std::path::{Path, PathBuf};

/// `(file, needle, why it must be there)`.
///
/// Both halves, and both of the two thread contexts they run in — the
/// collecting thread's own enumeration, and the deposit a thread makes just
/// before it parks (a parked thread cannot be asked for its thread-locals, so
/// it hands them over first).
const REQUIRED: &[(&str, &str, &str)] = &[
    (
        "src/memory/roots.rs",
        "for_each_stashed_deopt_object",
        "the SCAN half: without it the collector never sees the stashed \
         addresses and is free to reclaim the objects a stashed frame names",
    ),
    (
        "src/memory/gc.rs",
        "remap_stashed_deopt_objects",
        "the REMAP half: without it a moving collection leaves the stashed \
         addresses pointing at the objects' old locations",
    ),
    (
        "src/vm/vm_exec.rs",
        "for_each_stashed_deopt_object",
        "the SCAN half for a thread about to park, which cannot be asked for \
         its own thread-locals once it has",
    ),
    (
        "src/vm/vm_exec.rs",
        "remap_stashed_deopt_objects",
        "the REMAP half for that same deposit",
    ),
];

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

#[test]
fn both_halves_of_the_deopt_stash_root_wiring_are_present() {
    let root = crate_root();
    let mut missing: Vec<String> = Vec::new();

    for (rel, needle, why) in REQUIRED {
        let path = root.join(rel);
        let Ok(src) = std::fs::read_to_string(&path) else {
            missing.push(format!("  {rel}\n      unreadable — {why}"));
            continue;
        };
        // Ignore the comment lines that NAME the wiring (several of them do,
        // including the one this test exists to protect); only a real call
        // counts.
        let called = src.lines().any(|line| {
            let t = line.trim_start();
            !t.starts_with("//") && !t.starts_with("///") && t.contains(needle)
        });
        if !called {
            missing.push(format!("  {rel}\n      `{needle}` is not called — {why}"));
        }
    }

    assert!(
        missing.is_empty(),
        "{} half/halves of the deopt-stash GC-root wiring are gone:\n\n{}\n\n\
         Code that KEEPS a `ReconstructedFrame` across an allocation depends on \
         this — `route_implicit_exc_through_callee` in `vm/src/jit/helpers.rs` \
         holds the exceptional frame across `create_exception_object` precisely \
         so a handler can recover its non-parameter locals. Restore the wiring, \
         or restore that clear and accept that RBC.6's field-op admission is a \
         miscompile again.\n\n\
         See `docs/jit/deopt-thread-local-roots.md` — and note its status line \
         was stale once already, which is how this was lost the first time.",
        missing.len(),
        missing.join("\n"),
    );
}

/// The scan and the remap must be reachable from the SAME crate the doc names,
/// so a future move cannot satisfy the test above by leaving a stub behind.
///
/// Cheap corroboration: the `jit/` side must still export both visitors. If it
/// stops, the call sites above would not compile — but this says so with the
/// reason attached rather than as a bare resolution error.
#[test]
fn the_jit_side_still_exports_both_visitors() {
    // Referencing the items is the check; a missing export is a compile error
    // here, with this test's name and doc comment as the explanation.
    let _scan: fn(&mut dyn FnMut(u64)) = |f| cratonvm_jit::deopt::for_each_stashed_deopt_object(f);
    let _remap: fn(&mut dyn FnMut(u64) -> Option<u64>) =
        |f| cratonvm_jit::deopt::remap_stashed_deopt_objects(f);
}
