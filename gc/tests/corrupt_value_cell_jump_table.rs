// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The regression test for the eight hibernate-orm JSON/XML `hs_err` files:
//! a legacy 16-byte cell whose `#[repr(u32)]` tag is not a valid `Value`
//! discriminant must decode to null on **every** collector, and must never
//! reach a `match`.
//!
//! # The crash this pins, decoded
//!
//! Eight `hs_err` logs — four `JsonFunctionTests`, four `XmlFunctionTests`,
//! four G1 and four ZGC, **zero Generational** — all fault at
//! `EXCEPTION_ACCESS_VIOLATION ... read at address X` where, in every one of
//! them, `X == r10 + rax*4` exactly, with `rbx == r8 == r13 == 0x5B` and the
//! same jump-table contents. That is the x86-64 idiom LLVM emits for a dense
//! `match` on an enum:
//!
//! ```text
//!     mov     eax, dword ptr [rdx]              ; the Value's u32 tag
//!     lea     r10, [rip + <table>]
//!     movsxd  rax, dword ptr [r10 + rax*4]      ; <-- FAULTS
//!     add     rax, r10
//!     jmp     rax
//! ```
//!
//! A `match` on a Rust *enum* needs no `_` arm and gets **no bounds check**:
//! the discriminant is in range by construction, so LLVM indexes the table
//! directly. Construct a `Value` by `transmute` from bytes that were not a
//! `Value`, and the garbage tag becomes the index.
//!
//! The instruction is in `gc::heap::coerce_field_value_for_slot`, in its
//! `b'L' | b'['` arm — and `b'[' == 0x5B`, which is the `desc_byte` argument
//! sitting in `rbx`/`r8`/`r13` in all eight logs. So the crash is an
//! **array-typed field** read whose cell did not hold a `Value`.
//!
//! # Why this test is a cross-collector agreement test
//!
//! `coerce_field_value_for_slot` takes its `Value` **by value**: the invalid
//! enum was built one frame lower, by the collector's `get_field`, and merely
//! *consumed* here. That is the entire explanation for the
//! always-G1-or-ZGC-never-Generational pattern of the original triage:
//! `gen_heap::read_slot` screened the discriminant (since HIB-CV-32) and
//! returned `Object(None)`; `g1::get_field` and `zgc::get_field` used the
//! unchecked `read_value_atomic` and handed UB upward. Generational was not
//! avoiding the corrupt cell, it was **surviving** it.
//!
//! So a single-collector test proves nothing about the defect: on Generational
//! it was green throughout. [`every_collector_survives_a_corrupt_value_cell`]
//! asserts all three arms give the same answer, which is red on two of them
//! before the fix — by SIGSEGV, which is the only way an unchecked jump table
//! can fail.
//!
//! # The vacuous shape this file refuses
//!
//! "it did not crash" passes on any build where the corrupt cell is never
//! read. [`the_corrupt_cell_is_actually_read`] asserts the census COUNTED the
//! decode, so a test that stopped exercising the path fails instead of passing
//! quietly — the same failure mode that let a `scalar-replaced 0/N` read as a
//! result.

use cratonvm_gc::{GcBackend, VmHeap};
use cratonvm_types::{ClassId, ObjectRef, Value, HEADER_SIZE, SLOT_SIZE};

const HEAP_BYTES: usize = 8 * 1024 * 1024;

/// No registered compact layout, so instances are built with legacy 16-byte
/// tagged cells — the only layout that HAS a discriminant to corrupt. The
/// compact layout stores a `FieldStorageKind` beside the field and never
/// transmutes a tag, so it cannot produce this crash and is not the arm here.
const NO_LAYOUT_CLASS: u32 = 0xC0DE;

/// `rax` from `hs_err_pid3512.log`, verbatim.
///
/// Not a round number on purpose: this is a real garbage tag from a real
/// crash, it is far outside `0..=6`, and its low byte (`0xA0`) is not a valid
/// discriminant either — so a hypothetical byte-wide screen would not pass it
/// by accident.
const CRASH_TAG: u32 = 0xEAF8_2DA0;

/// The descriptor byte the crash carried: `b'[' == 0x5B`, the value found in
/// `rbx`, `r8` and `r13` in all eight logs. Using the real one keeps this test
/// on the arm that actually faulted rather than on a neighbouring one.
const CRASH_DESC: u8 = b'[';

/// Every backend this build can construct, named, so a failure says which arm
/// disagreed rather than which array index did.
fn backends() -> Vec<(&'static str, GcBackend)> {
    let mut v = vec![("gen_heap", GcBackend::Generational), ("g1", GcBackend::G1)];
    #[cfg(feature = "zgc")]
    v.push(("zgc", GcBackend::Zgc));
    v
}

