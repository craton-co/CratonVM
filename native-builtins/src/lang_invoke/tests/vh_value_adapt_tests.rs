#[allow(unused_imports)]
use super::*;


/// Round 11 wave 16, lane vh: the `asType` value adaptation for a PRIMITIVE
/// variable (`vh_adapt_value_args`), driven through the registered natives
/// with the value-site cell armed the way `vm_exec`'s dispatch arms it. Probe
/// twins: `R11W15RtVarHandleObjectSite`, `R11W16Vh*`.
#[cfg(test)]
mod vh_value_adapt_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::poly_call_site::{arm_vh_value_site, vh_site};
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn pack(prev: u8, last: u8) -> u16 {
    ((prev as u16) << 8) | last as u16
}

fn obj(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    }
}

fn wrapper(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = obj(ctx, class);
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

/// A handle over field 0 of a fresh `Box`, whose descriptor is `desc`.
fn handle(ctx: &mut MockNativeContext, desc: &str) -> (ObjectRef, ObjectRef) {
    let recv = obj(ctx, "Box");
    let cid = ctx.class_id_of_object(recv).as_u32();
    let vh = obj(ctx, "java/lang/invoke/VarHandle");
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind: VH_KIND_INSTANCE,
            class_name: "Box".to_string(),
            field_name: "f".to_string(),
            field_desc: desc.to_string(),
            field_index: 0,
            class_id: cid,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    (vh, recv)
}

fn set(
    ctx: &mut MockNativeContext,
    vh: ObjectRef,
    recv: ObjectRef,
    v: Value,
    site: u16,
) -> MethodCallResult {
    arm_vh_value_site(site);
    varhandle_set(
        ctx,
        &[Value::Object(Some(vh)), Value::Object(Some(recv)), v],
    )
}

fn is_cce(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        ))
    )
}

fn is_npe(err: &MethodCallFailed) -> bool {
    matches!(
        err,
        MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        ))
    )
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

/// The probe's `boxed` and `short` rows: an `Integer` / `Short` through an
/// `Object` site stores its VALUE. Before: the wrapper's address.
#[test]
fn an_object_site_unboxes_and_widens_a_wrapper() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    let site = pack(vh_site::OTHER, vh_site::OBJECT);
    let five = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
    set(&mut ctx, vh, recv, five, site).unwrap();
    assert_eq!(ctx.get_field(recv, 0), Value::Int(5));
    let seven = wrapper(&mut ctx, "java/lang/Short", Value::Int(7));
    set(&mut ctx, vh, recv, seven, site).unwrap();
    assert_eq!(ctx.get_field(recv, 0), Value::Int(7), "Short widens to int");
    let c = wrapper(&mut ctx, "java/lang/Character", Value::Int(0x41));
    set(&mut ctx, vh, recv, c, site).unwrap();
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(0x41),
        "Character widens to int"
    );
}

/// The probe's `long` and `objstr` rows (CCE through an `Object` site) and
/// its `str` row (WMTE through a `String` site), plus null (NPE) and a
/// `Long`-typed site (WMTE, refused statically). Nothing is stored.
#[test]
fn refusals_follow_the_call_site_type() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    ctx.set_field(recv, 0, Value::Int(3));
    let object_site = pack(vh_site::OTHER, vh_site::OBJECT);
    let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(9));
    let err = set(&mut ctx, vh, recv, long, object_site).unwrap_err();
    assert!(is_cce(&err), "Long -> int through Object is CCE: {err:?}");
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let err = set(&mut ctx, vh, recv, s, object_site).unwrap_err();
    assert!(is_cce(&err), "String through Object is CCE: {err:?}");
    let err = set(&mut ctx, vh, recv, Value::Object(None), object_site).unwrap_err();
    assert!(is_npe(&err), "null through Object is NPE: {err:?}");
    let err = set(&mut ctx, vh, recv, s, pack(vh_site::OTHER, vh_site::OTHER)).unwrap_err();
    assert!(
        is_wmte(&ctx, &err),
        "String through String is WMTE: {err:?}"
    );
    let long_site = pack(vh_site::OTHER, vh_site::W_LONG);
    let err = set(&mut ctx, vh, recv, long, long_site).unwrap_err();
    assert!(is_wmte(&ctx, &err), "a Long-typed site is WMTE: {err:?}");
    let err = set(
        &mut ctx,
        vh,
        recv,
        Value::Object(None),
        pack(vh_site::OTHER, vh_site::OTHER),
    )
    .unwrap_err();
    assert!(
        is_wmte(&ctx, &err),
        "null through String is WMTE, not NPE: {err:?}"
    );
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Int(3),
        "no refused store landed"
    );
}

/// No door armed (the JIT's site-cached dispatch, a direct call): the
/// run-time `Object`-site rules, so a wrapper still stores its value.
#[test]
fn an_unarmed_call_applies_the_object_site_rules() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    let five = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
    set(&mut ctx, vh, recv, five, 0).unwrap();
    assert_eq!(ctx.get_field(recv, 0), Value::Int(5));
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    assert!(is_cce(&set(&mut ctx, vh, recv, s, 0).unwrap_err()));
}

/// A PRIMITIVE site: `set(h, 5)` on a `float` field stores `5.0f` (the heap
/// coercion used to reinterpret the `Int` as float BITS), and on a `byte`
/// field it is a narrowing `asType` refuses (WMTE).
#[test]
fn a_primitive_site_widens_numerically_and_refuses_narrowing() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "F");
    set(
        &mut ctx,
        vh,
        recv,
        Value::Int(5),
        pack(vh_site::OTHER, b'I'),
    )
    .unwrap();
    assert_eq!(ctx.get_field(recv, 0), Value::Float(5.0));
    set(&mut ctx, vh, recv, Value::Int(6), 0).unwrap();
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Float(6.0),
        "unarmed Int is an int"
    );
    let (bvh, brecv) = handle(&mut ctx, "B");
    let int_site = pack(vh_site::OTHER, b'I');
    let err = set(&mut ctx, bvh, brecv, Value::Int(5), int_site).unwrap_err();
    assert!(is_wmte(&ctx, &err), "int -> byte narrows: {err:?}");
    set(
        &mut ctx,
        bvh,
        brecv,
        Value::Int(5),
        pack(vh_site::OTHER, b'B'),
    )
    .unwrap();
    assert_eq!(ctx.get_field(brecv, 0), Value::Int(5));
}

/// `compareAndSet(h, (Object) 5, (Object) 9)` on an `int` field: the
/// EXPECTED value is unboxed too, so the CAS succeeds. Before, an
/// `Integer` reference never matched the `int` slot.
#[test]
fn an_object_site_cas_unboxes_both_values() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    ctx.set_field(recv, 0, Value::Int(5));
    let expected = wrapper(&mut ctx, "java/lang/Integer", Value::Int(5));
    let new = wrapper(&mut ctx, "java/lang/Integer", Value::Int(9));
    arm_vh_value_site(pack(vh_site::OBJECT, vh_site::OBJECT));
    let ok = varhandle_compare_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            expected,
            new,
        ],
    )
    .unwrap();
    assert_eq!(
        ok,
        Some(Value::Int(1)),
        "boxed expectation matches the int slot"
    );
    assert_eq!(ctx.get_field(recv, 0), Value::Int(9));
}

/// The cell is TAKEN: a native that runs leaves nothing for the next one.
#[test]
fn the_native_takes_the_value_site() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    set(
        &mut ctx,
        vh,
        recv,
        Value::Int(1),
        pack(vh_site::OTHER, b'I'),
    )
    .unwrap();
    assert_eq!(cratonvm_native_api::poly_call_site::take_vh_value_site(), 0);
}

/// Round 11 wave 17 (r11w16-vh-primitive-into-reference-variable-stores-null):
/// `set(h, 100000)` on an `Object` field stores an `Integer` whose payload
/// is the value, armed or not. Before: `coerce_field_value_for_slot` turned
/// the raw `Int` into `null` at the reference slot. The values sit outside
/// every wrapper cache, so no VM-scoped cache entry from another test's
/// mock heap can answer.
#[test]
fn a_primitive_into_an_object_variable_is_boxed() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "Ljava/lang/Object;");
    set(
        &mut ctx,
        vh,
        recv,
        Value::Int(100_000),
        pack(vh_site::OTHER, b'I'),
    )
    .unwrap();
    let Value::Object(Some(b)) = ctx.get_field(recv, 0) else {
        panic!("the store must leave a box, not null");
    };
    assert_eq!(ctx.get_field(b, 0), Value::Int(100_000));
    // Unarmed: the variant decides (`Long` -> `Long`).
    set(&mut ctx, vh, recv, Value::Long(1 << 40), 0).unwrap();
    let Value::Object(Some(b)) = ctx.get_field(recv, 0) else {
        panic!("the unarmed store must leave a box, not null");
    };
    assert_eq!(ctx.get_field(b, 0), Value::Long(1 << 40));
}

/// nio21 wave 12, lane L3 (`r21-varhandle-instance-cas-does-not-cast-expected`):
/// a wrong-typed EXPECTATION is the JDK body's `fieldType.cast(expected)`
/// `ClassCastException` in `compareAndSet` and `compareAndExchange`, as a
/// wrong-typed new value already was, and nothing is stored. Judged only
/// where the mock can judge the new value's cast too (its class model is
/// the same predicate's).
#[test]
fn a_wrong_typed_expectation_is_cce_in_both_cas_doors() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "Ljava/lang/Integer;");
    let keep = wrapper(&mut ctx, "java/lang/Integer", Value::Int(1));
    ctx.set_field(recv, 0, keep);
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let h = Value::Object(Some(vh));
    let r = Value::Object(Some(recv));
    let site = pack(vh_site::OBJECT, vh_site::OBJECT);
    arm_vh_value_site(site);
    let wrong_new = varhandle_compare_and_set(&mut ctx, &[h, r, keep, s]);
    if !matches!(&wrong_new, Err(e) if is_cce(e)) {
        return;
    }
    arm_vh_value_site(site);
    let err = varhandle_compare_and_set(&mut ctx, &[h, r, s, Value::Object(None)]).unwrap_err();
    assert!(
        is_cce(&err),
        "compareAndSet with a String expectation: {err:?}"
    );
    arm_vh_value_site(site);
    let err =
        varhandle_compare_and_exchange(&mut ctx, &[h, r, s, Value::Object(None)]).unwrap_err();
    assert!(
        is_cce(&err),
        "compareAndExchange with a String expectation: {err:?}"
    );
    assert_eq!(ctx.get_field(recv, 0), keep, "no refused CAS landed");
}

/// `asType`'s static refusal: `int` into a `String` field, `char` into a
/// `Number` field and `int` into a `Long` field are WMTE, and nothing is
/// stored.
#[test]
fn a_primitive_the_reference_variable_cannot_hold_is_wmte() {
    let mut ctx = MockNativeContext::new();
    let (svh, srecv) = handle(&mut ctx, "Ljava/lang/String;");
    ctx.set_field(srecv, 0, Value::Object(None));
    let err = set(
        &mut ctx,
        svh,
        srecv,
        Value::Int(5),
        pack(vh_site::OTHER, b'I'),
    )
    .unwrap_err();
    assert!(is_wmte(&ctx, &err), "int -> String is WMTE: {err:?}");
    let (nvh, nrecv) = handle(&mut ctx, "Ljava/lang/Number;");
    let err = set(
        &mut ctx,
        nvh,
        nrecv,
        Value::Int(0x41),
        pack(vh_site::OTHER, b'C'),
    )
    .unwrap_err();
    assert!(is_wmte(&ctx, &err), "char -> Number is WMTE: {err:?}");
    let (lvh, lrecv) = handle(&mut ctx, "Ljava/lang/Long;");
    let err = set(
        &mut ctx,
        lvh,
        lrecv,
        Value::Int(5),
        pack(vh_site::OTHER, b'I'),
    )
    .unwrap_err();
    assert!(is_wmte(&ctx, &err), "int -> Long is WMTE: {err:?}");
    assert_eq!(
        ctx.get_field(srecv, 0),
        Value::Object(None),
        "no refused store landed"
    );
}

/// The static table itself, beside `vh_site_admits_primitive`.
#[test]
fn the_reference_admits_primitive_table() {
    assert!(vh_reference_admits_primitive("Ljava/lang/Object;", b'Z'));
    assert!(vh_reference_admits_primitive(
        "Ljava/lang/Comparable;",
        b'C'
    ));
    assert!(vh_reference_admits_primitive("Ljava/lang/Number;", b'B'));
    assert!(!vh_reference_admits_primitive("Ljava/lang/Number;", b'Z'));
    assert!(vh_reference_admits_primitive(
        "Ljava/lang/constant/ConstantDesc;",
        b'D'
    ));
    assert!(!vh_reference_admits_primitive(
        "Ljava/lang/constant/ConstantDesc;",
        b'S'
    ));
    assert!(vh_reference_admits_primitive("Ljava/lang/Integer;", b'I'));
    assert!(!vh_reference_admits_primitive("Ljava/lang/Integer;", b'J'));
    assert!(!vh_reference_admits_primitive("[Ljava/lang/Object;", b'I'));
    assert!(!vh_reference_admits_primitive("Ljava/util/List;", b'I'));
}

/// `asType` checks every parameter before converting any: a null expected
/// value through an `Object` site beside a `String`-typed new value is
/// WMTE (the second value's static refusal), not the first value's NPE.
#[test]
fn a_static_refusal_of_either_value_wins_over_a_run_time_one() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "I");
    ctx.set_field(recv, 0, Value::Int(3));
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    arm_vh_value_site(pack(vh_site::OBJECT, vh_site::OTHER));
    let err = varhandle_compare_and_set(
        &mut ctx,
        &[
            Value::Object(Some(vh)),
            Value::Object(Some(recv)),
            Value::Object(None),
            s,
        ],
    )
    .unwrap_err();
    assert!(
        is_wmte(&ctx, &err),
        "WMTE before the expected value's NPE: {err:?}"
    );
    assert_eq!(ctx.get_field(recv, 0), Value::Int(3));
}

/// A handle of any `kind` over field 0 of `Box`, for the arity tests.
fn handle_of_kind(
    ctx: &mut MockNativeContext,
    kind: i32,
    class_name: &str,
    desc: &str,
) -> ObjectRef {
    let vh = obj(ctx, "java/lang/invoke/VarHandle");
    vh_meta_put(
        ctx,
        vh,
        VarHandleMeta {
            kind,
            class_name: class_name.to_string(),
            field_name: "f".to_string(),
            field_desc: desc.to_string(),
            field_index: 0,
            class_id: 0,
            read_only: false,
            exact: None,
            ref_type_class_id: 0,
            holder_class_id: 0,
        },
    );
    vh
}

