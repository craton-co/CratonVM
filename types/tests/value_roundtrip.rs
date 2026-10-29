// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Property-based round-trip invariants for the `Value` codec.
//!
//! Pins two contracts:
//!
//!   * `decode_value(encode_value(v)) == v` for every `Value` variant the
//!     codec is defined on.
//!   * `CompactValue::from_value(v).to_value()` matches `v`, modulo the
//!     documented Long/Double untagged ambiguity (see comment below).
//!
//! Types gap §2.3-1 from `.claude/review-2026-05-24/types.md`.
//!
//! # Object-Some canonicalization caveat
//!
//! For `Value::Object(Some(_))` the underlying `ObjectRef` wraps a raw
//! pointer; the codec only round-trips bit-exactly when the pointer is
//! non-null, 8-byte aligned, above the null guard page, and known through
//! prior `ObjectRef` construction. `decode_value` degrades rejected object
//! payloads to `Value::Object(None)`, while `CompactValue::to_value` preserves
//! rejected `SUB_OBJECT` payloads as bit-exact longs. The strategy below
//! generates only well-formed object pointers — anything else is covered
//! by the dedicated degradation tests inside `src/value.rs` and
//! `src/compact_value.rs`.

use cratonvm_types::{decode_value, encode_value, CompactValue, ObjectRef, Value};
use proptest::prelude::*;

/// A proptest strategy that yields every legal `Value` variant.  Object
/// payloads come from the well-formed-pointer subspace defined above.
fn arbitrary_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        any::<i32>().prop_map(Value::Int),
        any::<i64>().prop_map(Value::Long),
        any::<u32>().prop_map(|bits| Value::Float(f32::from_bits(bits))),
        any::<u64>().prop_map(|bits| Value::Double(f64::from_bits(bits))),
        // Well-formed (non-null, 8-byte aligned) object pointers.  The
        // The lower bound stays above the null guard page, and clearing the
        // low bits keeps the 8-byte alignment invariant. Capped well within
        // the 47-bit address space that `CompactValue` also tolerates.
        (0x1000u64..(1u64 << 40)).prop_map(|n| {
            let raw = (n & !0x7) as *mut u8;
            // SAFETY: `raw` is non-null and 8-byte aligned by construction.
            Value::Object(Some(unsafe { ObjectRef::from_raw(raw) }))
        }),
        Just(Value::Object(None)),
        any::<u32>().prop_map(Value::ReturnAddress),
        Just(Value::Uninitialized),
    ]
}

proptest! {
    // The Value (u64, u8) codec: encode_value ↔ decode_value round-trips
    // bit-exactly for every variant.  Floats and doubles use bitwise
    // comparison via the proptest strategy to avoid NaN != NaN issues.
    #[test]
    fn encode_decode_value_round_trip(v in arbitrary_value()) {
        let (bits, tag) = encode_value(v);
        let decoded = decode_value(bits, tag);
        // Compare via raw bits for the float/double payloads (NaN != NaN
        // under PartialEq); other variants compare directly.
        match (v, decoded) {
            (Value::Float(a), Value::Float(b)) => {
                // Float NaNs are not == NaN under PartialEq; compare bits.
                prop_assert_eq!(a.to_bits(), b.to_bits());
            }
            (Value::Double(a), Value::Double(b)) => {
                prop_assert_eq!(a.to_bits(), b.to_bits());
            }
            (a, b) => prop_assert_eq!(a, b),
        }
    }
}

proptest! {
    // CompactValue::from_value(v).to_value() must reproduce `v`, with the
    // single documented caveat that an untagged 8-byte slot cannot
    // distinguish Long from Double — `to_value` always decodes such a
    // slot as Double.  So a `Value::Long(n)` whose bit pattern is not in
    // the NaN-tagged collision space round-trips through `to_value` as
    // `Value::Double(f64::from_bits(n as u64))`.  The same caveat is
    // documented at length on `CompactValue::to_value`.
    //
    // A second caveat applies to NaN doubles whose bit pattern collides
    // with the NaN-tag pattern: `CompactValue::double` canonicalises
    // them to the standard quiet NaN.  We accept any NaN as a valid
    // round-trip for a NaN input.
    #[test]
    fn compact_value_from_to_round_trip(v in arbitrary_value()) {
        let cv = CompactValue::from_value(v);
        let decoded = cv.to_value();
        match (v, decoded) {
            // Untagged-Long case: `from_value` stores raw i64 bits;
            // `to_value` returns `Value::Double` for any untagged slot
            // whose bits don't match the NaN-tag pattern.  The caller
            // can recover the long via `as_long_unchecked()` /
            // `decode_by_descriptor(b'J')`; the round-trip through the
            // generic `to_value` path canonicalises to Double.
            (Value::Long(_), Value::Double(_)) => {
                // Documented ambiguity — allowed.  The `as_long` API
                // covers exact long round-trips and is exercised by
                // `compact_value.rs::tests::long_*` tests already.
                let arg_bits = match v { Value::Long(n) => n as u64, _ => unreachable!() };
                let dec_bits = match decoded { Value::Double(d) => d.to_bits(), _ => unreachable!() };
                prop_assert_eq!(arg_bits, dec_bits);
            }
            // Collision case where the long bits land in the SUB_LONG_*
            // sub-tag space: `to_value` returns `Value::Long(_)` and we
            // expect bit equality.
            (Value::Long(a), Value::Long(b)) => prop_assert_eq!(a, b),
            (Value::Float(a), Value::Float(b)) => {
                prop_assert_eq!(a.to_bits(), b.to_bits());
            }
            (Value::Double(a), Value::Double(b)) => {
                // NaN canonicalization: if `a` is NaN, `from_value`
                // may have replaced its bit pattern with the canonical
                // quiet NaN.  Accept any NaN as a valid round-trip in
                // that case; otherwise require exact bit equality.
                if a.is_nan() {
                    prop_assert!(b.is_nan(), "NaN must round-trip as some NaN");
                } else {
                    prop_assert_eq!(a.to_bits(), b.to_bits());
                }
            }
            (a, b) => prop_assert_eq!(a, b),
        }
    }
}
