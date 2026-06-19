// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP1.6 — runtime-side `VarHandle` support.
//!
//! `java.lang.invoke.VarHandle` is the replacement for `sun.misc.Unsafe`
//! field accessors added in Java 9. It provides atomic, volatile,
//! opaque, and plain read / write / CAS / atomic-update operations on:
//!
//! * instance fields (`Lookup.findVarHandle`)
//! * static fields (`Lookup.findStaticVarHandle`)
//! * array elements (`MethodHandles.arrayElementVarHandle`)
//!
//! Most of the per-method native plumbing already lives in
//! `native-builtins/src/lang_invoke.rs` where the synthetic-field layout
//! is defined. This module adds the **runtime helpers** that the
//! interpreter / JIT glue needs when a VH operation is resolved in
//! terms of a real (loaded) field on a real class — specifically the
//! (kind, field descriptor, memory ordering) canonicalisation used by
//! WP1.6's probe acceptance tests and by the `findVarHandle` entry
//! points that create a VH pointing at a regular `volatile int` field.

use std::sync::atomic::{AtomicUsize, Ordering};

use crate::types::Value;

/// The kind of access a `VarHandle` points at.
///
/// Matches the synthetic slot-0 tag in
/// `native-builtins/src/lang_invoke.rs` (`VH_KIND_*` constants) one-for-one.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VarHandleKind {
    /// `findVarHandle` — per-instance field access.
    InstanceField,
    /// `findStaticVarHandle` — static field access.
    StaticField,
    /// `arrayElementVarHandle` — element of an array.
    ArrayElement,
}

impl VarHandleKind {
    /// Decode the `VH_KIND` tag stored in the VarHandle synthetic field.
    /// Returns `None` for unknown values.
    pub fn from_tag(tag: i32) -> Option<Self> {
        match tag {
            0 => Some(VarHandleKind::InstanceField),
            1 => Some(VarHandleKind::StaticField),
            2 => Some(VarHandleKind::ArrayElement),
            _ => None,
        }
    }

    /// Encode back to the synthetic tag value.
    pub fn to_tag(self) -> i32 {
        match self {
            VarHandleKind::InstanceField => 0,
            VarHandleKind::StaticField => 1,
            VarHandleKind::ArrayElement => 2,
        }
    }
}

/// Memory ordering of a VarHandle access method.
///
/// Mirrors `java.lang.invoke.VarHandle.AccessMode`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VarHandleOrder {
    /// `get`/`set` — plain (no fence).
    Plain,
    /// `getOpaque`/`setOpaque` — ordered but non-atomic.
    Opaque,
    /// `getAcquire`/`setRelease` — one-way barriers.
    AcquireRelease,
    /// `getVolatile`/`setVolatile` — full JMM volatile.
    Volatile,
}

impl VarHandleOrder {
    /// Decide the memory ordering implied by a `VarHandle` access-method name.
    ///
    /// Unrecognised names fall back to `Plain`.
    pub fn from_method_name(name: &str) -> Self {
        match name {
            "get" | "set" => VarHandleOrder::Plain,
            "getOpaque" | "setOpaque" => VarHandleOrder::Opaque,
            "getAcquire"
            | "setRelease"
            | "compareAndExchangeAcquire"
            | "compareAndExchangeRelease"
            | "weakCompareAndSetAcquire"
            | "weakCompareAndSetRelease" => VarHandleOrder::AcquireRelease,
            "getVolatile"
            | "setVolatile"
            | "compareAndSet"
            | "compareAndExchange"
            | "weakCompareAndSet"
            | "weakCompareAndSetPlain"
            | "getAndSet"
            | "getAndAdd"
            | "getAndBitwiseOr"
            | "getAndBitwiseAnd"
            | "getAndBitwiseXor" => VarHandleOrder::Volatile,
            _ => VarHandleOrder::Plain,
        }
    }

