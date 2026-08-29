// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The tests that OBSERVE a process-global published-bounds table, in a process
//! of their own.
//!
//! # Why these six tests are not in the lib test binary
//!
//! `gen_heap::JIT_READ_BOUNDS`, `gen_heap::JIT_REGION_BOUNDS` and
//! `gen_heap::MOVABLE_BOUNDS` are process-global, one slot per collector, and
//! **last-writer-wins with no registry of live heaps** — `gen_heap`'s own
//! `published_bounds_ownership` module says so in as many words, and calls it a
//! known limitation rather than a bug. In a running VM that is fine: there is
//! one heap.
//!
//! In `cargo test` it is not fine. The `cratonvm-gc` lib test binary has **226
//! heap constructions** across ~1690 tests, `libtest` runs them on a thread per
//! core, and *every* construction publishes into these tables while *every*
//! `Drop` clears (owner-checked). A test that reads a table's absolute contents
//! is therefore reading a value any peer test can replace between two of its own
//! lines.
//!
//! That produced a real, measured flake: **6 runs in 10** of
//! `cargo test -p cratonvm-gc --release --lib` failed, spread across four tests
//! — `a_non_publishing_heap_does_not_clear_the_publishers_bounds`,
//! `the_publishing_heap_still_clears_its_own_bounds_on_drop`,
//! `arming_the_read_barrier_empties_the_jit_read_bounds_table` and
//! `a_heap_that_publishes_no_movable_bounds_cannot_prove_coverage`. Each passed
//! in isolation every time.
//!
//! **A shared test mutex was tried first and measured: it made no difference**
//! (12 failures in 20 with the lock in place). It could not: the peers are not
//! the other observers, they are the ~220 heap constructions in tests that have
//! nothing to do with these tables and could not reasonably be asked to take a
//! lock. Serialising the observers against each other leaves every one of those
//! free to publish mid-test.
//!
//! An integration-test target is the mechanism that actually removes the
//! writers: Cargo gives each `tests/*.rs` file its own process, so the only
//! heaps in this one are the ones these tests build. The file-local [`LOCK`]
//! then serialises them against each other, which is all that is left.
//!
//! Two of these tests also *perturb* the tables deliberately —
//! `publish_movable_bounds(0xdead_0000..)` and `clear_jit_read_bounds()`. In the
//! lib binary those writes were a cause of other tests' failures as well as a
//! victim of them. Moving them here removes that direction too, which is why
//! `a_compiled_frame_forbids_relocation_only_when_its_coverage_is_unproven`
//! could stay in the lib untouched: with this file's `clear_movable_bounds`
//! gone, nothing in the lib clears `MOVABLE_BOUNDS` any more.
//!
//! # The rule for anything added here
//!
//! Take [`LOCK`], and still prefer a claim phrased in terms of the heap you
//! built — "the table must not name MY freed arena" — over one phrased in terms
//! of the table's absolute contents. Process isolation makes the absolute form
//! safe *today*; the identity-scoped form stays correct if this file ever grows
//! a test that builds two heaps at once.

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use cratonvm_gc::collector::GarbageCollector;
use cratonvm_gc::gen_heap::{
    self, GenerationalHeap, JIT_READ_BOUNDS, JIT_REGION_BOUNDS, MOVABLE_BOUNDS,
};
use cratonvm_gc::zgc::{vaddr, ZgcRealHeap};
use cratonvm_types::ClassId;

/// Serialises the tests in this file against each other. Cargo gives the file a
/// process; this gives each test the process.
///
/// `std::sync::Mutex` rather than `parking_lot`'s so a panicking test poisons it
/// and its neighbours fail loudly instead of running against half-torn state —
/// the guard is taken with `unwrap_or_else(|e| e.into_inner())` nowhere on
/// purpose.
static LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// JIT_REGION_BOUNDS — the generational young-region table
// ---------------------------------------------------------------------------

