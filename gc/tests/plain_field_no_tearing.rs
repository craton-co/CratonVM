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

use cratonvm_gc::GenerationalHeap;
use cratonvm_types::{ClassId, Value};

const ITERATIONS: usize = 2_000_000;

#[test]
fn plain_field_int_slot_never_tears_under_concurrent_access() {
    let heap = Arc::new(GenerationalHeap::with_capacity(4 * 1024 * 1024));
    let class_id = ClassId::new(1);
    let obj = heap.alloc_object(class_id, 1);

    // Two fully-distinct Int payloads: word0 packs [discriminant | payload]
    // for an Int slot, so if that word ever tears across these two writes,
    // the observed bits would mismatch both legitimate values.
    const A: i32 = 0x1111_1111;
    const B: i32 = 0x2222_2222_u32 as i32;

    // Establish A as the slot'''s initial value BEFORE spawning the reader: a
    // freshly-allocated slot is zero-initialized (decodes as Value::Int(0)),
    // and 0 is neither A nor B, so a reader started before the writer'''s first
    // store would see a legitimate-but-unaccounted-for transient value -- a
    // test-harness race, not a torn read.
    heap.set_field(obj, 0, Value::Int(A));
    let stop = Arc::new(AtomicBool::new(false));

    let h_writer = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for i in 0..ITERATIONS {
                let v = if i % 2 == 0 { A } else { B };
                heap.set_field(obj, 0, Value::Int(v));
            }
            stop.store(true, Ordering::Release);
        })
    };

    let h_reader = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut seen_a = 0usize;
            let mut seen_b = 0usize;
            while !stop.load(Ordering::Acquire) {
                match heap.get_field(obj, 0) {
                    Value::Int(v) if v == A => seen_a += 1,
                    Value::Int(v) if v == B => seen_b += 1,
                    other => panic!(
                        "torn read: observed {other:?}, neither of the two \
                         legitimate written values ({A:#x}, {B:#x})"
                    ),
                }
            }
            (seen_a, seen_b)
        })
    };

    h_writer.join().unwrap();
    let (seen_a, seen_b) = h_reader.join().unwrap();
    // Sanity: the reader actually observed both values, so this wasn't a
    // vacuous pass from running entirely outside the race window.
    assert!(
        seen_a > 0 && seen_b > 0,
        "reader never observed both written values (seen_a={seen_a}, \
         seen_b={seen_b}) -- test may not be exercising real contention"
    );
}

#[test]
fn plain_field_object_slot_never_tears_under_concurrent_access() {
    let heap = Arc::new(GenerationalHeap::with_capacity(4 * 1024 * 1024));
    let class_id = ClassId::new(1);
    let obj = heap.alloc_object(class_id, 1);

    // Two distinct heap objects used as the "old"/"new" reference payload --
    // mirrors ReentrantReadWriteLock$Sync's plain `firstReader: Thread` field.
    let ref_a = heap.alloc_object(class_id, 0);
    let ref_b = heap.alloc_object(class_id, 0);

    // Establish ref_a as the slot'''s initial value before spawning the reader
    // -- see the matching note in the Int test above (a fresh slot decodes as
    // Value::Int(0), i.e. Value::Object(None) here, neither ref_a nor ref_b).
    heap.set_field(obj, 0, Value::Object(Some(ref_a)));
    let stop = Arc::new(AtomicBool::new(false));

    let h_writer = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for i in 0..ITERATIONS {
                let v = if i % 2 == 0 {
                    Value::Object(Some(ref_a))
                } else {
                    Value::Object(Some(ref_b))
                };
                heap.set_field(obj, 0, v);
            }
            stop.store(true, Ordering::Release);
        })
    };

    let h_reader = {
        let heap = Arc::clone(&heap);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut seen_a = 0usize;
            let mut seen_b = 0usize;
            while !stop.load(Ordering::Acquire) {
                match heap.get_field(obj, 0) {
                    Value::Object(Some(r)) if r == ref_a => seen_a += 1,
                    Value::Object(Some(r)) if r == ref_b => seen_b += 1,
                    other => panic!(
                        "torn read: observed {other:?}, neither of the two \
                         legitimate written references"
                    ),
                }
            }
            (seen_a, seen_b)
        })
    };

    h_writer.join().unwrap();
    let (seen_a, seen_b) = h_reader.join().unwrap();
    assert!(
        seen_a > 0 && seen_b > 0,
        "reader never observed both written references (seen_a={seen_a}, \
         seen_b={seen_b})"
    );
}