    /// Convert this ordering to the `std::sync::atomic::Ordering` that a
    /// load/store fence should use. Release/Acquire variants distinguish
    /// load vs store via the caller picking `load_ordering()` /
    /// `store_ordering()`.
    pub fn load_ordering(self) -> Ordering {
        match self {
            VarHandleOrder::Plain => Ordering::Relaxed,
            VarHandleOrder::Opaque => Ordering::Relaxed,
            VarHandleOrder::AcquireRelease => Ordering::Acquire,
            VarHandleOrder::Volatile => Ordering::SeqCst,
        }
    }

    /// Matching store ordering — see [`Self::load_ordering`].
    pub fn store_ordering(self) -> Ordering {
        match self {
            VarHandleOrder::Plain => Ordering::Relaxed,
            VarHandleOrder::Opaque => Ordering::Relaxed,
            VarHandleOrder::AcquireRelease => Ordering::Release,
            VarHandleOrder::Volatile => Ordering::SeqCst,
        }
    }
}

/// A fully-resolved VarHandle target: what class, what field, what type.
///
/// Produced by `findVarHandle` / `findStaticVarHandle` during bootstrap of
/// the invoking call-site. The interpreter uses this plus a
/// [`VarHandleOrder`] derived from the method name to dispatch to the
/// right heap operation.
#[derive(Clone, Debug)]
pub struct ResolvedVarHandle {
    /// Access mode (instance/static/array).
    pub kind: VarHandleKind,
    /// Target class (internal name, slash-separated). `None` for array
    /// element handles, which carry the array class on the receiver.
    pub class: Option<String>,
    /// Field name (ignored for array-element handles).
    pub field: Option<String>,
    /// JVM field descriptor, e.g. `"I"`, `"J"`, `"Ljava/lang/String;"`.
    pub descriptor: String,
    /// Resolved zero-based field slot index on the declaring class (or
    /// array-element offset for kind=ArrayElement). `usize::MAX` means
    /// unresolved — the caller must resolve before dispatch.
    pub field_index: usize,
}

impl ResolvedVarHandle {
    /// Build an unresolved handle — `field_index` defaulted to
    /// `usize::MAX`. Callers resolve the slot index once and cache the
    /// populated `ResolvedVarHandle`.
    pub fn new(kind: VarHandleKind, descriptor: impl Into<String>) -> Self {
        Self {
            kind,
            class: None,
            field: None,
            descriptor: descriptor.into(),
            field_index: usize::MAX,
        }
    }

    /// `true` if the descriptor encodes a 64-bit primitive (long/double)
    /// that occupies two JVM local slots.
    pub fn is_category2(&self) -> bool {
        matches!(self.descriptor.as_str(), "J" | "D")
    }

    /// Default value for this VH's descriptor (used by CAS when the
    /// expected value is `null` / zero).
    pub fn default_value(&self) -> Value {
        match self.descriptor.chars().next() {
            Some('I') | Some('B') | Some('C') | Some('S') | Some('Z') => Value::Int(0),
            Some('J') => Value::Long(0),
            Some('F') => Value::Float(0.0),
            Some('D') => Value::Double(0.0),
            _ => Value::Object(None),
        }
    }
}

