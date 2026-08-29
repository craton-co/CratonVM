// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! W7-84 — a primitive stored into a slot the class declares as a REFERENCE
//! must round-trip, and every collector must give the SAME answer.
//!
//! # What was red before this file's fix, and on which arm
//!
//! One question — "a native wrote `Value::Int(0x5EED)` into a declared-reference
//! field; what does the next read see?" — had four answers, and which one a
//! program got depended on the collector flag:
//!
//! | arm | before | after |
//! |---|---|---|
//! | `GenerationalHeap`, compact layout | `Int(0x5EED)` (boxed into an `AUTOBOX_CLASS_ID` wrapper, unboxed on read) | `Int(0x5EED)` |
//! | `ZgcRealHeap`, compact layout | **`Object(None)`** — `write_compact_field`'s `Reference` arm maps every non-`Object` value to raw 0 | `Int(0x5EED)` |
//! | `G1Collector`, compact layout | **`Object(None)`**, same encoder | `Int(0x5EED)` |
//! | any of them, legacy 16-byte `Value` cell | `Int(0x5EED)` | `Int(0x5EED)` |
//!
//! ZGC has been the default since 2026-08-10, so the discarding answer was the
//! one most code actually got — and on a real `java.time.Month` it nulls
//! `Enum.name` on a shared enum constant, after which an `unwrap_or(1)`
//! fallback answers January for every month of the year
//! (W7-77-guarded-slot-maps.md §4).
//!
//! # Why the cross-arm assertion is the deliverable and a single arm is not
//!
//! The observable DIFFERED per collector, so a one-collector test proves the
//! least interesting third of the claim: run under `gen_heap` alone it was
//! green before the fix, and run under ZGC alone it says nothing about whether
//! the arms now agree. [`every_collector_agrees_on_a_primitive_in_a_reference_slot`]
//! collects all three answers and asserts they are equal to each other AND to
//! the value that was stored.
//!
//! # The vacuous shape this file refuses
//!
//! "the read returns something" passes against `Object(None)` (the ZGC/G1 bug),
//! against `Object(Some(wrapper))` (a wrapper that escaped un-unboxed) and
//! against a raw `Int` — i.e. against every state this file exists to tell
//! apart. So every assertion here compares against an exact `Value`, and
//! [`the_three_wrong_shapes_are_each_named`] pins that each of the three is
//! rejected by name rather than by a shape test that cannot see the
//! difference.

use std::sync::Arc;

use cratonvm_gc::collector::{MonitorCleanup, StopTheWorldToken};
use cratonvm_gc::{GcBackend, VmHeap};
use cratonvm_types::{
    compact_ref_fields_enabled, is_compact_object, register_class_layout,
    set_compact_ref_fields_enabled, ClassId, CompactLayout, FieldStorageKind, ObjectRef, Value,
    FIRST_LAYOUT_DOMAIN,
};

/// No-op monitor cleanup — these tests never touch the monitor table.
struct NoMonitors;
impl MonitorCleanup for NoMonitors {
    fn remap_after_gc(&self, _pointer_map: &cratonvm_types::PointerMap) {}
}

/// Test-only `StopTheWorldToken`: this file's tests are single-threaded per
/// heap, so the STW invariant is trivially satisfied.
#[inline]
fn stw() -> StopTheWorldToken {
    // SAFETY: each heap here is created, used and dropped on one thread.
    unsafe { StopTheWorldToken::new() }
}

/// `{ Object ref; int n; }` — field 0 is a REFERENCE, which is the whole point:
/// the store under test is a primitive into slot 0.
const REF_FIRST_CLASS: u32 = 0xC0FE;
/// Same shape, no registered layout, so an instance is allocated with legacy
/// 16-byte tagged cells. The A/B partner for `REF_FIRST_CLASS`.
const NO_LAYOUT_CLASS: u32 = 0xC0FF;

const HEAP_BYTES: usize = 8 * 1024 * 1024;

/// The stored payload. Deliberately not 0, not 1 and not a small ordinal: a
/// dropped write reads back as `Object(None)`, whose `as_int()` is `None`, and
/// several call sites in the tree then answer `unwrap_or(1)`. A sentinel that
/// collides with either of those would make a real failure look like a pass.
const SENTINEL: i32 = 0x5EED_BEEFu32 as i32;

fn ref_first_layout() -> Arc<CompactLayout> {
    Arc::new(CompactLayout {
        field_offsets: vec![0, 8],
        is_ref: vec![true, false],
        field_kinds: vec![FieldStorageKind::Reference, FieldStorageKind::Int],
        ref_offsets: vec![0],
        body_size: 16,
    })
}

