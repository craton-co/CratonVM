// SPDX-License-Identifier: Apache-2.0
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
//! The two only agree when the slot's stored tag already matches its declared
//! type. It does not for an *unwritten reference slot*: `alloc_object` zeroes
//! the field block, and after `Value::Object` gained its `NonNull` niche the
//! all-zero 16-byte slot decodes as `Value::Int(0)` — discriminant 0 — not as
//! `Value::Object(None)` (`gc/src/heap.rs:398-411` documents exactly this).
//! `init_primitive_fields` (`vm/src/runtime/interpreter.rs:2798`) writes a
//! typed zero for every *primitive* field but deliberately skips reference
//! fields, on a comment ("already `Object(None)` from zero memory",
//! interpreter.rs:2815) that the niche change made false.
//!
//! Net effect, for an object whose fields have not been written yet:
//!
//! | declared descriptor | `get_field_by_name` | `get_field(index)` |
//! |---------------------|---------------------|--------------------|
//! | `I` `B` `C` `S` `Z` | `Int(0)`            | `Int(0)`           |
//! | `J`                 | `Long(0)`           | `Long(0)`          |
//! | `F`                 | `Float(0.0)`        | `Float(0.0)`       |
//! | `D`                 | `Double(0.0)`       | `Double(0.0)`      |
//! | `L…;` / `[…`        | **`Int(0)`**        | **`Object(None)`** |
//! | *(no such field)*   | `Object(None)`      | *(unresolvable)*   |
//!
//! Two consequences that have each cost a real bug on this branch:
//!
//! 1. `matches!(get_field_by_name(..), Value::Object(None))` is **not** a null
//!    test. It is false for an unwritten reference field and true for a field
//!    the class does not declare at all.
//! 2. Returning a `get_field_by_name` result straight out of a native whose
//!    registered descriptor is a reference type can hand `Value::Int(0)` to
//!    bytecode that is about to `areturn` / `checkcast` it.
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
pub(crate) fn declares_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> bool {
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
pub(crate) fn ref_field(ctx: &dyn NativeContext, this: ObjectRef, field_name: &str) -> Value {
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
pub(crate) fn ref_field_obj(
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
pub(crate) fn ref_field_is_null(
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
pub(crate) fn int_field_strict(
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
mod tests {
    use super::*;
    use crate::test_utils::mock_ctx;
    // The heap accessors these tests drive (`new_object`, `create_string`,
    // `get_field_by_name`, `set_field_by_name`) are trait methods, and the
    // trait has to be in scope to call them on the mock. `class_id_of_object`
    // is declared on the `NativeClassAccess` supertrait, which must be named
    // separately for method resolution.
    use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeHeapAccess};

    /// Declare `class_id` as owning a single instance field, so the name
    /// RESOLVES on objects of that class without anything ever writing the
    /// slot.
    fn declare_one_field(
        ctx: &crate::test_utils::MockNativeContext,
        class_id: cratonvm_types::ClassId,
        name: &str,
        descriptor: &str,
    ) {
        ctx.set_declared_fields(
            class_id,
            vec![FieldMetadata {
                name: name.to_string(),
                descriptor: descriptor.to_string(),
                access_flags: 0,
                slot_index: 0,
                declaring_class_id: class_id,
                is_static: false,
            }],
        );
    }

    /// The `Value::Int(0)` this module is about is the one production really
    /// produces: a name that **resolves**, to a **reference** slot that has
    /// never been written.
    ///
    /// This test used to assert `Int(0)` for an *unresolvable* name and cite it
    /// as production behaviour. It is not: production
    /// (`vm/src/vm/vm_exec.rs:10613-10623`, and the trait contract at
    /// `native-api/src/registry.rs:2351-2354`) answers `Value::Object(None)`
    /// there. Only `MockNativeContext` answers `Int(0)` to an absent name, and
    /// that divergence is deliberate and opt-out
    /// (`test_utils.rs::set_absent_field_answers_null`). Encoding it here made
    /// the mock's lie look like a documented VM fact, and three lanes reasoned
    /// from it. So: turn the faithful answer ON, and assert the two cases
    /// apart.
    #[test]
    fn by_name_read_of_a_resolvable_unwritten_reference_slot_is_int_zero() {
        let mut ctx = mock_ctx();
        ctx.set_absent_field_answers_null(true);
        let obj = match ctx.new_object("p/Unwritten").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        let cid = ctx.class_id_of_object(obj);
        declare_one_field(&ctx, cid, "payload", "Ljava/lang/String;");

        // The name resolves ...
        assert!(
            declares_field(&ctx, obj, "payload"),
            "the hazard is about a field the class DOES declare"
        );
        // ... and nothing has written slot 0. `new_object` hands back a
        // zero-filled field block, which is the state `alloc_object` leaves a
        // reference slot in: `init_primitive_fields`
        // (`vm/src/runtime/interpreter.rs:2798`) writes a typed zero for every
        // PRIMITIVE field and deliberately skips reference fields. The raw
        // by-name read applies no descriptor decode, so those zero bytes come
        // back as the niche-0 discriminant.
        assert_eq!(
            ctx.get_field_by_name(obj, "payload"),
            Value::Int(0),
            "THIS is the production Int(0): a resolvable, unwritten reference slot"
        );
        // The absent name is a different thing and answers null.
        assert_eq!(
            ctx.get_field_by_name(obj, "no_such_field_at_all"),
            Value::Object(None),
            "an unresolvable name answers null in production -- not Int(0)"
        );
        // Both are indistinguishable to a caller that wants a reference, and
        // `ref_field` is the reader that gets both right.
        assert_eq!(ref_field(&ctx, obj, "payload"), Value::Object(None));
        assert!(ref_field_is_null(&ctx, obj, "payload"));
    }

    #[test]
    fn ref_field_never_surfaces_a_primitive_tag() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("java/lang/Object").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        assert_eq!(
            ref_field(&ctx, obj, "no_such_field_at_all"),
            Value::Object(None),
            "a reference-typed read must degrade a primitive tag to null"
        );
    }

    #[test]
    fn ref_field_obj_is_none_for_a_primitive_tagged_slot() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("java/lang/Object").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        assert!(ref_field_obj(&ctx, obj, "no_such_field_at_all").is_none());
        assert!(ref_field_is_null(&ctx, obj, "no_such_field_at_all"));
    }

    #[test]
    fn ref_field_round_trips_a_genuine_reference() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("java/lang/Object").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        let payload = ctx.create_string("payload");
        // `mock_jdk_field_slot` maps "name" onto a concrete slot, so this
        // exercises the resolving path rather than the degrade path.
        ctx.set_field_by_name(obj, "name", Value::Object(Some(payload)));
        assert_eq!(
            ref_field(&ctx, obj, "name"),
            Value::Object(Some(payload)),
            "a genuine reference must survive the safety coercion untouched"
        );
        assert_eq!(ref_field_obj(&ctx, obj, "name"), Some(payload));
        assert!(!ref_field_is_null(&ctx, obj, "name"));
    }

    #[test]
    fn ref_field_reports_null_for_an_explicitly_null_reference() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("java/lang/Object").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        ctx.set_field_by_name(obj, "name", Value::Object(None));
        assert_eq!(ref_field(&ctx, obj, "name"), Value::Object(None));
        assert!(ref_field_is_null(&ctx, obj, "name"));
    }

    /// The fail-open guard shape, reproduced against the PRODUCTION answer for
    /// an absent name.
    ///
    /// This is the `sun/nio/ch/FileChannelImpl.truncate` defect in miniature.
    /// The old version of this test asserted `get_field_by_name(obj, "rounds")
    /// == Value::Int(0)` — the mock's divergent answer — and so demonstrated
    /// the opposite of the real hazard: under the mock the negated `Int(0)`
    /// match fails CLOSED, which is exactly why a unit test of `truncate`'s
    /// guard read as protective while production truncated the file.
    #[test]
    fn int_field_strict_refuses_an_unresolvable_field() {
        let mut ctx = mock_ctx();
        // Opt in to production's answer for an unresolvable name
        // (`test_utils.rs::set_absent_field_answers_null`). Without it both
        // guard forms below agree and the test proves nothing.
        ctx.set_absent_field_answers_null(true);
        let obj = match ctx.new_object("p/Fabricated").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };

        assert!(!declares_field(&ctx, obj, "writable"));
        assert_eq!(
            ctx.get_field_by_name(obj, "writable"),
            Value::Object(None),
            "production answers null for a name the receiver's class does not declare"
        );
        // `Object(None)` is not `Int(0)`, so the negated match reads TRUE —
        // "this channel is writable" — about a receiver that never declared
        // the field. A guard written that way FAILS OPEN.
        assert!(
            !matches!(ctx.get_field_by_name(obj, "writable"), Value::Int(0)),
            "the retired guard shape admits an undeclared field as `true`"
        );
        // The strict reader refuses, so the same guard FAILS CLOSED instead.
        assert_eq!(int_field_strict(&ctx, obj, "writable"), None);
        assert!(
            !matches!(int_field_strict(&ctx, obj, "writable"), Some(v) if v != 0),
            "the replacement guard denies the operation on an undeclared field"
        );

        // ... and it is not vacuous: a declared, written `int` still reads.
        let cid = ctx.class_id_of_object(obj);
        declare_one_field(&ctx, cid, "writable", "Z");
        ctx.set_field_by_name(obj, "writable", Value::Int(1));
        assert_eq!(int_field_strict(&ctx, obj, "writable"), Some(1));
        assert!(matches!(int_field_strict(&ctx, obj, "writable"), Some(v) if v != 0));
    }

    #[test]
    fn declares_field_is_false_for_an_unknown_name() {
        let mut ctx = mock_ctx();
        let obj = match ctx.new_object("java/lang/Object").unwrap() {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected an object, got {other:?}"),
        };
        assert!(!declares_field(&ctx, obj, "no_such_field_at_all"));
    }
}
