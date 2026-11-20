use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::poly_call_site::{arm_vh_param_site, vh_param_site_classes};
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    }
}

fn boxed(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = obj(ctx, class);
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
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

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

fn runtime(err: &MethodCallFailed) -> Option<&RuntimeError> {
    match err {
        MethodCallFailed::InternalError(VmError::Runtime(r)) => Some(r),
        _ => None,
    }
}

fn int_of(v: MethodCallResult, ctx: &MockNativeContext) -> i32 {
    match v.unwrap() {
        Some(Value::Int(i)) => i,
        // `get` boxes: the payload of the canonical `Integer`.
        Some(Value::Object(Some(b))) => match ctx.get_field(b, 0) {
            Value::Int(i) => i,
            other => panic!("box payload {other:?}"),
        },
        other => panic!("answered {other:?}"),
    }
}

/// The probe's `arr*` roads on the VM's own `int[]` handle: a `long` index
/// is WMTE (and stores nothing), an `Integer` through an `Object` site
/// reads its element, and an armed `long`/`boolean` site class refuses even
/// an `Int` representation while a `short` one reads.
#[test]
fn an_array_handles_index_converts_by_the_site_type() {
    if !vh_index_coordinate_kinds_enabled() {
        return; // an explicit `0` in the test environment wins
    }
    let mut ctx = MockNativeContext::new();
    // Its own canonical-box row (see `a_reference_index_follows_unbox_widen`;
    // interpreter round i2 wave 59, lane L4).
    ctx.set_vm_identity(0x15F5_0004);
    let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
    let arr = ctx.new_array(ArrayElementType::Int, 8);
    for i in 0..8 {
        ctx.set_array_element(arr, i, Value::Int(100 + i as i32));
    }
    let a = Value::Object(Some(arr));
    let err = varhandle_get(&mut ctx, &[av, a, Value::Long(3)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long index: {err:?}");
    let three = boxed(&mut ctx, "java/lang/Integer", Value::Int(3));
    assert_eq!(int_of(varhandle_get(&mut ctx, &[av, a, three]), &ctx), 103);
    let err = varhandle_set(&mut ctx, &[av, a, Value::Long(5), Value::Int(-1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long index store: {err:?}");
    assert_eq!(
        ctx.get_array_element(arr, 5),
        Value::Int(105),
        "no refused store landed"
    );
    // A boxed index with a negative value is bounds-checked as itself.
    let minus = boxed(&mut ctx, "java/lang/Integer", Value::Int(-1));
    let err = varhandle_get(&mut ctx, &[av, a, minus]).unwrap_err();
    assert!(
        matches!(
            runtime(&err),
            Some(RuntimeError::ArrayIndexOutOfBoundsException { .. })
        ),
        "boxed -1: {err:?}"
    );
    // Armed word: `([IJ)I` refuses an `Int` too; `([IS)I` reads.
    arm_vh_param_site(vh_param_site_classes("([IJ)I"));
    let err = varhandle_get(&mut ctx, &[av, a, Value::Int(2)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "armed long site: {err:?}");
    arm_vh_param_site(vh_param_site_classes("([IZ)I"));
    let err = varhandle_get(&mut ctx, &[av, a, Value::Int(1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "armed boolean site: {err:?}");
    arm_vh_param_site(vh_param_site_classes("([IS)I"));
    assert_eq!(
        int_of(varhandle_get(&mut ctx, &[av, a, Value::Int(2)]), &ctx),
        102
    );
    // `compareAndSet` counts two values behind the index: `([IJII)Z`.
    arm_vh_param_site(vh_param_site_classes("([IJII)Z"));
    let err = varhandle_compare_and_set(
        &mut ctx,
        &[av, a, Value::Int(4), Value::Int(104), Value::Int(9)],
    )
    .unwrap_err();
    assert!(is_wmte(&ctx, &err), "armed long site, CAS: {err:?}");
    assert_eq!(ctx.get_array_element(arr, 4), Value::Int(104));
}

/// Interpreter round i2 wave 57, lane L4: the word's return byte
/// (`vh_param_site_word`) is `asType`'s return direction. An `int`
/// variable reads at a `long` site and refuses a `short` one; a `long`
/// variable refuses an `int` site, and a refused `getAndAdd` adds nothing.
#[test]
fn a_primitive_return_site_the_variable_does_not_widen_to_is_refused() {
    use cratonvm_native_api::poly_call_site::vh_param_site_word;
    if !crate::nbflags().vh_primitive_return_astype {
        return; // the switch is off in this environment
    }
    let mut ctx = MockNativeContext::new();
    // `get` answers the CANONICAL box from the process-wide cache keyed by
    // `vm_identity()`: under the mock default `0` a full parallel lib run
    // served a box another test's (dropped) mock heap had filed, `invalid
    // ObjectRef in mock heap` in `int_of` (host `--release` run, waves 57
    // and 58). Its own row, as `a_reference_index_follows_unbox_widen`
    // does (interpreter round i2 wave 59, lane L4).
    ctx.set_vm_identity(0x15F5_0057);
    let iv = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
    let ints = ctx.new_array(ArrayElementType::Int, 4);
    ctx.set_array_element(ints, 1, Value::Int(7));
    let a = Value::Object(Some(ints));
    arm_vh_param_site(vh_param_site_word("([II)J"));
    assert_eq!(
        int_of(varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]), &ctx),
        7
    );
    arm_vh_param_site(vh_param_site_word("([II)I"));
    assert_eq!(
        int_of(varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]), &ctx),
        7
    );
    arm_vh_param_site(vh_param_site_word("([II)S"));
    let err = varhandle_get(&mut ctx, &[iv, a, Value::Int(1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "int read at a short site: {err:?}");
    let lv = handle(&mut ctx, VH_KIND_ARRAY, "[J", "J");
    let longs = ctx.new_array(ArrayElementType::Long, 4);
    ctx.set_array_element(longs, 2, Value::Long(40));
    let l = Value::Object(Some(longs));
    arm_vh_param_site(vh_param_site_word("([JI)I"));
    let err = varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long read at an int site: {err:?}");
    arm_vh_param_site(vh_param_site_word("([JII)I"));
    let err =
        varhandle_get_and_add(&mut ctx, &[lv, l, Value::Int(2), Value::Int(1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "getAndAdd at an int site: {err:?}");
    assert_eq!(
        ctx.get_array_element(longs, 2),
        Value::Long(40),
        "nothing added"
    );
    // An `Object` site carries no return byte: not judged here.
    arm_vh_param_site(vh_param_site_word("([JI)Ljava/lang/Object;"));
    assert!(varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]).is_ok());
    // A reference site takes a primitive variable whose wrapper it is
    // assignable from: `Number`, `Long`; not `Integer`, `String`, `int[]`.
    for (site, ok) in [
        ("([JI)Ljava/lang/Number;", true),
        ("([JI)Ljava/lang/Long;", true),
        ("([JI)Ljava/lang/constant/ConstantDesc;", true),
        ("([JI)Ljava/lang/Integer;", false),
        ("([JI)Ljava/lang/String;", false),
        ("([JI)[J", false),
    ] {
        arm_vh_param_site(vh_param_site_word(site));
        let r = varhandle_get(&mut ctx, &[lv, l, Value::Int(2)]);
        match r {
            Ok(_) => assert!(ok, "{site}: answered"),
            Err(e) => assert!(!ok && is_wmte(&ctx, &e), "{site}: {e:?}"),
        }
    }
}

/// A null index box is NPE, a `Long` box is CCE (`unboxWiden` refuses a
/// narrowing wrapper), and a `String`-typed site is WMTE statically.
#[test]
fn a_reference_index_follows_unbox_widen() {
    if !vh_index_coordinate_kinds_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    // `get` answers the CANONICAL `Integer` box of the element, from the
    // process-wide cache keyed by `vm_identity()`; the mock default `0` is
    // shared with every other test, whose dropped mock heaps leave boxes
    // this heap does not hold (round 15 wave 6, lane mh5: `invalid
    // ObjectRef in mock heap` once wave 5's boxing test filled the row).
    ctx.set_vm_identity(0x15F5_0003);
    let av = handle(&mut ctx, VH_KIND_ARRAY, "[I", "I");
    let a = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 4)));
    let err = varhandle_get(&mut ctx, &[av, a, Value::Object(None)]).unwrap_err();
    assert!(
        matches!(
            runtime(&err),
            Some(RuntimeError::NullPointerException { .. })
        ),
        "null index: {err:?}"
    );
    let long = boxed(&mut ctx, "java/lang/Long", Value::Long(1));
    let err = varhandle_get(&mut ctx, &[av, a, long]).unwrap_err();
    assert!(
        matches!(runtime(&err), Some(RuntimeError::ClassCastException { .. })),
        "Long index: {err:?}"
    );
    let short = boxed(&mut ctx, "java/lang/Short", Value::Int(2));
    assert_eq!(int_of(varhandle_get(&mut ctx, &[av, a, short]), &ctx), 0);
    arm_vh_param_site(vh_param_site_classes("([ILjava/lang/String;)I"));
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let err = varhandle_get(&mut ctx, &[av, a, s]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "String-typed index: {err:?}");
}

/// The probe's `bv*` roads: a byte view read with an `Integer` index reads
/// at THAT offset (it read offset 0), a `long` index is WMTE, and a store
/// with a boxed index writes at its offset.
#[test]
fn a_byte_views_index_is_the_real_offset() {
    if !vh_index_coordinate_kinds_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x15F5_0005); // its own canonical-box row (wave 59)
    let bv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
    let bytes = ctx.new_array(ArrayElementType::Byte, 16);
    ctx.set_array_element(bytes, 8, Value::Int(0x2a));
    let b = Value::Object(Some(bytes));
    let eight = boxed(&mut ctx, "java/lang/Integer", Value::Int(8));
    assert_eq!(int_of(varhandle_get(&mut ctx, &[bv, b, eight]), &ctx), 0x2a);
    let err = varhandle_get(&mut ctx, &[bv, b, Value::Long(8)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long offset: {err:?}");
    let four = boxed(&mut ctx, "java/lang/Integer", Value::Int(4));
    varhandle_set(&mut ctx, &[bv, b, four, Value::Int(0x0102_0304)]).unwrap();
    assert_eq!(ctx.get_array_element(bytes, 4), Value::Int(0x04));
    assert_eq!(ctx.get_array_element(bytes, 7), Value::Int(0x01));
    assert_eq!(
        ctx.get_array_element(bytes, 0),
        Value::Int(0),
        "offset 0 untouched"
    );
    let err = varhandle_set(&mut ctx, &[bv, b, Value::Long(0), Value::Int(9)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long offset store: {err:?}");
    assert_eq!(
        ctx.get_array_element(bytes, 0),
        Value::Int(0),
        "no refused store landed"
    );
}

/// `CRATONVM_VH_VIEW_VALUE_ADAPT`: an `int` into a `float` view stores
/// `5.0f`, a boxed `Integer` into an `int` view stores its value, and a
/// `long` from an armed `long` site into an `int` view is WMTE.
#[test]
fn a_byte_views_value_is_adapted() {
    if !vh_view_value_adapt_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let fv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "F");
    let bytes = ctx.new_array(ArrayElementType::Byte, 8);
    let b = Value::Object(Some(bytes));
    varhandle_set(&mut ctx, &[fv, b, Value::Int(0), Value::Int(5)]).unwrap();
    let stored = byte_view_get(&ctx, bytes, 0, b'F', true);
    assert_eq!(stored, Value::Float(5.0), "int widens to float");
    let iv = handle(&mut ctx, VH_KIND_BYTE_VIEW_LE, "", "I");
    let seven = boxed(&mut ctx, "java/lang/Integer", Value::Int(7));
    varhandle_set(&mut ctx, &[iv, b, Value::Int(4), seven]).unwrap();
    assert_eq!(byte_view_get(&ctx, bytes, 4, b'I', true), Value::Int(7));
    arm_vh_param_site(vh_param_site_classes("([BIJ)V"));
    let err = varhandle_set(&mut ctx, &[iv, b, Value::Int(4), Value::Long(9)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "long into an int view: {err:?}");
    assert_eq!(byte_view_get(&ctx, bytes, 4, b'I', true), Value::Int(7));
}