/// Register the compact layout once per process and confirm the compact arm is
/// the one under test.
///
/// `CRATONVM_COMPACT_REF_FIELDS` defaults to ON, and the compact layout is what
/// almost everything reaches — a change that only moved the legacy arm would
/// reach nearly nothing. If the flag were off, every assertion below would be
/// exercising the legacy 16-byte cell, which was already correct, and the whole
/// file would pass vacuously. So this asserts rather than adapts.
fn install_layout() {
    set_compact_ref_fields_enabled(true);
    assert!(
        compact_ref_fields_enabled(),
        "these tests must run on the COMPACT arm — that is the default and the \
         arm the four heaps disagreed on; on the legacy arm every store below \
         was already value-preserving and the file would pass without testing \
         anything",
    );
    register_class_layout(FIRST_LAYOUT_DOMAIN, REF_FIRST_CLASS, ref_first_layout());
}

/// Store `value` into field 0 (a declared REFERENCE slot) of a fresh instance,
/// then read it straight back. One fresh object per call so no test can pass
/// on a value another test left behind.
fn round_trip(heap: &VmHeap, class_id: u32, value: Value) -> Value {
    let obj = heap.alloc_object(ClassId::new(class_id), 2);
    heap.set_field(obj, 0, value);
    heap.get_field(obj, 0)
}

/// Every backend this build can construct, named, so a failure message says
/// which arm disagreed instead of which array index did.
fn backends() -> Vec<(&'static str, GcBackend)> {
    let mut v = vec![("gen_heap", GcBackend::Generational), ("g1", GcBackend::G1)];
    #[cfg(feature = "zgc")]
    v.push(("zgc", GcBackend::Zgc));
    v
}

/// The deliverable. One store, one read, three collectors, and the assertion
/// is that they AGREE — and agree on the value that was stored.
#[test]
fn every_collector_agrees_on_a_primitive_in_a_reference_slot() {
    install_layout();
    let arms = backends();
    assert!(
        arms.len() >= 3,
        "this test is a cross-ARM agreement assertion; with {} arm(s) it proves \
         at most a third of the claim. The `zgc` feature is default-on — a build \
         that drops it must not silently reduce this to a single-collector test.",
        arms.len(),
    );

    let answers: Vec<(&str, Value)> = arms
        .iter()
        .map(|&(name, backend)| {
            let heap = VmHeap::new(backend, HEAP_BYTES);
            (
                name,
                round_trip(&heap, REF_FIRST_CLASS, Value::Int(SENTINEL)),
            )
        })
        .collect();

    // (1) Every arm gives the SAME answer. This is the half a single-collector
    //     test cannot express, and it is red before the fix in BOTH directions:
    //     gen_heap said `Int`, zgc and g1 said `Object(None)`.
    let (first_name, first) = answers[0];
    for &(name, v) in &answers[1..] {
        assert_eq!(
            v, first,
            "collectors disagree about a primitive stored in a reference slot: \
             {first_name} => {first:?}, {name} => {v:?}. One question with two \
             answers is the defect, whichever answer is the better one.",
        );
    }

    // (2) …and the answer they agree on is the value that was stored. Without
    //     this, three collectors that all dropped the write to null would pass.
    for &(name, v) in &answers {
        assert_eq!(
            v,
            Value::Int(SENTINEL),
            "{name} did not round-trip the stored primitive",
        );
    }
}

/// The compact layout must not change what a program observes — it is a
/// REPRESENTATION switch. Before the fix, `CRATONVM_COMPACT_REF_FIELDS` turned
/// `Int(SENTINEL)` into `null` on ZGC and G1, so compact and legacy were a
/// second pair of implementations of the same primitive that disagreed.
///
/// Exercised inside one process by allocating an instance of a class with NO
/// registered layout: that object is built with legacy 16-byte tagged cells
/// regardless of the flag, so this is a genuine A/B and not a re-run of the
/// same arm.
#[test]
fn the_compact_layout_answers_the_same_as_the_legacy_cell() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let compact = round_trip(&heap, REF_FIRST_CLASS, Value::Int(SENTINEL));
        let legacy = round_trip(&heap, NO_LAYOUT_CLASS, Value::Int(SENTINEL));
        assert_eq!(
            compact, legacy,
            "{name}: the compact layout and the legacy 16-byte cell disagree \
             ({compact:?} vs {legacy:?}) — the compact layout is a \
             representation change and must not be observable",
        );
        assert_eq!(compact, Value::Int(SENTINEL), "{name}: compact arm");
        assert_eq!(legacy, Value::Int(SENTINEL), "{name}: legacy arm");
    }
}