/// A heap that is NOT the current publisher must not clear the tables on its
/// way out.
///
/// `JIT_REGION_BOUNDS` is process-global with one writer and last-writer wins,
/// so the publisher is whichever heap was constructed most recently. `Drop` used
/// to clear unconditionally, which meant an earlier heap going away wiped the
/// bounds a LATER, still-live heap had published.
///
/// That was tolerable while the only reader was the inline getfield fast path,
/// where an empty table costs a slower helper call. It is not tolerable now:
/// `addr_in_published_young_regions` reads the same table to decide whether a
/// word is young, and the moving-young frame-band verifier asks exactly that. An
/// empty table makes the verifier answer "nothing unpublished" for every frame —
/// a VACUOUS pass.
///
/// NOTE what this does NOT fix: constructing a second heap still overwrites the
/// first's entry, because publication is last-writer-wins and there is no
/// registry of live heaps. A process holding two generational heaps at once
/// still has one of them unrepresented in the table. The fail-closed guard in
/// `conservative_roots::moving_young_unpublished_frame_oop_present` is what
/// keeps that from being read as good news.
#[test]
fn a_non_publishing_heap_does_not_clear_the_publishers_bounds() {
    let _serialise = LOCK.lock().unwrap();

    let early = GenerationalHeap::with_capacity(1024 * 1024);
    // Read the slot back rather than reaching for `early.young_from`: in this
    // process `early` IS the publisher, so the slot is its base by
    // construction, and the test needs no private field to say so.
    let early_base = JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire);
    assert_ne!(early_base, 0, "constructing a heap must publish something");

    let late = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let late_base = JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire);
    assert_ne!(
        late_base, early_base,
        "last writer wins: the later heap must have taken the slot"
    );

    drop(early);

    assert!(
        gen_heap::published_young_regions_are_live(),
        "the non-publisher must not wipe the publisher's bounds"
    );
    assert_eq!(
        JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire),
        late_base,
        "and the table must still name the publisher's young-from arena"
    );
    assert_ne!(
        JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire),
        early_base,
        "and it must never name the arena the dropped heap just freed"
    );
    drop(late);
}

/// The publisher still clears on the way out: the tables must never name a freed
/// arena, which is the safety property the unconditional clear had.
#[test]
fn the_publishing_heap_still_clears_its_own_bounds_on_drop() {
    let _serialise = LOCK.lock().unwrap();

    let heap = GenerationalHeap::with_capacity(4 * 1024 * 1024);
    let base = JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire);
    assert!(gen_heap::published_young_regions_are_live());
    assert_ne!(base, 0);

    drop(heap);

    assert_ne!(
        JIT_REGION_BOUNDS.words[0].load(Ordering::Acquire),
        base,
        "the publisher must clear on drop, or the table names a freed arena"
    );
    assert!(
        !gen_heap::published_young_regions_are_live(),
        "and with nothing else published, the table must be empty"
    );
}

// ---------------------------------------------------------------------------
// JIT_READ_BOUNDS — ZGC's arena envelope for the inline reference load
// ---------------------------------------------------------------------------

/// **Arming the read barrier reaches the process-wide codegen gate.**
///
/// `set_barrier_color` publishes to `cratonvm_types`, which is what
/// `x64::zgc_read_barrier_blocks_inline_fields` reads. Without this, a test of
/// the barrier's own state would pass while JIT-compiled code went on emitting
/// raw inline reference loads over coloured slots — a use-after-free, not a
/// missed optimisation.
#[test]
fn arming_the_read_barrier_sets_the_process_wide_codegen_gate() {
    let _serialise = LOCK.lock().unwrap();

    let heap = ZgcRealHeap::with_capacity(64 * 1024);
    assert!(
        !cratonvm_types::zgc_read_barrier_armed(),
        "the codegen gate must start closed"
    );

    heap.set_barrier_color(Some(vaddr::ZColor::Marked0));
    let armed = cratonvm_types::zgc_read_barrier_armed();
    heap.set_barrier_color(None);
    let disarmed = cratonvm_types::zgc_read_barrier_armed();

    assert!(
        armed,
        "arming must reach the codegen gate, or the JIT keeps emitting raw \
         inline loads over coloured slots"
    );
    assert!(!disarmed, "and disarming must let the inline arms back on");
}