/// Round 11 wave 19 (r11w18-orch-varhandle-set-accepts-a-surplus-reference-
/// argument, probe `R11W18VhArrayValueArity`): `asType` refuses a call
/// site whose argument COUNT differs from the access mode's, before any
/// value is looked at. `A.set(h, arr, "s")` on an `int[]` field STORED
/// `arr`; a missing value stored a default. Every access-mode family,
/// surplus reference, surplus primitive and missing argument.
#[test]
fn a_surplus_or_missing_argument_is_wmte_in_every_access_mode_family() {
    let mut ctx = MockNativeContext::new();
    let (vh, recv) = handle(&mut ctx, "[I");
    let sentinel = ctx.new_array(ArrayElementType::Int, 1);
    let arr = ctx.new_array(ArrayElementType::Int, 3);
    ctx.set_field(recv, 0, Value::Object(Some(sentinel)));
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let h = Value::Object(Some(vh));
    let r = Value::Object(Some(recv));
    let a = Value::Object(Some(arr));
    let five = Value::Int(5);

    // set: surplus reference (the page's row), surplus int, missing value.
    for args in [vec![h, r, a, s], vec![h, r, a, five], vec![h, r]] {
        let err = varhandle_set(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "set {args:?}: {err:?}");
        let err = varhandle_set_volatile(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "setVolatile {args:?}: {err:?}");
    }
    assert_eq!(
        ctx.get_field(recv, 0),
        Value::Object(Some(sentinel)),
        "no refused store landed"
    );
    // get: a surplus argument.
    let err = varhandle_get(&mut ctx, &[h, r, s]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "get: {err:?}");
    // compareAndSet / compareAndExchange: surplus and missing.
    for args in [vec![h, r, a, a, s], vec![h, r, a]] {
        let err = varhandle_compare_and_set(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "compareAndSet {args:?}: {err:?}");
        let err = varhandle_compare_and_exchange(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "compareAndExchange {args:?}: {err:?}");
    }
    // getAndSet: surplus and missing.
    for args in [vec![h, r, a, s], vec![h, r]] {
        let err = varhandle_get_and_set(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "getAndSet {args:?}: {err:?}");
    }
    assert_eq!(ctx.get_field(recv, 0), Value::Object(Some(sentinel)));

    // The exact shape still stores.
    varhandle_set(&mut ctx, &[h, r, a]).unwrap();
    assert_eq!(ctx.get_field(recv, 0), Value::Object(Some(arr)));

    // getAndAdd / getAndBitwise* on an `int` field (an `int[]` field
    // refuses them with UOE first, which is HotSpot's order too).
    let (ivh, irecv) = handle(&mut ctx, "I");
    ctx.set_field(irecv, 0, Value::Int(10));
    let ih = Value::Object(Some(ivh));
    let ir = Value::Object(Some(irecv));
    for args in [vec![ih, ir, Value::Int(1), s], vec![ih, ir]] {
        let err = varhandle_get_and_add(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "getAndAdd {args:?}: {err:?}");
        let err = varhandle_get_and_bitwise_or(&mut ctx, &args).unwrap_err();
        assert!(is_wmte(&ctx, &err), "getAndBitwiseOr {args:?}: {err:?}");
    }
    assert_eq!(
        ctx.get_field(irecv, 0),
        Value::Int(10),
        "no refused update landed"
    );
}

/// The coordinate count comes from the handle's kind: none for a static
/// field, two for an array element. A handle with no side-table entry is
/// not judged (the count is not known).
#[test]
fn the_coordinate_count_follows_the_handle_kind() {
    let mut ctx = MockNativeContext::new();
    let s = Value::Object(Some(obj(&mut ctx, "java/lang/String")));
    let stat = Value::Object(Some(handle_of_kind(&mut ctx, VH_KIND_STATIC, "Box", "I")));
    let err = varhandle_set(&mut ctx, &[stat, s, Value::Int(1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "static set with a coordinate: {err:?}");
    let err = varhandle_get(&mut ctx, &[stat, s]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "static get with a coordinate: {err:?}");

    let av = Value::Object(Some(handle_of_kind(&mut ctx, VH_KIND_ARRAY, "[I", "I")));
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 2)));
    let err = varhandle_get(&mut ctx, &[av, arr, Value::Int(0), Value::Int(1)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "array get with a surplus: {err:?}");
    let err = varhandle_set(&mut ctx, &[av, arr, Value::Int(0)]).unwrap_err();
    assert!(is_wmte(&ctx, &err), "array set with no value: {err:?}");
    assert!(
        vh_check_arity(&mut ctx, &[av, arr, Value::Int(0), Value::Int(7)], 1, "set").is_ok()
    );
    // (A handle with no side-table entry is not judged. Not asserted here:
    // the side table is process-global and keyed by the identity hash,
    // which the mock takes from the address truncated to `i32`, so past
    // 4096 mock contexts in one test binary "no entry" is not a property
    // one test can own.)
}
}

/// gc-common w15-e: the references `mh_dispatch` and `bindTo` hold across a GC
/// point (an allocation, or a first `<clinit>`) are rooted and re-read, and
/// every root is released on the way out.
///
/// The mock moves nothing on an allocation or a class initialisation (it has
/// only an `invoke_virtual` hook), so these tests pin down the observable
/// half of each fix: the values land where they must, and the pin stack is
/// balanced. Each test owns its VM identity (`0xE15_01xx`).
#[cfg(test)]
mod w15e_stale_reference_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const OBJ_OBJ_DESC: &str = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn handle(v: MethodCallResult, what: &str) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("{what} answered {other:?}"),
    }
}

fn identity(ctx: &mut MockNativeContext) -> ObjectRef {
    alloc_method_handle(
        ctx,
        "java/lang/invoke/MethodHandles",
        "identity",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_IDENTITY,
    )
    .unwrap()
}

/// `factory.bindTo(a).bindTo(b)` on a lambda factory: the second bind
/// allocates the capture array after reading the first bind's captures,
/// `b` and the new handle. All three must survive into the new array, the
/// first handle's array must be left alone, and no pin may leak.
#[test]
fn a_second_lambda_bind_keeps_both_captures_and_releases_its_pins() {
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    let bind = registry
        .find(
            "java/lang/invoke/MethodHandle",
            "bindTo",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandle.bindTo should be registered");
    let mut ctx = vm_ctx(0xE15_0101);
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let factory = alloc_method_handle(
        &mut ctx,
        "77",
        "",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Runnable;",
        MH_KIND_LAMBDA_FACTORY,
    )
    .unwrap();
    // No `type` MethodType: `bindTo`'s leading-parameter check then lets
    // the bind through without reading mock class mirrors.
    ctx.set_field_by_name(factory, "type", Value::Object(None));

    let once = handle(
        bind(
            &mut ctx,
            &[Value::Object(Some(factory)), Value::Object(Some(a))],
        ),
        "the first bindTo",
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "the first bind releases its roots"
    );
    ctx.set_field_by_name(once, "type", Value::Object(None));
    let twice = handle(
        bind(
            &mut ctx,
            &[Value::Object(Some(once)), Value::Object(Some(b))],
        ),
        "the second bindTo",
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "the second bind releases its roots"
    );

    let Value::Object(Some(captures)) = ctx.get_field(twice, MH_BOUND) else {
        panic!("the twice-bound factory must carry a capture array");
    };
    assert_eq!(ctx.array_length(captures), 2);
    assert_eq!(ctx.get_array_element(captures, 0), Value::Object(Some(a)));
    assert_eq!(ctx.get_array_element(captures, 1), Value::Object(Some(b)));
    let Value::Object(Some(first)) = ctx.get_field(once, MH_BOUND) else {
        panic!("the once-bound factory must carry a capture array");
    };
    assert_eq!(
        ctx.array_length(first),
        1,
        "the first handle's captures are not rewritten"
    );
    assert_eq!(ctx.get_array_element(first, 0), Value::Object(Some(a)));
}

/// A constructor handle roots `mh` and its arguments across the class's
/// first `<clinit>` and releases every root (the arm's lowest pin moved
/// from `new_obj` to `mh`).
#[test]
fn constructor_dispatch_releases_every_root() {
    let mut ctx = vm_ctx(0xE15_0102);
    let ctor = alloc_method_handle(
        &mut ctx,
        "java/lang/Object",
        "<init>",
        "(Ljava/lang/Object;)V",
        MH_KIND_CONSTRUCTOR,
    )
    .unwrap();
    let arg = ctx.fresh_object_ref();
    let made = mh_dispatch(&mut ctx, ctor, &[Value::Object(Some(arg))]).unwrap();
    assert!(
        matches!(made, Some(Value::Object(Some(_)))),
        "a constructor handle answers the new object, got {made:?}"
    );
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `asCollector(Object[].class, 2)` over `identity`: both arguments are
/// read through their roots after the collector array's allocation.
#[test]
fn a_collector_gathers_the_rooted_arguments() {
    let mut ctx = vm_ctx(0xE15_0103);
    let target = identity(&mut ctx);
    let wrapper = alloc_mh_carrier(&mut ctx, "__mh_collect_wrapper__", 3);
    ctx.set_field(wrapper, 0, Value::Object(Some(target)));
    ctx.set_field(wrapper, 1, Value::Int(2));
    ctx.set_field(wrapper, 2, Value::Object(None));
    let collector = alloc_method_handle(
        &mut ctx,
        "__adapter__",
        "collect",
        OBJ_OBJ_DESC,
        MH_KIND_COLLECT,
    )
    .unwrap();
    ctx.set_field(collector, MH_BOUND, Value::Object(Some(wrapper)));
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());

    let arr = handle(
        mh_dispatch(
            &mut ctx,
            collector,
            &[Value::Object(Some(a)), Value::Object(Some(b))],
        ),
        "the collector",
    );
    assert_eq!(ctx.array_length(arr), 2);
    assert_eq!(ctx.get_array_element(arr, 0), Value::Object(Some(a)));
    assert_eq!(ctx.get_array_element(arr, 1), Value::Object(Some(b)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// `spreadInvoker(type, 0)` SPREADS its trailing `Object[]` into the
/// target's arguments (wave 26: it used to pack the arguments into a new
/// array, the inverse of `invoker(type).asSpreader(..)`), with
/// `asSpreader`'s length and null checks, and releases every root.
#[test]
fn a_spread_invoker_spreads_its_trailing_array() {
    let mut ctx = vm_ctx(0xE15_0104);
    let target = identity(&mut ctx);
    // `MH_DESC` is the TARGET descriptor (as `mh_alloc_invoker` records
    // it since wave 27), `(Object)Object`: one spread argument.
    let invoker = alloc_method_handle(
        &mut ctx,
        "__invoker__",
        "0",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        MH_KIND_INVOKER,
    )
    .unwrap();
    ctx.set_field_by_name(invoker, "type", Value::Object(None));
    let a = ctx.fresh_object_ref();
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    ctx.set_array_element(arr, 0, Value::Object(Some(a)));

    let spread = mh_dispatch(
        &mut ctx,
        invoker,
        &[Value::Object(Some(target)), Value::Object(Some(arr))],
    );
    assert_eq!(spread.unwrap(), Some(Value::Object(Some(a))));
    let short = ctx.new_array(ArrayElementType::Reference, 0);
    assert!(matches!(
        mh_dispatch(
            &mut ctx,
            invoker,
            &[Value::Object(Some(target)), Value::Object(Some(short))],
        ),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::IllegalArgumentException { .. }
        )))
    ));
    assert!(matches!(
        mh_dispatch(
            &mut ctx,
            invoker,
            &[Value::Object(Some(target)), Value::Object(None)],
        ),
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        )))
    ));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}

/// A reflective lambda factory stores the rooted captures (bound first,
/// then direct) into the proxy it allocates.
#[test]
fn a_lambda_factory_stores_the_rooted_captures() {
    let mut ctx = vm_ctx(0xE15_0105);
    let (a, b) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let factory = alloc_method_handle(
        &mut ctx,
        "77",
        "",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Runnable;",
        MH_KIND_LAMBDA_FACTORY,
    )
    .unwrap();
    let bound = ctx.new_array(ArrayElementType::Reference, 1);
    ctx.set_array_element(bound, 0, Value::Object(Some(a)));
    ctx.set_field(factory, MH_BOUND, Value::Object(Some(bound)));

    let proxy = handle(
        mh_dispatch(&mut ctx, factory, &[Value::Object(Some(b))]),
        "the lambda factory",
    );
    assert_eq!(ctx.get_field(proxy, 0), Value::Object(Some(a)));
    assert_eq!(ctx.get_field(proxy, 1), Value::Object(Some(b)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "every root must be released"
    );
}
}

/// gc-common w19-c (stale-handle triage). The mock never moves an object, so
/// these check that the rooted rewrites keep their answers and release every
/// root; the move itself is not expressible here.
#[cfg(test)]
mod w19c_stale_handle_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const MT_TO_MH: &str = "(Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;";

fn vm_ctx(vm: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(vm);
    ctx
}

fn handle(v: MethodCallResult, what: &str) -> ObjectRef {
    match v {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("{what} answered {other:?}"),
    }
}

/// Drops the VarHandle meta rows the test filed, on every exit path.
struct ForgetVarHandleMeta(usize);

impl Drop for ForgetVarHandleMeta {
    fn drop(&mut self) {
        forget_vm_var_handle_meta(self.0);
    }
}

/// `exactInvoker` / `invoker` / `spreadInvoker` stamp the CALLER's
/// MethodType (read through its root, after `alloc_method_handle`), and
/// `spreadInvoker` its count.
#[test]
fn the_invoker_factories_stamp_the_callers_type_and_release_their_roots() {
    let mut registry = NativeMethodRegistry::new();
    register_t28_method_handle_completeness(&mut registry);
    let mhs = "java/lang/invoke/MethodHandles";
    let mut ctx = vm_ctx(0xC19_0301);
    let mt = ctx.fresh_object_ref();
    for name in ["exactInvoker", "invoker"] {
        let factory = registry
            .find(mhs, name, MT_TO_MH)
            .unwrap_or_else(|| panic!("MethodHandles.{name} should be registered"));
        let mh = handle(factory(&mut ctx, &[Value::Object(Some(mt))]), name);
        assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
        assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_INVOKER));
        assert_eq!(
            ctx.native_pin_count_for_test(),
            0,
            "{name} releases its roots"
        );
    }
    let spread = registry
        .find(
            mhs,
            "spreadInvoker",
            "(Ljava/lang/invoke/MethodType;I)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandles.spreadInvoker should be registered");
    let mh = handle(
        spread(&mut ctx, &[Value::Object(Some(mt)), Value::Int(3)]),
        "spreadInvoker",
    );
    assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
    let Value::Object(Some(count)) = ctx.get_field(mh, MH_NAME) else {
        panic!("spreadInvoker must record its count");
    };
    assert_eq!(ctx.read_string(count).as_deref(), Some("3"));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// `CallSite.makeUninitializedCallSite(type)`: same shape, `args[1]`.
#[test]
fn an_uninitialized_call_site_handle_carries_the_callers_type() {
    let mut registry = NativeMethodRegistry::new();
    register_method_handle_combinator_extras_bridge(&mut registry);
    let make = registry
        .find(
            "java/lang/invoke/CallSite",
            "makeUninitializedCallSite",
            MT_TO_MH,
        )
        .expect("CallSite.makeUninitializedCallSite should be registered");
    let mut ctx = vm_ctx(0xC19_0302);
    let (cs, mt) = (ctx.fresh_object_ref(), ctx.fresh_object_ref());
    let mh = handle(
        make(
            &mut ctx,
            &[Value::Object(Some(cs)), Value::Object(Some(mt))],
        ),
        "makeUninitializedCallSite",
    );
    assert_eq!(ctx.get_field_by_name(mh, "type"), Value::Object(Some(mt)));
    assert_eq!(ctx.get_field(mh, MH_KIND), Value::Int(MH_KIND_CONSTANT));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}

/// Both VarHandle mints store their three names through the rooted
/// `vh_store_names`.
#[test]
fn the_var_handle_mints_store_their_names_and_release_their_roots() {
    const VM: usize = 0xC19_0303;
    let _forget = ForgetVarHandleMeta(VM);
    let mut ctx = vm_ctx(VM);
    fn text(ctx: &MockNativeContext, vh: ObjectRef, slot: usize) -> Option<String> {
        match ctx.get_field(vh, slot) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        }
    }

    let vh = alloc_instance_var_handle(&mut ctx, "a/B", "count", "I", 3, ClassId::new(7), None)
        .unwrap();
    assert_eq!(text(&ctx, vh, VH_CLASS).as_deref(), Some("a/B"));
    assert_eq!(text(&ctx, vh, VH_FIELD).as_deref(), Some("count"));
    assert_eq!(text(&ctx, vh, VH_FIELD_DESC).as_deref(), Some("I"));
    assert_eq!(ctx.get_field(vh, VH_FIELD_INDEX), Value::Int(3));
    assert_eq!(ctx.native_pin_count_for_test(), 0);

    let vh = alloc_static_var_handle(&mut ctx, "a/B", "TOTAL", "J", None).unwrap();
    assert_eq!(text(&ctx, vh, VH_CLASS).as_deref(), Some("a/B"));
    assert_eq!(text(&ctx, vh, VH_FIELD).as_deref(), Some("TOTAL"));
    assert_eq!(text(&ctx, vh, VH_FIELD_DESC).as_deref(), Some("J"));
    assert_eq!(ctx.get_field(vh, VH_FIELD_INDEX), Value::Int(-1));
    assert_eq!(ctx.native_pin_count_for_test(), 0);
}
}