/// Guard against the A/B above degenerating: `NO_LAYOUT_CLASS` really is
/// allocated legacy and `REF_FIRST_CLASS` really is allocated compact. If a
/// future change made both legacy, `the_compact_layout_answers_the_same_as_the_legacy_cell`
/// would compare an arm against itself and pass forever.
#[test]
fn the_two_arms_of_the_ab_are_actually_different_layouts() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let compact_obj = heap.alloc_object(ClassId::new(REF_FIRST_CLASS), 2);
        let legacy_obj = heap.alloc_object(ClassId::new(NO_LAYOUT_CLASS), 2);
        assert!(
            is_compact_object(heap.get_header(compact_obj)),
            "{name}: the registered-layout class was not allocated compact, so \
             the compact arm of every test in this file is not being exercised",
        );
        assert!(
            !is_compact_object(heap.get_header(legacy_obj)),
            "{name}: the no-layout class was allocated compact, so the legacy \
             arm of the A/B is a duplicate of the compact one",
        );
    }
}

/// Every non-reference tag survives, not just `Int`. A `Long`/`Double` dropped
/// to raw 0 reads back as `Object(None)` exactly as an `Int` does, so an
/// `Int`-only test would leave three quarters of the encoder unpinned.
#[test]
fn every_primitive_tag_round_trips_on_every_collector() {
    install_layout();
    let cases = [
        Value::Int(SENTINEL),
        Value::Long(-0x0123_4567_89AB_CDEF),
        Value::Float(3.5),
        Value::Double(-17.5),
    ];
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        for stored in cases {
            let got = round_trip(&heap, REF_FIRST_CLASS, stored);
            assert_eq!(
                got, stored,
                "{name} did not round-trip {stored:?} through a reference slot",
            );
        }
    }
}

/// The three wrong shapes, each rejected by name.
///
/// "the read returns something" is true of all three of them, which is why this
/// file never asserts that. Read this as documentation of what the assertions
/// above are discriminating between.
#[test]
fn the_three_wrong_shapes_are_each_named() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let got = round_trip(&heap, REF_FIRST_CLASS, Value::Int(SENTINEL));
        assert_ne!(
            got,
            Value::Object(None),
            "{name}: the write was DROPPED to null — `write_compact_field`'s \
             `FieldStorageKind::Reference` arm mapping a non-`Object` value to \
             raw 0. This is what ZGC and G1 did before W7-84.",
        );
        assert!(
            !matches!(got, Value::Object(Some(_))),
            "{name}: the read returned the auto-box WRAPPER itself ({got:?}) \
             instead of its payload — the store side boxed but the read side \
             did not un-box, so the wrapper escapes to Java as an object with \
             no class name and no methods.",
        );
        assert_ne!(
            got,
            Value::Int(0),
            "{name}: the read returned a zeroed primitive rather than the \
             stored one — the slot was written at the wrong width or read \
             through the wrong storage kind.",
        );
    }
}

/// The convergence must not disturb the ordinary traffic through a reference
/// slot. A genuine reference stays itself, and — the one that matters —
/// `Object(None)` is NOT boxed: boxing it would replace a null field with a
/// non-null wrapper, which is the single way this scheme can make a program
/// worse (see `vm/src/vm/vm_init.rs`'s `System.out` slot-0 guard, written
/// against exactly that hazard on the collector that already boxed).
#[test]
fn references_and_nulls_are_untouched_on_every_collector() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = heap.alloc_object(ClassId::new(REF_FIRST_CLASS), 2);
        let other = heap.alloc_object(ClassId::new(REF_FIRST_CLASS), 2);

        heap.set_field(obj, 0, Value::Object(Some(other)));
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Object(Some(other)),
            "{name}: a genuine reference must come back as itself",
        );

        heap.set_field(obj, 0, Value::Object(None));
        assert_eq!(
            heap.get_field(obj, 0),
            Value::Object(None),
            "{name}: a null store must read back as null, NOT as a wrapper \
             carrying `Int(0)` — a `field == null` check must keep passing",
        );
    }
}

/// The neighbouring primitive field must be untouched by any of this. A boxing
/// bug that wrote 8 bytes of pointer where a 4-byte `int` lives would be
/// invisible to every assertion above.
#[test]
fn the_adjacent_primitive_field_is_not_clobbered() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = heap.alloc_object(ClassId::new(REF_FIRST_CLASS), 2);
        heap.set_field(obj, 1, Value::Int(0x1234_5678));
        heap.set_field(obj, 0, Value::Int(SENTINEL));
        assert_eq!(
            heap.get_field(obj, 1),
            Value::Int(0x1234_5678),
            "{name}: boxing the reference slot disturbed the adjacent int field",
        );
        assert_eq!(heap.get_field(obj, 0), Value::Int(SENTINEL), "{name}");
    }
}

