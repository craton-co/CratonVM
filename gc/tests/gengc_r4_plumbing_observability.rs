// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational GC round 4, lane `plumbing` (2026-09-23): the observability
//! surface of the generational collector's two public entry points.
//!
//! An integration test (its own process) rather than a `gen_heap.rs` unit
//! test because two of these facts are process-global: the JVMTI GC hooks are
//! `OnceLock`s that the FIRST installer in a process owns, and `gc.rs`'s own
//! unit tests install theirs — a unit test here could not know whose callback
//! it was counting.
//!
//! See `docs/internal/reviews/gengc-round4-plumbing-20260923.md`.

use std::cell::Cell;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::GenerationalHeap;
use cratonvm_types::ClassId;

struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: each test drives its own heap from a single thread; no other
    // mutator of that heap exists, so the STW invariant holds trivially.
    unsafe { StopTheWorldToken::new() }
}

// Thread-local, like `gc.rs`'s own hook tests: libtest runs each test on its
// own thread and the hooks fire synchronously on the collecting thread, so a
// test sees exactly the collections it drove.
thread_local! {
    static STARTS: Cell<u32> = const { Cell::new(0) };
    static FINISHES: Cell<u32> = const { Cell::new(0) };
}

fn on_start() {
    STARTS.with(|c| c.set(c.get() + 1));
}

fn on_finish() {
    FINISHES.with(|c| c.set(c.get() + 1));
}

fn install_hooks() {
    // Idempotent; every test in this binary installs the same pair.
    cratonvm_gc::install_gc_start_hook(on_start);
    cratonvm_gc::install_gc_finish_hook(on_finish);
}

/// JVMTI `GarbageCollectionStart` / `GarbageCollectionFinish` must fire once
/// per generational collection, through both public entry points.
///
/// Before 2026-09-23 their only callers were `gc::collect` /
/// `gc::collect_with_finalizers` — the Cheney driver of the vestigial
/// semi-space `Heap` no backend selects — so an agent enabling the two events
/// received neither on a generational run.
#[test]
fn jvmti_gc_hooks_fire_once_per_generational_collection() {
    install_hooks();
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let mut roots = vec![heap.alloc_object(ClassId::new(1), 1)];

    let (s0, f0) = (STARTS.with(Cell::get), FINISHES.with(Cell::get));
    let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    assert_eq!(STARTS.with(Cell::get), s0 + 1, "collect_garbage: one GarbageCollectionStart");
    assert_eq!(FINISHES.with(Cell::get), f0 + 1, "collect_garbage: one GarbageCollectionFinish");

    let _ = heap.collect_garbage_with_finalizers(&stw(), &mut roots, &[], &NoMonitors);
    assert_eq!(
        STARTS.with(Cell::get),
        s0 + 2,
        "collect_garbage_with_finalizers: one GarbageCollectionStart"
    );
    assert_eq!(
        FINISHES.with(Cell::get),
        f0 + 2,
        "collect_garbage_with_finalizers: one GarbageCollectionFinish"
    );
}

/// `gc_pause_totals` counts exactly the collections `minor_gc_count` counts,
/// and its millisecond total advances by at least one per collection.
///
/// gen r5w3/obs7: that ceiled total is now only the single-bean fallback's
/// figure (`VmHeap::collection_time_ms`); the Serial beans'
/// `getCollectionTime()` is the floor of the microsecond sum, as on HotSpot
/// (`gc_metrics::serial_collectors`).
#[test]
fn pause_totals_track_every_completed_collection() {
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let mut roots = vec![
        heap.alloc_object(ClassId::new(1), 1),
        heap.alloc_object(ClassId::new(1), 1),
    ];
    assert_eq!(heap.gc_pause_totals(), (0, 0, 0), "a fresh heap has paused for nothing");

    let minor_before = heap.stats().snapshot().minor_gc_count;
    for _ in 0..3 {
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
    }
    let minor_after = heap.stats().snapshot().minor_gc_count;
    let (count, total_us, total_ms_ceiled) = heap.gc_pause_totals();

    assert_eq!(
        count,
        minor_after - minor_before,
        "a refused cycle is excluded from both, a completed one counted by both"
    );
    assert!(count > 0, "at least one of three collections must complete");
    assert!(
        total_ms_ceiled >= count,
        "every completed collection must advance the millisecond total by >= 1 \
         (count={count}, ms={total_ms_ceiled})"
    );
    assert!(
        total_ms_ceiled * 1000 >= total_us,
        "rounding UP per collection can never under-report the microsecond total \
         (us={total_us}, ms={total_ms_ceiled})"
    );
}