#[cfg(test)]
mod w20f_carrier_tests {
//! gc-common w20-f: `alloc_mh_carrier_holding` stores every slot, the
//! references at their current addresses, and releases what it roots.
use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn w20f_carrier_holding_stores_each_slot_and_releases_its_roots() {
    let mut ctx = mock_ctx();
    let target = ctx.fresh_object_ref();
    let filter = ctx.fresh_object_ref();
    let pins_before = ctx.native_pin_count_for_test();
    let wrapper = alloc_mh_carrier_holding(
        &mut ctx,
        "__mh_w20f_test_wrapper__",
        &[
            Value::Object(Some(target)),
            Value::Object(None),
            Value::Int(7),
            Value::Object(Some(filter)),
        ],
    );
    assert_eq!(ctx.get_field(wrapper, 0), Value::Object(Some(target)));
    assert_eq!(ctx.get_field(wrapper, 1), Value::Object(None));
    assert_eq!(ctx.get_field(wrapper, 2), Value::Int(7));
    assert_eq!(ctx.get_field(wrapper, 3), Value::Object(Some(filter)));
    assert_eq!(
        ctx.native_pin_count_for_test(),
        pins_before,
        "no root left behind"
    );
}
}

/// Interpreter round i1 wave 9, lane L6: the reflective `LambdaMetafactory`
/// path records the `Lookup`'s lookup class as the proxy's host, keys its
/// `CallSite` cache by it, and drops a host's rows when it unloads.
#[cfg(test)]
mod reflective_lambda_host_tests {
use super::*;
use crate::test_utils::mock_ctx;
use cratonvm_native_api::NativeHeapAccess;

#[test]
fn the_host_is_the_lookup_class_of_args_0() {
    let mut ctx = mock_ctx();
    // The mock's reverse map reads a mirror's slot 0 as its class id.
    let mirror = ctx.alloc_object(ClassId::new(1), 1);
    ctx.set_field(mirror, 0, Value::Int(42));
    let lookup = ctx.alloc_object(ClassId::new(2), 3);
    ctx.set_field(lookup, 0, Value::Object(Some(mirror)));
    assert_eq!(
        reflective_lambda_host(&ctx, Some(&Value::Object(Some(lookup)))),
        Some(ClassId::new(42))
    );
    // No Lookup, a null Lookup, or a Lookup with no lookup class: no host.
    assert_eq!(reflective_lambda_host(&ctx, None), None);
    assert_eq!(
        reflective_lambda_host(&ctx, Some(&Value::Object(None))),
        None
    );
    let empty = ctx.alloc_object(ClassId::new(2), 3);
    ctx.set_field(empty, 0, Value::Object(None));
    assert_eq!(
        reflective_lambda_host(&ctx, Some(&Value::Object(Some(empty)))),
        None
    );
}

#[test]
fn a_host_s_cached_call_sites_go_when_it_unloads_and_only_then() {
    let mut ctx = mock_ctx();
    let ccs = ctx.alloc_object(ClassId::new(3), 1);
    // The `impl_` key component: a real object's address (the scan turns
    // it back into an `ObjectRef`); the other components are 0.
    let impl_addr = ctx.alloc_object(ClassId::new(4), 0).as_ptr() as usize;
    // A VM identity no other test uses.
    let vm = usize::MAX - 0x196;
    let other_vm = vm - 1;
    let key = |host: u32| LambdaKey {
        host,
        invoked: 0,
        sam: 0,
        impl_: impl_addr,
        instantiated: 0,
    };
    {
        let mut cache = lambda_callsite_cache().lock();
        for host in [0u32, 7, 8] {
            cache.insert((vm, key(host)), ccs);
        }
        cache.insert((other_vm, key(7)), ccs);
    }
    forget_lambda_callsites_of_unloaded_hosts(vm, &|host| host == 7);
    {
        let cache = lambda_callsite_cache().lock();
        assert!(
            !cache.contains_key(&(vm, key(7))),
            "the unloaded host's row goes"
        );
        assert!(cache.contains_key(&(vm, key(8))), "a live host's row stays");
        assert!(cache.contains_key(&(vm, key(0))), "a host-less row stays");
        assert!(
            cache.contains_key(&(other_vm, key(7))),
            "another VM's row with a colliding host id stays"
        );
    }
    // The scan reports each row's values with its host as the owner.
    let mut seen: Vec<Option<u32>> = Vec::new();
    gc_scan_lambda_callsite_cache_roots(vm, &mut |owner, _| seen.push(owner));
    seen.sort_unstable();
    // One value per row (the `impl_` key component; the others are 0),
    // plus the `CallSite` itself.
    assert_eq!(seen, vec![None, None, Some(8), Some(8)]);
    forget_vm_lambda_callsite_cache(vm);
    forget_vm_lambda_callsite_cache(other_vm);
}
}

/// Static field handles named through a subclass (interpreter round i1
/// wave 16, lane L3): they read and write the DECLARING class's static.
#[cfg(test)]
mod i16_l3_static_member_tests {
use super::{
    alloc_method_handle, inherited_static_field_holder, mh_dispatch, static_field_holder,
    vh_static_slot, MH_KIND_GETTER, MH_KIND_SETTER,
};
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{FieldMetadata, NativeClassAccess, NativeSystemAccess};
use cratonvm_types::{ClassId, Value};

const ACC_STATIC: u16 = 0x0008;

fn static_int(name: &str, slot: usize, declaring: ClassId) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: "I".to_string(),
        access_flags: ACC_STATIC,
        slot_index: slot,
        declaring_class_id: declaring,
        is_static: true,
    }
}

/// `p/I` (an interface) declares `k`, `p/P` declares `f` and `k`,
/// `p/C extends P implements I` declares `own`.
fn hierarchy(ctx: &mut MockNativeContext) -> (ClassId, ClassId, ClassId) {
    let i = ctx
        .ensure_class_initialized("p/I")
        .unwrap_or(ClassId::new(0));
    let p = ctx
        .ensure_class_initialized("p/P")
        .unwrap_or(ClassId::new(0));
    let c = ctx
        .ensure_class_initialized("p/C")
        .unwrap_or(ClassId::new(0));
    ctx.set_declared_fields(i, vec![static_int("k", 0, i)]);
    ctx.set_declared_fields(p, vec![static_int("f", 3, p), static_int("k", 4, p)]);
    ctx.set_declared_fields(c, vec![static_int("own", 1, c)]);
    ctx.set_superclass(c, p);
    ctx.set_interfaces(c, vec![i]);
    (i, p, c)
}

/// JVMS 5.4.3.2 order: the class itself, then superinterfaces, then the
/// superclass; a field the named class declares is not "inherited".
#[test]
fn the_holder_follows_the_field_lookup_order() {
    let mut ctx = MockNativeContext::new();
    let (i, p, c) = hierarchy(&mut ctx);
    assert_eq!(static_field_holder(&ctx, c, "own"), Some(c));
    assert_eq!(static_field_holder(&ctx, c, "f"), Some(p));
    assert_eq!(
        static_field_holder(&ctx, c, "k"),
        Some(i),
        "interfaces first"
    );
    assert_eq!(static_field_holder(&ctx, c, "absent"), None);
    assert_eq!(inherited_static_field_holder(&ctx, c, "own"), None);
    assert_eq!(inherited_static_field_holder(&ctx, c, "f"), Some(p));
    assert_eq!(inherited_static_field_holder(&ctx, p, "f"), None);
    assert_eq!(inherited_static_field_holder(&ctx, c, "absent"), None);
}

/// `findStaticGetter(C.class, "f", int.class).invoke()` for an `f` that
/// `P` declares reads `P.f`; the setter writes it. The arms read slot 0
/// of `C` and wrote nothing before wave 16.
#[test]
fn a_static_field_handle_named_through_a_subclass_reaches_the_declaring_class() {
    let mut ctx = MockNativeContext::new();
    let (_, p, c) = hierarchy(&mut ctx);
    ctx.set_static_field(p, 3, Value::Int(42));
    ctx.set_static_field(c, 0, Value::Int(-1));
    let getter = alloc_method_handle(&mut ctx, "p/C", "f", "()I", MH_KIND_GETTER);
    let getter = getter.unwrap_or_else(|e| panic!("getter: {e:?}"));
    let read = mh_dispatch(&mut ctx, getter, &[]);
    assert!(
        matches!(read, Ok(Some(Value::Int(42)))),
        "C.f reads P.f: {read:?}"
    );

    let setter = alloc_method_handle(&mut ctx, "p/C", "f", "(I)V", MH_KIND_SETTER);
    let setter = setter.unwrap_or_else(|e| panic!("setter: {e:?}"));
    let wrote = mh_dispatch(&mut ctx, setter, &[Value::Int(7)]);
    assert!(wrote.is_ok(), "{wrote:?}");
    assert_eq!(ctx.get_static_field(p, 3), Value::Int(7));
    assert_eq!(
        ctx.get_static_field(c, 0),
        Value::Int(-1),
        "C's slot 0 untouched"
    );
}

/// The static VarHandle slot of an inherited field is the declaring
/// class's; a field the named class declares keeps the named class.
#[test]
fn a_static_var_handle_slot_is_the_declaring_classes() {
    let mut ctx = MockNativeContext::new();
    let (_, p, c) = hierarchy(&mut ctx);
    let slot = |ctx: &mut MockNativeContext, field: &str, owner: Option<ClassId>| {
        vh_static_slot(
            ctx,
            "p/C",
            field,
            owner,
            &mut [],
            super::VhStaticInit {
                mode: "",
                variable: 0,
            },
        )
        .unwrap_or(None)
    };
    assert_eq!(slot(&mut ctx, "f", None), Some((p, 3)));
    assert_eq!(slot(&mut ctx, "f", Some(c)), Some((p, 3)));
    assert_eq!(slot(&mut ctx, "own", None), Some((c, 1)));
}
}

#[cfg(test)]
mod i17_l4_lookup_init_tests {
use super::{
    alloc_method_handle, lookup_find_constructor, lookup_find_getter, lookup_find_setter,
    lookup_find_special, lookup_find_static_getter, lookup_find_static_setter,
    lookup_find_static_var_handle, lookup_find_var_handle, lookup_find_virtual,
    arm_entry_call_site, lookup_load_class, mh_dispatch, vh_static_slot, MH_BOUND,
    MH_KIND_CONSTRUCTOR,
    MH_KIND_GETTER, MH_KIND_SETTER,
};
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeHeapAccess, NativeSystemAccess,
};
use cratonvm_types::error::{MethodCallFailed, RuntimeError, VmError};
use cratonvm_types::{ClassId, Value};

/// `findStaticGetter` / `findStaticSetter` / `findVarHandle` /
/// `findStaticVarHandle` with a null `refc`, `name` or `type` throw
/// `NullPointerException` (HotSpot's `resolveOrFail`), like the other
/// finders; they handed back an inert handle or a null VarHandle before
/// wave 17.
#[test]
fn the_static_and_var_handle_finders_refuse_a_null_argument() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("p/Target")
        .unwrap_or(ClassId::new(0));
    let mirror = Value::Object(Some(ctx.get_class_mirror(cid)));
    let name = Value::Object(Some(ctx.create_string("f")));
    let null = Value::Object(None);
    type Finder = fn(
        &mut dyn cratonvm_native_api::NativeContext,
        &[Value],
    ) -> cratonvm_types::error::MethodCallResult;
    let finders: [(&str, Finder); 4] = [
        ("findStaticGetter", lookup_find_static_getter),
        ("findStaticSetter", lookup_find_static_setter),
        ("findVarHandle", lookup_find_var_handle),
        ("findStaticVarHandle", lookup_find_static_var_handle),
    ];
    for (what, finder) in finders {
        for args in [
            [null, null, name, mirror],
            [null, mirror, null, mirror],
            [null, mirror, name, null],
        ] {
            let r = finder(&mut ctx, &args);
            assert!(
                matches!(
                    r,
                    Err(MethodCallFailed::InternalError(VmError::Runtime(
                        RuntimeError::NullPointerException { .. }
                    )))
                ),
                "{what}: {r:?}"
            );
        }
    }
}

const ACC_STATIC: u16 = 0x0008;

/// `p/S` with one static `int f` in slot 0.
fn class_with_static(ctx: &mut MockNativeContext) -> ClassId {
    let s = ctx
        .ensure_class_initialized("p/S")
        .unwrap_or(ClassId::new(0));
    ctx.set_declared_fields(
        s,
        vec![FieldMetadata {
            name: "f".to_string(),
            descriptor: "I".to_string(),
            access_flags: ACC_STATIC,
            slot_index: 0,
            declaring_class_id: s,
            is_static: true,
        }],
    );
    s
}

/// HotSpot's `find*` run no `<clinit>`: the finders LOAD the class and
/// leave initialization to the handle's first call. Whatever each finder
/// answers here (the mock declares no members), it must not have asked
/// to initialize the class it was handed.
#[test]
fn the_finders_load_the_class_without_initializing_it() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("p/Target")
        .unwrap_or(ClassId::new(0));
    let mirror = Value::Object(Some(ctx.get_class_mirror(cid)));
    let name = Value::Object(Some(ctx.create_string("m")));
    let mt = Value::Object(Some(ctx.alloc_object(ClassId::new(0), 2)));
    let before = ctx.class_init_requests().len();
    // A null Lookup receiver skips the access gate (`lk_enforce_find_access`).
    let lk = Value::Object(None);
    let _ = lookup_find_virtual(&mut ctx, &[lk, mirror, name, mt]);
    let _ = lookup_find_special(&mut ctx, &[lk, mirror, name, mt, mirror]);
    let _ = lookup_find_constructor(&mut ctx, &[lk, mirror, mt]);
    let _ = lookup_find_getter(&mut ctx, &[lk, mirror, name, mirror]);
    let _ = lookup_find_setter(&mut ctx, &[lk, mirror, name, mirror]);
    let loaded = lookup_load_class(&mut ctx, "p/Target");
    assert!(matches!(loaded, Ok(Some(id)) if id == cid), "{loaded:?}");
    let requests = ctx.class_init_requests();
    assert!(
        !requests[before..].iter().any(|n| n == "p/Target"),
        "a finder initialized its class: {requests:?}"
    );
}

/// A static getter / setter handle on a class whose `<clinit>` failed
/// throws, as `getstatic` / `putstatic` do; it answered `0` and dropped
/// the write before wave 17.
#[test]
fn a_static_field_handle_propagates_the_initialization_error() {
    let mut ctx = MockNativeContext::new();
    let s = class_with_static(&mut ctx);
    ctx.set_static_field(s, 0, Value::Int(5));
    ctx.fail_class_init("p/S");

    let getter = alloc_method_handle(&mut ctx, "p/S", "f", "()I", MH_KIND_GETTER);
    let getter = getter.unwrap_or_else(|e| panic!("getter: {e:?}"));
    let read = mh_dispatch(&mut ctx, getter, &[]);
    assert!(read.is_err(), "the getter must throw: {read:?}");

    let setter = alloc_method_handle(&mut ctx, "p/S", "f", "(I)V", MH_KIND_SETTER);
    let setter = setter.unwrap_or_else(|e| panic!("setter: {e:?}"));
    let wrote = mh_dispatch(&mut ctx, setter, &[Value::Int(7)]);
    assert!(wrote.is_err(), "the setter must throw: {wrote:?}");
    assert_eq!(ctx.get_static_field(s, 0), Value::Int(5), "no write lands");
}

/// A constructor handle allocates before adapting its entry arguments.
/// If that adaptation refuses the call, it must withdraw the allocation's
/// finalizer registration before releasing the pinned object.
#[test]
fn a_constructor_handle_withdraws_when_entry_adaptation_fails() {
    let mut ctx = MockNativeContext::new();
    let cid = ctx
        .ensure_class_initialized("p/F")
        .unwrap_or(ClassId::new(0));
    let ctor = alloc_method_handle(&mut ctx, "p/F", "<init>", "(I)V", MH_KIND_CONSTRUCTOR)
        .unwrap_or_else(|e| panic!("ctor: {e:?}"));
    ctx.set_field(ctor, MH_BOUND, Value::Int(cid.as_u32() as i32));
    arm_entry_call_site("(I)V");
    let rejected = mh_dispatch(&mut ctx, ctor, &[]);
    assert!(rejected.is_err(), "wrong arity must fail: {rejected:?}");
    assert_eq!(
        ctx.withdraw_finalizable_before_constructor_calls.borrow().len(),
        1,
        "the abandoned allocation's finalizer row is withdrawn"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "pins are released");
}

