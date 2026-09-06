// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Regression test for the plain-field 16-byte slot tearing fix
//! (2026-07-06, see
//! elasticsearch-lucene-binary-docvalues-range-hangs.md #3).
//!
//! `GenerationalHeap::get_field`/`set_field` (the interpreter's plain,
//! non-`volatile` field accessors) used to read/write the 16-byte `Value`
//! slot via a bare, non-atomic `ptr::read`/`ptr::write`. Two mutator threads
//! doing ordinary `getfield`/`putfield` on the SAME field slot could tear
//! each other's writes -- exactly the pattern real JDK library code legally
//! relies on being tear-free (e.g. `ReentrantReadWriteLock$Sync`'s plain
//! `firstReader`/`firstReaderHoldCount`, published via a nearby
//! `volatile`/CAS write to a DIFFERENT field, `state`).
//!
//! This test hammers a SINGLE field slot with one writer thread alternating
//! between two distinct, fully-formed values while a reader thread spins
//! reading it, asserting every observed value is EXACTLY one of the two
//! legitimate values -- never a torn hybrid. `GenerationalHeap` is the
//! default collector (see vm-cli's collector selection); `Heap` (heap.rs)
//! and `G1Collector` (g1.rs) got the same fix and share the same underlying
//! `cratonvm_types::{read_value_atomic, write_value_atomic}` primitives, so
//! this test's coverage of the shared mechanism extends to all three.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

/// How long the writer keeps hammering while it waits for the reader to observe
/// both values.
///
/// # Why the run ends on the reader's signal and not on an iteration count
///
/// This was `const ITERATIONS: usize = 2_000_000`, and the writer stopped when
/// it reached that number whether or not the reader had ever run. Two million
/// plain field stores take about ten milliseconds, so on a loaded box the
/// reader could be scheduled for the first time AFTER the writer had finished
/// and set `stop`. The anti-vacuity guard at the end then failed the test with
/// `seen_a=0, seen_b=0` -- the reader's loop body having never executed once.
///
/// Measured 2026-09-06 on the shared Azure box at load ~45: **25 of 30 runs
/// passed, and every one of the five failures was that guard reporting
/// `seen_a=0, seen_b=0`. Not one was a torn read.** The property under test was
/// never violated; the harness was reporting that it had not managed to look.
///
/// Ending on the reader's own signal — AFTER the original write volume, see
/// [`MIN_WRITES`] — fixes that at the root rather than by retrying: the writer
/// does everything it used to and then keeps alternating until the reader says
/// it has seen both values, so the two threads are guaranteed to overlap and
/// the tearing window is exercised for LONGER than before, never less. The
/// deadline is only a backstop, and reaching it means a thread got no CPU for
/// five seconds, which is worth failing on.
///
/// Both halves are checked, not assumed: with `write_value_atomic` replaced by
/// a byte-wise non-atomic store, this file still reports `torn read` — which is
/// how the too-eager first version of this fix was caught.
const READER_DEADLINE: Duration = Duration::from_secs(5);

/// Writes the writer performs BEFORE it will even consider stopping — the
/// original fixed `ITERATIONS`, kept as a FLOOR.
///
/// This is the tearing window, and it is the reason the reader's signal is a
/// floor-plus condition rather than the whole stop rule. An earlier version of
/// this file stopped at the first moment the reader had seen both values, which
/// can be microseconds in: the run finished in 0.02 s and **a deliberately
/// non-atomic `write_value_atomic` went undetected**, where the original caught
/// it 3 times out of 3. Overlap is necessary for the test to mean anything; it
/// is not sufficient for it to have looked hard enough.
const MIN_WRITES: usize = 2_000_000;

/// Bound on the writer's spin so a wedged reader cannot hang the suite. Far
/// more than the reader needs; only reached if the deadline does not fire.
const MAX_WRITES: usize = 200_000_000;

