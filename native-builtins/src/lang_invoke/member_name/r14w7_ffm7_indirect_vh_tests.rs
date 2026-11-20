//! Round 14 wave 7 (lane ffm7): an access through an `IndirectVarHandle`
//! is served through its mode handle, or refused -- never `null`.
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

struct Fixture {
    ctx: MockNativeContext,
    vh: ObjectRef,
    direct: ObjectRef,
    table: ObjectRef,
    other_cid: ClassId,
}

fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
    match ctx.ensure_class_initialized(name) {
        Ok(cid) => cid,
        Err(err) => panic!("no class {name}: {err:?}"),
    }
}

fn field(
    cid: ClassId,
    name: &str,
    desc: &str,
    slot: usize,
) -> cratonvm_native_api::FieldMetadata {
    cratonvm_native_api::FieldMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: 0,
        slot_index: slot,
        declaring_class_id: cid,
        is_static: false,
    }
}

/// An `IndirectVarHandle` on the real class's field names (the inherited
/// `methodHandleTable` at slot 0), with a `directTarget` and an empty
/// mode-handle table; `target` and `handleFactory` unset.
fn fixture() -> Fixture {
    let mut ctx = mock_ctx();
    ctx.set_absent_field_answers_null(true);
    let table_desc = "[Ljava/lang/invoke/MethodHandle;";
    let base = class(&mut ctx, VAR_HANDLE_CLASS);
    ctx.set_declared_fields(base, vec![field(base, "methodHandleTable", table_desc, 0)]);
    let cid = class(&mut ctx, INDIRECT_VAR_HANDLE_CLASS);
    ctx.set_declared_fields(
        cid,
        vec![
            field(cid, "methodHandleTable", table_desc, 0),
            field(cid, "target", "Ljava/lang/invoke/VarHandle;", 1),
            field(cid, "directTarget", "Ljava/lang/invoke/VarHandle;", 2),
            field(cid, "handleFactory", "Ljava/util/function/BiFunction;", 3),
            field(cid, "value", "Ljava/lang/Class;", 4),
            field(cid, "coordinates", "[Ljava/lang/Class;", 5),
        ],
    );
    let other_cid = class(&mut ctx, "p/R14w7Other");
    let vh = ctx.alloc_object(cid, 6);
    let direct = ctx.alloc_object(other_cid, 1);
    ctx.set_field(vh, 2, Value::Object(Some(direct)));
    let table = ctx.new_array(ArrayElementType::Reference, VH_ACCESS_MODES.len());
    ctx.set_field(vh, 0, Value::Object(Some(table)));
    Fixture {
        ctx,
        vh,
        direct,
        table,
        other_cid,
    }
}

/// Answers `invokeWithArguments` with element `SLOT` of its array.
fn echo<const SLOT: usize>(
    ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if name != "invokeWithArguments" || desc != "([Ljava/lang/Object;)Ljava/lang/Object;" {
        return None;
    }
    let Some(Value::Object(Some(arr))) = args.first() else {
        return None;
    };
    Some(Ok(Some(ctx.get_array_element(*arr, SLOT))))
}

#[test]
fn access_mode_table_matches_the_enum_spelling() {
    assert_eq!(VH_ACCESS_MODES.len(), 31);
    for (ordinal, (name, method)) in VH_ACCESS_MODES.iter().enumerate() {
        assert_eq!(access_mode_method_name(name), *method, "{name}");
        assert_eq!(
            vh_access_mode_is_set(ordinal),
            name.starts_with("SET"),
            "{name}"
        );
    }
}

#[test]
fn primitive_arguments_box_as_the_declared_type() {
    assert_eq!(
        vh_indirect_box_input(Value::Int(1), Some("Z")),
        Some((Value::Int(1), "Z"))
    );
    assert_eq!(
        vh_indirect_box_input(Value::Int(3), Some("J")),
        Some((Value::Long(3), "J"))
    );
    assert_eq!(
        vh_indirect_box_input(Value::Int(3), Some("Ljava/lang/Object;")),
        Some((Value::Int(3), "I"))
    );
    assert_eq!(
        vh_indirect_box_input(Value::Long(3), None),
        Some((Value::Long(3), "J"))
    );
    assert_eq!(vh_indirect_box_input(Value::Object(None), Some("I")), None);
}

#[test]
fn a_handle_that_is_not_indirect_is_left_to_the_native() {
    let mut ctx = mock_ctx();
    let cid = class(&mut ctx, "p/R14w7Plain");
    let vh = ctx.alloc_object(cid, 6);
    assert!(
        vh_indirect_serve(&mut ctx, &[Value::Object(Some(vh)), Value::Int(1)], "get").is_none()
    );
}

#[test]
fn get_invokes_the_cached_mode_handle_with_the_direct_target_leading() {
    let mut f = fixture();
    let mh = f.ctx.alloc_object(f.other_cid, 1);
    f.ctx.set_array_element(f.table, 0, Value::Object(Some(mh)));
    f.ctx.set_invoke_virtual_hook(echo::<0>);
    let got = varhandle_get(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
    assert_eq!(got.ok(), Some(Some(Value::Object(Some(f.direct)))));
}

#[test]
fn a_primitive_coordinate_reaches_the_mode_handle_boxed() {
    let mut f = fixture();
    let mh = f.ctx.alloc_object(f.other_cid, 1);
    f.ctx.set_array_element(f.table, 0, Value::Object(Some(mh)));
    f.ctx.set_invoke_virtual_hook(echo::<1>);
    let got = varhandle_get(
        &mut f.ctx,
        &[Value::Object(Some(f.vh)), Value::Int(100_000)],
    );
    let Ok(Some(Value::Object(Some(boxed)))) = &got else {
        panic!("the coordinate did not arrive boxed: {got:?}");
    };
    assert_eq!(f.ctx.get_field(*boxed, 0), Value::Int(100_000));
}

#[test]
fn set_answers_void_after_the_mode_handle_runs() {
    let mut f = fixture();
    let mh = f.ctx.alloc_object(f.other_cid, 1);
    f.ctx.set_array_element(f.table, 1, Value::Object(Some(mh)));
    f.ctx.set_invoke_virtual_hook(echo::<0>);
    let got = varhandle_set(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
    assert_eq!(got.ok(), Some(None));
}

#[test]
fn an_access_that_cannot_be_served_throws_instead_of_answering_null() {
    // No cached handle and no `target`: before wave 7 this answered
    // `Ok(Some(null))` / `Ok(None)`.
    let mut f = fixture();
    let get = varhandle_get(&mut f.ctx, &[Value::Object(Some(f.vh)), Value::Int(7)]);
    let err = match get {
        Err(err) => format!("{err:?}"),
        Ok(v) => panic!("an unserved get answered {v:?}"),
    };
    assert!(err.contains("IndirectVarHandle.get"), "{err}");
    let cas = varhandle_compare_and_set(
        &mut f.ctx,
        &[Value::Object(Some(f.vh)), Value::Int(1), Value::Int(2)],
    );
    assert!(cas.is_err(), "an unserved compareAndSet answered {cas:?}");
}

#[test]
fn var_type_of_an_indirect_handle_is_its_value_field() {
    let mut f = fixture();
    let mirror = f.ctx.alloc_object(f.other_cid, 1);
    f.ctx.set_field(f.vh, 4, Value::Object(Some(mirror)));
    let got = varhandle_var_type(&mut f.ctx, &[Value::Object(Some(f.vh))]);
    assert_eq!(got.ok(), Some(Some(Value::Object(Some(mirror)))));
}