/// The constructor arm throws the initialization error instead of
/// answering `null`.
#[test]
fn a_constructor_handle_propagates_the_initialization_error() {
    let mut ctx = MockNativeContext::new();
    let s = class_with_static(&mut ctx);
    ctx.fail_class_init("p/S");
    let ctor = alloc_method_handle(&mut ctx, "p/S", "<init>", "()V", MH_KIND_CONSTRUCTOR);
    let ctor = ctor.unwrap_or_else(|e| panic!("ctor: {e:?}"));
    ctx.set_field(ctor, MH_BOUND, Value::Int(s.as_u32() as i32));
    let made = mh_dispatch(&mut ctx, ctor, &[]);
    assert!(made.is_err(), "the constructor must throw: {made:?}");
}

/// `vh_static_slot` propagates the error on both the owner and the
/// by-name route, and hands the operands back unchanged when the holder
/// initializes.
#[test]
fn a_static_var_handle_slot_propagates_the_initialization_error() {
    let mut ctx = MockNativeContext::new();
    let s = class_with_static(&mut ctx);
    let mut live = [Value::Int(9)];
    let ok = vh_static_slot(
        &mut ctx,
        "p/S",
        "f",
        Some(s),
        &mut live,
        super::VhStaticInit {
            mode: "",
            variable: 0,
        },
    );
    assert!(matches!(ok, Ok(Some((c, 0))) if c == s), "{ok:?}");
    assert_eq!(live[0], Value::Int(9));

    ctx.fail_class_init("p/S");
    let by_owner = vh_static_slot(
        &mut ctx,
        "p/S",
        "f",
        Some(s),
        &mut live,
        super::VhStaticInit {
            mode: "",
            variable: 0,
        },
    );
    assert!(by_owner.is_err(), "{by_owner:?}");
    let by_name = vh_static_slot(
        &mut ctx,
        "p/S",
        "f",
        None,
        &mut live,
        super::VhStaticInit {
            mode: "",
            variable: 0,
        },
    );
    assert!(by_name.is_err(), "{by_name:?}");
}
}

#[cfg(test)]
mod w33b_static_lock_pin_order_tests {
use super::vh_with_static_lock;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
use cratonvm_types::{ClassId, ObjectRef, Value};

/// The current depth of the mock's pin stack (the index the next pin gets).
fn pin_depth(ctx: &mut MockNativeContext, probe: ObjectRef) -> usize {
    let depth = ctx.pin_native_root(probe);
    ctx.unpin_native_roots(depth);
    depth
}

/// gc-common w33-b: `vh_with_static_lock` pins the caller's operands
/// BEFORE `get_class_mirror`, which mints the mirror on first use and so
/// can collect. The mock cannot move an object, so the test pins the
/// ORDER: the first pin the call takes is the first reference operand,
/// not the mirror. It also checks that the body receives the operands
/// (primitives untouched) and that the call releases every pin it took,
/// with and without reference operands.
#[test]
fn w33b_operands_are_pinned_before_the_mirror_is_fetched() {
    let mut ctx = MockNativeContext::new();
    let witness = ctx.alloc_object(ClassId::new(0), 1);
    let stored = ctx.alloc_object(ClassId::new(0), 1);
    let before = pin_depth(&mut ctx, witness);
    let operands = [
        Value::Object(Some(witness)),
        Value::Int(3),
        Value::Object(Some(stored)),
    ];
    let (first_pinned, seen) =
        vh_with_static_lock(&mut ctx, ClassId::new(7), &operands, |ctx, live| {
            (ctx.read_native_pin(before, stored), live.to_vec())
        });
    assert_eq!(
        first_pinned, witness,
        "the first pin must be the first reference operand, not the mirror"
    );
    assert_eq!(seen, operands.to_vec());
    assert_eq!(pin_depth(&mut ctx, witness), before, "every pin released");

    let answer = vh_with_static_lock(&mut ctx, ClassId::new(7), &[], |_, live| live.len());
    assert_eq!(answer, 0);
    assert_eq!(
        pin_depth(&mut ctx, witness),
        before,
        "the mirror's pin is released when there is no operand"
    );
}
}

#[cfg(test)]
mod r12w3_hfix_lookup_inherited_tests {
//! Round 12 wave 3 (hfix): `findVirtual` / `findSpecial` /
//! `MethodHandleNatives.resolve` resolve an instance method a class
//! inherits from a superinterface (JVMS §5.4.3.3), `findStatic` does not.
use super::{lk_superinterface_declares_instance_method, lookup_require_method};
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{MethodMetadata, NativeClassAccess};
use cratonvm_types::ClassId;

fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
    ctx.ensure_class_initialized(name)
        .unwrap_or(ClassId::new(0))
}

fn method(name: &str, desc: &str, access_flags: u16, owner: ClassId) -> MethodMetadata {
    MethodMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags,
        declaring_class_id: owner,
        exceptions: Vec::new(),
        signature: None,
    }
}

#[test]
fn an_inherited_interface_method_resolves_for_the_instance_finders_only() {
    let mut ctx = MockNativeContext::new();
    // Greeter <- Loud (sub-interface) <- Animal implements Loud <- Dog.
    let greeter = class(&mut ctx, "p/Greeter");
    let loud = class(&mut ctx, "p/Loud");
    let animal = class(&mut ctx, "p/Animal");
    let dog = class(&mut ctx, "p/Dog");
    ctx.set_interfaces(loud, vec![greeter]);
    ctx.set_interfaces(animal, vec![loud]);
    ctx.set_superclass(dog, animal);
    // A public default, a public abstract, a private and a public static.
    ctx.set_declared_methods(
        greeter,
        vec![
            method("greet", "(I)I", 0x0001, greeter),
            method("shout", "()V", 0x0401, greeter),
            method("helper", "(I)I", 0x0002, greeter),
            method("util", "(I)I", 0x0009, greeter),
        ],
    );
    let found = |cls: &str, name: &str, desc: &str| {
        lk_superinterface_declares_instance_method(&ctx, cls, name, desc)
    };
    // A default the class inherits, directly and through a superclass.
    assert!(found("p/Animal", "greet", "(I)I"));
    assert!(found("p/Dog", "greet", "(I)I"));
    // A superinterface's method, asked on an interface.
    assert!(found("p/Loud", "greet", "(I)I"));
    // An abstract declaration resolves too.
    assert!(found("p/Dog", "shout", "()V"));
    // The descriptor must match; private and static are not inherited.
    assert!(!found("p/Dog", "greet", "(J)I"));
    assert!(!found("p/Dog", "helper", "(I)I"));
    assert!(!found("p/Dog", "util", "(I)I"));
    // A class's own declarations are `method_exists`'s answer, not this.
    assert!(!found("p/Greeter", "greet", "(I)I"));
    assert!(!found("p/Missing", "greet", "(I)I"));

    // The mock's `method_exists` answers false for everything, so only the
    // superinterface step can admit.
    let require = |loaded: bool, name: &str, desc: &str, instance: bool| {
        let ref_kind = if instance {
            "invokeVirtual"
        } else {
            "invokeStatic"
        };
        lookup_require_method(&ctx, loaded, "p/Dog", name, desc, ref_kind, instance)
    };
    assert!(require(true, "greet", "(I)I", true).is_ok());
    assert!(require(true, "greet", "(I)I", false).is_err());
    assert!(require(true, "util", "(I)I", true).is_err());
    assert!(require(false, "nothing", "()V", true).is_ok());
}

#[test]
fn a_cyclic_interface_graph_terminates() {
    let mut ctx = MockNativeContext::new();
    let a = class(&mut ctx, "p/A");
    let b = class(&mut ctx, "p/B");
    let c = class(&mut ctx, "p/C");
    ctx.set_interfaces(a, vec![b]);
    ctx.set_interfaces(b, vec![a]);
    ctx.set_interfaces(c, vec![a]);
    ctx.set_superclass(c, c);
    assert!(!lk_superinterface_declares_instance_method(
        &ctx, "p/C", "m", "()V"
    ));
}

/// Interpreter round i3 wave 83: the holder `MethodHandleNatives.resolve`
/// writes for a method member (HotSpot's `init_method_MemberName`); the
/// end-to-end rows are `L4W83RevealInheritedMethods.java`.
#[test]
fn a_method_resolves_to_its_declaring_class() {
    use super::declaring_class_id;
    let mut ctx = MockNativeContext::new();
    let object = class(&mut ctx, "java/lang/Object");
    let base = class(&mut ctx, "p/Base");
    let sub = class(&mut ctx, "p/Sub");
    let leaf = class(&mut ctx, "p/Leaf");
    let super_i = class(&mut ctx, "p/SuperI");
    let sub_i = class(&mut ctx, "p/SubI");
    let d = class(&mut ctx, "p/D");
    let impl_ = class(&mut ctx, "p/Impl");
    ctx.set_superclass(base, object);
    ctx.set_superclass(sub, base);
    ctx.set_superclass(leaf, sub);
    ctx.set_superclass(impl_, object);
    for iface in [super_i, sub_i, d] {
        ctx.set_is_interface(iface, true);
    }
    ctx.set_interfaces(sub_i, vec![super_i]);
    ctx.set_interfaces(impl_, vec![d]);
    ctx.set_declared_methods(
        object,
        vec![
            method("hashCode", "()I", 0x0101, object),
            method("clone", "()Ljava/lang/Object;", 0x0104, object),
        ],
    );
    ctx.set_declared_methods(
        base,
        vec![
            method("s", "()I", 0x0009, base),
            method("m", "()I", 0x0001, base),
        ],
    );
    ctx.set_declared_methods(sub, vec![method("own", "()I", 0x0009, sub)]);
    ctx.set_declared_methods(
        super_i,
        vec![
            method("si", "()I", 0x0401, super_i),
            method("st", "()I", 0x0009, super_i),
        ],
    );
    ctx.set_declared_methods(d, vec![method("d", "()I", 0x0001, d)]);
    let holder =
        |start, name: &str, desc: &str| declaring_class_id(&ctx, start, name, desc, false);

    // Declared by the named class: no holder to write.
    assert_eq!(holder(sub, "own", "()I"), None);
    assert_eq!(holder(base, "m", "()I"), None);
    // Inherited from a superclass, one or two levels down; the descriptor must match.
    assert_eq!(holder(sub, "s", "()I"), Some(base));
    assert_eq!(holder(leaf, "m", "()I"), Some(base));
    assert_eq!(holder(leaf, "hashCode", "()I"), Some(object));
    assert_eq!(holder(leaf, "m", "()J"), None);
    // Through an interface: its superinterface's instance method, then
    // `Object`'s public ones -- not a static, nor a protected `clone`.
    assert_eq!(holder(sub_i, "si", "()I"), Some(super_i));
    assert_eq!(holder(sub_i, "st", "()I"), None);
    assert_eq!(holder(sub_i, "hashCode", "()I"), Some(object));
    assert_eq!(holder(sub_i, "clone", "()Ljava/lang/Object;"), None);
    // A default method a class inherits stays the class's.
    assert_eq!(holder(impl_, "d", "()I"), None);
}
}

#[cfg(test)]
mod r12w4_find_special_receiver_tests {
//! Round 12 wave 4 (hunter2): `findSpecial(refc, name, type, specialCaller)`
//! hands back a handle typed `(specialCaller, ...)`, and dispatches on
//! `specialCaller`'s direct superclass when `refc` is further up
//! (JVMS §6.5 `invokespecial`, `Lookup.getDirectMethodCommon`).
use super::{
    build_method_type_from_descriptor, descriptor_from_method_type, lookup_find_special,
    mh_read_class, special_dispatch_class,
};
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{MethodMetadata, NativeClassAccess, NativeHeapAccess};
use cratonvm_types::{ClassId, ObjectRef, Value};

fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
    ctx.ensure_class_initialized(name)
        .unwrap_or(ClassId::new(0))
}

