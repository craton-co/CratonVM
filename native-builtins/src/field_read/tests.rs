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

/// The `Value::Int(0)` this module is about is the one production used to
/// produce for a name that **resolves**, to a **reference** slot that has
/// never been written — `G56-1` (2026-08-17) closed that gap for any
/// object allocated the ordinary way (`init_primitive_fields` now writes
/// `Object(None)` for reference fields too, see this module's header),
/// so what this test actually pins today is the CLASS of hazard, exercised
/// through `MockNativeContext`, which never runs `init_primitive_fields`
/// and so still leaves a declared reference field un-written after
/// `new_object` — the same raw-zero shape a hand-built synthetic carrier
/// that skips a slot would leave in production.
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
    // zero-filled field block and, unlike production `alloc_object`, never
    // runs `init_primitive_fields` — so this mock still leaves a reference
    // field's raw zero bytes to decode as the niche-0 `Int` discriminant,
    // which is the shape this module's readers must still handle even
    // though ordinary production allocation no longer produces it.
    assert_eq!(
        ctx.get_field_by_name(obj, "payload"),
        Value::Int(0),
        "the mock's raw zero, standing in for any carrier init_primitive_fields never ran on"
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
    assert!(!declares_field(&ctx, obj, "no_such_field_at_all"));    }