/// A live `new Object()` — `ClassId(0)`, no fields — is a young survivor.
///
/// gen r5w3/obs7 (2026-09-26): this test's precondition was written for the
/// header layout of 2026-09-23, where word 0 was `class_id | shape` and was
/// legitimately ZERO for a `ClassId(0)` object with no fields (so
/// `is_live_young_survivor` had to read the mark word in word 1 as well). The
/// header has since been re-laid-out (`types/src/heap_types.rs`,
/// `ObjectHeader`): `class_id` at offset 0 and the MARK WORD at offset 4
/// (`MARK_WORD_OFFSET`), `shape` / `aux` at 8 / 12. Word 0 is therefore
/// `class_id | mark_word << 32`, and the mark word's quartet carries
/// `GC_FLAG_HEADER` (flags byte, mark bits 24..27) from allocation on — so
/// word 0 of a live `new Object()` is now NON-zero, and the precondition
/// `word0 == 0` failed for a correct heap (first seen at 9e252c8b2). The
/// predicate itself was right under both layouts (word 0 non-zero ⇒ live);
/// the test now pins the current layout's reason instead: the class word is
/// zero, the mark word carries the header flag, so word 0 is non-zero.
#[test]
fn a_live_field_less_object_is_a_young_survivor() {
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let obj = heap.alloc_object(ClassId::new(0), 0);
    let addr = obj.as_ptr() as usize;
    assert!(
        heap.is_in_young(obj.as_ptr()),
        "precondition: a fresh small allocation lands in young"
    );
    // SAFETY: `obj` was just allocated by this heap; its 8-byte first header
    // word (class id + mark word) is readable, and both 4-byte halves are
    // 4-aligned inside it.
    let (class_word, mark_word, word0) = unsafe {
        (
            std::ptr::read(addr as *const u32),
            std::ptr::read((addr + cratonvm_types::MARK_WORD_OFFSET) as *const u32),
            std::ptr::read(addr as *const u64),
        )
    };
    assert_eq!(class_word, 0, "precondition: java.lang.Object is ClassId 0");
    assert_ne!(
        (mark_word >> 24) & u32::from(cratonvm_types::GC_FLAG_HEADER),
        0,
        "precondition: the mark word carries GC_FLAG_HEADER from allocation on \
         (mark={mark_word:#010x})"
    );
    assert_ne!(
        word0, 0,
        "precondition: the mark word lives in header word 0, so word 0 of a live \
         object is never zero"
    );
    assert!(
        heap.is_live_young_survivor(addr),
        "a live field-less java.lang.Object must not read as a reclaimed span"
    );
    assert!(
        !heap.is_live_young_survivor(addr + 4),
        "an unaligned address is never an object"
    );
}

/// The JFR `tenuringThreshold` source reports this collector's promotion
/// policy, not G1's 15.
///
/// gen r5w2/obs6: in HotSpot's unit (`gengc-r4w6-young6-tenuring-threshold-
/// units-differ-across-backends`): promoted on the third survival is HotSpot's
/// threshold `2`, which is what the JFR field (`VmHeap::tenuring_threshold`)
/// now carries, as G1's arm already did. The collector's own accessor keeps
/// its "survivals" unit (`3`).
#[test]
fn tenuring_threshold_is_the_generational_promotion_age() {
    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    assert_eq!(heap.tenuring_threshold(), 3);
    assert_eq!(heap.hotspot_tenuring_threshold(), 2);
    let heap = cratonvm_gc::vm_heap::VmHeap::Generational(heap);
    assert_eq!(heap.tenuring_threshold(), Some(2), "the JFR field is in HotSpot's unit");
}