/// `p/Greeter` (interface, default `m(I)I`) <- `p/Grand` <- `p/Mid` <-
/// `p/Sub`, plus an unrelated `p/Other`.
fn hierarchy(ctx: &mut MockNativeContext) -> [ClassId; 6] {
    let greeter = class(ctx, "p/Greeter");
    let grand = class(ctx, "p/Grand");
    let mid = class(ctx, "p/Mid");
    let sub = class(ctx, "p/Sub");
    let other = class(ctx, "p/Other");
    let sub_iface = class(ctx, "p/SubIface");
    ctx.set_is_interface(greeter, true);
    ctx.set_is_interface(sub_iface, true);
    ctx.set_declared_methods(
        greeter,
        vec![MethodMetadata {
            name: "m".to_string(),
            descriptor: "(I)I".to_string(),
            access_flags: 0x0001,
            declaring_class_id: greeter,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    ctx.set_interfaces(grand, vec![greeter]);
    ctx.set_superclass(mid, grand);
    ctx.set_superclass(sub, mid);
    [greeter, grand, mid, sub, other, sub_iface]
}

fn find_special(ctx: &mut MockNativeContext, refc: ClassId, caller: ClassId) -> ObjectRef {
    let refc = Value::Object(Some(ctx.get_class_mirror(refc)));
    let caller = Value::Object(Some(ctx.get_class_mirror(caller)));
    let name = Value::Object(Some(ctx.create_string("m")));
    let mt = match build_method_type_from_descriptor(ctx, "(I)I") {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType (I)I: {other:?}"),
    };
    // A null Lookup receiver skips the access gate.
    let args = [
        Value::Object(None),
        refc,
        name,
        Value::Object(Some(mt)),
        caller,
    ];
    match lookup_find_special(ctx, &args) {
        Ok(Some(Value::Object(Some(mh)))) => mh,
        other => panic!("findSpecial: {other:?}"),
    }
}

fn type_of(ctx: &MockNativeContext, mh: ObjectRef) -> String {
    match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => descriptor_from_method_type(ctx, mt),
        other => panic!("type field: {other:?}"),
    }
}

/// `R12HfixLookupInherited`: `findSpecial(Animal, greet, (int)int, Sub)`
/// was typed `(Animal,int)int`, so `invokeExact(sub, x)` threw
/// `WrongMethodTypeException`.
#[test]
fn the_receiver_type_is_the_special_caller() {
    let mut ctx = MockNativeContext::new();
    let [_, _, mid, sub, _, _] = hierarchy(&mut ctx);
    let mh = find_special(&mut ctx, mid, sub);
    assert_eq!(type_of(&ctx, mh), "(Lp/Sub;I)I");
    // `Mid` is `Sub`'s direct superclass: dispatch stays on it.
    assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Mid"));
}

/// `findSpecial(Grand, m, Sub)` runs what `super.m()` in `Sub` runs: the
/// search restarts at `Sub`'s direct superclass `Mid`.
#[test]
fn a_refc_above_the_direct_superclass_dispatches_on_the_direct_superclass() {
    let mut ctx = MockNativeContext::new();
    let [_, grand, _, sub, _, _] = hierarchy(&mut ctx);
    let mh = find_special(&mut ctx, grand, sub);
    assert_eq!(type_of(&ctx, mh), "(Lp/Sub;I)I");
    assert_eq!(mh_read_class(&ctx, mh).as_deref(), Some("p/Mid"));
}

/// `CRATONVM_MH_FIND_SPECIAL_JDK=0` restores the pre-wave-4 handle:
/// typed `(refc, ...)`, dispatching on `refc`.
#[test]
fn the_rebind_leaves_every_other_shape_alone() {
    let mut ctx = MockNativeContext::new();
    let [greeter, grand, mid, sub, other, sub_iface] = hierarchy(&mut ctx);
    let d = |ctx: &MockNativeContext, refc: &str, r: ClassId, c: Option<ClassId>| {
        special_dispatch_class(ctx, refc, Some(r), c, "m", "(I)I")
    };
    // The caller itself (a private method), and no caller at all.
    assert_eq!(d(&ctx, "p/Sub", sub, Some(sub)), "p/Sub");
    assert_eq!(d(&ctx, "p/Grand", grand, None), "p/Grand");
    // An interface refc (`I.super.m()`), and an interface caller.
    assert_eq!(d(&ctx, "p/Greeter", greeter, Some(sub)), "p/Greeter");
    assert_eq!(d(&ctx, "p/Grand", grand, Some(sub_iface)), "p/Grand");
    // An unrelated caller: nothing to restart from.
    assert_eq!(d(&ctx, "p/Grand", grand, Some(other)), "p/Grand");
    // Already the direct superclass.
    assert_eq!(d(&ctx, "p/Grand", grand, Some(mid)), "p/Grand");
    assert_eq!(d(&ctx, "p/Grand", grand, Some(sub)), "p/Mid");
    // A method `refc` declares private is not inherited: no restart.
    ctx.set_declared_methods(
        grand,
        vec![MethodMetadata {
            name: "m".to_string(),
            descriptor: "(I)I".to_string(),
            access_flags: 0x0002,
            declaring_class_id: grand,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    assert_eq!(d(&ctx, "p/Grand", grand, Some(sub)), "p/Grand");
}
}

#[cfg(test)]
mod r12w5_invoke_declared_cast_tests {
//! Round 12 wave 5 (hunter3): `MethodHandle.invoke` casts every reference
//! argument to the handle's DECLARED `type`, receiver included. A
//! `findSpecial(Mid, m, (int)int, Sub)` handle is typed `(Sub,int)int`, so
//! `invoke((Object) new Mid(), 3)` is a `ClassCastException` on HotSpot;
//! the leaf-only cast judged the receiver against `MH_CLASS` (`Mid`) and
//! let it through (`R12Hunt2FindSpecial`, the `cce` row).
use super::{
    alloc_method_handle, build_method_type_from_descriptor, declared_cast_params,
    declared_position_judged, invoke_exact_inner_cast_refusal, invoke_reference_cast_refusal,
    lookup_find_special, return_cast_refusal, MH_KIND_SPECIAL, MH_KIND_STATIC, MH_KIND_VIRTUAL,
};
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ArrayElementType, ClassId, ObjectRef, Value};

fn class(ctx: &mut MockNativeContext, name: &str) -> ClassId {
    ctx.ensure_class_initialized(name)
        .unwrap_or(ClassId::new(0))
}

/// `p/Greeter` (interface declaring `m(I)I`, which is what lets the mock's
/// method resolution find `m` at all) <- `p/Grand` <- `p/Mid` <- `p/Sub`;
/// returns `(mid, sub)`. The wave-4 tests' hierarchy, minus the extras.
fn hierarchy(ctx: &mut MockNativeContext) -> (ClassId, ClassId) {
    let greeter = class(ctx, "p/Greeter");
    let grand = class(ctx, "p/Grand");
    let mid = class(ctx, "p/Mid");
    let sub = class(ctx, "p/Sub");
    ctx.set_is_interface(greeter, true);
    ctx.set_declared_methods(
        greeter,
        vec![MethodMetadata {
            name: "m".to_string(),
            descriptor: "(I)I".to_string(),
            access_flags: 0x0001,
            declaring_class_id: greeter,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    ctx.set_interfaces(grand, vec![greeter]);
    ctx.set_superclass(mid, grand);
    ctx.set_superclass(sub, mid);
    (mid, sub)
}

/// `findSpecial(refc, "m", (int)int, caller)` through a null `Lookup`
/// (which skips the access gate), exactly as the wave-4 tests mint it.
fn find_special(ctx: &mut MockNativeContext, refc: ClassId, caller: ClassId) -> ObjectRef {
    let refc = Value::Object(Some(ctx.get_class_mirror(refc)));
    let caller = Value::Object(Some(ctx.get_class_mirror(caller)));
    let name = Value::Object(Some(ctx.create_string("m")));
    let mt = match build_method_type_from_descriptor(ctx, "(I)I") {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType (I)I: {other:?}"),
    };
    let args = [
        Value::Object(None),
        refc,
        name,
        Value::Object(Some(mt)),
        caller,
    ];
    match lookup_find_special(ctx, &args) {
        Ok(Some(Value::Object(Some(mh)))) => mh,
        other => panic!("findSpecial: {other:?}"),
    }
}

/// The mock resolves no `MethodType` field by name; declare the two
/// `methodtype_to_descriptor` reads, at the slots
/// `build_method_type_from_descriptor` writes (`rtype` 0, `ptypes` 1).
fn make_declared_type_readable(ctx: &mut MockNativeContext, mh: ObjectRef) {
    let mt = match ctx.get_field_by_name(mh, "type") {
        Value::Object(Some(mt)) => mt,
        other => panic!("type field: {other:?}"),
    };
    let cid = ctx.class_id_of_object(mt);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
}

/// The class a `ClassCastException` refusal names as its target.
fn cce_target(refusal: Option<MethodCallFailed>) -> Option<String> {
    match refusal {
        Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        ))) => message.rsplit(" to ").next().map(str::to_string),
        _ => None,
    }
}

/// The mock can only give a POSITIVE mismatch for an array (an array is an
/// `Object`, `Cloneable` and `Serializable` and nothing else), so the
/// receiver here is an array: what the test pins is WHICH type the refusal
/// names -- the declared `Sub`, not the leaf's `Mid`.
#[test]
fn invoke_casts_the_receiver_to_the_declared_special_caller() {
    let mut ctx = MockNativeContext::new();
    let (mid, sub) = hierarchy(&mut ctx);
    let mh = find_special(&mut ctx, mid, sub);
    make_declared_type_readable(&mut ctx, mh);
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let args = [Value::Object(Some(arr)), Value::Int(3)];
    let refusal =
        invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, true, false, &args, None);
    assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));

    // A `Sub` receiver passes both the declared and the leaf cast.
    let ok = ctx.alloc_object(sub, 1);
    let args = [Value::Object(Some(ok)), Value::Int(3)];
    let refusal =
        invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, true, false, &args, None);
    assert!(refusal.is_none());
}

/// With the declared pass off, the LEAF pass still refuses the `Mid`: the
/// handle recorded its `specialCaller` (`MH_SPECIAL_CALLER`).
/// `findSpecial(Mid, m, (int)int, Sub).asType((Object,int)int)` through
/// `invokeExact`: the copy's `type` names `Object`, the leaf still `Sub`.
#[test]
fn invoke_exact_casts_an_astype_copy_of_a_special_handle_to_the_special_caller() {
    let mut ctx = MockNativeContext::new();
    let (mid, sub) = hierarchy(&mut ctx);
    let mh = find_special(&mut ctx, mid, sub);
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let args = [Value::Object(Some(arr)), Value::Int(3)];
    let widened = Some("(Ljava/lang/Object;I)I");
    let refusal =
        invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, widened, &args);
    assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));
    // The handle itself, at its own exact type: not judged.
    let exact = Some("(Lp/Sub;I)I");
    let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_SPECIAL, exact, &args);
    assert!(refusal.is_none());
}

/// `CRATONVM_MH_INVOKE_DECLARED_CAST=0` with
/// `CRATONVM_MH_SPECIAL_CALLER_CAST=0` restores the wave-4 cast, which
/// judges the receiver against `MH_CLASS`.
/// `findVirtual(int[].class, "clone", ..)` is typed `([I)Object` on
/// HotSpot. The leaf check wrapped the array class name as `L[I;`, read it
/// back as a class named `[I`, and refused every array receiver.
#[test]
fn an_array_class_receiver_is_not_refused() {
    let mut ctx = MockNativeContext::new();
    let mh = handle(&mut ctx, "[I", "()Ljava/lang/Object;", MH_KIND_VIRTUAL);
    let arr = ctx.new_array(ArrayElementType::Int, 2);
    let args = [Value::Object(Some(arr))];
    let refusal =
        invoke_reference_cast_refusal(&mut ctx, mh, MH_KIND_VIRTUAL, true, false, &args, None);
    assert!(refusal.is_none(), "{:?}", cce_target(refusal));
}

fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    }
}

/// `findStatic(C, takesSub, (Sub)int).asType((Object)int).invokeExact(x)`:
/// the adapter's cast from `Object` to `Sub` is owed by `invokeExact`.
#[test]
fn invoke_exact_casts_an_astype_copy_to_the_leaf_parameter() {
    let mut ctx = MockNativeContext::new();
    let _ = hierarchy(&mut ctx);
    let mh = handle(&mut ctx, "p/C", "(Lp/Sub;)I", MH_KIND_STATIC);
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let args = [Value::Object(Some(arr))];
    let widened = Some("(Ljava/lang/Object;)I");
    let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, widened, &args);
    assert_eq!(cce_target(refusal).as_deref(), Some("p.Sub"));
    // An unadapted handle is never judged: `invokeExact`'s own call site
    // already typed the argument.
    let exact = Some("(Lp/Sub;)I");
    let refusal = invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, exact, &args);
    assert!(refusal.is_none());
    // Unknown declared type, or another arity: not judged.
    assert!(
        invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, None, &args).is_none()
    );
    let two = Some("(Ljava/lang/Object;Ljava/lang/Object;)I");
    assert!(
        invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_STATIC, two, &args).is_none()
    );
}

/// A virtual handle's leaf receiver is `MH_CLASS`; an array class names
/// its own descriptor.
#[test]
fn invoke_exact_casts_a_widened_virtual_receiver() {
    let mut ctx = MockNativeContext::new();
    let _ = hierarchy(&mut ctx);
    let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    let arr = ctx.new_array(ArrayElementType::Reference, 1);
    let args = [Value::Object(Some(arr)), Value::Int(3)];
    let widened = Some("(Ljava/lang/Object;I)I");
    let refusal =
        invoke_exact_inner_cast_refusal(&mut ctx, mh, MH_KIND_VIRTUAL, widened, &args);
    assert_eq!(cce_target(refusal).as_deref(), Some("p.Mid"));

    let clone = handle(&mut ctx, "[I", "()Ljava/lang/Object;", MH_KIND_VIRTUAL);
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    let args = [Value::Object(Some(ints))];
    let widened = Some("(Ljava/lang/Object;)Ljava/lang/Object;");
    let refusal =
        invoke_exact_inner_cast_refusal(&mut ctx, clone, MH_KIND_VIRTUAL, widened, &args);
    assert!(refusal.is_none());
}

/// `CRATONVM_MH_INVOKEEXACT_INNER_CAST=0` restores the uncast dispatch.
/// `(String) boxer.invoke(x)` / `boxer.asType((int)String).invokeExact(x)`
/// on a `(int)Object` handle: the reference RETURN is cast where the JDK's
/// adapter casts it. An array value is the mock's positive mismatch.
#[test]
fn only_an_aligned_declared_type_is_judged() {
    assert_eq!(
        declared_cast_params("(Lp/Sub;I)I", 2),
        Some(vec!["Lp/Sub;".to_string(), "I".to_string()])
    );
    // A collector (or a spread) presents another arity: not judged.
    assert_eq!(declared_cast_params("(Lp/Sub;I)I", 3), None);
    // The fabricated `()V` stand-in, and nothing to cast.
    assert_eq!(declared_cast_params("()V", 1), None);
    assert_eq!(declared_cast_params("()V", 0), None);
    assert_eq!(declared_cast_params("not a descriptor", 1), None);
}

#[test]
fn the_leaf_pass_skips_only_an_identical_target() {
    let declared = vec!["Lp/Sub;".to_string(), "I".to_string()];
    let d = Some(declared.as_slice());
    assert!(declared_position_judged(d, 0, "Lp/Sub;"));
    assert!(!declared_position_judged(d, 0, "Lp/Mid;"));
    assert!(declared_position_judged(d, 1, "I"));
    assert!(!declared_position_judged(d, 2, "I"));
    assert!(!declared_position_judged(None, 0, "Lp/Sub;"));
}
}

/// Interpreter round i1 wave 26, lane L4: the leaf arms' fallible argument
/// conversion (`adapt_leaf_args`) and the placeholders the adapters pass
/// (`mh_zero_value`, `loop_variable_shape`). The end-to-end rows are
/// `L4W26AdapterLeafConversions.java`.
#[cfg(test)]
mod w26_leaf_conversion_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn wrapper(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

fn is_cce(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        )))
    )
}

fn is_npe(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { .. }
        )))
    )
}

/// The page's rows at the leaf: a `String`, a non-widening `Long` and a
/// `null` at an `int` parameter are refused; a `Short` converts. Every
/// pin taken is released on both outcomes.
#[test]
fn a_reference_at_a_primitive_parameter_converts_or_is_refused() {
    let mut ctx = MockNativeContext::new();
    let s = wrapper(&mut ctx, "java/lang/String", Value::Int(0));
    let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(2));
    let short = wrapper(&mut ctx, "java/lang/Short", Value::Int(2));
    let one = Value::Int(1);
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[one, s],
        "(II)I",
        None,
        false
    )));
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[one, long],
        "(II)I",
        None,
        false
    )));
    assert!(is_npe(&adapt_leaf_args(
        &mut ctx,
        &[one, Value::Object(None)],
        "(II)I",
        None,
        false
    )));
    assert_eq!(
        adapt_leaf_args(&mut ctx, &[one, short], "(II)I", None, false).unwrap(),
        vec![one, Value::Int(2)]
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// What `adapt_single_arg` did and still does: a widening wrapper is
/// unboxed AND widened, a raw primitive is widened, a reference parameter
/// keeps whatever it is given (a `null` included).
#[test]
fn the_accepted_shapes_are_unchanged() {
    let mut ctx = MockNativeContext::new();
    let int = wrapper(&mut ctx, "java/lang/Integer", Value::Int(7));
    let chr = wrapper(&mut ctx, "java/lang/Character", Value::Int(0x41));
    let s = wrapper(&mut ctx, "java/lang/String", Value::Int(0));
    let out = adapt_leaf_args(
        &mut ctx,
        &[int, Value::Int(3), chr, s, Value::Object(None)],
        "(JDILjava/lang/Object;Ljava/lang/String;)V",
        None,
        false,
    )
    .unwrap();
    assert_eq!(
        out,
        vec![
            Value::Long(7),
            Value::Double(3.0),
            Value::Int(0x41),
            s,
            Value::Object(None)
        ]
    );
}

/// A site that typed the argument as a primitive passed it raw; a
/// reference there is the VM's own and keeps the plain unbox.
#[test]
fn a_primitive_site_keeps_the_plain_unbox() {
    let mut ctx = MockNativeContext::new();
    let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(2));
    let site = vec!["I".to_string()];
    assert_eq!(
        adapt_leaf_args(&mut ctx, &[long], "(I)V", Some(&site), false).unwrap(),
        vec![Value::Long(2)]
    );
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[long],
        "(I)V",
        None,
        false
    )));
}

/// Under an `explicitCastArguments` stamp nothing is refused: a `null` is
/// the zero, a wrapper is unboxed and cast (narrowing included), a raw
/// value is cast, `boolean` takes the low-order bit.
#[test]
fn an_explicit_cast_scope_casts_instead_of_refusing() {
    let mut ctx = MockNativeContext::new();
    let long = wrapper(&mut ctx, "java/lang/Long", Value::Long(0x1_0000_0005));
    let out = adapt_leaf_args(
        &mut ctx,
        &[
            Value::Object(None),
            long,
            Value::Int(300),
            Value::Double(2.9),
            Value::Int(6),
        ],
        "(IIBIZ)V",
        None,
        true,
    )
    .unwrap();
    assert_eq!(
        out,
        vec![
            Value::Int(0),
            Value::Int(5),
            Value::Int(44),
            Value::Int(2),
            Value::Int(0)
        ]
    );
    assert_eq!(explicit_cast_primitive(Value::Int(7), "J"), Value::Long(7));
    assert_eq!(
        explicit_cast_primitive(Value::Int(-1), "C"),
        Value::Int(0xFFFF)
    );
}

/// The marking bits share `MH_VARARGS`: setting one keeps the other.
#[test]
fn the_explicit_cast_mark_and_the_varargs_mark_are_independent() {
    let mut ctx = MockNativeContext::new();
    let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
    assert!(!mh_is_explicit_cast(&ctx, h));
    mh_set_varargs_collector(&ctx, h, true);
    mh_mark_explicit_cast(&ctx, h);
    assert!(mh_is_varargs_collector(&ctx, h));
    assert!(mh_is_explicit_cast(&ctx, h));
    mh_set_varargs_collector(&ctx, h, false);
    assert!(!mh_is_varargs_collector(&ctx, h));
    assert!(
        mh_is_explicit_cast(&ctx, h),
        "clearing varargs keeps the cast mark"
    );
}