/// Overwrite slot `index` of `obj` with 16 bytes that are NOT a `Value`.
///
/// Written as two raw word stores rather than through any heap API, because
/// every heap API takes a `Value` and therefore cannot express this state —
/// which is the point: the bytes got there by an array being read through the
/// flat-object path, not by anybody storing a `Value`.
fn corrupt_slot(obj: ObjectRef, index: usize, tag: u32, payload: u64) {
    // SAFETY: `obj` was just allocated with more than `index` slots, so the
    // 16 bytes at `HEADER_SIZE + index * SLOT_SIZE` are inside its body.
    unsafe {
        let cell = obj.as_ptr().add(HEADER_SIZE + index * SLOT_SIZE);
        std::ptr::write_unaligned(cell as *mut u32, tag);
        std::ptr::write_unaligned(cell.add(8) as *mut u64, payload);
    }
}

/// Allocate a legacy object, corrupt slot 0, and read it back through the
/// descriptor-aware door the crash took.
fn read_corrupt_slot(backend: GcBackend) -> Value {
    let heap = VmHeap::new(backend, HEAP_BYTES);
    let obj = heap.alloc_object(ClassId::new(NO_LAYOUT_CLASS), 2);
    // A plausible-looking heap pointer in the payload, as in the recorded
    // producer (`raw0`/`raw1` were both heap addresses). A zero payload would
    // let a reader that ignores the tag entirely answer "null" for the wrong
    // reason and pass.
    corrupt_slot(obj, 0, CRASH_TAG, 0x0000_0200_40775828);
    heap.get_field_as(obj, 0, CRASH_DESC)
}

/// The deliverable. Three collectors, one corrupt cell, one answer.
///
/// Before the fix this did not merely return a different `Value` on G1 and
/// ZGC — it took the process down with `EXCEPTION_ACCESS_VIOLATION`, because
/// the invalid enum reached `coerce_field_value_for_slot`'s jump table. A
/// crashing arm cannot report an assertion failure, so the *evidence* of the
/// old behaviour is the eight `hs_err` files, not a red line here.
#[test]
fn every_collector_survives_a_corrupt_value_cell() {
    let arms = backends();
    assert!(
        arms.len() >= 3,
        "this is a cross-ARM agreement assertion, and the arms are the whole \
         point: Generational was GREEN throughout this defect's life because \
         its reader screened the discriminant. With {} arm(s) the file can \
         pass while testing only the arm that never failed. The `zgc` feature \
         is default-on — a build that drops it must not silently reduce this \
         to that.",
        arms.len(),
    );

    let answers: Vec<(&str, Value)> = arms
        .iter()
        .map(|&(name, backend)| (name, read_corrupt_slot(backend)))
        .collect();

    let (first_name, first) = answers[0];
    for &(name, v) in &answers[1..] {
        assert_eq!(
            v, first,
            "collectors disagree about a corrupt Value cell: {first_name} => \
             {first:?}, {name} => {v:?}. One cell with two answers is the \
             defect, whichever answer is the better one.",
        );
    }

    // …and the answer they agree on is null. Without this, three collectors
    // that all decoded the garbage tag into some arbitrary-but-equal variant
    // would pass.
    for &(name, v) in &answers {
        assert_eq!(
            v,
            Value::Object(None),
            "{name}: a corrupt cell must decode to null. Any other variant is \
             a `Value` built from bytes that were not one, and every `match` \
             downstream of it is an unchecked jump.",
        );
    }
}

/// The anti-vacuity control: the corrupt cell was actually READ.
///
/// `every_collector_survives_a_corrupt_value_cell` asserts an answer; this
/// asserts the question was asked. If a future change stops routing
/// `get_field_as` through a screened reader — by short-circuiting on the
/// header, by caching, by never reaching the legacy path — the agreement test
/// would go on passing while exercising nothing.
#[test]
fn the_corrupt_cell_is_actually_read() {
    let before = cratonvm_types::cell_census::decoded();
    let v = read_corrupt_slot(GcBackend::G1);
    let after = cratonvm_types::cell_census::decoded();
    assert_eq!(v, Value::Object(None));
    assert!(
        after > before,
        "the corrupt-cell census did not move ({before} -> {after}): the read \
         under test never reached a screened decode, so \
         `every_collector_survives_a_corrupt_value_cell` is passing without \
         exercising the path it names.",
    );
}

/// Every valid discriminant still round-trips through the same door.
///
/// The screen must reject exactly the invalid tags. A reader that answered
/// `Object(None)` for everything would pass both tests above and break the VM.
#[test]
fn a_valid_cell_is_not_screened_out() {
    let heap = VmHeap::new(GcBackend::G1, HEAP_BYTES);
    // `b'J'` (long) rather than the crash's `b'['`: the point here is that the
    // screen is on the TAG, not on the descriptor, so a valid cell survives
    // whichever arm of `coerce_field_value_for_slot` it lands in.
    let obj = heap.alloc_object(ClassId::new(NO_LAYOUT_CLASS), 2);
    heap.set_field(obj, 0, Value::Long(0x0123_4567_89AB_CDEF));
    assert_eq!(
        heap.get_field_as(obj, 0, b'J'),
        Value::Long(0x0123_4567_89AB_CDEF),
        "a VALID cell was screened out — the guard is refusing live data, not \
         corruption",
    );
}