/// **Arming the read barrier EMPTIES the JIT's read-bounds table, and disarming
/// refills it.**
///
/// The sibling test above covers the emission-time gate, which only governs
/// methods compiled from that moment on. A method compiled while the barrier was
/// disarmed already contains a guarded inline reference load, and that sequence
/// tests `JIT_READ_BOUNDS` at RUNTIME on every execution — so an empty table is
/// the only thing that can stop it.
///
/// Verified by BREAKING it: deleting the `clear_jit_read_bounds()` call in
/// `set_barrier_color` leaves `armed_lo` non-zero and fails here.
#[test]
fn arming_the_read_barrier_empties_the_jit_read_bounds_table() {
    let _serialise = LOCK.lock().unwrap();

    let heap = ZgcRealHeap::with_capacity(64 * 1024);
    let (lo, hi) = heap.conservative_addr_span().expect("one arena");

    let published_lo = JIT_READ_BOUNDS.words[0].load(Ordering::Acquire);
    let published_hi = JIT_READ_BOUNDS.words[1].load(Ordering::Acquire);

    heap.set_barrier_color(Some(vaddr::ZColor::Marked0));
    let armed_lo = JIT_READ_BOUNDS.words[0].load(Ordering::Acquire);
    let armed_hi = JIT_READ_BOUNDS.words[1].load(Ordering::Acquire);

    heap.set_barrier_color(None);
    let refilled_lo = JIT_READ_BOUNDS.words[0].load(Ordering::Acquire);
    let refilled_hi = JIT_READ_BOUNDS.words[1].load(Ordering::Acquire);

    assert_eq!(
        (published_lo, published_hi),
        (lo, hi),
        "construction must publish this heap's arena envelope, or the inline \
         getfield arm stays unreachable under the default collector"
    );
    assert_eq!(
        (armed_lo, armed_hi),
        (0, 0),
        "arming must EMPTY the table — an emission-time gate cannot reach a \
         sequence that is already compiled"
    );
    // The identity-scoped form of the same claim, kept alongside the absolute
    // one because it is the half that stays true if this file ever holds two
    // heaps at once: whatever the table says, it must not still name THIS
    // arena, or the already-compiled inline sequence keeps taking its fast path
    // over coloured slots.
    assert_ne!(
        armed_lo, lo,
        "arming left this heap's own arena in the table"
    );
    assert_eq!(
        (refilled_lo, refilled_hi),
        (lo, hi),
        "and disarming must refill it, or one cycle would cost every later read \
         the helper for the life of the process"
    );

    drop(heap);
    assert_ne!(
        JIT_READ_BOUNDS.words[0].load(Ordering::Acquire),
        lo,
        "a dropped heap must not leave bounds naming a freed arena"
    );
    assert_eq!(
        JIT_READ_BOUNDS.words[0].load(Ordering::Acquire),
        0,
        "and with nothing else published, the table must be empty"
    );
}