/// `MH_MARKS` is the one table of the marking bits: distinct powers of
/// two, and every `MH_MARK_*` constant of the file is a row (wave 51,
/// lane L4; the exact-invoker / cast-chain collision of wave 50).
#[test]
fn the_marking_bits_are_one_table() {
    let mut seen = 0i32;
    for (name, bit, _) in MH_MARKS {
        assert!(
            bit > 0 && bit.count_ones() == 1,
            "{name}: {bit} is not one bit"
        );
        assert_eq!(seen & bit, 0, "{name}: bit {bit} collides");
        seen |= bit;
    }
    assert_eq!(MH_MARKS_DIVERT_DIRECT_LANE & !seen, 0);
    assert_eq!(MH_MARKS_INHERITED & !seen, 0);
    // Every `MH_MARK_*` constant declared in this file is in the table.
    let src = LANG_INVOKE_SRC;
    let decl = concat!("const ", "MH_MARK_");
    let declared = src.matches(decl).count();
    assert_eq!(
        declared,
        MH_MARKS.len(),
        "a MH_MARK_* constant without an MH_MARKS row"
    );
}

/// No `MH_VARARGS` store outside `mh_write_marking_bits` (a source scan,
/// whitespace-insensitive, as the other ratchets do).
#[test]
fn the_marking_bits_have_one_writer() {
    let src: String = LANG_INVOKE_SRC
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let store = concat!("MH_VARARGS", ",Value::");
    assert_eq!(
        src.matches(store).count(),
        1,
        "a second writer of MH_VARARGS"
    );
}

/// A copy keeps every marking bit `MH_MARKS` says it inherits.
#[test]
fn a_copy_keeps_the_inherited_marking_bits() {
    let mut ctx = MockNativeContext::new();
    let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
    mh_set_varargs_collector(&ctx, h, true);
    mh_mark_explicit_cast(&ctx, h);
    mh_mark_exact_invoker(&ctx, h);
    let before = mh_marking_bits(&ctx, h);
    let copy = mh_clone_handle(&mut ctx, h).expect("a synthetic handle copies");
    assert_eq!(mh_marking_bits(&ctx, copy), before & MH_MARKS_INHERITED);
}

/// A bound value leads the list the site does not name.
#[test]
fn site_params_align_past_a_bound_value() {
    assert_eq!(
        leaf_site_params(Some("(Ljava/lang/Object;I)V"), 1, 3),
        Some(vec![
            String::new(),
            "Ljava/lang/Object;".to_string(),
            "I".to_string()
        ])
    );
    assert_eq!(
        leaf_site_params(Some("(I)V"), 0, 2),
        None,
        "a reshaped list has no site"
    );
    assert_eq!(leaf_site_params(None, 0, 0), None);
}

/// `tryFinally`'s result slot and a loop variable with no `init`.
#[test]
fn placeholders_are_the_primitive_zero() {
    assert_eq!(mh_zero_value("I"), Value::Int(0));
    assert_eq!(mh_zero_value("J"), Value::Long(0));
    assert_eq!(mh_zero_value("D"), Value::Double(0.0));
    assert_eq!(mh_zero_value("Ljava/lang/String;"), Value::Object(None));
    assert_eq!(mh_zero_value(""), Value::Object(None));

    let mut ctx = MockNativeContext::new();
    let mut body = |desc: &str| {
        let h = alloc_method_handle(&mut ctx, "C", "b", desc, MH_KIND_STATIC).unwrap();
        // No `type`: the shape is read from `MH_DESC`.
        ctx.set_field_by_name(h, "type", Value::Object(None));
        loop_variable_shape(&mut ctx, Some(h))
    };
    assert_eq!(body("(II)I"), (false, Some(Value::Int(0))));
    assert_eq!(body("(I)V"), (true, None));
    assert_eq!(
        body("(Ljava/lang/String;I)Ljava/lang/String;"),
        (false, None)
    );
}
}

/// Wave 27, lane L4: `asType`'s reference -> reference cast at a leaf
/// ([`leaf_reference_refusal`], [`leaf_receiver_refusal`]). The mock context
/// has no by-name hierarchy walk (`class_assignable_to_name` answers `None`),
/// so the only refusals it can produce are the ones the SHAPE settles: an
/// array where a class is wanted, a non-array or a primitive array where
/// another array type is wanted. Everything else must fail open. The
/// hierarchy rows are `L4W27AdapterLeafReferenceCasts.java`.
#[cfg(test)]
mod w27_leaf_reference_cast_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

fn object(ctx: &mut MockNativeContext, class: &str) -> ObjectRef {
    match ctx.new_object(class).unwrap() {
        Some(Value::Object(Some(o))) => o,
        _ => unreachable!(),
    }
}

fn is_cce(r: &Result<Vec<Value>, MethodCallFailed>) -> bool {
    matches!(
        r,
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { .. }
        )))
    )
}

#[test]
fn shapes_the_hierarchy_cannot_contradict_are_refused() {
    let mut ctx = MockNativeContext::new();
    let s = Value::Object(Some(object(&mut ctx, "java/lang/String")));
    let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let refs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 1)));
    // A non-array at an array parameter.
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[s],
        "([Ljava/lang/String;)V",
        None,
        false
    )));
    // A primitive array at a reference-array parameter, and at a class.
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[ints],
        "([Ljava/lang/Object;)V",
        None,
        false
    )));
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[ints],
        "(Ljava/lang/String;)V",
        None,
        false
    )));
    // A reference array at `int[]`, after a converted primitive.
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[Value::Int(1), refs],
        "(I[I)V",
        None,
        false
    )));
    // An explicit cast to a CLASS still casts.
    assert!(is_cce(&adapt_leaf_args(
        &mut ctx,
        &[ints],
        "(Ljava/lang/String;)V",
        None,
        true
    )));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

#[test]
fn what_the_cast_cannot_contradict_passes() {
    let mut ctx = MockNativeContext::new();
    let s = Value::Object(Some(object(&mut ctx, "java/lang/String")));
    let ints = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let refs = Value::Object(Some(ctx.new_array(ArrayElementType::Reference, 1)));
    let null = Value::Object(None);
    let out = adapt_leaf_args(
        &mut ctx,
        &[s, null, s, ints, refs, ints, refs],
        "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/Object;[I[Ljava/lang/String;Ljava/lang/Cloneable;[Ljava/lang/Object;)V",
        None,
        false,
    )
    .unwrap();
    assert_eq!(out, vec![s, null, s, ints, refs, ints, refs]);
    // A list that does not line up 1:1 is not judged (the varargs repair's
    // flat lists): the `String` at the array position passes.
    assert_eq!(
        adapt_leaf_args(&mut ctx, &[s, s], "([Ljava/lang/String;)V", None, false).unwrap(),
        vec![s, s]
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

#[test]
fn a_receiver_is_judged_against_its_class() {
    let mut ctx = MockNativeContext::new();
    let s = object(&mut ctx, "java/lang/String");
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    assert!(leaf_receiver_refusal(&mut ctx, s, "java/lang/String", false).is_none());
    assert!(leaf_receiver_refusal(&mut ctx, ints, "java/lang/String", false).is_some());
    assert!(leaf_receiver_refusal(&mut ctx, ints, "[I", false).is_none());
    assert!(leaf_receiver_refusal(&mut ctx, s, "[I", false).is_some());
    // No class to judge against, or `Object`: never refused.
    assert!(leaf_receiver_refusal(&mut ctx, ints, "", false).is_none());
    assert!(leaf_receiver_refusal(&mut ctx, ints, "java/lang/Object", false).is_none());
}

/// Interpreter round i2 wave 62, lane L4 (`CRATONVM_MH_INSTANCE_OWNER_ID`):
/// the owner's id admits its own class; a nameless owner (a lambda
/// proxy's) refuses another class itself; a named owner leaves a mismatch
/// to the by-name cast, so the switch moves no named-class answer.
#[test]
fn an_owner_id_judges_the_receiver() {
    let mut ctx = MockNativeContext::new();
    let s = object(&mut ctx, "java/lang/String");
    let string = ctx.class_id_of_object(s);
    let ints = ctx.new_array(ArrayElementType::Int, 1);
    let i = object(&mut ctx, "java/lang/Integer");
    assert!(matches!(
        owner_receiver_verdict(&mut ctx, s, string, "java/lang/String"),
        Some(None)
    ));
    assert!(owner_receiver_verdict(&mut ctx, i, string, "java/lang/String").is_none());
    assert!(owner_receiver_verdict(&mut ctx, ints, string, "java/lang/String").is_none());
    assert!(matches!(
        owner_receiver_verdict(&mut ctx, i, string, ""),
        Some(Some(_))
    ));
    assert!(matches!(
        owner_receiver_verdict(&mut ctx, s, string, ""),
        Some(None)
    ));
}

/// An invoker's own type (HotSpot 25.0.3, `L4W27InvokerTypes.java`): the
/// target handle leads, and a spread invoker's trailing parameters are
/// one `Object[]`.
#[test]
fn an_invokers_type_leads_with_the_target_handle() {
    let mh = "Ljava/lang/invoke/MethodHandle;";
    assert_eq!(
        invoker_type_descriptor("([Ljava/lang/Object;)Ljava/lang/Object;", None),
        Some(format!("({mh}[Ljava/lang/Object;)Ljava/lang/Object;"))
    );
    assert_eq!(
        invoker_type_descriptor("(Ljava/lang/String;I)Ljava/lang/String;", Some(0)),
        Some(format!("({mh}[Ljava/lang/Object;)Ljava/lang/String;"))
    );
    assert_eq!(
        invoker_type_descriptor(
            "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
            Some(1)
        ),
        Some(format!(
            "({mh}Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;"
        ))
    );
    assert_eq!(
        invoker_type_descriptor("(II)I", Some(2)),
        Some(format!("({mh}II[Ljava/lang/Object;)I"))
    );
    assert_eq!(
        invoker_type_descriptor("()V", None),
        Some(format!("({mh})V"))
    );
    assert_eq!(
        invoker_type_descriptor("(I)V", Some(2)),
        None,
        "lead past the arity"
    );
    assert_eq!(invoker_type_descriptor("(I)V", Some(-1)), None);
    assert_eq!(invoker_type_descriptor("", None), None);
}
}

/// Interpreter round i2 wave 63, lane L4: stage 2 of the owner-by-id proposal
/// (`CRATONVM_MH_OWNER_ID_AUTHORITY`). The helpers are flag-free or take the
/// switch as a parameter, so both answers are asked here; the end-to-end rows
/// are `L4W63ForeignOwnerCast.java`.
#[cfg(test)]
mod i63_l4_owner_id_authority_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// `p/Base` of loader 3, a second `p/Base` of loader 4 and its subclass
/// `p/Sub` (loader 4), a third `p/Base` whose loader is unknown.
struct Namesakes {
    base_a: ClassId,
    base_b: ClassId,
    sub_b: ClassId,
    base_unknown: ClassId,
}

fn namesakes(ctx: &mut MockNativeContext) -> Namesakes {
    let base_a = ctx.ensure_class_initialized("p/Base").expect("p/Base");
    let base_b = ctx.declare_second_copy("p/Base");
    let sub_b = ctx.declare_second_copy("p/Sub");
    let base_unknown = ctx.declare_second_copy("p/Base");
    ctx.set_superclass(sub_b, base_b);
    ctx.set_loader_id_override(base_a, 3);
    ctx.set_loader_id_override(base_b, 4);
    ctx.set_loader_id_override(sub_b, 4);
    Namesakes {
        base_a,
        base_b,
        sub_b,
        base_unknown,
    }
}

fn cce_message(r: Option<Option<MethodCallFailed>>) -> Option<String> {
    match r {
        Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message },
        )))) => Some(message),
        _ => None,
    }
}

#[test]
fn another_loaders_namesake_is_foreign() {
    let mut ctx = MockNativeContext::new();
    let n = namesakes(&mut ctx);
    assert!(owner_foreign_namesake(&ctx, n.base_b, n.base_a, "p/Base"));
    assert!(
        owner_foreign_namesake(&ctx, n.sub_b, n.base_a, "p/Base"),
        "a namesake's subclass"
    );
    assert!(
        !owner_foreign_namesake(&ctx, n.base_a, n.base_a, "p/Base"),
        "the owner itself"
    );
    // A loader the VM does not know (a stand-in) keeps the by-name answer.
    assert!(!owner_foreign_namesake(
        &ctx,
        n.base_unknown,
        n.base_a,
        "p/Base"
    ));
    assert!(!owner_foreign_namesake(
        &ctx,
        n.base_b,
        n.base_unknown,
        "p/Base"
    ));
    // Another name, no name, an array class: never.
    let int = ctx
        .ensure_class_initialized("java/lang/Integer")
        .expect("Integer");
    ctx.set_loader_id_override(int, 0);
    assert!(!owner_foreign_namesake(&ctx, int, n.base_a, "p/Base"));
    assert!(!owner_foreign_namesake(&ctx, n.base_b, n.base_a, ""));
    assert!(!owner_foreign_namesake(
        &ctx,
        n.base_b,
        n.base_a,
        "[Lp/Base;"
    ));
    // The same loader's second copy is not judged foreign.
    ctx.set_loader_id_override(n.base_unknown, 3);
    assert!(!owner_foreign_namesake(
        &ctx,
        n.base_unknown,
        n.base_a,
        "p/Base"
    ));
}

#[test]
fn the_authority_refuses_a_namesake_receiver_with_hotspots_message() {
    let mut ctx = MockNativeContext::new();
    let n = namesakes(&mut ctx);
    let a = ctx.alloc_object(n.base_a, 2);
    let b = ctx.alloc_object(n.base_b, 2);
    let sb = ctx.alloc_object(n.sub_b, 2);
    // The owner's own instance is admitted by id, either way.
    assert!(matches!(
        owner_receiver_verdict_with(&mut ctx, a, n.base_a, "p/Base", true),
        Some(None)
    ));
    assert!(matches!(
        owner_receiver_verdict_with(&mut ctx, a, n.base_a, "p/Base", false),
        Some(None)
    ));
    // Off: a named owner's mismatch is left to the by-name cast (stage 1).
    assert!(owner_receiver_verdict_with(&mut ctx, b, n.base_a, "p/Base", false).is_none());
    assert!(owner_receiver_verdict_with(&mut ctx, sb, n.base_a, "p/Base", false).is_none());
    // On: refused, named as HotSpot's `Class.cast` names them.
    assert_eq!(
        cce_message(owner_receiver_verdict_with(
            &mut ctx, b, n.base_a, "p/Base", true
        ))
        .as_deref(),
        Some("Cannot cast p.Base to p.Base")
    );
    assert_eq!(
        cce_message(owner_receiver_verdict_with(
            &mut ctx, sb, n.base_a, "p/Base", true
        ))
        .as_deref(),
        Some("Cannot cast p.Sub to p.Base")
    );
    // An unrelated class is still the by-name cast's to judge.
    let s = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("String");
    let str_obj = ctx.alloc_object(s, 1);
    assert!(owner_receiver_verdict_with(&mut ctx, str_obj, n.base_a, "p/Base", true).is_none());
}

#[test]
fn a_field_slot_resolves_in_the_owner_by_id() {
    let mut ctx = MockNativeContext::new();
    let field = |cid: ClassId, slot_index: usize| FieldMetadata {
        name: "cellValueZ".to_string(),
        descriptor: "I".to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    };
    let by_name = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    ctx.set_declared_fields(by_name, vec![field(by_name, 3)]);
    let second = ctx.declare_second_copy("p/Cell");
    ctx.set_declared_fields(second, vec![field(second, 1)]);
    // The owner's own layout, not whichever copy the name finds.
    assert_eq!(
        mh_instance_field_slot(&ctx, Some(second), "p/Cell", "cellValueZ"),
        Some(1)
    );
    assert_eq!(
        mh_instance_field_slot(&ctx, Some(by_name), "p/Cell", "cellValueZ"),
        Some(3)
    );
    // No owner: the by-name lookup, as before stage 2.
    assert_eq!(
        mh_instance_field_slot(&ctx, None, "p/Cell", "cellValueZ"),
        Some(3)
    );
    // An owner that does not declare it falls back to the name.
    let other = ctx.declare_second_copy("p/Other");
    assert_eq!(
        mh_instance_field_slot(&ctx, Some(other), "p/Cell", "cellValueZ"),
        Some(3)
    );
}
}

