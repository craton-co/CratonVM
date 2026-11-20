// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Descriptor-safe instance-field reads for natives.
//!
//! # Why this module exists
//!
//! [`NativeContext::get_field_by_name`] and [`NativeContext::get_field`] do
//! **not** answer the same question about the same slot of the same object.
//! Verified against the production `NativeContextImpl`
//! (`vm/src/vm/vm_exec.rs`, 2026-08-01):
//!
//! * `get_field(obj, index)` (vm_exec.rs:8605) resolves the declared field
//!   descriptor for `(class_id, index)` and routes the slot through
//!   `heap.get_field_as`, i.e. `coerce_field_value_by_descriptor`
//!   (`gc/src/heap.rs:1444`). A reference-typed (`L`/`[`) field therefore
//!   **always** surfaces as `Value::Object(..)`.
//! * `get_field_by_name(obj, name)` (vm_exec.rs:8735) resolves the name to a
//!   slot and then calls the **raw** `heap.get_field` with no descriptor
//!   decode at all.
//!
//! The two used to disagree for an *unwritten reference slot*: `alloc_object`
//! zeroes the field block, and after `Value::Object` gained its `NonNull`
//! niche the all-zero 16-byte slot decodes as `Value::Int(0)` — discriminant
//! 0 — not as `Value::Object(None)` (`gc/src/heap.rs:398-411` documents
//! exactly this). `init_primitive_fields`
//! (`vm/src/runtime/interpreter/gc_and_alloc.rs`) used to write a typed zero
//! for every *primitive* field but deliberately skip reference fields, on a
//! comment ("already `Object(None)` from zero memory") that the niche change
//! made false.
//!
//! **`G56-1` (2026-08-17) fixed that skip**: `init_primitive_fields` now
//! writes `Value::Object(None)` for every reference field too, so for any
//! object that went through the ordinary `alloc_object` + `init_primitive_fields`
//! sequence — which is every `new` in the interpreter, and (as of that
//! record's NOMINATION 1) the JIT's inline allocation path too — the raw
//! by-name read and the descriptor-coerced indexed read now agree on an
//! unwritten reference slot: both answer `Object(None)`. The row below is
//! kept because the CLASS of disagreement — a raw, uncoerced read seeing a
//! stale or mis-tagged bit pattern that a descriptor-aware read would not —
//! is still real for any allocation path that does NOT run
//! `init_primitive_fields` (a hand-built synthetic carrier that only writes
//! some of its slots, for instance), and because `get_field_by_name` still
//! answers `Object(None)` for a field the class does not declare at all,
//! which is a *different* null from an unwritten one and consequence (1)
//! below still applies to it.
//!
//! Net effect, for an object whose fields have not been written yet:
//!
//! | declared descriptor | `get_field_by_name` | `get_field(index)` |
//! |---------------------|---------------------|--------------------|
//! | `I` `B` `C` `S` `Z` | `Int(0)`            | `Int(0)`           |
//! | `J`                 | `Long(0)`           | `Long(0)`          |
//! | `F`                 | `Float(0.0)`        | `Float(0.0)`       |
//! | `D`                 | `Double(0.0)`       | `Double(0.0)`      |
//! | `L…;` / `[…`, `init_primitive_fields` ran | `Object(None)` | `Object(None)` |
//! | `L…;` / `[…`, it did not | **`Int(0)`**   | **`Object(None)`** |
//! | *(no such field)*   | `Object(None)`      | *(unresolvable)*   |
//!
//! Two consequences that have each cost a real bug on this branch:
//!
//! 1. `matches!(get_field_by_name(..), Value::Object(None))` is **not** a null
//!    test on its own — it cannot distinguish "declared, unwritten" from
//!    "not declared at all", and (on a carrier `init_primitive_fields` never
//!    touched) from a genuinely unwritten reference field.
//! 2. Returning a `get_field_by_name` result straight out of a native whose
//!    registered descriptor is a reference type can still hand `Value::Int(0)`
//!    to bytecode that is about to `areturn` / `checkcast` it, on such a
//!    carrier.
//!
//! # What to use instead
//!
//! * [`ref_field`] / [`ref_field_obj`] — reference-typed reads that can never
//!   surface a primitive tag.
//! * [`ref_field_is_null`] — the null test that (1) above is not.
//! * [`declares_field`] — distinguishes "absent" from "null".
//! * [`int_field_strict`] — fail-closed primitive read for security- and
//!   crypto-relevant parameters, where a defaulted `0` is not a safe answer.
//!
//! All of these resolve the slot index first and read *by index*, so the
//! descriptor decode applies. They fall back to the by-name read only when the
//! index does not resolve, and even then they refuse to surface a value whose
//! tag contradicts the read's declared intent.

