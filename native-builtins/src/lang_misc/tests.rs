use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::{NativeClassAccess, NativeContext, NativeHeapAccess};

/// `Enum.name()` must answer `java.lang.Enum`'s OWN `name`, never a field
/// of the same name declared by the enum subclass.
///
/// `resolve_field_index_by_class_id` returns the MOST-DERIVED declaration
/// (§6.2 of `docs/feature-designs/by-name-field-reads.md`), so resolving
/// `"name"` on the receiver's class picks the subclass's field whenever an
/// enum declares one. Spring Boot's `WebEndpointTest.Infrastructure` does
/// — `JERSEY("Jersey")` — and `name()` then answered `"Jersey"` instead of
/// `"JERSEY"`. `Enum.valueOf` matches on `name()`, so it threw
/// `IllegalArgumentException: No enum constant JERSEY`, the enum-valued
/// annotation attribute resolved to nothing, and JUnit rejected the whole
/// test class with `displayName must not be null or blank` — zero tests
/// run. The mock resolver maps `test/ShadowedNameEnum`'s `"name"` to slot
/// 2 so this fails if that branch ever comes back.
#[test]
fn enum_name_reads_enums_own_slot_not_a_shadowing_subclass_field() {
    let mut ctx = mock_ctx();
    let obj = match ctx.new_object("test/ShadowedNameEnum").unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    let constant = ctx.create_string("JERSEY");
    let shadowing = ctx.create_string("Jersey");
    // Slot 0 is `java.lang.Enum.name`; slot 2 is the subclass's own field,
    // which is what the by-name resolver answers with.
    ctx.set_field(obj, ENUM_NAME_SLOT, Value::Object(Some(constant)));
    ctx.set_field(obj, 2, Value::Object(Some(shadowing)));
    assert_eq!(
        ctx.resolve_field_index_by_class_id(ctx.class_id_of_object(obj), "name"),
        Some(2),
        "premise: the by-name resolver prefers the subclass's shadowing field",
    );

    let got = native_enum_name(&mut ctx, &[Value::Object(Some(obj))])
        .expect("native_enum_name must not fail")
        .expect("native_enum_name must return a value");
    let got = match got {
        Value::Object(Some(s)) => ctx.read_string(s),
        other => panic!("expected a String reference, got {other:?}"),
    };
    assert_eq!(
        got.as_deref(),
        Some("JERSEY"),
        "Enum.name() must be the JVM constant name, not the subclass's `name` field",
    );
}

/// The soundness property `5bc7458e4` was written for still holds: both
/// methods are `()Ljava/lang/String;`, so an unwritten slot must surface as
/// null rather than as the `Value::Int(0)` a raw read produces.
#[test]
fn enum_name_degrades_an_unwritten_slot_to_null_not_a_primitive() {
    let mut ctx = mock_ctx();
    let obj = match ctx.new_object("test/UninitialisedEnum").unwrap() {
        Some(Value::Object(Some(o))) => o,
        other => panic!("expected an object, got {other:?}"),
    };
    assert_eq!(
        ctx.get_field(obj, ENUM_NAME_SLOT),
        Value::Int(0),
        "premise: an unwritten reference slot reads back as a primitive tag",
    );
    assert_eq!(
        native_enum_name(&mut ctx, &[Value::Object(Some(obj))]).unwrap(),
        Some(Value::Object(None)),
    );
}