/// Hammer ONE field slot with a writer alternating between two fully-formed
/// values while a reader spins on it, and assert every value the reader
/// observes is exactly one of the two. A torn 16-byte `Value` cell decodes as
/// neither, which is the whole test.
///
/// `make_values` is handed the heap because a reference payload has to be
/// allocated OUT OF THE HEAP UNDER TEST -- an `ObjectRef` from a different,
/// shorter-lived heap would dangle.
fn assert_no_tearing(
    what: &'static str,
    make_values: impl FnOnce(&GenerationalHeap) -> (Value, Value),
) {
    let heap = Arc::new(GenerationalHeap::with_capacity(4 * 1024 * 1024));
    let (a, b) = make_values(&heap);
    let obj = heap.alloc_object(ClassId::new(1), 1);

    // Establish `a` BEFORE the reader starts: a freshly-allocated slot is
    // zero-initialised and decodes as `Value::Int(0)`, which is neither of the
    // two legitimate values, so a reader that started first would report a
    // harness race as a torn read.
    heap.set_field(obj, 0, a);

    let stop = Arc::new(AtomicBool::new(false));
    // Raised by the READER once it has observed both values; this is what ends
    // the run. See `READER_DEADLINE`.
    let seen_both = Arc::new(AtomicBool::new(false));

    let h_writer = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        let seen_both = Arc::clone(&seen_both);
        std::thread::spawn(move || {
            let deadline = Instant::now() + READER_DEADLINE;
            let mut writes = 0usize;
            for i in 0..MAX_WRITES {
                heap.set_field(obj, 0, if i % 2 == 0 { a } else { b });
                writes = i + 1;
                // `MIN_WRITES` FIRST, then the reader's signal. Checked in
                // blocks so the store loop stays tight -- the tearing window
                // must not be paced by this bookkeeping.
                if i >= MIN_WRITES
                    && i % 4_096 == 0
                    && (seen_both.load(Ordering::Acquire) || Instant::now() >= deadline)
                {
                    break;
                }
            }
            stop.store(true, Ordering::Release);
            writes
        })
    };

    let h_reader = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        let seen_both = Arc::clone(&seen_both);
        std::thread::spawn(move || {
            let (mut seen_a, mut seen_b) = (0usize, 0usize);
            while !stop.load(Ordering::Acquire) {
                let observed = heap.get_field(obj, 0);
                if observed == a {
                    seen_a += 1;
                } else if observed == b {
                    seen_b += 1;
                } else {
                    panic!(
                        "torn read on a {what} slot: observed {observed:?}, neither of the \
                         two legitimate written values ({a:?}, {b:?})"
                    );
                }
                if seen_a > 0 && seen_b > 0 && !seen_both.load(Ordering::Relaxed) {
                    seen_both.store(true, Ordering::Release);
                }
            }
            (seen_a, seen_b)
        })
    };

    let writes = h_writer.join().unwrap();
    let (seen_a, seen_b) = h_reader.join().unwrap();
    assert!(
        seen_a > 0 && seen_b > 0,
        "the reader never observed both written {what} values within {READER_DEADLINE:?} \
         (seen_a={seen_a}, seen_b={seen_b}, writer made {writes} stores) -- the two threads \
         never overlapped, so this run tested nothing. That is a STARVED READER, not a torn \
         read: a torn read panics in the reader naming the value it saw."
    );
}

#[test]
fn plain_field_int_slot_never_tears_under_concurrent_access() {
    // `word0` packs `[discriminant | payload]` for an `Int` slot, so a tear
    // across these two writes decodes as neither.
    const A: i32 = 0x1111_1111;
    const B: i32 = 0x2222_2222_u32 as i32;
    assert_no_tearing("int", |_heap| (Value::Int(A), Value::Int(B)));
}

#[test]
fn plain_field_object_slot_never_tears_under_concurrent_access() {
    // Two distinct heap objects as the reference payload -- mirrors
    // `ReentrantReadWriteLock$Sync`'s plain `firstReader: Thread` field. Both
    // come out of the heap under test; see `assert_no_tearing`.
    assert_no_tearing("reference", |heap| {
        let ref_a = heap.alloc_object(ClassId::new(1), 0);
        let ref_b = heap.alloc_object(ClassId::new(1), 0);
        (Value::Object(Some(ref_a)), Value::Object(Some(ref_b)))
    });
}