use cratonvm_native_api::NativeContext;
use cratonvm_types::{ObjectRef, Value};

/// Resolve `field_name` to an in-range slot index on `this`'s own class.
///
/// This mirrors what the production `get_field_by_name` does internally
/// (`resolve_field_index_in_hierarchy`, which is `resolve_field_index_in_hierarchy_desc`
/// with `descriptor: None`), so in the VM the two agree exactly on *which*
/// slot is addressed — the difference is only the descriptor decode applied
/// afterwards.
fn slot_of(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> Option<usize> {
    let class_id = ctx.class_id_of_object(this);
    let index = ctx.resolve_field_index_by_class_id(class_id, field_name)?;
    // Fail closed rather than hand an out-of-range index to `get_field`: a
    // synthetic stand-in allocated with fewer slots than the real class
    // declares is exactly the "half-real object" shape.
    (index < ctx.object_num_fields(this)).then_some(index)
}

/// Does `this`'s class (or a superclass) declare an instance field called
/// `field_name`?
///
/// This is the oracle that separates the two meanings `get_field_by_name`
/// conflates into `Value::Object(None)`: "the field is null" versus "there is
/// no such field". Use it before treating `Object(None)` as a null.
pub fn declares_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> bool {
    ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(this), field_name)
        .is_some()
}

/// Read a **reference-typed** field, guaranteeing a `Value::Object(..)`.
///
/// Reads by resolved index (descriptor-decoded) when the field resolves. When
/// it does not, the by-name read is consulted but any non-`Object` tag is
/// degraded to `Value::Object(None)` — a primitive tag from a reference-typed
/// read is always either an unwritten slot or upstream tag drift, and
/// returning it to bytecode expecting a reference is unsound in both cases.
pub fn ref_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> Value {
    if let Some(index) = slot_of(ctx, this, field_name) {
        let v = ctx.get_field(this, index);
        if matches!(v, Value::Object(_)) {
            return v;
        }
        return Value::Object(None);
    }
    match ctx.get_field_by_name(this, field_name) {
        v @ Value::Object(_) => v,
        _ => Value::Object(None),
    }
}

/// [`ref_field`] as an `Option<ObjectRef>`: `None` for null, absent, or
/// unwritten.
pub fn ref_field_obj(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> Option<ObjectRef> {
    match ref_field(ctx, this, field_name) {
        Value::Object(o) => o,
        _ => None,
    }
}

/// The null test that `matches!(get_field_by_name(..), Value::Object(None))`
/// is not: true when the reference field is null **or** unwritten **or**
/// absent.
///
/// If you need to tell "absent" apart from "null", pair this with
/// [`declares_field`].
pub fn ref_field_is_null(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> bool {
    ref_field_obj(ctx, this, field_name).is_none()
}

/// Fail-closed `int`-typed field read.
///
/// Returns `Some(v)` only when the field resolves to an in-range slot on
/// `this`'s class **and** decodes as `Value::Int`. Returns `None` when the
/// field is absent or holds a non-`Int` tag, so the caller can refuse rather
/// than substitute a default.
///
/// Deliberately does **not** fall back to `get_field_by_name`: the whole point
/// of this reader is that the caller cannot afford to confuse "field missing"
/// with "field is 0", and the by-name read answers `Int(0)` to both.
pub fn int_field_strict(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field_name: &str,
) -> Option<i32> {
    match ctx.get_field(this, slot_of(ctx, this, field_name)?) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
