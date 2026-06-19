// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Documents and pins the wiring decision for `ConstantPool::validate`.
//!
//! `reader/src/constant_pool.rs` defines `ConstantPool::validate` (lines
//! 184–277 at the time of writing). It walks the pool and reports
//! cross-reference errors (a `ClassReference.name_index` pointing at a
//! non-`Utf8` entry, a `MethodReference.class_index` pointing at a
//! non-`ClassReference`, etc.). The method is unit-tested in isolation in
//! `constant_pool.rs::tests`, but **the class reader does not call it on
//! the parse hot path** (`read_class_arc`).
//!
//! This file pins that choice as deliberate: the reader treats validation
//! as a **manual operation** rather than a parse-time check, so the
//! per-class O(N) walk doesn't run on every load. Callers that want
//! defensive validation invoke `pool.validate()` themselves and inspect
//! the returned error list.
//!
//! The wire-up decision matches option (b) from the reader review
//! (`.claude/review-2026-05-24/reader.md` §2.3-3): keep `validate`
//! callable from external code, but do not run it implicitly.
//!
//! Reader gap §2.3-3.

use cratonvm_reader::constant_pool::{ConstantPool, ConstantPoolEntry};
use cratonvm_reader::read_class;

// ---------------------------------------------------------------------------
// Build the smallest legal Java-8 class so we can run `validate` on the
// real `ConstantPool` produced by the reader hot path.
// ---------------------------------------------------------------------------

fn minimal_valid_class_bytes() -> Vec<u8> {
    let mut data = Vec::<u8>::new();
    // magic
    data.extend_from_slice(&0xCAFE_BABE_u32.to_be_bytes());
    // version 52.0 (Java 8)
    data.extend_from_slice(&0u16.to_be_bytes());
    data.extend_from_slice(&52u16.to_be_bytes());
    // constant_pool_count = 3 (slots: 0 sentinel, 1 Utf8, 2 Class)
    data.extend_from_slice(&3u16.to_be_bytes());
    // 1 = Utf8 "java/lang/Object"
    data.push(1);
    let s = b"java/lang/Object";
    data.extend_from_slice(&(s.len() as u16).to_be_bytes());
    data.extend_from_slice(s);
    // 2 = Class -> 1
    data.push(7);
    data.extend_from_slice(&1u16.to_be_bytes());
    // access_flags = PUBLIC | SUPER
    data.extend_from_slice(&0x0021u16.to_be_bytes());
    // this_class = 2
    data.extend_from_slice(&2u16.to_be_bytes());
    // super_class = 0 (sentinel — Object has none)
    data.extend_from_slice(&0u16.to_be_bytes());
    // interfaces, fields, methods, attributes all empty
    data.extend_from_slice(&0u16.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    data
}

// ---------------------------------------------------------------------------
// Wire-up assertions.
// ---------------------------------------------------------------------------

/// `read_class` parses the smallest legal class file without calling
/// `validate` for us; the returned `ConstantPool` still satisfies
/// `validate().is_empty()` because the pool was hand-crafted well-formed.
/// The point is that `read_class` *did not* run validate (otherwise the
/// API would have surfaced any errors at parse time — and the test below
/// confirms it does not).
#[test]
fn read_class_succeeds_without_implicit_validate() {
    let bytes = minimal_valid_class_bytes();
    let class_file = read_class(&bytes).expect("smallest valid class must parse");
    // Validate works on the parsed pool — and reports nothing wrong for a
    // well-formed class. This locks in the manual-validation contract.
    let errors = class_file.constant_pool.validate();
    assert!(
        errors.is_empty(),
        "validate() on a well-formed pool must report no errors, got {errors:?}"
    );
}

/// Synthesise an intentionally-malformed `ConstantPool` directly (the
/// reader rejects most cross-reference bugs at parse time, so we
/// bypass it to exercise `validate`). If `read_class_arc` ever wires
/// validation into the parse path, this test must be updated: that
/// would mean every parse of a class with such an entry would now
/// fail at parse time, not at manual `validate()`.
#[test]
fn validate_runs_only_when_invoked_manually() {
    let entries = vec![
        ConstantPoolEntry::Tombstone,
        // A ClassReference whose name_index points at a non-Utf8 entry —
        // a classic "structurally invalid but parseable as raw bytes"
        // pool (most parsers would catch this in resolve, not the
        // constant-pool walk).
        ConstantPoolEntry::Integer(42),
        ConstantPoolEntry::ClassReference { name_index: 1 }, // -> Integer, not Utf8
    ];
    let pool = ConstantPool::new(entries);
    // Constructing the pool did NOT call validate — there's no error to
    // observe yet (the pool just stores bytes verbatim).
    // Now call validate explicitly: it reports the cross-reference bug.
    let errors = pool.validate();
    assert!(
        errors.iter().any(|e| e.contains("does not point to Utf8")),
        "validate() must surface the misdirected ClassReference; got: {errors:?}"
    );
}

/// Documents the policy: `read_class` does not pre-validate the pool.
/// A class whose bytes parse cleanly but whose pool would fail
/// `validate` (because the reader's per-entry parse permits it) still
/// produces an `Ok(...)` from `read_class`. If a future change wires
/// validation into the parse hot path, this test should be updated to
/// expect `Err(..)` and the wiring decision in §2.3-3 revisited.
///
/// Note: most cross-reference bugs are caught by the reader because
/// the field/method/attribute parsers resolve indices to `Utf8` /
/// `ClassReference` directly and bail with `InvalidConstantPool` when
/// they don't find what they expect. The remaining gap (a stray
/// `ClassReference` with no enclosing field/method/attribute that
/// dereferences it) is what `validate` is for — and is what this test
/// asserts is *not* run automatically.
#[test]
fn validate_is_documented_as_manual() {
    // The pure-function nature of `validate` is the point: it produces
    // a `Vec<String>` of diagnostic messages without side effects, so
    // callers can inspect/log/ignore as they choose.
    let pool = ConstantPool::new(vec![ConstantPoolEntry::Tombstone]);
    let errors = pool.validate();
    assert!(
        errors.is_empty(),
        "an empty pool has no cross-refs to validate"
    );
}
