// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Wave-4 lane A — the block-offset jump, on the arm every young pause takes.
//!
//! # What this binary is evidence of
//!
//! `CRATONVM_G1_BLOCK_OFFSETS=1` lets the remembered-set source walk ENTER a
//! dirty card at an object start the card table recorded, instead of sizing
//! every object from the region's base to reach it. It is the only lever in
//! lane A that changes WHICH objects a source walk visits, so it is the only
//! one whose failure mode is a lost remembered-set edge rather than a slower
//! pause — and a lost remembered-set edge is a use-after-free.
//!
//! The workload is `g1_w2a_walk_common`, unchanged, and it is the right one for
//! exactly one reason: **every live object except the root is reachable ONLY
//! through a remembered-set edge out of a tenured holder.** An object the jump
//! skips is therefore not slow to find, it is lost, and the read-back at the
//! end of `run()` faults or returns the wrong id. A fixture whose objects were
//! also reachable from a root would pass with the jump landing anywhere.
//!
//! Three holder shapes are in that graph (flat, reference array, and a chained
//! flat holder whose referent is itself a holder), so the jump is exercised
//! against all three of the walk's dispatch arms and against a closure that has
//! to be fed transitively afterwards.
//!
//! # Why the arms are separate binaries
//!
//! `CRATONVM_G1_BLOCK_OFFSETS`, `CRATONVM_G1_PARALLEL_EVAC` and
//! `CRATONVM_G1_CARD_CURSOR` all latch in a `OnceLock` on first read, so one
//! configuration is one process and `cargo test` gives each `tests/*.rs` file
//! its own binary. This file is the PARALLEL arm — the default evacuator, and
//! the one on which `CRATONVM_G1_PARALLEL_EVAC_SCREEN` leaves the per-object
//! header screens disarmed, which is precisely the arm on which a mis-entry
//! would have the least standing between it and a wild read. The jump's own
//! entry screen runs regardless of that flag, and this binary is where that
//! matters.
//!
//! Its twin is `g1_w4a_block_offsets_serial`.

mod g1_w2a_walk_common;





/// The lever's own engagement count, asserted rather than assumed.
///
/// `g1_w2a_walk_common::run()` owns its collector and does not hand it back, so
/// the reading is the process-wide one. Without this the file passes
/// identically with the jump never taken — which is the failure
/// `orchestrator-wave-1-measurements.md` §7.3 names as the common cause of this
/// round's three retracted measurements: the experiment did not check that it
/// was exercising the thing it named.
fn assert_engaged() {
    let (jumps, bytes, refused) = cratonvm_gc::g1::block_offset_process_census();
    assert!(
        jumps > 0,
        "the block-offset jump never fired, so this run says nothing about it \
         (bytes_jumped={bytes} refused={refused})"
    );
    assert_eq!(
        refused, 0,
        "a candidate entry failed its header screen on a healthy heap -- the \
         block-offset table named an address that is not an object"
    );
}