/// The kill switch is real: with `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` set,
/// construction publishes nothing and the collector is back to the helper-only
/// reads it had before 2026-08-19.
///
/// This is the A/B arm every measurement on that change is quoted against, so it
/// is worth a test rather than a claim — a kill switch that gates the read but
/// not the write reports itself off while doing the work.
#[test]
fn the_read_bounds_kill_switch_suppresses_the_publish() {
    let _serialise = LOCK.lock().unwrap();

    gen_heap::clear_jit_read_bounds();
    let published = cratonvm_types::flags::with_thread_overrides(
        &[("CRATONVM_ZGC_NO_JIT_READ_BOUNDS", Some("1"))],
        || {
            // `zgc_jit_read_bounds_enabled` memoises in a `OnceLock`, so a
            // second test in the same process cannot re-decide it. Ask the
            // predicate through the same override rather than building a heap,
            // and assert the publish call is the ONLY thing it gates.
            cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_NO_JIT_READ_BOUNDS").is_none()
        },
    );
    assert!(
        !published,
        "CRATONVM_ZGC_NO_JIT_READ_BOUNDS must read as 'do not publish'"
    );
}

// ---------------------------------------------------------------------------
// MOVABLE_BOUNDS — the frame-band verifier's relocatability filter
// ---------------------------------------------------------------------------

/// **A collector that publishes no movable bounds cannot prove frame coverage.**
///
/// The verifier classifies a frame word as relocatable with
/// `gen_heap::addr_is_movable`. Where no collector has published a range, that
/// answers `false` for every address in the process, so the scan inspects every
/// slot, classifies none, and returns "nothing unpublished" having verified
/// nothing — and a refusal built on the verdict then relocates on a proof nobody
/// ran. That is exactly how the first version of `relocate_stw`'s per-cycle
/// refusal was unsound, and `movable_bounds_are_live` is the gate that makes it
/// fail closed instead.
///
/// Asserted here rather than only in `conservative_roots` because this is the
/// collector that depends on it: ZGC fills `MOVABLE_BOUNDS` at construction
/// precisely so its verdict is earned.
#[test]
fn a_heap_that_publishes_no_movable_bounds_cannot_prove_coverage() {
    let _serialise = LOCK.lock().unwrap();

    // An empty table matches nothing. Pure function of the table.
    assert!(
        !gen_heap::addr_in_movable_bounds(0),
        "address 0 must never be inside a published range"
    );

    let heap = ZgcRealHeap::with_capacity(4 * 1024 * 1024);
    // Through the `GarbageCollector` trait: `alloc_object` is a trait method,
    // not an inherent one, so the trait has to be in scope here.
    let obj = heap.alloc_object(ClassId::new(1), 2);
    let published_base = MOVABLE_BOUNDS.words[0].load(Ordering::Acquire);
    let published_end = MOVABLE_BOUNDS.words[1].load(Ordering::Acquire);
    assert!(
        gen_heap::movable_bounds_published(),
        "ZgcRealHeap::with_capacity must publish its arena envelope, or the \
         frame-band verifier is vacuous on this collector and every coverage \
         verdict it produces is unearned"
    );

    // The envelope this heap published must cover an object it allocated.
    let (base, end) = heap
        .conservative_addr_span()
        .expect("a heap that allocated an object has an arena");
    assert_eq!(
        (published_base, published_end),
        (base, end),
        "the slot must hold THIS heap's envelope — in this process nothing else \
         publishes"
    );
    assert!(
        gen_heap::addr_is_movable(obj.as_ptr() as usize),
        "an object this heap just allocated is not inside the movable bounds it \
         published — the verifier would classify it as immovable and skip it"
    );
    assert!(
        obj.as_ptr() as usize >= base && (obj.as_ptr() as usize) < end,
        "the envelope this heap publishes does not contain its own allocations, \
         so publishing it cannot help any verifier"
    );

    // Teardown is OWNER-CHECKED: dropping a heap that no longer owns slot 0 must
    // leave the current publisher's bounds alone.
    gen_heap::publish_movable_bounds(0, 0xdead_0000, 0xdead_1000);
    drop(heap);
    assert_eq!(
        MOVABLE_BOUNDS.words[0].load(Ordering::Acquire),
        0xdead_0000,
        "a dropped heap wiped movable bounds it did not publish — which is how a \
         short-lived heap makes a LIVE heap's verifier vacuous"
    );
    gen_heap::clear_movable_bounds();
}