/// Interpreter round i2 wave 64, lane L4: an instance field handle carries its
/// slot from mint (`CRATONVM_MH_FIELD_SLOT_FROM_MINT`). The helpers take the
/// switch as a parameter; the end-to-end rows are
/// `L4W64FieldSlotFromMint.java`.
#[cfg(test)]
mod i64_l4_field_slot_from_mint_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn field(cid: ClassId, slot_index: usize) -> FieldMetadata {
    FieldMetadata {
        name: "cellValueZ".to_string(),
        descriptor: "I".to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    }
}

fn getter(ctx: &mut MockNativeContext) -> ObjectRef {
    // The descriptor is irrelevant to the slot helpers (the mock builds no
    // `MethodType` of an unknown class).
    alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap()
}

#[test]
fn the_packed_state_round_trips_and_never_reads_as_linked() {
    for slot in [0usize, 1, 7, 0xFFFF] {
        let state = field_link_minted_state(slot).expect("fits");
        assert_eq!(state & FL_LINKED, 0, "a minted slot is not a link");
        assert_eq!(field_link_state_minted_slot(state), Some(slot));
        assert_eq!((state >> FL_LINKS_SHIFT) & 0x7, 0, "no relink counted");
    }
    assert_eq!(field_link_minted_state(0x1_0000), None);
    assert_eq!(field_link_state_minted_slot(0), None);
    // A link without a minted slot carries none.
    let linked = FL_LINKED | (5 << 16) | 3 | (1 << FL_LINKS_SHIFT);
    assert_eq!(field_link_state_minted_slot(linked), None);
}

#[test]
fn a_link_keeps_the_minted_bit_only_for_the_same_slot() {
    let old = field_link_minted_state(3).unwrap();
    let linked = FL_LINKED | (5 << 16) | 3 | (1 << FL_LINKS_SHIFT);
    let kept = field_link_state_keeping_mint(linked, old, 3);
    assert_eq!(field_link_state_minted_slot(kept), Some(3));
    assert_ne!(kept & FL_LINKED, 0);
    assert_eq!((kept >> 16) & 0xFF, 5, "type index intact");
    assert_eq!((kept >> FL_LINKS_SHIFT) & 0x7, 1, "relink count intact");
    // Another slot, or no minted slot: the link alone.
    assert_eq!(field_link_state_keeping_mint(linked, old, 4), linked);
    assert_eq!(field_link_state_keeping_mint(linked, 0, 3), linked);
}

#[test]
fn a_copy_keeps_the_minted_slot_and_drops_the_link() {
    let minted_linked =
        FL_MINT_SLOT | FL_LINKED | FL_SETTER | (5 << 16) | 3 | (2 << FL_LINKS_SHIFT);
    let kept = field_link_state_kept_by_copy(minted_linked);
    assert_eq!(kept, field_link_minted_state(3).unwrap());
    assert_eq!(kept & FL_LINKED, 0);
    let plain_linked = FL_LINKED | (5 << 16) | 3 | (2 << FL_LINKS_SHIFT);
    assert_eq!(field_link_state_kept_by_copy(plain_linked), 0);
    assert_eq!(field_link_state_kept_by_copy(0), 0);
}

#[test]
fn a_mint_records_the_owners_slot_only_when_on() {
    let mut ctx = MockNativeContext::new();
    let by_name = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    ctx.set_declared_fields(by_name, vec![field(by_name, 3)]);
    let second = ctx.declare_second_copy("p/Cell");
    ctx.set_declared_fields(second, vec![field(second, 1)]);

    // Off: nothing recorded, the link slot stays `0`.
    let mh = getter(&mut ctx);
    mh_record_minted_field_slot_with(&ctx, mh, Some(second), "cellValueZ", false);
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(0)));
    assert_eq!(mh_minted_field_slot_with(&ctx, mh, true), None);

    // On: the OWNER's slot (the second copy's), not the name's.
    mh_record_minted_field_slot_with(&ctx, mh, Some(second), "cellValueZ", true);
    assert_eq!(mh_minted_field_slot_with(&ctx, mh, true), Some(1));
    // The reader asks the switch too.
    assert_eq!(mh_minted_field_slot_with(&ctx, mh, false), None);

    // No owner, or an owner that does not declare the name: nothing.
    let mh2 = getter(&mut ctx);
    mh_record_minted_field_slot_with(&ctx, mh2, None, "cellValueZ", true);
    assert_eq!(mh_minted_field_slot_with(&ctx, mh2, true), None);
    let other = ctx.declare_second_copy("p/Other");
    mh_record_minted_field_slot_with(&ctx, mh2, Some(other), "cellValueZ", true);
    assert_eq!(mh_minted_field_slot_with(&ctx, mh2, true), None);
}

#[test]
fn a_narrow_handle_records_and_reads_nothing() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    ctx.set_declared_fields(cell, vec![field(cell, 2)]);
    let narrow = ctx.alloc_object(cell, MH_SLOT_COUNT - 1);
    mh_record_minted_field_slot_with(&ctx, narrow, Some(cell), "cellValueZ", true);
    assert_eq!(mh_minted_field_slot_with(&ctx, narrow, true), None);
}

#[test]
fn a_clone_keeps_the_minted_slot() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    ctx.set_declared_fields(cell, vec![field(cell, 2)]);
    let mh = getter(&mut ctx);
    mh_record_minted_field_slot_with(&ctx, mh, Some(cell), "cellValueZ", true);
    let copy = mh_clone_handle(&mut ctx, mh).expect("copy");
    assert_eq!(mh_minted_field_slot_with(&ctx, copy, true), Some(2));
    // A handle with no minted slot copies to `0`, as before.
    let plain = getter(&mut ctx);
    let copy = mh_clone_handle(&mut ctx, plain).expect("copy");
    assert!(matches!(ctx.get_field(copy, MH_FIELD_LINK), Value::Int(0)));
}
}

/// Interpreter round i2 wave 65, lane L4: a bound field handle keeps its owner
/// and minted slot (`CRATONVM_MH_BOUND_FIELD_OWNER`), and an owned
/// field-accessor link admits the owner's subclasses
/// (`CRATONVM_MH_FIELD_LINK_OWNER_ADMITS`). The helpers take the switch (and
/// the owner) as parameters; the end-to-end rows are
/// `L4W65BoundFieldOwner.java` and
/// `L4W65FieldLinkSubclasses.java`.
#[cfg(test)]
mod i65_l4_bound_field_and_owner_link_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn field(cid: ClassId, slot_index: usize) -> FieldMetadata {
    FieldMetadata {
        name: "cellValueZ".to_string(),
        descriptor: "I".to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: cid,
        is_static: false,
    }
}

/// A handle of `kind` (the descriptor the carry judges is passed to it
/// separately: the mock builds no `MethodType` of an unknown class).
fn handle(ctx: &mut MockNativeContext, kind: i32) -> ObjectRef {
    alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap()
}

#[test]
fn the_carry_is_an_unbound_instance_field_handles_owner_and_minted_slot() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    ctx.set_declared_fields(cell, vec![field(cell, 2)]);
    let getter = handle(&mut ctx, MH_KIND_GETTER);
    let g = "(Lp/Cell;)I";

    // Off, or no recorded owner: nothing.
    assert_eq!(
        bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), false),
        None
    );
    assert_eq!(
        bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, None, true),
        None
    );

    // The owner alone: the narrow bound carrier is enough.
    let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
        .expect("owner carried");
    assert_eq!(
        carry,
        BoundFieldCarry {
            owner: cell,
            link_state: 0
        }
    );
    assert!(!carry.needs_link_slots());

    // A minted slot rides too, and asks for the link slots.
    mh_record_minted_field_slot_with(&ctx, getter, Some(cell), "cellValueZ", true);
    let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
        .expect("owner and slot carried");
    assert_eq!(carry.link_state, field_link_minted_state(2).unwrap());
    assert!(carry.needs_link_slots());

    // A LINKED source hands over the minted slot only, never its link.
    let linked = field_link_minted_state(2).unwrap()
        | FL_LINKED
        | FL_OWNER_ADMITS
        | (3 << 16)
        | (2 << FL_LINKS_SHIFT);
    ctx.set_field(getter, MH_FIELD_LINK, Value::Int(linked));
    let carry = bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, g, Some(cell), true)
        .expect("carried");
    assert_eq!(carry.link_state, field_link_minted_state(2).unwrap());

    // An instance setter carries; the static shapes and the other kinds
    // do not.
    let setter = handle(&mut ctx, MH_KIND_SETTER);
    assert!(bound_field_carry_with(
        &ctx,
        setter,
        MH_KIND_SETTER,
        "(Lp/Cell;I)V",
        Some(cell),
        true
    )
    .is_some());
    assert_eq!(
        bound_field_carry_with(&ctx, setter, MH_KIND_SETTER, "(I)V", Some(cell), true),
        None
    );
    assert_eq!(
        bound_field_carry_with(&ctx, getter, MH_KIND_GETTER, "()I", Some(cell), true),
        None
    );
    // Classless (the mock builds no receiver-typed `MethodType`).
    let virt = alloc_method_handle(&mut ctx, "", "m", "()I", MH_KIND_VIRTUAL).unwrap();
    assert_eq!(
        bound_field_carry_with(&ctx, virt, MH_KIND_VIRTUAL, g, Some(cell), true),
        None
    );

    // An already-bound source carries nothing (its receiver is captured).
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(setter, MH_BOUND, Value::Object(Some(r)));
    assert_eq!(
        bound_field_carry_with(
            &ctx,
            setter,
            MH_KIND_SETTER,
            "(Lp/Cell;I)V",
            Some(cell),
            true
        ),
        None
    );
}

#[test]
fn the_carry_is_written_as_wide_as_the_copy_allows() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let state = field_link_minted_state(2).unwrap();
    let carry = BoundFieldCarry {
        owner: cell,
        link_state: state,
    };

    // A copy with the link slots: the owner and the minted slot.
    let wide = handle(&mut ctx, MH_KIND_GETTER);
    mh_write_bound_field_carry(&ctx, wide, carry);
    assert_eq!(
        ctx.get_field(wide, MH_STATIC_OWNER),
        Value::Int(cell.as_u32() as i32)
    );
    assert_eq!(mh_minted_field_slot_with(&ctx, wide, true), Some(2));
    assert_eq!(ctx.get_field(wide, MH_FIELD_LINK), Value::Int(state));

    // The narrow bound carrier: the owner alone.
    let narrow = ctx.alloc_object(cell, MH_STATIC_OWNER + 1);
    mh_write_bound_field_carry(&ctx, narrow, carry);
    assert_eq!(
        ctx.get_field(narrow, MH_STATIC_OWNER),
        Value::Int(cell.as_u32() as i32)
    );
    assert_eq!(mh_minted_field_slot_with(&ctx, narrow, true), None);

    // Narrower still: nothing (and no write past the end).
    let tiny = ctx.alloc_object(cell, MH_STATIC_OWNER);
    mh_write_bound_field_carry(&ctx, tiny, carry);
}

#[test]
fn the_owner_admits_bit_is_its_own() {
    let others = [
        FL_TYPE_INDEX_MASK << 16,
        0xFFFF,
        FL_LINKED,
        FL_STATIC,
        FL_SETTER,
        FL_VALUE_CHECK,
        FL_MINT_SLOT,
        0x7 << FL_LINKS_SHIFT,
    ];
    for bits in others {
        assert_eq!(
            bits & FL_OWNER_ADMITS,
            0,
            "{bits:#x} overlaps FL_OWNER_ADMITS"
        );
    }
    // A copy drops it with the link.
    let linked = FL_LINKED | FL_OWNER_ADMITS | FL_MINT_SLOT | 4;
    assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
}

#[test]
fn an_owner_link_admits_the_owner_and_its_subclasses_by_id() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
    let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
    let deep = ctx.ensure_class_initialized("p/Deep").expect("p/Deep");
    let other = ctx.ensure_class_initialized("p/Other").expect("p/Other");
    ctx.set_superclass(sub, base);
    ctx.set_superclass(deep, sub);
    // Another loader's `p/Base`: the name alone cannot tell them apart.
    let foreign = ctx.declare_second_copy("p/Base");
    let this = ctx.alloc_object(base, MH_SLOT_COUNT);
    ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(0));
    let on_base = ctx.alloc_object(base, 2);
    let on_sub = ctx.alloc_object(sub, 2);
    let on_deep = ctx.alloc_object(deep, 2);
    let on_other = ctx.alloc_object(other, 2);
    let on_foreign = ctx.alloc_object(foreign, 2);
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    let owner_raw = base.as_u32() as i32;
    let memo = |ctx: &MockNativeContext| ctx.get_field(this, MH_FIELD_LINK_VALUE);

    // A plain link: the exact class only (the pre-wave-65 rule).
    let plain = FL_LINKED | 1;
    assert!(field_link_receiver_admitted(
        &mut ctx, this, plain, owner_raw, on_base
    ));
    assert!(!field_link_receiver_admitted(
        &mut ctx, this, plain, owner_raw, on_sub
    ));
    assert_eq!(memo(&ctx), Value::Int(0), "a plain link remembers nothing");

    // An owner link: the owner, and each subclass, remembered last-seen.
    let owned = plain | FL_OWNER_ADMITS;
    assert!(field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_base
    ));
    assert!(field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_sub
    ));
    assert_eq!(memo(&ctx), Value::Int(sub.as_u32() as i32));
    assert!(field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_deep
    ));
    assert_eq!(memo(&ctx), Value::Int(deep.as_u32() as i32));
    assert!(field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_deep
    ));

    // Never an unrelated class, another loader's namesake, or an array;
    // none of them disturbs the memo.
    assert!(!field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_other
    ));
    assert!(!field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, on_foreign
    ));
    assert!(!field_link_receiver_admitted(
        &mut ctx, this, owned, owner_raw, arr
    ));
    assert_eq!(memo(&ctx), Value::Int(deep.as_u32() as i32));

    // A setter with a value check keeps its value class in that slot:
    // a subclass is admitted by the hierarchy, and the slot is untouched.
    let checked = owned | FL_SETTER | FL_VALUE_CHECK;
    ctx.set_field(this, MH_FIELD_LINK_VALUE, Value::Int(77));
    assert!(field_link_receiver_admitted(
        &mut ctx, this, checked, owner_raw, on_sub
    ));
    assert_eq!(memo(&ctx), Value::Int(77));
    assert!(!field_link_receiver_admitted(
        &mut ctx, this, checked, owner_raw, on_foreign
    ));
}
}

