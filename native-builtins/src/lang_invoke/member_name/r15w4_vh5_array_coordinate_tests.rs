use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    }
}

fn handle(ctx: &mut MockNativeContext, kind: i32, class_name: &str, desc: &str) -> Value {
    let vh = obj(ctx, "java/lang/invoke/VarHandle");
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind,
            class_name: class_name.to_string(),
            field_name: String::new(),
            field_desc: desc.to_string(),
            field_index: -1,
            class_id: 0,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    Value::Object(Some(vh))
}

fn is_cce(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        ))
    )
}

/// The probe's `longArr` / `refArrSet` roads on the VM's own `int[]`
/// handle: a `long[]` read and an `Object[]` store are CCE and the
/// `Object[]` keeps its element; the `int[]` control reads.
#[test]
fn an_int_array_handle_casts_its_array() {
    if !vh_array_coordinate_cast_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let mut ctx = MockNativeContext::new();
    let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
    let ints = ctx.new_array(ArrayElementType::Int, 4);
    ctx.set_array_element(ints, 1, Value::Int(41));
    let longs = ctx.new_array(ArrayElementType::Long, 4);
    let objs = ctx.new_array(ArrayElementType::Reference, 4);
    let marker = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    ctx.set_array_element(objs, 2, marker);
    let got = varhandle_get(&mut ctx, &[av, Value::Object(Some(ints)), Value::Int(1)]);
    assert!(got.is_ok(), "int[] control: {got:?}");
    let err =
        varhandle_get(&mut ctx, &[av, Value::Object(Some(longs)), Value::Int(1)]).unwrap_err();
    assert!(is_cce(&err), "long[] read: {err:?}");
    let err = varhandle_set(
        &mut ctx,
        &[av, Value::Object(Some(objs)), Value::Int(2), Value::Int(7)],
    )
    .unwrap_err();
    assert!(is_cce(&err), "Object[] store: {err:?}");
    assert_eq!(
        ctx.get_array_element(objs, 2),
        marker,
        "no refused store landed"
    );
    let err = varhandle_compare_and_set(
        &mut ctx,
        &[
            av,
            Value::Object(Some(longs)),
            Value::Int(0),
            Value::Long(0),
            Value::Long(9),
        ],
    )
    .unwrap_err();
    assert!(is_cce(&err), "long[] CAS: {err:?}");
    assert_eq!(
        ctx.get_array_element(longs, 0),
        Value::Int(0),
        "no refused CAS landed"
    );
    // A non-array coordinate is CCE too; a null one is still the NPE.
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let err = varhandle_get(&mut ctx, &[av, s, Value::Int(0)]).unwrap_err();
    assert!(is_cce(&err), "String coordinate: {err:?}");
    if let Err(err) = varhandle_get(&mut ctx, &[av, Value::Object(None), Value::Int(0)]) {
        assert!(!is_cce(&err), "null coordinate: {err:?}");
    }
}

/// A reference-array handle admits every reference array the VM cannot
/// refuse (the mock's `checkcast` answers "cannot tell") and refuses a
/// primitive array; `Object[]` admits any reference array.
#[test]
fn a_reference_array_handle_refuses_only_primitive_arrays() {
    if !vh_array_coordinate_cast_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let ov = handle(
        &mut ctx,
        VH_KIND_ARRAY,
        "[Ljava/lang/Object;",
        "Ljava/lang/Object;",
    );
    let sv = handle(
        &mut ctx,
        VH_KIND_ARRAY,
        "[Ljava/lang/String;",
        "Ljava/lang/String;",
    );
    let objs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 2)));
    let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 2)));
    assert!(varhandle_get(&mut ctx, &[ov, objs, Value::Int(0)]).is_ok());
    assert!(varhandle_get(&mut ctx, &[sv, objs, Value::Int(0)]).is_ok());
    let err = varhandle_get(&mut ctx, &[ov, ints, Value::Int(0)]).unwrap_err();
    assert!(is_cce(&err), "int[] into Object[] handle: {err:?}");
}

/// The probe's `bvIntArr` road: a byte view casts its coordinate to
/// `byte[]`, and a mode the view refuses is still the UOE, not the CCE.
#[test]
fn a_byte_view_casts_to_byte_array_after_its_mode_refusal() {
    if !vh_array_coordinate_cast_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let bv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
    let bytes = Value::Object(Some(ctx.new_array(ArrayElementType::Byte, 8)));
    let ints = ctx.new_array(ArrayElementType::Int, 8);
    ctx.set_array_element(ints, 0, Value::Int(0x7f));
    let ints = Value::Object(Some(ints));
    assert!(varhandle_get(&mut ctx, &[bv, bytes, Value::Int(0)]).is_ok());
    let err = varhandle_get(&mut ctx, &[bv, ints, Value::Int(0)]).unwrap_err();
    assert!(is_cce(&err), "int[] through a byte view: {err:?}");
    let err = varhandle_set(&mut ctx, &[bv, ints, Value::Int(0), Value::Int(1)]).unwrap_err();
    assert!(is_cce(&err), "int[] store through a byte view: {err:?}");
    let err =
        varhandle_get_and_set(&mut ctx, &[bv, ints, Value::Int(0), Value::Int(1)]).unwrap_err();
    assert!(
        matches!(
            err,
            MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::UnsupportedOperationException { .. }
            ))
        ),
        "getAndSet on a byte view: {err:?}"
    );
}

/// The admission predicate alone: exact element kinds, `Object[]` over
/// every reference array, never a primitive array into a reference type.
#[test]
fn the_admission_predicate_is_checkcasts_shape() {
    let mut ctx = MockNativeContext::new();
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    let bools = ctx.new_array(ArrayElementType::Boolean, 1);
    let refs = ctx.new_array(ArrayElementType::Reference, 1);
    let s = obj(&mut ctx, "java/lang/String");
    let c: &dyn NativeContext = &ctx;
    assert_eq!(vh_array_coordinate_admits(c, ints, "[I"), Some(true));
    assert_eq!(vh_array_coordinate_admits(c, ints, "[J"), Some(false));
    assert_eq!(vh_array_coordinate_admits(c, bools, "[Z"), Some(true));
    assert_eq!(vh_array_coordinate_admits(c, bools, "[B"), Some(false));
    assert_eq!(
        vh_array_coordinate_admits(c, refs, "[Ljava/lang/Object;"),
        Some(true)
    );
    assert_eq!(
        vh_array_coordinate_admits(c, ints, "[Ljava/lang/Object;"),
        Some(false)
    );
    assert_eq!(vh_array_coordinate_admits(c, ints, "[[I"), Some(false));
    assert_eq!(vh_array_coordinate_admits(c, s, "[I"), Some(false));
    assert_eq!(vh_array_coordinate_admits(c, ints, "I"), None);
}