/// Diagnostic counter — number of `ResolvedVarHandle`s produced this VM
/// run. Integration tests use this to verify the cache path fires.
static RESOLVED_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Record that a VH was resolved. Called by the native side after
/// `findVarHandle` completes successfully.
pub fn note_resolved() {
    RESOLVED_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Current number of successful `findVarHandle` / `findStaticVarHandle`
/// calls. Primarily exposed for integration tests.
pub fn resolved_count() -> usize {
    RESOLVED_COUNT.load(Ordering::Relaxed)
}

/// Compare two primitive `Value`s for CAS equality — used by the
/// VarHandle `compareAndSet` family when the expected value was supplied
/// via an autoboxed `Integer`/`Long`/etc. Returns `false` for any
/// unsupported pair rather than panicking.
pub fn values_equal_for_vh_cas(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Long(x), Value::Long(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => x.to_bits() == y.to_bits(),
        (Value::Double(x), Value::Double(y)) => x.to_bits() == y.to_bits(),
        (Value::Object(None), Value::Object(None)) => true,
        (Value::Object(Some(x)), Value::Object(Some(y))) => x.as_ptr() == y.as_ptr(),
        // Cross-category checks: treat `Value::Object(None)` as the zero of
        // the other side's primitive type (autoboxing from null).
        (Value::Object(None), Value::Int(0))
        | (Value::Int(0), Value::Object(None))
        | (Value::Object(None), Value::Long(0))
        | (Value::Long(0), Value::Object(None)) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_tag_roundtrip() {
        for &k in &[
            VarHandleKind::InstanceField,
            VarHandleKind::StaticField,
            VarHandleKind::ArrayElement,
        ] {
            assert_eq!(VarHandleKind::from_tag(k.to_tag()), Some(k));
        }
        assert_eq!(VarHandleKind::from_tag(99), None);
    }

    #[test]
    fn method_name_ordering_maps_correctly() {
        assert_eq!(
            VarHandleOrder::from_method_name("get"),
            VarHandleOrder::Plain
        );
        assert_eq!(
            VarHandleOrder::from_method_name("getOpaque"),
            VarHandleOrder::Opaque
        );
        assert_eq!(
            VarHandleOrder::from_method_name("getAcquire"),
            VarHandleOrder::AcquireRelease
        );
        assert_eq!(
            VarHandleOrder::from_method_name("setRelease"),
            VarHandleOrder::AcquireRelease
        );
        assert_eq!(
            VarHandleOrder::from_method_name("compareAndSet"),
            VarHandleOrder::Volatile
        );
        assert_eq!(
            VarHandleOrder::from_method_name("getVolatile"),
            VarHandleOrder::Volatile
        );
        // Unknown names fall back to Plain so callers never panic.
        assert_eq!(
            VarHandleOrder::from_method_name("mystery"),
            VarHandleOrder::Plain
        );
    }

    #[test]
    fn orderings_map_to_sensible_atomic_ordering() {
        assert_eq!(VarHandleOrder::Plain.load_ordering(), Ordering::Relaxed);
        assert_eq!(VarHandleOrder::Volatile.load_ordering(), Ordering::SeqCst);
        assert_eq!(
            VarHandleOrder::AcquireRelease.store_ordering(),
            Ordering::Release
        );
        assert_eq!(
            VarHandleOrder::AcquireRelease.load_ordering(),
            Ordering::Acquire
        );
    }

    #[test]
    fn resolved_handle_default_values_by_descriptor() {
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "I");
        assert_eq!(h.default_value(), Value::Int(0));
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "J");
        assert_eq!(h.default_value(), Value::Long(0));
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "D");
        assert_eq!(h.default_value(), Value::Double(0.0));
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "Ljava/lang/String;");
        assert_eq!(h.default_value(), Value::Object(None));
    }

    #[test]
    fn cas_equality_handles_primitives_and_null() {
        assert!(values_equal_for_vh_cas(&Value::Int(42), &Value::Int(42)));
        assert!(!values_equal_for_vh_cas(&Value::Int(1), &Value::Int(2)));
        assert!(values_equal_for_vh_cas(&Value::Long(-1), &Value::Long(-1)));
        assert!(values_equal_for_vh_cas(
            &Value::Object(None),
            &Value::Object(None)
        ));
        // Null vs primitive zero cross-category
        assert!(values_equal_for_vh_cas(
            &Value::Object(None),
            &Value::Int(0)
        ));
        assert!(values_equal_for_vh_cas(
            &Value::Long(0),
            &Value::Object(None)
        ));
        // NaN in bits form still equal to itself via to_bits()
        let nan = Value::Double(f64::NAN);
        assert!(values_equal_for_vh_cas(&nan, &nan));
    }

    #[test]
    fn is_category2_for_long_and_double() {
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "J");
        assert!(h.is_category2());
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "D");
        assert!(h.is_category2());
        let h = ResolvedVarHandle::new(VarHandleKind::InstanceField, "I");
        assert!(!h.is_category2());
    }

    #[test]
    fn resolved_counter_increments_on_each_note() {
        let before = resolved_count();
        note_resolved();
        note_resolved();
        let after = resolved_count();
        assert!(after >= before + 2);
    }
}