/// A boxed slot must still be a REFERENCE as far as the collector is
/// concerned: the wrapper is a real object and a garbage collection must not
/// lose it or leave a dangling word behind.
///
/// This is the half W7-69 §6 predicted wrongly in the other direction — it
/// filed the defect as "a bogus pointer for the collector to mark and move",
/// which never happened because the write was being dropped. After the
/// convergence there IS a pointer there, and it is a legitimate one, so the
/// thing to prove is that tracing keeps it alive rather than that no pointer
/// exists.
#[test]
fn a_boxed_slot_survives_a_collection_on_every_collector() {
    install_layout();
    for (name, backend) in backends() {
        let heap = VmHeap::new(backend, HEAP_BYTES);
        let obj = heap.alloc_object(ClassId::new(REF_FIRST_CLASS), 2);
        heap.set_field(obj, 0, Value::Int(SENTINEL));

        // Churn so the collector has something to reclaim, then trace from
        // `obj` alone — the wrapper is reachable ONLY through the boxed slot,
        // so nothing but real tracing can keep it alive.
        for _ in 0..64 {
            let _ = heap.alloc_object(ClassId::new(NO_LAYOUT_CLASS), 2);
        }
        let mut roots: Vec<ObjectRef> = vec![obj];
        let _ = heap.collect_garbage(&stw(), &mut roots, &NoMonitors);
        // `roots` is updated in place by a moving collector — read the
        // post-collection address back rather than reusing the stale handle.
        let obj = roots[0];

        assert_eq!(
            heap.get_field(obj, 0),
            Value::Int(SENTINEL),
            "{name}: the auto-box wrapper did not survive a collection that \
             traced its holder — the boxed slot is a real reference edge and \
             must be marked and remapped like any other",
        );
    }
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

/// Every field-store implementation in this crate goes through
/// `crate::autobox`, both halves.
///
/// The behavioural tests above cover the three arms `VmHeap` can construct.
/// `gc/src/heap.rs`'s `Heap` is a FOURTH implementation of the same primitive
/// with the same encoder, and it is not reachable through `VmHeap` at all — so
/// nothing above would notice it drifting back, and nothing above would notice
/// a FIFTH heap arriving without the calls. That is exactly how this
/// disagreement got here: `gen_heap` grew the boxing arm and the others did
/// not, and no test could see the difference.
///
/// A text scan, deliberately. The property is "these four files call this one
/// module", which is structural, and a structural property closes by becoming a
/// ratchet rather than by being re-argued.
#[test]
fn all_four_field_store_implementations_route_through_the_shared_primitive() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for file in ["gen_heap.rs", "zgc.rs", "g1.rs", "heap.rs"] {
        let path = src.join(file);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            text.contains("autobox::box_for_reference_slot"),
            "{file} defines a field store but does not call \
             `crate::autobox::box_for_reference_slot`. A primitive written into \
             a declared-reference slot there will be handed to \
             `write_compact_field`, whose `FieldStorageKind::Reference` arm maps \
             it to raw 0 — i.e. the write is silently dropped to null while the \
             other heaps preserve it. That is the W7-84 defect returning.",
        );
        assert!(
            text.contains("autobox::unbox_reference_slot"),
            "{file} does not call `crate::autobox::unbox_reference_slot`, so a \
             boxed slot read there hands the caller the wrapper — an object of \
             the synthetic `AUTOBOX_CLASS_ID` with no class name and no methods \
             — instead of the value inside it.",
        );
    }
}

/// …and there is no FIFTH one hiding. If a new heap appears, it must be added
/// to the list above (and to `backends()`) rather than quietly inheriting the
/// bug the list exists to prevent.
#[test]
fn there_are_exactly_four_field_store_implementations() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    // RECURSIVE, not `read_dir` on the top level. `gc/src/zgc/` alone holds a
    // dozen modules; a heap added under any of them would be invisible to a
    // flat scan, which is the exact blind spot this ratchet exists to close.
    let mut stack = vec![src];
    let mut found: Vec<String> = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("gc/src is readable") {
            let path = entry.expect("readable dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            // The two spellings the four implementations, the trait declaration
            // and the dispatcher all share. Validated against ground truth
            // (`grep -rln` over `gc/src`) before this expectation was written,
            // rather than after: it returns these six files and no others.
            if text.contains("fn set_field(&self, obj: ObjectRef, index: usize, value: Value)")
                || text
                    .contains("fn set_field(&self, obj_ref: ObjectRef, index: usize, value: Value)")
            {
                found.push(
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
    }
    found.sort();
    assert_eq!(
        found,
        vec![
            "collector.rs".to_string(),
            "g1.rs".to_string(),
            "gen_heap.rs".to_string(),
            "heap.rs".to_string(),
            "vm_heap.rs".to_string(),
            "zgc.rs".to_string(),
        ],
        "the set of files declaring a field store changed. `collector.rs` is \
         the trait declaration and `vm_heap.rs` is the dispatcher; the other \
         four are implementations and every one of them must appear in \
         `all_four_field_store_implementations_route_through_the_shared_primitive`. \
         A new implementation that is not in that list inherits the W7-84 \
         disagreement by default.",
    );
}