/// Interpreter round i2 wave 66, lane L4: a bound field handle links too
/// (`CRATONVM_MH_FIELD_LINK_BOUND`), and the `bindTo` fast mint covers field
/// handles (`CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`). The helpers take the
/// switches as parameters, and the fast path is driven on a hand-linked
/// handle; the end-to-end rows are
/// `L4W66BoundFieldLink.java`.
#[cfg(test)]
mod i66_l4_bound_field_link_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn strings(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn the_bound_bit_is_its_own_and_the_type_index_still_fits() {
    let others = [
        FL_TYPE_INDEX_MASK << 16,
        0xFFFF,
        FL_OWNER_ADMITS,
        FL_LINKED,
        FL_STATIC,
        FL_SETTER,
        FL_VALUE_CHECK,
        FL_MINT_SLOT,
        0x7 << FL_LINKS_SHIFT,
    ];
    for bits in others {
        assert_eq!(bits & FL_BOUND, 0, "{bits:#x} overlaps FL_BOUND");
    }
    // `MethodHandle.type` is declared below the synthetic window.
    assert!(MH_BASE <= FL_TYPE_INDEX_MASK as usize);
    // A copy drops the bound link with the link, keeping the minted slot.
    let linked = FL_LINKED | FL_BOUND | FL_MINT_SLOT | (5 << 16) | 4;
    assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
}

#[test]
fn the_bound_shapes_are_the_unbound_links_one_parameter_shorter() {
    let g = |leaf: &[&str], lr: &str, decl: &[&str], dr: &str| {
        let (leaf, decl) = (strings(leaf), strings(decl));
        bound_field_link_value_type(false, &leaf, lr, &decl, dr).map(|v| v.map(str::to_string))
    };
    let s = |leaf: &[&str], decl: &[&str]| {
        let (leaf, decl) = (strings(leaf), strings(decl));
        bound_field_link_value_type(true, &leaf, "V", &decl, "V").map(|v| v.map(str::to_string))
    };
    // A getter: the same return, or a reference read as `Object`.
    assert_eq!(g(&["Lp/Cell;"], "I", &[], "I"), Some(None));
    assert_eq!(
        g(
            &["Lp/Cell;"],
            "Ljava/lang/String;",
            &[],
            "Ljava/lang/Object;"
        ),
        Some(None)
    );
    // A converted return, a static or misshapen descriptor: the door's.
    assert_eq!(g(&["Lp/Cell;"], "I", &[], "J"), None);
    assert_eq!(g(&["Lp/Cell;"], "I", &[], "Ljava/lang/Object;"), None);
    assert_eq!(g(&[], "I", &[], "I"), None);
    assert_eq!(g(&["Lp/Cell;"], "I", &["Lp/Cell;"], "I"), None);
    assert_eq!(g(&["I"], "I", &[], "I"), None);
    // A setter: a primitive value of the same type, a reference value
    // checked unless its leaf type is `Object`.
    assert_eq!(s(&["Lp/Cell;", "I"], &["I"]), Some(None));
    assert_eq!(s(&["Lp/Cell;", "I"], &["J"]), None);
    assert_eq!(
        s(&["Lp/Cell;", "Ljava/lang/Object;"], &["Ljava/lang/Object;"]),
        Some(None)
    );
    assert_eq!(
        s(&["Lp/Cell;", "Ljava/lang/String;"], &["Ljava/lang/String;"]),
        Some(Some("Ljava/lang/String;".to_string()))
    );
    assert_eq!(s(&["Lp/Cell;", "Ljava/lang/String;"], &["I"]), None);
    assert_eq!(s(&["I"], &[]), None);
    let (leaf, decl) = (strings(&["Lp/Cell;", "I"]), strings(&["I"]));
    assert_eq!(
        bound_field_link_value_type(true, &leaf, "I", &decl, "V"),
        None
    );
}

#[test]
fn the_wide_copy_and_the_fast_mint_kinds_follow_their_switches() {
    let cell = ClassId::new(7);
    let owned = Some(BoundFieldCarry {
        owner: cell,
        link_state: 0,
    });
    let minted = Some(BoundFieldCarry {
        owner: cell,
        link_state: field_link_minted_state(2).unwrap(),
    });
    assert!(!bound_field_copy_needs_link_slots(None, true));
    assert!(!bound_field_copy_needs_link_slots(owned, false));
    assert!(bound_field_copy_needs_link_slots(owned, true));
    assert!(bound_field_copy_needs_link_slots(minted, false));

    for kind in [MH_KIND_STATIC, MH_KIND_VIRTUAL, MH_KIND_SPECIAL] {
        assert!(bindto_fast_mint_fills_kind(kind, false));
    }
    for kind in [MH_KIND_GETTER, MH_KIND_SETTER] {
        assert!(!bindto_fast_mint_fills_kind(kind, false));
        assert!(bindto_fast_mint_fills_kind(kind, true));
    }
    assert!(!bindto_fast_mint_fills_kind(MH_KIND_ARRAY_GET, true));
    assert!(!bindto_fast_mint_fills_kind(MH_KIND_CONSTANT, true));
}

#[test]
fn a_hit_re_reads_the_carry_and_misses_on_another_owner() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let other = ctx.ensure_class_initialized("p/Other").expect("p/Other");
    let src =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
    let mut answer = BindToMintAnswer {
        kind: MH_KIND_GETTER,
        type_desc: std::rc::Rc::from("()I"),
        collects: false,
        static_owner: None,
        recv_class: None,
        field_carry: Some(BoundFieldCarry {
            owner: cell,
            link_state: 0,
        }),
        prelink: None,
        alt: None,
    };
    // The source's minted slot (and only that, past a link) rides.
    let linked =
        field_link_minted_state(3).unwrap() | FL_LINKED | (5 << 16) | (1 << FL_LINKS_SHIFT);
    ctx.set_field(src, MH_FIELD_LINK, Value::Int(linked));
    assert!(bindto_fast_mint_refresh_carry_with(
        &ctx,
        src,
        &mut answer,
        Some(cell)
    ));
    assert_eq!(
        answer.field_carry,
        Some(BoundFieldCarry {
            owner: cell,
            link_state: field_link_minted_state(3).unwrap()
        })
    );
    // Another owner, or none: the slow path.
    assert!(!bindto_fast_mint_refresh_carry_with(
        &ctx,
        src,
        &mut answer,
        Some(other)
    ));
    assert!(!bindto_fast_mint_refresh_carry_with(
        &ctx,
        src,
        &mut answer,
        None
    ));
    // A direct line has nothing to refresh.
    let mut direct = BindToMintAnswer {
        field_carry: None,
        kind: MH_KIND_VIRTUAL,
        ..answer
    };
    assert!(bindto_fast_mint_refresh_carry_with(
        &ctx,
        src,
        &mut direct,
        None
    ));
    assert_eq!(direct.field_carry, None);
}

/// A bound getter / setter over `p/Cell`, hand-linked as
/// [`mh_field_link_try_bound`] links it (the mock renders no `MethodType`,
/// so `MH_CLASS`'s string stands in for the `type` object), with the
/// bound receiver of class `cell` and slot 2.
fn linked_bound(
    ctx: &mut MockNativeContext,
    kind: i32,
    receiver: ObjectRef,
    class_raw: i32,
    extra_flags: i32,
    site: &str,
) -> ObjectRef {
    let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap();
    assert!(matches!(ctx.get_field(mh, MH_VARARGS), Value::Int(0)));
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
        panic!("MH_CLASS holds a string");
    };
    ctx.set_field(mh, MH_BOUND, Value::Object(Some(receiver)));
    let site_id = field_link_site_id(site).expect("interned") as i32;
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(class_raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
    let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT) | extra_flags;
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
    mh
}

#[test]
fn a_bound_link_serves_its_receiver_and_nothing_else() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let stranger = ctx.ensure_class_initialized("p/Str").expect("p/Str");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(42));
    let raw = cell.as_u32() as i32;

    // A getter: the bound receiver's slot, no argument.
    let getter = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
    let got = mh_field_link_fast(&mut ctx, getter, &[], Some("()I"));
    assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));
    // An argument, or another call site: the door.
    assert!(mh_field_link_fast(&mut ctx, getter, &[Value::Int(1)], Some("()I")).is_none());
    if mh_strict_exact_signature() {
        assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()J")).is_none());
    }

    // A bound receiver of another class than the link names (a `String`):
    // the door, which casts and throws.
    let s = ctx.alloc_object(stranger, 4);
    ctx.set_field(s, 2, Value::Int(7));
    ctx.set_field(getter, MH_BOUND, Value::Object(Some(s)));
    assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()I")).is_none());

    // An UNBOUND link never serves a bound handle, nor a bound link an
    // unbound one.
    let plain = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, 0, "()I");
    assert!(mh_field_link_fast(&mut ctx, plain, &[], Some("()I")).is_none());
    assert!(
        mh_field_link_fast(&mut ctx, plain, &[Value::Object(Some(r))], Some("()I")).is_none()
    );
    let unbound = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
    ctx.set_field(unbound, MH_BOUND, Value::Object(None));
    assert!(
        mh_field_link_fast(&mut ctx, unbound, &[Value::Object(Some(r))], Some("()I")).is_none()
    );
    // Never a bound static link.
    let st = linked_bound(
        &mut ctx,
        MH_KIND_GETTER,
        r,
        raw,
        FL_BOUND | FL_STATIC,
        "()I",
    );
    assert!(mh_field_link_fast(&mut ctx, st, &[], Some("()I")).is_none());

    // A setter: the value alone, stored in the bound receiver's slot.
    let setter = linked_bound(
        &mut ctx,
        MH_KIND_SETTER,
        r,
        raw,
        FL_BOUND | FL_SETTER,
        "(I)V",
    );
    let put = mh_field_link_fast(&mut ctx, setter, &[Value::Int(9)], Some("(I)V"));
    assert!(matches!(put, Some(Ok(Some(Value::Object(None))))));
    assert_eq!(ctx.get_field(r, 2), Value::Int(9));
    assert!(mh_field_link_fast(&mut ctx, setter, &[], Some("(I)V")).is_none());
    assert!(mh_field_link_fast(
        &mut ctx,
        setter,
        &[Value::Object(Some(r)), Value::Int(1)],
        Some("(I)V")
    )
    .is_none());
    assert_eq!(ctx.get_field(r, 2), Value::Int(9));
}
}

/// Interpreter round i2 wave 67, lane L4: `bindTo` pre-links a bound field
/// copy (`CRATONVM_MH_BINDTO_PRELINK_FIELD`) and the `invoke` door serves a
/// linked field accessor at exactly its declared type
/// (`CRATONVM_MH_FIELD_LINK_SERVES_INVOKE`). Since the wave-67 triage the second is default on;
/// the tests drive the helpers below them either way; the end-to-end rows are
/// `L4W67BindToPrelink.java` and
/// `L4W67FieldLinkInvoke.java`.
#[cfg(test)]
mod i67_l4_prelink_and_invoke_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

#[test]
fn bindto_judges_no_call_and_links_no_value_check() {
    let args = [Value::Int(3)];
    let invoke = BoundLinkCall::Invoke {
        extra: &args,
        call_site: Some("(I)V"),
    };
    // An `invokeExact` passes its arguments at the declared arity only.
    assert_eq!(
        bound_link_call_extra(invoke, None, 1).map(<[Value]>::len),
        Some(1)
    );
    assert!(bound_link_call_extra(invoke, None, 0).is_none());
    let empty = BoundLinkCall::Invoke {
        extra: &[],
        call_site: Some("()I"),
    };
    assert_eq!(
        bound_link_call_extra(empty, None, 0).map(<[Value]>::len),
        Some(0)
    );
    // `bindTo`: nothing to judge, so a getter or an unchecked setter links
    // with no arguments, and a reference setter that needs its value
    // checked is left to its first call.
    assert_eq!(
        bound_link_call_extra(BoundLinkCall::BindTo, None, 0).map(<[Value]>::len),
        Some(0)
    );
    assert_eq!(
        bound_link_call_extra(BoundLinkCall::BindTo, None, 1).map(<[Value]>::len),
        Some(0)
    );
    assert!(
        bound_link_call_extra(BoundLinkCall::BindTo, Some("Ljava/lang/String;"), 1).is_none()
    );
    // `invokeExact`'s reference setter still judges its one value.
    let s = [Value::Object(None)];
    let checked = BoundLinkCall::Invoke {
        extra: &s,
        call_site: None,
    };
    assert!(bound_link_call_extra(checked, Some("Ljava/lang/String;"), 1).is_some());
}

#[test]
fn the_candidate_checks_kind_marking_and_relinks() {
    let mut ctx = MockNativeContext::new();
    if !mh_field_link_mode_admits(&ctx)
        || !mh_field_link_enabled()
        || crate::nbflags().dbg_mh_dispatch
    {
        return;
    }
    let getter =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
    assert_eq!(
        field_link_candidate(&mut ctx, getter),
        Some(FieldLinkCandidate {
            setter: false,
            old: 0,
            links: 0
        })
    );
    let setter =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "(I)V", MH_KIND_SETTER).unwrap();
    assert_eq!(
        field_link_candidate(&mut ctx, setter).map(|c| c.setter),
        Some(true)
    );
    // Another kind, a marked handle, a handle out of relinks: none.
    let virt =
        alloc_method_handle(&mut ctx, "p/Cell", "cellValueZ", "()I", MH_KIND_GETTER).unwrap();
    ctx.set_field(virt, MH_KIND, Value::Int(MH_KIND_VIRTUAL));
    assert_eq!(field_link_candidate(&mut ctx, virt), None);
    mh_write_marking_bits(&ctx, setter, 1);
    assert_eq!(field_link_candidate(&mut ctx, setter), None);
    let spent = FL_MAX_LINKS << FL_LINKS_SHIFT;
    ctx.set_field(getter, MH_FIELD_LINK, Value::Int(spent));
    assert_eq!(field_link_candidate(&mut ctx, getter), None);
    let once = FL_LINKED | (1 << FL_LINKS_SHIFT) | 2;
    ctx.set_field(getter, MH_FIELD_LINK, Value::Int(once));
    assert_eq!(
        field_link_candidate(&mut ctx, getter),
        Some(FieldLinkCandidate {
            setter: false,
            old: once,
            links: 1
        })
    );
}

#[test]
fn a_pre_link_of_a_copy_with_no_declared_type_writes_nothing() {
    // No `type` (an unknown signature): the shared admission stops at the
    // declared type, exactly where a first-call link would.
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    let mh = alloc_method_handle(
        &mut ctx,
        "p/Cell",
        "cellValueZ",
        "(Lp/Cell;)I",
        MH_KIND_GETTER,
    )
    .unwrap();
    ctx.set_field_by_name(mh, "type", Value::Object(None));
    ctx.set_field(mh, MH_BOUND, Value::Object(Some(r)));
    ctx.set_field(mh, MH_STATIC_OWNER, Value::Int(cell.as_u32() as i32));
    mh_field_link_try_bound(
        &mut ctx,
        mh,
        r,
        false,
        0,
        0,
        BoundLinkCall::BindTo,
        None,
        &mut None,
    );
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
    mh_bindto_prelink_bound_field(&mut ctx, mh);
    assert!(matches!(ctx.get_field(mh, MH_FIELD_LINK), Value::Int(s) if s & FL_LINKED == 0));
}

/// A getter over `p/Cell` linked at `site` (unbound, receiver class `raw`,
/// slot 2), as `mh_field_link_try` links it; `MH_CLASS`'s string stands
/// in for the `type` object the mock does not render.
fn linked_getter(ctx: &mut MockNativeContext, raw: i32, site: &str) -> ObjectRef {
    let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "(Lp/Cell;)I", MH_KIND_GETTER)
        .unwrap();
    let Value::Object(Some(type_obj)) = ctx.get_field(mh, MH_CLASS) else {
        panic!("MH_CLASS holds a string");
    };
    let site_id = field_link_site_id(site).expect("interned") as i32;
    ctx.set_field(mh, MH_FIELD_LINK_TYPE, Value::Object(Some(type_obj)));
    ctx.set_field(mh, MH_FIELD_LINK_CLASS, Value::Int(raw));
    ctx.set_field(mh, MH_FIELD_LINK_VALUE, Value::Int(0));
    ctx.set_field(mh, MH_FIELD_LINK_SITE, Value::Int(site_id));
    let state = FL_LINKED | ((MH_CLASS as i32) << 16) | 2 | (1 << FL_LINKS_SHIFT);
    ctx.set_field(mh, MH_FIELD_LINK, Value::Int(state));
    mh
}

#[test]
fn invoke_reaches_the_link_only_at_the_declared_type() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(42));
    let getter = linked_getter(&mut ctx, cell.as_u32() as i32, "(Lp/Cell;)I");
    assert!(field_link_site_is(&ctx, getter, Some("(Lp/Cell;)I")));
    // An `Object`-typed receiver, another return, no site: an `asType`
    // the link must not skip -- the door.
    assert!(!field_link_site_is(
        &ctx,
        getter,
        Some("(Ljava/lang/Object;)I")
    ));
    assert!(!field_link_site_is(&ctx, getter, Some("(Lp/Cell;)J")));
    assert!(!field_link_site_is(&ctx, getter, None));
    // Unlinked (a copy keeps only the minted slot): the door.
    let unlinked = alloc_method_handle(
        &mut ctx,
        "p/Cell",
        "cellValueZ",
        "(Lp/Cell;)I",
        MH_KIND_GETTER,
    )
    .unwrap();
    assert!(!field_link_site_is(&ctx, unlinked, Some("(Lp/Cell;)I")));
    // The switch is off by default: `invoke` keeps the door.
    if !mh_field_link_serves_invoke_enabled() {
        let args = [Value::Object(Some(r))];
        assert!(
            mh_field_link_fast_for_invoke(&mut ctx, getter, &args, Some("(Lp/Cell;)I"))
                .is_none()
        );
    }
}
}
