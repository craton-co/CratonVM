// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane W2-C — a cycle's TAMS must not outlive the cycle that defines it.
//!
//! `abort_concurrent_mark` clears both the per-region `mark_start` and the
//! collector's `mark_start_snapshot`, and says why: a stale TAMS would let a
//! later `cleanup` driven outside a cycle treat post-snapshot bytes as
//! implicitly live "on the strength of a cycle that never finished".
//!
//! `cleanup` cleared neither. So after a COMPLETED cycle the snapshot stayed
//! in place and every later liveness question — `is_live_after_mark`, a second
//! bare `cleanup()` with no intervening `start_concurrent_mark` — went on being
//! answered by a cycle that had ended. The direction is safe today (a region
//! recycled since the snapshot fails the epoch/type match and is treated as
//! wholly live), which is exactly why it survived: it is not a bug, it is state
//! whose lifetime does not match its meaning, and that is the shape this module
//! keeps getting bitten by.
//!
//! `CRATONVM_G1_CLEANUP_RESET_TAMS=1` mirrors the abort path's reset at the end
//! of `cleanup`. What it may NOT do is mirror the whole of it: the abort also
//! clears `marked_bytes_below_tams`, and that accumulator is what mixed GC's
//! region selection sorts on — mixed collections run AFTER a cleanup, not
//! before. An abort discards its bitmap and has no such consumer.

use std::sync::Arc;

use cratonvm_gc::collector::{GarbageCollector, StopTheWorldToken};
use cratonvm_gc::g1::G1CollectorConfig;
use cratonvm_gc::G1Collector;
use cratonvm_types::flags;
use cratonvm_types::{ClassId, ObjectRef, Value};

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: single-threaded test driver; no mutator is executing.
    unsafe { StopTheWorldToken::new() }
}

fn collector() -> Arc<G1Collector> {
    Arc::new(G1Collector::new(G1CollectorConfig {
        heap_size: 32 * 1024 * 1024,
        region_size: 1024 * 1024,
        ..Default::default()
    }))
}

/// A root with one live child and one unreferenced object, plus the completed
/// cycle that tells them apart.
fn cycle_over_a_live_and_a_dead_object(gc: &G1Collector) -> (ObjectRef, ObjectRef) {
    let root = gc.alloc_object(ClassId::new(61), 1);
    let live = gc.alloc_object(ClassId::new(62), 0);
    gc.set_field(root, 0, Value::Object(Some(live)));
    let dead = gc.alloc_object(ClassId::new(63), 0);

    gc.start_concurrent_mark(&stw());
    gc.remark(&stw(), &[root]);
    while !gc.concurrent_mark_step(usize::MAX) {}
    // The verdict must be available at this point — it is where the VM's
    // reference processing asks (`VmHeap::g1_final_remark_and_cleanup`), and it
    // is BEFORE cleanup, so no reset can have touched it yet.
    assert!(
        gc.is_live_after_mark(live.as_ptr() as usize),
        "test setup: the reachable object must be marked"
    );
    assert!(
        !gc.is_live_after_mark(dead.as_ptr() as usize),
        "test setup: the unreachable object must not be"
    );
    gc.cleanup(&stw());
    (live, dead)
}

/// Today: the finished cycle goes on answering.
#[test]
fn without_the_reset_a_finished_cycle_still_answers_liveness_questions() {
    flags::with_thread_overrides(&[("CRATONVM_G1_CLEANUP_RESET_TAMS", None)], || {
        let gc = collector();
        let (_live, dead) = cycle_over_a_live_and_a_dead_object(&gc);
        assert!(
            !gc.is_live_after_mark(dead.as_ptr() as usize),
            "the snapshot and the bitmap survive `cleanup`, so a question asked \
             with NO cycle in flight is still answered by the last one"
        );
    });
}

/// With the reset armed: no cycle is in force, so nothing is declared dead on
/// the strength of one. `is_live_after_mark`'s own empty-snapshot branch is
/// already written to answer conservatively, which is the safe direction and
/// the one the abort path has always taken.
#[test]
fn the_reset_retires_the_cycles_verdicts_with_the_cycle() {
    flags::with_thread_overrides(&[("CRATONVM_G1_CLEANUP_RESET_TAMS", Some("1"))], || {
        let gc = collector();
        let (live, dead) = cycle_over_a_live_and_a_dead_object(&gc);
        assert!(
            gc.is_live_after_mark(live.as_ptr() as usize),
            "a live object is live under any reading"
        );
        assert!(
            gc.is_live_after_mark(dead.as_ptr() as usize),
            "with the cycle retired there is no snapshot to answer from, and \
             `is_live_after_mark` answers conservatively — the same direction \
             `abort_concurrent_mark` has always taken"
        );
    });
}

/// A second bare `cleanup()` with no intervening `start_concurrent_mark` must
/// be harmless either way. It is the caller shape the residue page names
/// (`VmHeap::g1_signal_marking_complete`, the abort/teardown driver, calls
/// `cleanup` with no remark at all), so the reset must not have introduced a
/// new way for it to go wrong.
#[test]
fn a_second_bare_cleanup_is_harmless_with_the_reset_armed() {
    flags::with_thread_overrides(&[("CRATONVM_G1_CLEANUP_RESET_TAMS", Some("1"))], || {
        let gc = collector();
        let (live, _dead) = cycle_over_a_live_and_a_dead_object(&gc);
        let old_after_first = gc.old_gen_bytes();

        gc.cleanup(&stw());

        assert_eq!(
            gc.old_gen_bytes(),
            old_after_first,
            "a cleanup outside a cycle must free nothing"
        );
        assert!(
            gc.is_live_after_mark(live.as_ptr() as usize),
            "and must not have reached a verdict about anything"
        );
    });
}
