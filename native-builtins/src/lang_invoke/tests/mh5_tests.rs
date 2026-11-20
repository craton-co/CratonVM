#[allow(unused_imports)]
use super::*;

/// Round 15 wave 6, lane mh5: `asType` / `explicitCastArguments` of a
/// variable-arity handle answer the JDK's fixed-arity composition (MH4-1),
/// `insertArguments`' null arguments, and the capture-dropping raw descriptor
/// of every capturing kind (MH3-6). `MethodType`'s `rtype` / `ptypes` are
/// declared, as in `r13w10_callcost4_direct_lane_tests`, so a handle's `type`
/// reads back.
#[cfg(test)]
mod r15w6_mh5_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const CAT: &str = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/String;";
const THREE: &str =
    "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    let _ = ctx.ensure_class_initialized("p/C");
    let probe = mt(&mut ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
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
    ctx
}

fn handle(ctx: &mut MockNativeContext, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, "p/C", "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc: {e:?}"),
    }
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

fn answered_handle(r: Option<MethodCallResult>) -> ObjectRef {
    match r {
        Some(Ok(Some(Value::Object(Some(h))))) => h,
        other => panic!("answered {other:?}"),
    }
}

/// `asCollector(Class, int)` answered by its registered native, as the VM
/// would dispatch it.
fn as_collector_hook(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    if name != "asCollector" || desc != MH_AS_COLLECTOR_DESC {
        return None;
    }
    let mut registry = NativeMethodRegistry::new();
    register_method_handle_combinator_extras_bridge(&mut registry);
    let native = registry.find(
        "java/lang/invoke/MethodHandle",
        "asCollector",
        MH_AS_COLLECTOR_DESC,
    )?;
    let mut full = vec![Value::Object(Some(receiver))];
    full.extend_from_slice(args);
    Some(native(ctx, &full))
}

/// MH4-1: `cat.asType(t)` of a collector is fixed-arity (the receiver
/// keeps collecting); at the same arity with the array trailing it is the
/// fixed copy stamped, at another arity an `asCollector` adapter of the
/// fixed copy; an element that does not convert to the component, and an
/// arity below `collectArg`, are refused.
#[test]
fn the_astype_of_a_collector_is_fixed_arity() {
    if !mh_astype_fixed_collector_enabled() {
        return;
    }
    let mut ctx = setup();
    let cat = handle(&mut ctx, CAT, MH_KIND_STATIC);
    let three = mt(&mut ctx, THREE);
    // Not a collector: not this function's business.
    assert!(mh_astype_varargs_collector(&mut ctx, cat, three).is_none());
    mh_set_varargs_collector(&ctx, cat, true);
    // The receiver's own type: the receiver (the caller's stamp answers it).
    let own = mt(&mut ctx, CAT);
    assert!(mh_astype_varargs_collector(&mut ctx, cat, own).is_none());
    // Same arity, the array trailing: `asFixedArity().asType(t)`.
    let object_return = "(Ljava/lang/String;[Ljava/lang/String;)Ljava/lang/Object;";
    let t = mt(&mut ctx, object_return);
    let fixed = answered_handle(mh_astype_varargs_collector(&mut ctx, cat, t));
    assert_ne!(fixed, cat);
    assert!(!mh_is_varargs_collector(&ctx, fixed), "a fixed-arity copy");
    assert!(
        mh_is_varargs_collector(&ctx, cat),
        "the receiver is untouched"
    );
    assert_eq!(ctx.get_field(fixed, MH_KIND), Value::Int(MH_KIND_STATIC));
    assert_eq!(
        mh_declared_descriptor(&mut ctx, fixed).as_deref(),
        Some(object_return)
    );
    // Another arity: `asFixedArity().asCollector(String[], 2).asType(t)`.
    ctx.set_invoke_virtual_hook(as_collector_hook);
    let three = mt(&mut ctx, THREE);
    let collector = answered_handle(mh_astype_varargs_collector(&mut ctx, cat, three));
    assert_eq!(
        ctx.get_field(collector, MH_KIND),
        Value::Int(MH_KIND_COLLECT)
    );
    assert!(!mh_is_varargs_collector(&ctx, collector));
    assert_eq!(
        mh_declared_descriptor(&mut ctx, collector).as_deref(),
        Some(THREE)
    );
    let wrapper = match ctx.get_field(collector, MH_BOUND) {
        Value::Object(Some(w)) => w,
        other => panic!("collector wrapper {other:?}"),
    };
    assert_eq!(ctx.get_field(wrapper, 1), Value::Int(2), "two collected");
    match ctx.get_field(wrapper, 0) {
        Value::Object(Some(inner)) => {
            assert_ne!(inner, cat);
            assert!(!mh_is_varargs_collector(&ctx, inner), "over the fixed copy");
        }
        other => panic!("collector target {other:?}"),
    }
    // `int` elements do not convert to `String`: the inner `asType`'s WMTE.
    let ints = mt(&mut ctx, "(Ljava/lang/String;II)Ljava/lang/String;");
    match mh_astype_varargs_collector(&mut ctx, cat, ints) {
        Some(Err(e)) => assert!(is_wmte(&ctx, &e), "{e:?}"),
        other => panic!("(String,int,int)String: {other:?}"),
    }
    // Below `collectArg`: left to the refusal ("cannot build collector").
    let none = mt(&mut ctx, "()Ljava/lang/String;");
    assert!(mh_astype_varargs_collector(&mut ctx, cat, none).is_none());
    match mh_astype_refusal(&mut ctx, cat, none) {
        Some(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
        None => panic!("a collector asked for no parameters"),
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// MH4-1: `explicitCastArguments` of a collector is fixed-arity too; an
/// unchanged type answers the collector itself.
#[test]
fn the_explicit_cast_of_a_collector_is_fixed_arity() {
    if !mh_astype_fixed_collector_enabled() {
        return;
    }
    let mut ctx = setup();
    let cat = handle(&mut ctx, CAT, MH_KIND_STATIC);
    mh_set_varargs_collector(&ctx, cat, true);
    let mut registry = NativeMethodRegistry::new();
    register_method_handle_combinator_extras_bridge(&mut registry);
    let explicit = registry
        .find(
            "java/lang/invoke/MethodHandles",
            "explicitCastArguments",
            "(Ljava/lang/invoke/MethodHandle;Ljava/lang/invoke/MethodType;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandles.explicitCastArguments should be registered");
    let t = mt(
        &mut ctx,
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
    );
    let cast = match explicit(
        &mut ctx,
        &[Value::Object(Some(cat)), Value::Object(Some(t))],
    ) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("explicitCastArguments answered {other:?}"),
    };
    assert_ne!(cast, cat);
    assert!(!mh_is_varargs_collector(&ctx, cast));
    assert!(
        mh_is_explicit_cast(&ctx, cast),
        "the explicit marking is kept"
    );
    assert!(mh_is_varargs_collector(&ctx, cat));
    let own = mt(&mut ctx, CAT);
    // The mock mints a fresh Class mirror on every descriptor lookup.
    // Model the VM's canonical mirrors when asking for the same type.
    let original_type = match ctx.get_field_by_name(cat, "type") {
        Value::Object(Some(t)) => t,
        other => panic!("collector type {other:?}"),
    };
    for field in ["rtype", "ptypes"] {
        let value = ctx.get_field_by_name(original_type, field);
        ctx.set_field_by_name(own, field, value);
    }
    match explicit(
        &mut ctx,
        &[Value::Object(Some(cat)), Value::Object(Some(own))],
    ) {
        Ok(Some(Value::Object(Some(h)))) => {
            assert_eq!(h, cat);
            assert!(mh_is_varargs_collector(&ctx, h));
        }
        other => panic!("explicitCastArguments answered {other:?}"),
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// `insertArguments(t, pos, (Object[]) null)` and `insertArguments(null,
/// pos, values)`: the JDK body's two NPEs, `values` first.
#[test]
fn insert_arguments_refuses_a_null_values_or_target() {
    if !mh_insert_arguments_null_npe_enabled() {
        return;
    }
    let mut ctx = setup();
    let target = handle(&mut ctx, THREE, MH_KIND_STATIC);
    let object = crate::lang_class::reflection_component_id(&mut ctx, "java/lang/Object");
    let empty = ctx.new_ref_array(object, 0);
    let mut registry = NativeMethodRegistry::new();
    register_method_handle_combinator_extras_bridge(&mut registry);
    let insert = registry
        .find(
            "java/lang/invoke/MethodHandles",
            "insertArguments",
            "(Ljava/lang/invoke/MethodHandle;I[Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandles.insertArguments should be registered");
    let npe = |r: MethodCallResult| match r {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message, .. },
        ))) => message,
        other => panic!("expected an NPE, got {other:?}"),
    };
    let values = Some("Cannot read the array length because \"values\" is null".to_string());
    let target_npe = Some(
        "Cannot invoke \"java.lang.invoke.MethodHandle.type()\" because \"target\" is null"
            .to_string(),
    );
    let none = Value::Object(None);
    assert_eq!(
        npe(insert(
            &mut ctx,
            &[Value::Object(Some(target)), Value::Int(0), none]
        )),
        values
    );
    assert_eq!(
        npe(insert(&mut ctx, &[none, Value::Int(0), none])),
        values,
        "values first"
    );
    assert_eq!(
        npe(insert(
            &mut ctx,
            &[none, Value::Int(0), Value::Object(Some(empty))]
        )),
        target_npe
    );
}

/// MH3-6: the raw descriptor of a BOUND capturing handle drops the
/// capture for every kind whose `MH_DESC` names it, never for a virtual
/// one (its receiver was never in `MH_DESC`) and never unbound.
#[test]
fn a_bound_capture_kinds_raw_type_drops_the_capture() {
    if !mh_astype_capture_raw_desc_enabled() {
        return;
    }
    let mut ctx = setup();
    let getter = handle(&mut ctx, "([II)I", MH_KIND_ARRAY_GET);
    assert_eq!(
        bound_capture_raw_desc(&ctx, getter, "([II)I"),
        None,
        "not bound yet"
    );
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 2);
    ctx.set_field(getter, MH_BOUND, Value::Object(Some(arr)));
    assert_eq!(
        bound_capture_raw_desc(&ctx, getter, "([II)I").as_deref(),
        Some("(I)I")
    );
    let field = handle(&mut ctx, "(Lp/C;)I", MH_KIND_GETTER);
    let recv = Value::Object(Some(ctx.create_string("r")));
    ctx.set_field(field, MH_BOUND, recv);
    assert_eq!(
        bound_capture_raw_desc(&ctx, field, "(Lp/C;)I").as_deref(),
        Some("()I")
    );
    let virt = handle(&mut ctx, "(I)I", MH_KIND_VIRTUAL);
    ctx.set_field(virt, MH_BOUND, recv);
    assert_eq!(
        bound_capture_raw_desc(&ctx, virt, "(I)I"),
        None,
        "a receiver"
    );
    // The escape no longer lets a type one parameter longer through.
    let longer = "(Ljava/lang/Object;I)I";
    let refusal = mh_astype_refusal_desc(&mut ctx, getter, "(I)I", longer);
    assert!(
        refusal.as_ref().is_some_and(|e| is_wmte(&ctx, e)),
        "{refusal:?}"
    );
}
}

/// Round 13 wave 10, lane callcost4: the direct lane of the `invoke` /
/// `invokeExact` doors (`mh_direct_lane`). The mock has no by-name hierarchy
/// walk, so a POSITIVE reference mismatch is an array (the cast-chain tests'
/// convention); its static invokes answer `Ok(None)`, and a virtual call is
/// answered by the hook below. End-to-end rows: `R13Callcost4MhDirect`.
#[cfg(test)]
mod r13w10_callcost4_direct_lane_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object {class}: {other:?}"),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

/// Declare `MethodType`'s `rtype` / `ptypes` so `methodtype_to_descriptor`
/// can read a handle's declared type back.
fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    for name in ["p/C", "p/Mid", "p/Sub"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    let probe = mt(&mut ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
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
    ctx
}

fn handle(ctx: &mut MockNativeContext, class: &str, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, class, "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {class}: {e:?}"),
    }
}

/// `m(I)I` answers its argument plus 100.
fn plus_hundred(
    _ctx: &mut MockNativeContext,
    _receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    match (name, desc, args.first()) {
        ("m", "(I)I", Some(Value::Int(x))) => Some(Ok(Some(Value::Int(x + 100)))),
        _ => None,
    }
}

/// The int a door answered, raw or boxed.
fn answered_int(ctx: &MockNativeContext, r: &Option<MethodCallResult>) -> Option<i32> {
    match r {
        Some(Ok(Some(Value::Int(v)))) => Some(*v),
        Some(Ok(Some(Value::Object(Some(o))))) => match crate::lang_class::unbox_value(ctx, *o)
        {
            Value::Int(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

fn cce_target(r: &Option<MethodCallResult>) -> Option<String> {
    match r {
        Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        )))) => message.rsplit(" to ").next().map(str::to_string),
        _ => None,
    }
}

/// A `findVirtual` handle at its own exact type, through both doors: the
/// leaf's answer, every pin released.
#[test]
fn a_direct_virtual_handle_at_its_own_type_answers_the_leaf() {
    let mut ctx = setup();
    let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    let recv = object(&mut ctx, "p/Mid", Value::Int(0));
    ctx.set_invoke_virtual_hook(plus_hundred);
    for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
        let r = mh_direct_lane(
            &mut ctx,
            mh,
            &[recv, Value::Int(5)],
            Some("(Lp/Mid;I)I"),
            door,
        );
        assert_eq!(answered_int(&ctx, &r), Some(105), "{door:?}: {r:?}");
    }
    // The same handle bound to its receiver (`bindTo`'s shape: the value
    // in `MH_BOUND`, the leading parameter dropped from `type`).
    let bound = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    ctx.set_field(bound, MH_BOUND, recv);
    let dropped = mt(&mut ctx, "(I)I");
    ctx.set_field_by_name(bound, "type", Value::Object(Some(dropped)));
    for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
        let r = mh_direct_lane(&mut ctx, bound, &[Value::Int(7)], Some("(I)I"), door);
        assert_eq!(answered_int(&ctx, &r), Some(107), "bound {door:?}: {r:?}");
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// A static handle: the mock's static invoke answers `Ok(None)`, which the
/// lane hands back exactly as `auto_box_return` does for a primitive
/// return, and a `void` return is the door's `null`. The settle pin is
/// released.
#[test]
fn a_direct_static_handle_answers_as_the_door_boxes() {
    let mut ctx = setup();
    let int_mh = handle(&mut ctx, "p/C", "(IJ)I", MH_KIND_STATIC);
    let r = mh_direct_lane(
        &mut ctx,
        int_mh,
        &[Value::Int(1), Value::Long(2)],
        Some("(IJ)I"),
        DirectLaneDoor::InvokeExact,
    );
    assert!(matches!(r, Some(Ok(None))), "{r:?}");
    let void_mh = handle(&mut ctx, "p/C", "()V", MH_KIND_STATIC);
    for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
        let r = mh_direct_lane(&mut ctx, void_mh, &[], Some("()V"), door);
        assert!(
            matches!(r, Some(Ok(Some(Value::Object(None))))),
            "{door:?}: {r:?}"
        );
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// Everything the lane does not model is declined before it has done
/// anything, so the ordinary door runs.
#[test]
fn the_lane_declines_every_shape_it_does_not_model() {
    let mut ctx = setup();
    let mh = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    let recv = object(&mut ctx, "p/Mid", Value::Int(0));
    let boxed = object(&mut ctx, "java/lang/Integer", Value::Int(5));
    let exact = DirectLaneDoor::InvokeExact;
    let site = Some("(Lp/Mid;I)I");
    // No call site, another call site, a boxed argument, a null receiver.
    assert!(mh_direct_lane(&mut ctx, mh, &[recv, Value::Int(5)], None, exact).is_none());
    assert!(mh_direct_lane(
        &mut ctx,
        mh,
        &[recv, Value::Long(5)],
        Some("(Lp/Mid;J)I"),
        exact
    )
    .is_none());
    assert!(mh_direct_lane(&mut ctx, mh, &[recv, boxed], site, exact).is_none());
    assert!(mh_direct_lane(
        &mut ctx,
        mh,
        &[Value::Object(None), Value::Int(5)],
        site,
        exact
    )
    .is_none());
    // A variable-arity collector.
    let collector = handle(&mut ctx, "p/Mid", "(I)I", MH_KIND_VIRTUAL);
    mh_set_varargs_collector(&ctx, collector, true);
    assert!(mh_direct_lane(&mut ctx, collector, &[recv, Value::Int(5)], site, exact).is_none());
    // An `asType` copy: its `type` is not its member's own.
    let t = mt(&mut ctx, "(Ljava/lang/Object;I)I");
    let copy = match mh_with_stamped_type(&mut ctx, Value::Object(Some(mh)), t, false) {
        Ok(Some(Value::Object(Some(copy)))) => copy,
        other => panic!("stamp: {other:?}"),
    };
    assert!(mh_direct_lane(
        &mut ctx,
        copy,
        &[recv, Value::Int(5)],
        Some("(Ljava/lang/Object;I)I"),
        exact
    )
    .is_none());
    // An array parameter (the varargs repair's territory).
    let arr_mh = handle(&mut ctx, "p/C", "([I)I", MH_KIND_STATIC);
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    assert!(mh_direct_lane(&mut ctx, arr_mh, &[arr], Some("([I)I"), exact).is_none());
    // A kind the lane does not take.
    let getter = handle(&mut ctx, "p/C", "()I", MH_KIND_GETTER);
    assert!(mh_direct_lane(&mut ctx, getter, &[], Some("()I"), exact).is_none());
    // A static handle with a bound leading argument.
    let static_bound = handle(&mut ctx, "p/C", "(Ljava/lang/Object;)I", MH_KIND_STATIC);
    ctx.set_field(static_bound, MH_BOUND, recv);
    assert!(mh_direct_lane(&mut ctx, static_bound, &[], Some("()I"), exact).is_none());
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "nothing pinned by a decline"
    );
}

/// A reference the leaf would refuse is refused by the lane with the
/// ordinary door's own refusal: `invokeExact` with the leaf's
/// `ClassCastException`; `invoke` on a static handle earlier, with
/// `mh_entry_adapt`'s entry cast (a thrown Java exception), as the door.
#[test]
fn a_leaf_cast_is_refused_as_the_door_refuses_it() {
    let mut ctx = setup();
    let mh = handle(&mut ctx, "p/C", "(Lp/Sub;)I", MH_KIND_STATIC);
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let ordinary = adapt_leaf_args(&mut ctx, &[arr], "(Lp/Sub;)I", None, false);
    assert!(
        ordinary.is_err(),
        "the leaf refuses an array at a class parameter"
    );
    let exact = mh_direct_lane(
        &mut ctx,
        mh,
        &[arr],
        Some("(Lp/Sub;)I"),
        DirectLaneDoor::InvokeExact,
    );
    assert_eq!(cce_target(&exact).as_deref(), Some("p.Sub"), "{exact:?}");
    let entry = mh_entry_adapt(&mut ctx, mh, "(Lp/Sub;)I", "(Lp/Sub;)I", false, &[arr]);
    assert!(entry.is_err(), "the entry cast refuses it too");
    let invoke = mh_direct_lane(
        &mut ctx,
        mh,
        &[arr],
        Some("(Lp/Sub;)I"),
        DirectLaneDoor::Invoke,
    );
    assert!(matches!(invoke, Some(Err(_))), "{invoke:?}");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// The explicit-cast scope of an enclosing dispatch is cleared for the
/// lane's own call and restored on every exit, as the door does.
#[test]
fn the_explicit_scope_is_restored_on_every_exit() {
    let mut ctx = setup();
    let mh = handle(&mut ctx, "p/C", "()V", MH_KIND_STATIC);
    MH_EXPLICIT_SCOPE.with(|f| f.set(true));
    let answered = mh_direct_lane(&mut ctx, mh, &[], Some("()V"), DirectLaneDoor::InvokeExact);
    assert!(answered.is_some());
    assert!(
        MH_EXPLICIT_SCOPE.with(|f| f.get()),
        "restored after an answer"
    );
    let refused = handle(&mut ctx, "p/C", "(Lp/Sub;)V", MH_KIND_STATIC);
    let arr = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 1)));
    let r = mh_direct_lane(
        &mut ctx,
        refused,
        &[arr],
        Some("(Lp/Sub;)V"),
        DirectLaneDoor::InvokeExact,
    );
    assert!(matches!(r, Some(Err(_))), "{r:?}");
    assert!(
        MH_EXPLICIT_SCOPE.with(|f| f.get()),
        "restored after a refusal"
    );
    MH_EXPLICIT_SCOPE.with(|f| f.set(false));
}

/// The raw return keeps a primitive of the return's own variant and boxes
/// nothing else differently from `auto_box_return`.
#[test]
fn the_raw_return_is_taken_only_for_the_returns_own_variant() {
    let mut ctx = setup();
    let raw = mh_direct_lane_return(&mut ctx, Ok(Some(Value::Long(7))), "()J");
    if mh_direct_lane_raw_return_enabled() {
        assert_eq!(raw.ok(), Some(Some(Value::Long(7))));
    }
    // A mismatched variant is `auto_box_return`'s.
    let mismatched = mh_direct_lane_return(&mut ctx, Ok(Some(Value::Int(7))), "()J");
    assert!(
        !matches!(mismatched, Ok(Some(Value::Int(_)))),
        "{mismatched:?}"
    );
    // `void` and a missing reference are the door's `null`.
    assert!(matches!(
        mh_direct_lane_return(&mut ctx, Ok(None), "()V"),
        Ok(Some(Value::Object(None)))
    ));
    assert!(matches!(
        mh_direct_lane_return(&mut ctx, Ok(None), "()Ljava/lang/Object;"),
        Ok(Some(Value::Object(None)))
    ));
}
}

/// JIT round 16 (lane mh): MH3-2, the per-VM bound `MethodType` table. The
/// mock context owns a `MethodTypeTable` exactly as the VM's class realm does.
#[cfg(test)]
mod r16_mh_bound_type_table_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn table_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match bound_method_type_through_table(ctx, desc, None) {
        Ok(Some(mt)) => mt,
        other => panic!("{desc}: {other:?}"),
    }
}

/// The proposal's test: two builds of one descriptor answer one object;
/// the plain builder (the switch-off path) answers two.
#[test]
fn two_builds_of_one_descriptor_answer_one_object() {
    let mut ctx = MockNativeContext::new();
    for name in ["java/lang/String", "[I", "[Ljava/lang/String;"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    for desc in [
        "(I)I",
        "()V",
        "(Ljava/lang/String;J)[I",
        "([Ljava/lang/String;)Z",
    ] {
        let a = table_type(&mut ctx, desc);
        let b = table_type(&mut ctx, desc);
        assert_eq!(a, b, "{desc}: one object");
    }
    let a = table_type(&mut ctx, "(I)I");
    let c = table_type(&mut ctx, "(J)I");
    assert_ne!(a, c, "distinct descriptors, distinct rows");
    let f1 = build_method_type_from_descriptor(&mut ctx, "(I)I")
        .unwrap()
        .unwrap();
    let f2 = build_method_type_from_descriptor(&mut ctx, "(I)I")
        .unwrap()
        .unwrap();
    assert_ne!(f1, f2, "the builder itself still mints");
}

/// Only java.*-and-primitive types that are already loaded are shared:
/// an application class, an unloaded java.* class, a spun-looking name
/// and an array of an application class each get a fresh object.
#[test]
fn only_loaded_java_and_primitive_types_are_shared() {
    let mut ctx = MockNativeContext::new();
    for name in ["java/lang/String", "app/Foo", "[D", "[Lapp/Foo;"] {
        let _ = ctx.ensure_class_initialized(name);
    }
    assert!(bound_method_type_shareable(&ctx, "(IJ[D)V"));
    assert!(bound_method_type_shareable(&ctx, "(Ljava/lang/String;)I"));
    assert!(!bound_method_type_shareable(&ctx, "(Lapp/Foo;)I"));
    assert!(!bound_method_type_shareable(&ctx, "()Lapp/Foo;"));
    assert!(!bound_method_type_shareable(&ctx, "([Lapp/Foo;)V"));
    assert!(!bound_method_type_shareable(
        &ctx,
        "(Ljava/util/NotLoadedHere;)V"
    ));
    assert!(!bound_method_type_shareable(
        &ctx,
        "(Ljava/lang/X$$Lambda;)V"
    ));
    assert!(!bound_method_type_shareable(&ctx, "not-a-descriptor"));
    for desc in ["(Lapp/Foo;)I", "(Ljava/util/NotLoadedHere;)V"] {
        let a = table_type(&mut ctx, desc);
        let b = table_type(&mut ctx, desc);
        assert_ne!(a, b, "{desc}: not shared");
    }
    assert!(ctx.bound_method_type_get("(Lapp/Foo;)I").is_none());
}

/// A garbage descriptor keeps the builder's `Ok(None)`.
#[test]
fn a_garbage_descriptor_answers_none() {
    let mut ctx = MockNativeContext::new();
    assert!(matches!(
        bound_method_type_through_table(&mut ctx, "x", None),
        Ok(None)
    ));
    assert!(ctx.bound_method_type_get("x").is_none());
}
}

/// JIT round 16 (lane mh): the door refusals `R16MhErrors` found missing
/// against HotSpot 25 (the return conversion, a collector at another arity,
/// `bindTo(null)`).
#[cfg(test)]
mod r16_mh_door_refusal_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn handle(ctx: &mut MockNativeContext, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, "p/C", "m", desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {desc}: {e:?}"),
    }
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

/// `(String) intToInt.invoke(3)` / `(short) ...` are `asType`'s refusal;
/// a boxing, widening, `Object` or `void` return converts.
#[test]
fn a_return_the_handle_cannot_convert_to_is_refused() {
    if !mh_strict_exact_signature() || !mh_invoke_return_refusal_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let m = handle(&mut ctx, "(I)I", MH_KIND_STATIC);
    for refused in [
        "(I)Ljava/lang/String;",
        "(I)S",
        "(I)Ljava/lang/Long;",
        "(I)Z",
    ] {
        match invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_STATIC, refused, Some("(I)I")) {
            Some(e) => assert!(is_wmte(&ctx, &e), "{refused}: {e:?}"),
            None => panic!("{refused}: an int return does not convert"),
        }
    }
    for converts in [
        "(I)Ljava/lang/Integer;",
        "(I)Ljava/lang/Number;",
        "(I)Ljava/lang/Object;",
        "(I)J",
        "(I)D",
        "(I)V",
    ] {
        assert!(
            invoke_narrowing_arg_refusal(&mut ctx, m, MH_KIND_STATIC, converts, Some("(I)I"))
                .is_none(),
            "{converts} converts"
        );
    }
}

/// A collector called at another arity converts each collected argument
/// to the component (`vsum(int...)` at `(int,String)int` is refused); an
/// array at the site's tail is the pass-through, not an element.
#[test]
fn a_collector_at_another_arity_converts_each_collected_argument() {
    if !mh_strict_exact_signature() || !mh_invoke_collector_arity_refusal_enabled() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let vsum = handle(&mut ctx, "([I)I", MH_KIND_STATIC);
    mh_set_varargs_collector(&ctx, vsum, true);
    let declared = Some("([I)I");
    match invoke_narrowing_arg_refusal(
        &mut ctx,
        vsum,
        MH_KIND_STATIC,
        "(ILjava/lang/String;)I",
        declared,
    ) {
        Some(e) => assert!(is_wmte(&ctx, &e), "{e:?}"),
        None => panic!("a String is no int"),
    }
    for converts in [
        "(III)I",
        "()I",
        "(Ljava/lang/Object;I)I",
        "(ISB)I",
        "(I)Ljava/lang/Object;",
    ] {
        assert!(
            invoke_narrowing_arg_refusal(&mut ctx, vsum, MH_KIND_STATIC, converts, declared)
                .is_none(),
            "{converts} collects"
        );
    }
    assert!(
        invoke_narrowing_arg_refusal(
            &mut ctx,
            vsum,
            MH_KIND_STATIC,
            "(II)Ljava/lang/String;",
            declared
        )
        .is_some(),
        "the return is judged at a collecting arity too"
    );
    // Not a collector: another arity is not this refusal's.
    mh_set_varargs_collector(&ctx, vsum, false);
    assert!(invoke_narrowing_arg_refusal(
        &mut ctx,
        vsum,
        MH_KIND_STATIC,
        "(ILjava/lang/String;)I",
        declared
    )
    .is_none());
    // The array pass-through at equal arity.
    let tail = handle(&mut ctx, "(Ljava/lang/String;[I)V", MH_KIND_STATIC);
    mh_set_varargs_collector(&ctx, tail, true);
    assert!(invoke_narrowing_arg_refusal(
        &mut ctx,
        tail,
        MH_KIND_STATIC,
        "(Ljava/lang/Object;[I)V",
        Some("(Ljava/lang/String;[I)V")
    )
    .is_none());
    assert_eq!(
        collected_descriptor(&["I".into(), "[J".into()], "V", "J", 2),
        "(IJJ)V"
    );
}

// `bindTo(null)` takes the insert adapter for every capture kind only.
}

/// JIT round 16 (lane mh), follow-up: the converting lane (MH16-2), the
/// `MethodType` render memo (MH16-3) and the varargs collector's array type
/// (MH5-2).
#[cfg(test)]
mod r16_mh_lane_b_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

/// `MethodType`'s `rtype` / `ptypes` declared, so a `type` reads back.
fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    for name in [
        "p/C",
        "java/lang/Integer",
        "java/lang/String",
        "[Ljava/lang/String;",
    ] {
        let _ = ctx.ensure_class_initialized(name);
    }
    let probe = mt(&mut ctx, "()V");
    let cid = ctx.class_id_of_object(probe);
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
    ctx
}

fn handle(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match alloc_method_handle(ctx, "p/C", "m", desc, MH_KIND_STATIC) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {desc}: {e:?}"),
    }
}

fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object {class}: {other:?}"),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str) -> ObjectRef {
    let t = mt(ctx, desc);
    match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, false) {
        Ok(Some(Value::Object(Some(copy)))) => copy,
        other => panic!("stamp {desc}: {other:?}"),
    }
}

/// A result as the test compares it: the class of a thrown or returned
/// object, else the value.
fn describe(ctx: &MockNativeContext, r: &MethodCallResult) -> String {
    match r {
        Ok(Some(Value::Object(Some(o)))) => {
            format!("obj {:?}", ctx.class_name_of_id(ctx.class_id_of_object(*o)))
        }
        Ok(v) => format!("{v:?}"),
        Err(MethodCallFailed::ExceptionThrown(o)) => {
            format!(
                "throw {:?}",
                ctx.class_name_of_id(ctx.class_id_of_object(*o))
            )
        }
        Err(e) => format!("{e:?}"),
    }
}

/// The converting lane answers exactly what the door answers, through both
/// doors, for the shapes it admits, and releases every pin.
#[test]
fn the_converting_lane_answers_as_the_door() {
    let mut ctx = setup();
    let h = handle(&mut ctx, "(I)I");
    let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let copy = stamp(&mut ctx, h, obj_obj);
    let int5 = object(&mut ctx, "java/lang/Integer", Value::Int(5));
    let s = ctx.create_string("x");
    let cases: Vec<(ObjectRef, Value, &str, DirectLaneDoor)> = vec![
        (copy, int5, obj_obj, DirectLaneDoor::InvokeExact),
        (
            copy,
            Value::Object(Some(s)),
            obj_obj,
            DirectLaneDoor::InvokeExact,
        ),
        (
            copy,
            Value::Object(None),
            obj_obj,
            DirectLaneDoor::InvokeExact,
        ),
        (h, int5, obj_obj, DirectLaneDoor::Invoke),
        (h, Value::Object(Some(s)), obj_obj, DirectLaneDoor::Invoke),
        (h, Value::Long(5), "(J)I", DirectLaneDoor::Invoke),
        (h, Value::Int(5), "(I)J", DirectLaneDoor::Invoke),
    ];
    for (mh, arg, site, door) in cases {
        let lane = mh_converting_lane(&mut ctx, mh, &[arg], Some(site), door);
        let Some(lane) = lane else {
            panic!("{site} {door:?}: the lane declined");
        };
        let ordinary = match door {
            DirectLaneDoor::InvokeExact => {
                mh_invoke_exact_door(&mut ctx, mh, &[arg], Some(site), true)
            }
            DirectLaneDoor::Invoke => mh_invoke_door(&mut ctx, mh, &[arg], Some(site), true),
        };
        assert_eq!(
            describe(&ctx, &lane),
            describe(&ctx, &ordinary),
            "{site} {door:?} {arg:?}"
        );
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// Everything outside the converting lane's shape is declined before it
/// has done anything.
#[test]
fn the_converting_lane_declines_what_it_does_not_model() {
    let mut ctx = setup();
    let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let h = handle(&mut ctx, "(I)I");
    let copy = stamp(&mut ctx, h, obj_obj);
    let int5 = object(&mut ctx, "java/lang/Integer", Value::Int(5));
    let exact = DirectLaneDoor::InvokeExact;
    // `invokeExact` at a site that is not the declared type.
    assert!(mh_converting_lane(&mut ctx, copy, &[int5], Some("(I)I"), exact).is_none());
    // No call site; another arity.
    assert!(mh_converting_lane(&mut ctx, copy, &[int5], None, exact).is_none());
    assert!(
        mh_converting_lane(&mut ctx, copy, &[], Some("()Ljava/lang/Object;"), exact).is_none()
    );
    // A collector, an explicit stamp, a bound handle, a virtual handle.
    let collector = stamp(&mut ctx, h, obj_obj);
    mh_set_varargs_collector(&ctx, collector, true);
    assert!(mh_converting_lane(&mut ctx, collector, &[int5], Some(obj_obj), exact).is_none());
    let explicit = stamp(&mut ctx, h, obj_obj);
    mh_mark_explicit_cast(&ctx, explicit);
    assert!(mh_converting_lane(&mut ctx, explicit, &[int5], Some(obj_obj), exact).is_none());
    let bound = handle(&mut ctx, "(Ljava/lang/Object;)I");
    ctx.set_field(bound, MH_BOUND, int5);
    assert!(mh_converting_lane(&mut ctx, bound, &[], Some("()I"), exact).is_none());
    let virt = match alloc_method_handle(&mut ctx, "p/C", "m", "(I)I", MH_KIND_VIRTUAL) {
        Ok(v) => v,
        Err(e) => panic!("alloc virtual: {e:?}"),
    };
    assert!(mh_converting_lane(
        &mut ctx,
        virt,
        &[int5, Value::Int(1)],
        Some("(Lp/C;I)I"),
        DirectLaneDoor::Invoke
    )
    .is_none());
    // An array parameter anywhere.
    let arr = handle(&mut ctx, "([I)I");
    let empty = Value::Object(Some(ctx.new_array(ArrayElementType::Int, 0)));
    assert!(mh_converting_lane(&mut ctx, arr, &[empty], Some("([I)I"), exact).is_none());
    assert_eq!(
        ctx.native_pin_count_for_test(),
        0,
        "nothing pinned by a decline"
    );
}

/// MH16-3: a second render of one `MethodType` in one epoch is the memo's
/// answer (shown by rewriting `ptypes` behind it, which the invariant
/// forbids in production), the plain render is not memoised, and a
/// context with VM identity `0` is never memoised.
#[test]
fn the_type_render_memo_answers_a_second_render() {
    let mut ctx = setup();
    ctx.set_vm_identity(0x16A1_0001);
    let t = mt(&mut ctx, "(I)I");
    assert_eq!(
        methodtype_to_descriptor_memoised(&mut ctx, t).as_deref(),
        Some("(I)I")
    );
    let long = ctx.primitive_class_mirror("long");
    let Value::Object(Some(ptypes)) = ctx.get_field_by_name(t, "ptypes") else {
        panic!("no ptypes");
    };
    ctx.set_array_element(ptypes, 0, Value::Object(Some(long)));
    assert_eq!(
        methodtype_to_descriptor_memoised(&mut ctx, t).as_deref(),
        Some("(I)I")
    );
    assert_eq!(
        methodtype_to_descriptor_rendered(&mut ctx, t).as_deref(),
        Some("(J)I")
    );
    let mut plain = setup();
    let u = mt(&mut plain, "(I)I");
    assert_eq!(
        methodtype_to_descriptor_memoised(&mut plain, u).as_deref(),
        Some("(I)I")
    );
    let Value::Object(Some(ptypes)) = plain.get_field_by_name(u, "ptypes") else {
        panic!("no ptypes");
    };
    let long = plain.primitive_class_mirror("long");
    plain.set_array_element(ptypes, 0, Value::Object(Some(long)));
    assert_eq!(
        methodtype_to_descriptor_memoised(&mut plain, u).as_deref(),
        Some("(J)I")
    );
}

/// MH5-2: `asVarargsCollector(String[].class)` over `(Object,Object)Object`
/// answers a collector of type `(Object,String[])Object` (the receiver
/// untouched), and its entry collects against `String[]`.
#[test]
fn a_varargs_collector_takes_its_array_type() {
    if !mh_varargs_array_type_enabled() {
        return;
    }
    let mut ctx = setup();
    let two = "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;";
    let collected = "(Ljava/lang/Object;[Ljava/lang/String;)Ljava/lang/Object;";
    let h = handle(&mut ctx, two);
    let answer =
        mh_as_varargs_collector_retyped(&mut ctx, Some(Value::Object(Some(h))), collected);
    let c = match answer {
        Some(Ok(Some(Value::Object(Some(c))))) => c,
        other => panic!("retyped: {other:?}"),
    };
    assert_ne!(c, h, "a copy");
    assert!(mh_is_varargs_collector(&ctx, c));
    assert!(!mh_is_varargs_collector(&ctx, h));
    assert_eq!(
        mh_declared_descriptor(&mut ctx, c).as_deref(),
        Some(collected)
    );
    assert_eq!(mh_declared_descriptor(&mut ctx, h).as_deref(), Some(two));
    assert_eq!(
        mh_collector_declared_array_desc(&mut ctx, c, two).as_deref(),
        Some(collected)
    );
    assert!(mh_collector_declared_array_desc(&mut ctx, h, two).is_none());
    // A primitive trailing leaf parameter is never retyped at entry.
    let prim_tail = "(Ljava/lang/Object;I)Ljava/lang/Object;";
    assert!(mh_collector_declared_array_desc(&mut ctx, c, prim_tail).is_none());
}
}

/// JIT round 17 wave 2 (lane mh2): the names the `invokeExact` loader check
/// ([`mh_exact_loader_refusal`]) asks about.
#[cfg(test)]
mod r17w2_mh_exact_loader_tests {
use super::*;

fn names(desc: &str) -> Vec<String> {
    let mut out = Vec::new();
    mh_descriptor_any_loader_name(desc, |n| {
        out.push(n.to_string());
        false
    });
    out
}

#[test]
fn the_loader_names_skip_primitives_arrays_and_java() {
    assert!(names("(IJ)I").is_empty());
    assert!(names("(Ljava/lang/String;)Ljava/lang/Object;").is_empty());
    assert_eq!(names("(LBox;[Lp/Lib;I)Lq/R;"), ["Box", "p/Lib", "q/R"]);
    assert!(mh_descriptor_any_loader_name("(ILBox;)V", |n| n == "Box"));
    assert!(!mh_descriptor_any_loader_name("(ILBox;)V", |n| n == "Other"));
}

// JIT round 17 wave 2 (lane mh3): the `invoke` loader cast is a
// correctness fix with a `0` kill switch.
}

/// JIT round 16 (lane mh), wave 3: the admission of a constant handle to a
/// compiled site's direct call (MH15-5, [`mh_compiled_bind_target`]).
#[cfg(test)]
mod r16_mh_compiled_bind_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

fn method(cid: ClassId, name: &str, desc: &str, flags: u16) -> MethodMetadata {
    MethodMetadata {
        name: name.to_string(),
        descriptor: desc.to_string(),
        access_flags: flags,
        declaring_class_id: cid,
        exceptions: Vec::new(),
        signature: None,
    }
}

/// `p/C` with `static m(I)I`, `v(I)I`, private `p(I)I`, `s(Lp/I;)I`
/// (an interface parameter) and `t(Lp/C;)I`; `p/I` an interface.
fn setup() -> (MockNativeContext, ClassId) {
    let mut ctx = MockNativeContext::new();
    let mut cid = ClassId::new(0);
    for name in ["p/C", "p/I", "java/lang/String"] {
        let id = ctx.ensure_class_initialized(name).unwrap();
        if name == "p/C" {
            cid = id;
        }
        if name == "p/I" {
            ctx.set_is_interface(id, true);
        }
    }
    ctx.set_declared_methods(
        cid,
        vec![
            method(cid, "m", "(I)I", 0x0009),
            method(cid, "v", "(I)I", 0x0001),
            method(cid, "p", "(I)I", 0x0002),
            method(cid, "s", "(Lp/I;)I", 0x0009),
            method(cid, "t", "(Lp/C;)I", 0x0009),
        ],
    );
    let probe = mt(&mut ctx, "()V");
    let mt_cid = ctx.class_id_of_object(probe);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: mt_cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        mt_cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
    (ctx, cid)
}

fn handle(ctx: &mut MockNativeContext, name: &str, desc: &str, kind: i32) -> ObjectRef {
    match alloc_method_handle(ctx, "p/C", name, desc, kind) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc {name}{desc}: {e:?}"),
    }
}

#[test]
fn a_direct_handle_at_its_own_type_binds_as_its_member() {
    let (mut ctx, cid) = setup();
    let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
    assert_eq!(
        mh_compiled_bind_target(&mut ctx, st, "(I)I"),
        Some(MhCompiledBind {
            invoke_kind: 3,
            class_id: cid.as_u32(),
            class: "p/C".into(),
            name: "m".into(),
            desc: "(I)I".into(),
            bound: false,
        })
    );
    let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    assert_eq!(
        mh_compiled_bind_target(&mut ctx, vi, "(Lp/C;I)I").map(|b| b.invoke_kind),
        Some(0)
    );
    let ref_param = handle(&mut ctx, "t", "(Lp/C;)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_target(&mut ctx, ref_param, "(Lp/C;)I").is_some());
}

#[test]
fn every_shape_the_door_could_refuse_or_reshape_is_refused() {
    let (mut ctx, _) = setup();
    let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
    // Another site type; a marking bit; a bound value.
    assert!(mh_compiled_bind_target(&mut ctx, st, "(J)I").is_none());
    let collector = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
    mh_set_varargs_collector(&ctx, collector, true);
    assert!(mh_compiled_bind_target(&mut ctx, collector, "(I)I").is_none());
    let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    let s = ctx.create_string("x");
    ctx.set_field(vi, MH_BOUND, Value::Object(Some(s)));
    assert!(mh_compiled_bind_target(&mut ctx, vi, "(I)I").is_none());
    // An interface parameter (the verifier does not type it).
    let iface = handle(&mut ctx, "s", "(Lp/I;)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_target(&mut ctx, iface, "(Lp/I;)I").is_none());
    // A private virtual target; a kind that is not the member's.
    let private = handle(&mut ctx, "p", "(I)I", MH_KIND_VIRTUAL);
    assert!(mh_compiled_bind_target(&mut ctx, private, "(Lp/C;I)I").is_none());
    let not_static = handle(&mut ctx, "v", "(I)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_target(&mut ctx, not_static, "(I)I").is_none());
    // A member the class does not declare.
    let missing = handle(&mut ctx, "q", "(I)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_target(&mut ctx, missing, "(I)I").is_none());
    // A JDK member (caller-sensitive methods see their caller).
    let jdk = match alloc_method_handle(
        &mut ctx,
        "java/lang/String",
        "valueOf",
        "(I)Ljava/lang/String;",
        MH_KIND_STATIC,
    ) {
        Ok(h) => h,
        Err(e) => panic!("alloc jdk: {e:?}"),
    };
    assert!(mh_compiled_bind_target(&mut ctx, jdk, "(I)Ljava/lang/String;").is_none());
    // An array parameter.
    let arr = handle(&mut ctx, "m", "([I)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_target(&mut ctx, arr, "([I)I").is_none());
    // The guard slots are the decided-on ones, each below the minted width.
    assert!(MH_COMPILED_BIND_GUARD_SLOTS
        .iter()
        .all(|s| *s <= MH_STATIC_OWNER));
}

/// A `bindTo` of `v(I)I` as `bindTo` mints it: `MH_BOUND` the receiver,
/// `type` the member's own descriptor.
fn bound_virtual(ctx: &mut MockNativeContext, receiver: ObjectRef) -> ObjectRef {
    let h = handle(ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    ctx.set_field(h, MH_BOUND, Value::Object(Some(receiver)));
    let t = mt(ctx, "(I)I");
    ctx.set_field_by_name(h, "type", Value::Object(Some(t)));
    h
}

/// Round 16 wave 8 (lane mh): a bound virtual handle binds as its member,
/// called on the bound receiver, only with the switch's parameter on, and
/// only at its bound type.
#[test]
fn a_bound_virtual_handle_binds_as_its_member_on_its_receiver() {
    let (mut ctx, cid) = setup();
    let receiver = ctx.alloc_object(cid, 4);
    let bound = bound_virtual(&mut ctx, receiver);
    assert!(mh_compiled_bind_target_with(&mut ctx, bound, "(I)I", false).is_none());
    assert_eq!(
        mh_compiled_bind_target_with(&mut ctx, bound, "(I)I", true),
        Some(MhCompiledBind {
            invoke_kind: 0,
            class_id: cid.as_u32(),
            class: "p/C".into(),
            name: "v".into(),
            desc: "(I)I".into(),
            bound: true,
        })
    );
    // The receiver is no call-site argument.
    assert!(mh_compiled_bind_target_with(&mut ctx, bound, "(Lp/C;I)I", true).is_none());
    // A receiver of a subclass binds too (`invokevirtual` selects on it).
    let sub = ctx.ensure_class_initialized("p/D").unwrap();
    ctx.set_superclass(sub, cid);
    let sub_receiver = ctx.alloc_object(sub, 4);
    let bound_sub = bound_virtual(&mut ctx, sub_receiver);
    assert!(
        mh_compiled_bind_target_with(&mut ctx, bound_sub, "(I)I", true)
            .is_some_and(|b| b.bound)
    );
    // The helper reads the receiver from the `MH_BOUND` guard word.
    assert_eq!(
        MH_COMPILED_BIND_GUARD_SLOTS[MH_COMPILED_BIND_BOUND_GUARD],
        MH_BOUND
    );
}

/// Round 16 wave 8 (lane mh): what the bound admission refuses -- a bound
/// value the member's class does not admit (`bindTo` would have refused
/// it; the door's `leaf_receiver_refusal` would), an array, a bound
/// STATIC handle (its leading argument is the door's to place), and a
/// `type` that is not the member's own descriptor.
#[test]
fn a_bound_receiver_the_member_does_not_take_is_refused() {
    let (mut ctx, cid) = setup();
    let s = ctx.create_string("x");
    let wrong = bound_virtual(&mut ctx, s);
    assert!(mh_compiled_bind_target_with(&mut ctx, wrong, "(I)I", true).is_none());
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    let array_bound = bound_virtual(&mut ctx, arr);
    assert!(mh_compiled_bind_target_with(&mut ctx, array_bound, "(I)I", true).is_none());
    let receiver = ctx.alloc_object(cid, 4);
    let st = handle(&mut ctx, "t", "(Lp/C;)I", MH_KIND_STATIC);
    ctx.set_field(st, MH_BOUND, Value::Object(Some(receiver)));
    let t = mt(&mut ctx, "()I");
    ctx.set_field_by_name(st, "type", Value::Object(Some(t)));
    assert!(mh_compiled_bind_target_with(&mut ctx, st, "()I", true).is_none());
    let stale_type = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    ctx.set_field(stale_type, MH_BOUND, Value::Object(Some(receiver)));
    // `type` left the unbound `(Lp/C;I)I`: not the bound site's.
    assert!(mh_compiled_bind_target_with(&mut ctx, stale_type, "(I)I", true).is_none());
}

/// The wrapper classes the converting admission names.
fn ensure_wrappers(ctx: &mut MockNativeContext) -> ClassId {
    for name in ["java/lang/Object", "java/lang/Number", "java/lang/Long"] {
        ctx.ensure_class_initialized(name).unwrap();
    }
    ctx.ensure_class_initialized("java/lang/Integer").unwrap()
}

/// `asType(t)` (or `explicitCastArguments`) of `h`, as the native answers it.
fn stamp(ctx: &mut MockNativeContext, h: ObjectRef, desc: &str, explicit: bool) -> ObjectRef {
    let t = mt(ctx, desc);
    match mh_with_stamped_type(ctx, Value::Object(Some(h)), t, explicit) {
        Ok(Some(Value::Object(Some(copy)))) => copy,
        other => panic!("asType {desc}: {other:?}"),
    }
}

/// JIT round 16 wave 15 (lane mh6; MHB-1): the closed set of conversions,
/// position by position, and nothing else.
#[test]
fn the_converting_bind_models_only_the_int_wrapper_conversions() {
    let obj = "Ljava/lang/Object;";
    let cases: [(&str, &str, Option<(u32, bool)>); 9] = [
        (
            "(Ljava/lang/Object;)Ljava/lang/Object;",
            "(I)I",
            Some((1, true)),
        ),
        ("(Ljava/lang/Integer;)I", "(I)I", Some((1, false))),
        ("(I)Ljava/lang/Number;", "(I)I", Some((0, true))),
        (
            "(Lp/C;Ljava/lang/Object;)I",
            "(Lp/C;I)I",
            Some((0b10, false)),
        ),
        // Nothing converts: the exact bind's.
        ("(I)I", "(I)I", None),
        // Widening, another wrapper, a dropped return, a narrowing.
        ("(S)I", "(I)I", None),
        ("(Ljava/lang/Long;)I", "(I)I", None),
        ("(I)V", "(I)I", None),
        ("(Ljava/lang/Object;)Ljava/lang/Object;", "(J)J", None),
    ];
    for (from, to, want) in cases {
        assert_eq!(mh_bind_conversions(from, to), want, "{from} -> {to}");
    }
    assert!(mh_bind_int_wrapper_token(obj));
    assert!(!mh_bind_int_wrapper_token("Ljava/lang/String;"));
}

/// JIT round 16 wave 15 (lane mh6; MHB-1): `m(I)I` stamped to
/// `(Object)Object` binds at its declared type through either door, and
/// the unstamped handle binds an `invoke` at a converting site; an
/// `invokeExact` there stays the door's (`WrongMethodTypeException`).
#[test]
fn a_one_hop_int_conversion_binds_with_its_conversions() {
    let (mut ctx, cid) = setup();
    let integer = ensure_wrappers(&mut ctx);
    let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
    let copy = stamp(&mut ctx, st, obj_obj, false);
    let want = MhCompiledBind {
        invoke_kind: 3,
        class_id: cid.as_u32(),
        class: "p/C".into(),
        name: "m".into(),
        desc: "(I)I".into(),
        bound: false,
    };
    let conv = MhBindConversion {
        unbox_int: 1,
        box_int_return: true,
        integer_class_id: integer.as_u32(),
    };
    assert!(mh_compiled_bind_target(&mut ctx, copy, obj_obj).is_none());
    for invoke_door in [false, true] {
        assert_eq!(
            mh_compiled_bind_converting_target(&mut ctx, copy, obj_obj, invoke_door),
            Some((want.clone(), conv))
        );
    }
    assert_eq!(
        mh_compiled_bind_converting_target(&mut ctx, st, obj_obj, true),
        Some((want.clone(), conv))
    );
    assert!(mh_compiled_bind_converting_target(&mut ctx, st, obj_obj, false).is_none());
    // The member's own type is the exact bind's, not this one's.
    assert!(mh_compiled_bind_converting_target(&mut ctx, st, "(I)I", true).is_none());
    // An unbound virtual handle: its receiver passes through as word 0.
    let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    assert_eq!(
        mh_compiled_bind_converting_target(
            &mut ctx,
            vi,
            "(Lp/C;Ljava/lang/Object;)Ljava/lang/Object;",
            true
        )
        .map(|(b, c)| (b.invoke_kind, c.unbox_int, c.box_int_return)),
        Some((0, 0b10, true))
    );
}

/// JIT round 16 wave 15 (lane mh6; MHB-1): what the converting admission
/// leaves to the door -- two `asType` hops (a copy of a copy, `invoke` on
/// a copy at another type), an `explicitCastArguments` copy, a bound
/// handle, an interface parameter, and a receiver the site retypes.
#[test]
fn a_conversion_past_one_hop_or_off_the_exact_rules_is_refused() {
    let (mut ctx, cid) = setup();
    ensure_wrappers(&mut ctx);
    let obj_obj = "(Ljava/lang/Object;)Ljava/lang/Object;";
    let st = handle(&mut ctx, "m", "(I)I", MH_KIND_STATIC);
    let c1 = stamp(&mut ctx, st, "(Ljava/lang/Integer;)I", false);
    let c2 = stamp(&mut ctx, c1, "(Ljava/lang/Object;)I", false);
    assert!(
        mh_compiled_bind_converting_target(&mut ctx, c2, "(Ljava/lang/Object;)I", false)
            .is_none()
    );
    assert!(
        mh_compiled_bind_converting_target(&mut ctx, c1, "(Ljava/lang/Object;)I", true)
            .is_none()
    );
    let explicit = stamp(&mut ctx, st, obj_obj, true);
    assert!(mh_compiled_bind_converting_target(&mut ctx, explicit, obj_obj, false).is_none());
    let receiver = ctx.alloc_object(cid, 4);
    let bound = bound_virtual(&mut ctx, receiver);
    assert!(mh_compiled_bind_converting_target(&mut ctx, bound, obj_obj, true).is_none());
    let iface = handle(&mut ctx, "s", "(Lp/I;)I", MH_KIND_STATIC);
    assert!(mh_compiled_bind_converting_target(
        &mut ctx,
        iface,
        "(Lp/I;)Ljava/lang/Object;",
        true
    )
    .is_none());
    let vi = handle(&mut ctx, "v", "(I)I", MH_KIND_VIRTUAL);
    assert!(mh_compiled_bind_converting_target(
        &mut ctx,
        vi,
        "(Ljava/lang/Object;Ljava/lang/Object;)I",
        true
    )
    .is_none());
}
}

/// JIT round 16 wave 8 (lane mh; proposal MH5-1 of
/// `jit-r15w6-mh5-proposals-RETIRED-20261003.md`): door natives under the moving mock heap
/// (`MockNativeContext::set_moving_heap`). Every allocation and every Java call
/// relocates every object and retires the old addresses, so a native that
/// keeps a reference across one without a pin fails here ("stale ObjectRef")
/// instead of passing, as it did with the never-moving mock. The test's own
/// locals are not roots: each test re-reads what it holds through a global
/// root, and reads results through what the native returned. Each context has
/// a private VM identity (the `valueOf` caches and the address-keyed memos are
/// per VM); boxed values sit outside the `Integer` cache (a process-wide
/// per-VM table this mock does not forward).
#[cfg(test)]
mod r16w8_mh_moving_heap_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

const ONE: &str = "(Ljava/lang/String;)Ljava/lang/String;";

fn moving_ctx(identity: usize) -> MockNativeContext {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(identity);
    ctx
}

fn root(ctx: &MockNativeContext, handle: usize) -> ObjectRef {
    match ctx.resolve_global_root(handle) {
        Some(o) => o,
        None => panic!("global root {handle} is gone"),
    }
}

/// `alloc_resolved_member_name` (`InvokerBytecodeGenerator`'s customized-
/// code / entry-point bypasses) keeps its new `MemberName` rooted across
/// the mirror, name-string and `MethodType` allocations. As a raw local,
/// a G1 pause between them sent the vmindex sentinel into the real
/// object's `method` slot and the later stores (`type`, `flags`, a null
/// `resolution`) into whatever object took the old address
/// (`ObjectStreamLookupGcRaceProbe`'s early stream NPE). Under the moving
/// mock every allocation relocates the heap, so a stale `mn` panics.
#[test]
fn a_resolved_member_name_survives_a_relocation_per_allocation() {
    let mut ctx = moving_ctx(0x16A8_0002);
    ctx.set_moving_heap(true);
    let before = ctx.relocations_for_test();
    let mn = alloc_resolved_member_name(
        &mut ctx,
        "java/lang/invoke/LambdaForm",
        "interpret_V",
        "()V",
    )
    .expect("a MemberName is built");
    assert!(
        ctx.relocations_for_test() >= before + 3,
        "the mirror, the name and the MethodType each moved the heap"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    match mn_get(&mut ctx, mn, "name", MN_NAME) {
        Value::Object(Some(s)) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("interpret_V"))
        }
        other => panic!("name: {other:?}"),
    }
    assert!(matches!(
        mn_get(&mut ctx, mn, "type", MN_TYPE),
        Value::Object(Some(_))
    ));
    assert_ne!(mn_get_vmindex(&mut ctx, mn), 0);
    assert!(matches!(
        mn_get(&mut ctx, mn, "resolution", MN_RESOLUTION),
        Value::Object(None)
    ));
}

/// `build_varargs_array` into an `Object[]`: the array and both strings
/// are held across the boxing allocation (round 15 wave 6's GC-safety fix,
/// found by reading); under the moving mock each must be read back through
/// its pin, or this test fails.
#[test]
fn varargs_collection_survives_a_relocation_per_box() {
    let mut ctx = moving_ctx(0x16A8_0001);
    let a = Value::Object(Some(ctx.create_string("a")));
    let b = Value::Object(Some(ctx.create_string("b")));
    ctx.set_moving_heap(true);
    let before = ctx.relocations_for_test();
    let arr = build_varargs_array(
        &mut ctx,
        "Ljava/lang/Object;",
        &[a, Value::Int(1000), b],
        None,
        0,
    )
    .expect("an Object[] is built");
    assert!(
        ctx.relocations_for_test() >= before + 2,
        "the array and the box moved the heap"
    );
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    let element = |ctx: &MockNativeContext, i: usize| match ctx.get_array_element(arr, i) {
        Value::Object(Some(o)) => o,
        other => panic!("element {i}: {other:?}"),
    };
    assert_eq!(ctx.read_string(element(&ctx, 0)).as_deref(), Some("a"));
    assert_eq!(
        crate::lang_class::unbox_value(&ctx, element(&ctx, 1)),
        Value::Int(1000)
    );
    assert_eq!(ctx.read_string(element(&ctx, 2)).as_deref(), Some("b"));
}

/// `bindTo` of a virtual handle allocates the bound handle and its `type`
/// with the target and the receiver held: both are read back through
/// their pins (gen r4w3/rooting).
#[test]
fn bind_to_keeps_its_receiver_across_its_allocations() {
    let mut ctx = moving_ctx(0x16A8_0002);
    let concat =
        match alloc_method_handle(&mut ctx, "java/lang/String", "concat", ONE, MH_KIND_VIRTUAL)
        {
            Ok(mh) => mh,
            Err(e) => panic!("alloc: {e:?}"),
        };
    ctx.set_field_by_name(concat, "type", Value::Object(None));
    let dash = ctx.create_string("-");
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    let bind = registry
        .find(
            "java/lang/invoke/MethodHandle",
            "bindTo",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandle.bindTo should be registered");
    ctx.set_moving_heap(true);
    let before = ctx.relocations_for_test();
    let bound = match bind(
        &mut ctx,
        &[Value::Object(Some(concat)), Value::Object(Some(dash))],
    ) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("bindTo answered {other:?}"),
    };
    assert!(ctx.relocations_for_test() > before, "bindTo allocated");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
    assert_eq!(ctx.get_field(bound, MH_KIND), Value::Int(MH_KIND_VIRTUAL));
    match ctx.get_field(bound, MH_BOUND) {
        Value::Object(Some(r)) => assert_eq!(ctx.read_string(r).as_deref(), Some("-")),
        other => panic!("no bound receiver: {other:?}"),
    }
    assert_eq!(mh_read_name(&ctx, bound).as_deref(), Some("concat"));
}

/// `m(I)I` answers its argument plus 100, after checking that the
/// receiver it was handed is current (the mock forwards a Java call's
/// receiver as the VM's frame does).
fn plus_hundred(
    ctx: &mut MockNativeContext,
    receiver: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> Option<MethodCallResult> {
    let _ = ctx.class_id_of_object(receiver);
    match (name, desc, args.first()) {
        ("m", "(I)I", Some(Value::Int(x))) => Some(Ok(Some(Value::Int(x + 100)))),
        _ => None,
    }
}

fn answered_int(ctx: &MockNativeContext, r: &Option<MethodCallResult>) -> Option<i32> {
    match r {
        Some(Ok(Some(Value::Int(v)))) => Some(*v),
        Some(Ok(Some(Value::Object(Some(o))))) => match crate::lang_class::unbox_value(ctx, *o)
        {
            Value::Int(v) => Some(v),
            _ => None,
        },
        _ => None,
    }
}

/// `MethodType`'s `rtype` / `ptypes` declared (as in
/// `r13w10_callcost4_direct_lane_tests`), so a handle's `type` reads back
/// and the direct lane engages.
fn lane_ctx(identity: usize) -> MockNativeContext {
    let mut ctx = moving_ctx(identity);
    let _ = ctx.ensure_class_initialized("p/Mid");
    let probe = match build_method_type_from_descriptor(&mut ctx, "()V") {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType: {other:?}"),
    };
    let cid = ctx.class_id_of_object(probe);
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
    ctx
}

/// The direct lane's virtual arm, through both doors, unbound and bound:
/// the leaf call relocates the heap, and what the lane reads after it (the
/// receiver for the serialization-hook rule, the answer) must be current.
#[test]
fn the_direct_lane_answers_across_a_relocating_leaf_call() {
    let mut ctx = lane_ctx(0x16A8_0003);
    let mh = match alloc_method_handle(&mut ctx, "p/Mid", "m", "(I)I", MH_KIND_VIRTUAL) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc: {e:?}"),
    };
    let recv = match ctx.new_object("p/Mid") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object: {other:?}"),
    };
    // `bindTo`'s shape: the receiver in `MH_BOUND`, `type` without it.
    let bound = match alloc_method_handle(&mut ctx, "p/Mid", "m", "(I)I", MH_KIND_VIRTUAL) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc: {e:?}"),
    };
    ctx.set_field(bound, MH_BOUND, Value::Object(Some(recv)));
    let dropped = match build_method_type_from_descriptor(&mut ctx, "(I)I") {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType: {other:?}"),
    };
    ctx.set_field_by_name(bound, "type", Value::Object(Some(dropped)));
    let mh_root = ctx.add_global_root(mh);
    let recv_root = ctx.add_global_root(recv);
    let bound_root = ctx.add_global_root(bound);
    ctx.set_invoke_virtual_hook(plus_hundred);
    ctx.set_moving_heap(true);
    for door in [DirectLaneDoor::InvokeExact, DirectLaneDoor::Invoke] {
        let before = ctx.relocations_for_test();
        let (mh, recv) = (root(&ctx, mh_root), root(&ctx, recv_root));
        let r = mh_direct_lane(
            &mut ctx,
            mh,
            &[Value::Object(Some(recv)), Value::Int(5)],
            Some("(Lp/Mid;I)I"),
            door,
        );
        assert_eq!(answered_int(&ctx, &r), Some(105), "{door:?}: {r:?}");
        assert!(
            ctx.relocations_for_test() > before,
            "{door:?}: the leaf call moved"
        );
        let bound = root(&ctx, bound_root);
        let r = mh_direct_lane(&mut ctx, bound, &[Value::Int(7)], Some("(I)I"), door);
        assert_eq!(answered_int(&ctx, &r), Some(107), "bound {door:?}: {r:?}");
    }
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");    }}

/// JIT round 16 wave 11 (lane mhbind; proposal MHF-2): the `bindTo` fast mint.
/// The memo is forced on for the test's own thread (`BINDTO_MINT_TEST_VM`),
/// so the mock context (VM identity `0`, switch off) exercises it.
#[cfg(test)]
mod r16w11_mhbind_fast_mint_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, MethodMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// Forces the fast mint on for this thread; off again when dropped.
struct ForcedOn;
impl ForcedOn {
    fn new() -> Self {
        BINDTO_MINT_TEST_VM.with(|v| v.set(Some(0x16B1_0001)));
        BINDTO_MINT_TEST_HITS.with(|h| h.set(0));
        BINDTO_MINT_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
        ForcedOn
    }
}
impl Drop for ForcedOn {
    fn drop(&mut self) {
        BINDTO_MINT_TEST_VM.with(|v| v.set(None));
        BINDTO_MINT_MEMO.with(|m| m.borrow_mut().iter_mut().for_each(|l| *l = None));
    }
}

fn hits() -> u64 {
    BINDTO_MINT_TEST_HITS.with(|h| h.get())
}

fn mt(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match build_method_type_from_descriptor(ctx, desc) {
        Ok(Some(mt)) => mt,
        other => panic!("MethodType {desc}: {other:?}"),
    }
}

/// `p/C` (with `v(I)I`) and its subclass `p/D`, and `MethodType`'s
/// `rtype` / `ptypes` declared so a handle's `type` renders.
fn setup() -> (MockNativeContext, ClassId, ClassId) {
    let mut ctx = MockNativeContext::new();
    let cid = ctx.ensure_class_initialized("p/C").unwrap();
    let sub = ctx.ensure_class_initialized("p/D").unwrap();
    ctx.set_superclass(sub, cid);
    let _ = ctx.ensure_class_initialized("java/lang/String");
    ctx.set_declared_methods(
        cid,
        vec![MethodMetadata {
            name: "v".to_string(),
            descriptor: "(I)I".to_string(),
            access_flags: 0x0001,
            declaring_class_id: cid,
            exceptions: Vec::new(),
            signature: None,
        }],
    );
    let probe = mt(&mut ctx, "()V");
    let mt_cid = ctx.class_id_of_object(probe);
    let field = |name: &str, descriptor: &str, slot_index: usize| FieldMetadata {
        name: name.to_string(),
        descriptor: descriptor.to_string(),
        access_flags: 0,
        slot_index,
        declaring_class_id: mt_cid,
        is_static: false,
    };
    ctx.set_declared_fields(
        mt_cid,
        vec![
            field("rtype", "Ljava/lang/Class;", 0),
            field("ptypes", "[Ljava/lang/Class;", 1),
        ],
    );
    (ctx, cid, sub)
}

fn virtual_v(ctx: &mut MockNativeContext) -> ObjectRef {
    match alloc_method_handle(ctx, "p/C", "v", "(I)I", MH_KIND_VIRTUAL) {
        Ok(mh) => mh,
        Err(e) => panic!("alloc v: {e:?}"),
    }
}

fn bind(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> MethodCallResult {
    let mut registry = NativeMethodRegistry::new();
    register_t4_method_handle_invoke(&mut registry);
    let bind = registry
        .find(
            "java/lang/invoke/MethodHandle",
            "bindTo",
            "(Ljava/lang/Object;)Ljava/lang/invoke/MethodHandle;",
        )
        .expect("MethodHandle.bindTo should be registered");
    bind(ctx, &[Value::Object(Some(target)), value])
}

fn bound(ctx: &mut MockNativeContext, target: ObjectRef, value: Value) -> ObjectRef {
    match bind(ctx, target, value) {
        Ok(Some(Value::Object(Some(h)))) => h,
        other => panic!("bindTo answered {other:?}"),
    }
}

/// Every slot a bound handle is read by, as words, plus its rendered `type`.
fn shape(ctx: &mut MockNativeContext, h: ObjectRef) -> (Vec<u64>, Option<String>) {
    let words = [
        MH_CLASS,
        MH_NAME,
        MH_DESC,
        MH_KIND,
        MH_BOUND,
        MH_VARARGS,
        MH_STATIC_SETTLED,
        MH_SPECIAL_CALLER,
        MH_CAST_CHAIN,
        MH_STATIC_OWNER,
    ]
    .iter()
    .map(|s| bindto_mint_word(ctx.get_field(h, *s)))
    .collect();
    (words, mh_type_descriptor(ctx, h))
}

/// The second bind of one source is a hit, and the handle it mints is the
/// slow path's, slot for slot (the slow path asked again with the memo
/// off), with its own bound value.
#[test]
fn a_repeat_bind_is_a_hit_and_mints_the_slow_paths_handle() {
    let (mut ctx, cid, _) = setup();
    let src = virtual_v(&mut ctx);
    let r1 = ctx.alloc_object(cid, 4);
    let r2 = ctx.alloc_object(cid, 4);
    let (fast, first) = {
        let _on = ForcedOn::new();
        let first = bound(&mut ctx, src, Value::Object(Some(r1)));
        assert_eq!(hits(), 0, "the first bind fills");
        let fast = bound(&mut ctx, src, Value::Object(Some(r2)));
        assert_eq!(hits(), 1, "the second bind is a hit");
        (fast, first)
    };
    let slow = bound(&mut ctx, src, Value::Object(Some(r2)));
    assert_eq!(shape(&mut ctx, fast), shape(&mut ctx, slow));
    assert_eq!(mh_type_descriptor(&mut ctx, fast).as_deref(), Some("(I)I"));
    assert_eq!(ctx.get_field(fast, MH_BOUND), Value::Object(Some(r2)));
    assert_eq!(ctx.get_field(first, MH_BOUND), Value::Object(Some(r1)));
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// What is not a hit: a bound value of another class (a subclass the cast
/// admits, and one it cannot judge), a null (the INSERT adapter), and a
/// source whose marking changed in place since the line was filled.
#[test]
fn another_class_a_null_or_a_changed_source_is_the_slow_path() {
    let (mut ctx, cid, sub) = setup();
    let _on = ForcedOn::new();
    let src = virtual_v(&mut ctx);
    let r1 = ctx.alloc_object(cid, 4);
    let _ = bound(&mut ctx, src, Value::Object(Some(r1)));
    let d = ctx.alloc_object(sub, 4);
    let by_sub = bound(&mut ctx, src, Value::Object(Some(d)));
    assert_eq!(ctx.get_field(by_sub, MH_BOUND), Value::Object(Some(d)));
    assert_eq!(hits(), 0, "a subclass is not the remembered class");
    // A value the mock's hierarchy cannot judge (`None`): whatever the
    // slow path answers, it is not a hit and the line is not refilled.
    let s = ctx.create_string("x");
    let _ = bind(&mut ctx, src, Value::Object(Some(s)));
    assert_eq!(hits(), 0);
    let by_null = bound(&mut ctx, src, Value::Object(None));
    assert_eq!(ctx.get_field(by_null, MH_KIND), Value::Int(MH_KIND_INSERT));
    assert_eq!(hits(), 0);
    // The line now remembers `p/D` (the last fill); a `p/D` hits.
    let d2 = ctx.alloc_object(sub, 4);
    let _ = bound(&mut ctx, src, Value::Object(Some(d2)));
    assert_eq!(hits(), 1);
    // A slot rewritten in place is a miss.
    mh_set_varargs_collector(&ctx, src, true);
    let _ = bind(&mut ctx, src, Value::Object(Some(d2)));
    assert_eq!(hits(), 1, "the marking is in the key");
    assert_eq!(ctx.native_pin_count_for_test(), 0, "every pin released");
}

/// The cast verdicts a line may remember: an `Object` leading
/// parameter for any non-null non-array value; a positive reading by the
/// value's exact class; nothing for "cannot tell", a null or an array.
#[test]
fn only_lasting_cast_verdicts_are_remembered() {
    let (mut ctx, cid, _) = setup();
    let r = ctx.alloc_object(cid, 4);
    let v = Value::Object(Some(r));
    assert_eq!(
        bindto_fast_mint_admits(&ctx, v, "Ljava/lang/Object;", None),
        Some(None)
    );
    // An array parameter is judged since wave 60: nothing is remembered.
    assert_eq!(bindto_fast_mint_admits(&ctx, v, "[I", None), None);
    assert_eq!(
        bindto_fast_mint_admits(&ctx, v, "Lp/C;", Some(true)),
        Some(Some(cid.as_u32()))
    );
    assert_eq!(bindto_fast_mint_admits(&ctx, v, "Lp/X;", None), None);
    assert_eq!(
        bindto_fast_mint_admits(&ctx, Value::Object(None), "Lp/C;", None),
        None
    );
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Int, 1);
    assert_eq!(
        bindto_fast_mint_admits(&ctx, Value::Object(Some(arr)), "Ljava/lang/Object;", None),
        None
    );
}

/// MHF-1: a shape-keyed site compares every synthetic slot a `bindTo`
/// mint writes (`MH_CLASS` through `MH_STATIC_OWNER`), each once.
#[test]
fn the_shape_slots_cover_every_slot_a_bound_mint_writes() {
    let mut all: Vec<usize> = MH_COMPILED_BIND_GUARD_SLOTS
        .iter()
        .chain(MH_COMPILED_BIND_SHAPE_SLOTS.iter())
        .copied()
        .collect();
    all.sort_unstable();
    let want: Vec<usize> = (MH_CLASS..=MH_STATIC_OWNER).collect();
    assert_eq!(all, want);
}

/// Downcall carriers (`r16-ffmcost-downcall-invoke-door-answers-null`):
/// the slot an `asType` copy records its C descriptor in lies between this
/// file's synthetic window and the downcall state, so an unadapted carrier
/// (null-filled there) and an ordinary handle (narrower) record nothing.
#[test]
fn the_downcall_leaf_slot_sits_between_the_two_windows() {
    assert!(MH_DOWNCALL_LEAF_DESC >= MH_SLOT_COUNT);
    assert!(MH_DOWNCALL_LEAF_DESC < crate::panama::DOWNCALL_BASE);
    let (mut ctx, _, _) = setup();
    let ordinary = virtual_v(&mut ctx);
    assert_eq!(mh_downcall_leaf_desc(&ctx, ordinary), None);
    assert_eq!(mh_downcall_door_type(&mut ctx, ordinary), None);
}

/// A downcall's result converted from the C function's return to the
/// handle's `type`: unchanged when they agree, widened, boxed for a
/// reference return, `null` for `void` as an `Object`.
#[test]
fn a_downcall_result_takes_the_adapted_return() {
    let (mut ctx, _, _) = setup();
    let same = mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(4))), "(I)I", "(I)I");
    assert!(matches!(same, Ok(Some(Value::Int(4)))));
    let widened = mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(6))), "(I)I", "(I)J");
    assert!(matches!(widened, Ok(Some(Value::Long(6)))));
    let to_double =
        mh_downcall_outer_return(&mut ctx, Ok(Some(Value::Int(15))), "(I)I", "(S)D");
    assert!(matches!(to_double, Ok(Some(Value::Double(d))) if d == 15.0));
    let boxed = mh_downcall_outer_return(
        &mut ctx,
        // Outside -128..127: the mock's shared valueOf cache is process-wide
        // (backlog MH16-B5), so a cached box can belong to another test.
        Ok(Some(Value::Int(1300))),
        "(I)I",
        "(Ljava/lang/Integer;)Ljava/lang/Object;",
    );
    match boxed {
        // A non-null box. The mock's box-class bookkeeping is shared with
        // the other `lang_invoke` tests (backlog MH16-B5), so after them
        // `unbox_value` may not recognise the class; when it does, the
        // value must round-trip. End to end: `R16MhbindDowncall`.
        Ok(Some(Value::Object(Some(o)))) => match crate::lang_class::unbox_value(&ctx, o) {
            Value::Int(v) => assert_eq!(v, 1300),
            Value::Object(Some(_)) => {}
            other => panic!("unexpected unbox: {other:?}"),
        },
        other => panic!("not boxed: {other:?}"),
    }
    let void = mh_downcall_outer_return(&mut ctx, Ok(None), "(I)V", "(I)Ljava/lang/Object;");
    assert!(matches!(void, Ok(Some(Value::Object(None)))));
}

// The switch is on by default (a kill switch), and the memo is off for a
// context with no VM identity.
}

/// JIT round 16 wave 13 (lane wmte): a door's `WrongMethodTypeException`
/// hints the JDK method it stands for around the build (`mh_door_wmte`), and
/// MH16-1's opt-in admission of built-in-loader classes to the bound
/// `MethodType` table.
#[cfg(test)]
mod r16_wmte_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};

fn is_wmte(ctx: &MockNativeContext, err: &MethodCallFailed) -> bool {
    match err {
        MethodCallFailed::ExceptionThrown(o) => {
            ctx.class_name_of_id(ctx.class_id_of_object(*o)).as_deref()
                == Some("java/lang/invoke/WrongMethodTypeException")
        }
        _ => false,
    }
}

fn hints(ctx: &MockNativeContext) -> Vec<Option<MhWmteRaise>> {
    std::mem::take(&mut *ctx.mh_wmte_raise_hints.borrow_mut())
}

/// The hint is set before the exception is built and withdrawn after,
/// for both raises.
#[test]
fn a_door_refusal_hints_its_jdk_method_around_the_build() {
    let mut ctx = MockNativeContext::new();
    for (raise, message) in [
        (
            MhWmteRaise::ExactType,
            "handle's method type (I)I but found (J)I",
        ),
        (
            MhWmteRaise::AsType,
            "cannot convert MethodHandle(I)I to (Z)I",
        ),
    ] {
        let err = mh_door_wmte(&mut ctx, raise, message);
        assert!(is_wmte(&ctx, &err), "{err:?}");
        assert_eq!(hints(&ctx), vec![Some(raise), None], "{raise:?}");
    }
}

/// `CRATONVM_MH_WMTE_JDK_FRAMES=0`: the same exception, no hint.
/// The `invokeExact` check hints `checkExactType`, the `asType` refusal
/// hints `asType`.
#[test]
fn the_exact_and_as_type_refusals_hint_their_own_raise() {
    let mut ctx = MockNativeContext::new();
    let h = match alloc_method_handle(&mut ctx, "", "", "(I)I", MH_KIND_CONSTANT) {
        Ok(h) => h,
        Err(e) => panic!("alloc: {e:?}"),
    };
    if mh_strict_exact_signature() {
        let refusal =
            exact_call_site_refusal(&mut ctx, h, MH_KIND_CONSTANT, "(J)I", Some("(I)I"))
                .expect("another type is refused");
        assert!(is_wmte(&ctx, &refusal));
        assert_eq!(hints(&ctx), vec![Some(MhWmteRaise::ExactType), None]);
    }
    let _ = hints(&ctx);
    if let Some(refusal) = mh_astype_refusal_desc(&mut ctx, h, "(I)I", "(Z)I") {
        assert!(is_wmte(&ctx, &refusal));
        assert_eq!(hints(&ctx), vec![Some(MhWmteRaise::AsType), None]);
    }
}

/// MH16-1: off by default; on, a type naming a class whose defining
/// loader is KNOWN to be a built-in one is admitted to a class-keyed row,
/// and one naming a user loader's class, a class of no known loader, an
/// unloaded class or a spun-looking name is not. The descriptor-keyed
/// test never admits a class outside `java/` (round 16 wave 15, mhapp).
#[test]
fn built_in_loader_classes_are_shared_only_under_the_switch() {
    let mut ctx = MockNativeContext::new();
    let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
    let foo_arr = ctx
        .ensure_class_initialized("[Lapp/Foo;")
        .expect("[Lapp/Foo;");
    let bar = ctx.ensure_class_initialized("app/Bar").expect("app/Bar");
    ctx.set_loader_id_override(foo, 2);
    ctx.set_loader_id_override(bar, 7);
    let key = |ctx: &MockNativeContext, desc: &str| {
        bound_type_class_key_ids(ctx, desc, None)
            .filter(|ids| bound_type_class_key_admissible(ctx, desc, ids, None))
    };
    assert!(
        !bound_method_type_shareable(&ctx, "(Lapp/Foo;)I"),
        "never descriptor-keyed"
    );
    assert_eq!(key(&ctx, "(Lapp/Foo;)I"), Some(vec![foo.as_u32()]));
    assert_eq!(
        key(&ctx, "([Lapp/Foo;J)Lapp/Foo;"),
        Some(vec![foo_arr.as_u32(), foo.as_u32()])
    );
    assert_eq!(key(&ctx, "(Lapp/Bar;)I"), None, "a user loader");
    assert_eq!(key(&ctx, "(Lapp/NotLoaded;)I"), None);
    assert_eq!(key(&ctx, "(Lapp/Foo$$Lambda;)I"), None);
    let baz = ctx.ensure_class_initialized("app/Baz").expect("app/Baz");
    assert_eq!(key(&ctx, "(Lapp/Baz;)I"), None, "no known loader ({baz:?})");
    assert_eq!(key(&ctx, "not-a-descriptor"), None);
}

fn mhapp_type(ctx: &mut MockNativeContext, desc: &str) -> ObjectRef {
    match bound_method_type_through_table(ctx, desc, None) {
        Ok(Some(mt)) => mt,
        other => panic!("{desc}: {other:?}"),
    }
}

/// The `ClassId` a `MethodType` carrier's first parameter mirror names.
fn mhapp_first_param_class(ctx: &MockNativeContext, mt: ObjectRef) -> u32 {
    let Value::Object(Some(ptypes)) = ctx.get_field(mt, 1) else {
        panic!("ptypes");
    };
    let Value::Object(Some(mirror)) = ctx.get_array_element(ptypes, 0) else {
        panic!("ptypes[0]");
    };
    match ctx.get_field(mirror, 0) {
        Value::Int(id) => id as u32,
        other => panic!("mirror id {other:?}"),
    }
}


/// The builder reports the `ClassId` of every reference component it
/// resolved (parameters, then the return), and `None` over a stand-in.
#[test]
fn the_builder_reports_what_it_resolved() {
    let mut ctx = MockNativeContext::new();
    let foo = ctx.ensure_class_initialized("app/Foo").expect("app/Foo");
    let bar = ctx.ensure_class_initialized("app/Bar").expect("app/Bar");
    let built =
        build_method_type_from_descriptor_resolving(&mut ctx, "(ILapp/Foo;J)Lapp/Bar;", None);
    match built {
        Ok(Some((_, ids))) => assert_eq!(ids, Some(vec![foo.as_u32(), bar.as_u32()])),
        other => panic!("{other:?}"),
    }
    match build_method_type_from_descriptor_resolving(&mut ctx, "(Lapp/Missing;)V", None) {
        Ok(Some((_, ids))) => assert_eq!(ids, None, "a stand-in"),
        other => panic!("{other:?}"),
    }
    match build_method_type_from_descriptor_resolving(&mut ctx, "()V", None) {
        Ok(Some((_, ids))) => assert_eq!(ids, Some(vec![])),
        other => panic!("{other:?}"),
    }
}

/// Interning declines (and the builder keeps its carrier) where the image
/// has no loaded, initialized `MethodType` with the trusted factory: the
/// mock has none, so every build is fresh whatever the switch says.
/// The switches' defaults.
/// Two loaders, one name (round 16 wave 17, mhapp): the application
/// loader's `app/Box` (loader 2) and a user loader's (loader 9). Returns
/// `(app, user)`.
fn two_loader_boxes(ctx: &mut MockNativeContext) -> (ClassId, ClassId) {
    let app = ctx.ensure_class_initialized("app/Box").expect("app/Box");
    ctx.set_loader_id_override(app, 2);
    let user = ctx.declare_second_copy("app/Box");
    ctx.set_loader_id_override(user, 9);
    assert_ne!(app, user);
    (app, user)
}

/// A `findVirtual(Box, "add", (Box)int)` handle looked up on `refc`.
fn box_add_handle(ctx: &mut MockNativeContext, refc: ClassId) -> ObjectRef {
    alloc_method_handle_with_receiver(
        ctx,
        "app/Box",
        "add",
        "(Lapp/Box;)I",
        MH_KIND_VIRTUAL,
        None,
        Some(refc),
    )
    .expect("handle")
}

fn handle_type(ctx: &MockNativeContext, h: ObjectRef) -> ObjectRef {
    match ctx.get_field_by_name(h, "type") {
        Value::Object(Some(mt)) => mt,
        other => panic!("type {other:?}"),
    }
}

/// The builder resolves through `near`'s loader first: the user loader's
/// `Box` near the user class, the application's near it or with no
/// `near` at all.
#[test]
fn the_builder_resolves_names_through_the_near_loader() {
    let mut ctx = MockNativeContext::new();
    let (app, user) = two_loader_boxes(&mut ctx);
    let ids = |ctx: &mut MockNativeContext, near: Option<ClassId>| {
        match build_method_type_from_descriptor_resolving(ctx, "(Lapp/Box;)Lapp/Box;", near) {
            Ok(Some((_, ids))) => ids,
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(
        ids(&mut ctx, Some(user)),
        Some(vec![user.as_u32(), user.as_u32()])
    );
    assert_eq!(
        ids(&mut ctx, Some(app)),
        Some(vec![app.as_u32(), app.as_u32()])
    );
    assert_eq!(ids(&mut ctx, None), Some(vec![app.as_u32(), app.as_u32()]));
    assert_eq!(
        bound_type_class_key_ids(&ctx, "(Lapp/Box;I)V", Some(user)),
        Some(vec![user.as_u32()])
    );
}
}



#[cfg(test)]
mod i58_l4_invoke_return_unbox_tests {
//! Interpreter round i2 wave 58, lane L4: `invoke_return_unbox`, the
//! reference -> primitive return conversion of `invoke`'s
//! `asType(callSiteType)` and of an `asType` copy of a direct handle
//! (`L4W58MethodHandleReturnUnboxing`).
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn object(ctx: &mut MockNativeContext, class: &str, payload: Value) -> Value {
    let o = match ctx.new_object(class) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object {class}: {other:?}"),
    };
    ctx.set_field(o, 0, payload);
    Value::Object(Some(o))
}

fn setup() -> MockNativeContext {
    let mut ctx = MockNativeContext::new();
    for name in [
        "p/C",
        "java/lang/Long",
        "java/lang/Integer",
        "java/lang/Short",
        "java/lang/Boolean",
    ] {
        let _ = ctx.ensure_class_initialized(name);
    }
    ctx
}

fn judge(
    ctx: &mut MockNativeContext,
    value: Value,
    kind: i32,
    leaf: &str,
    declared: &str,
    target: &str,
) -> Option<Result<Value, MethodCallFailed>> {
    invoke_return_unbox_judged(ctx, &Ok(Some(value)), 0, kind, leaf, declared, target, true)
}

fn message(r: &Option<Result<Value, MethodCallFailed>>) -> String {
    match r {
        Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NullPointerException { message: Some(m) },
        )))) => format!("NullPointerException: {m}"),
        Some(Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        )))) => format!("ClassCastException: {message}"),
        other => format!("{other:?}"),
    }
}

fn unboxed(r: &Option<Result<Value, MethodCallFailed>>) -> Value {
    match r {
        Some(Ok(v)) => *v,
        other => panic!("not a conversion: {other:?}"),
    }
}

const OBJ: &str = "()Ljava/lang/Object;";

/// The outer conversion (`(int) objHandle.invoke()`): HotSpot 25.0.3's
/// answers, which the coercion after the native replaced with `0`.
#[test]
fn i58_l4_invoke_unboxes_a_reference_return_as_astype_does() {
    let mut ctx = setup();
    let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
    let short3 = object(&mut ctx, "java/lang/Short", Value::Int(3));
    let int4 = object(&mut ctx, "java/lang/Integer", Value::Int(4));
    let other = object(&mut ctx, "p/C", Value::Int(0));
    let k = MH_KIND_STATIC;
    let r = judge(&mut ctx, long6, k, OBJ, OBJ, "()I");
    assert!(
        message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
        "{r:?}"
    );
    let r = judge(&mut ctx, Value::Object(None), k, OBJ, OBJ, "()I");
    let m = message(&r);
    assert!(
        m.contains("NullPointerException") && m.contains("Number.intValue()"),
        "{m}"
    );
    let r = judge(&mut ctx, Value::Object(None), k, OBJ, OBJ, "()J");
    assert!(message(&r).contains("Number.longValue()"), "{r:?}");
    // A declared `Integer` unboxes through `unboxInteger(Integer x)`.
    let r = judge(
        &mut ctx,
        Value::Object(None),
        k,
        "()Ljava/lang/Integer;",
        "()Ljava/lang/Integer;",
        "()I",
    );
    assert!(
        message(&r).contains("\"java.lang.Integer.intValue()\" because \"x\" is null"),
        "{r:?}"
    );
    let r = judge(&mut ctx, other, k, OBJ, OBJ, "()I");
    assert!(
        message(&r).contains("cannot be cast to java.lang.Number"),
        "{r:?}"
    );
    // Conversions that succeed: the site's own wrapper, and widenings.
    let r = judge(&mut ctx, short3, k, OBJ, OBJ, "()I");
    assert_eq!(unboxed(&r), Value::Int(3));
    let r = judge(&mut ctx, int4, k, OBJ, OBJ, "()J");
    assert_eq!(unboxed(&r), Value::Long(4));
    let r = judge(&mut ctx, long6, k, OBJ, OBJ, "()J");
    assert_eq!(unboxed(&r), Value::Long(6));
}

/// The inner conversion: an `asType(()int)` copy of a `()Object` direct
/// handle unboxes to its declared `int`, then widens to the target.
#[test]
fn i58_l4_an_astype_copy_unboxes_its_leaf_reference_return() {
    let mut ctx = setup();
    let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
    let int4 = object(&mut ctx, "java/lang/Integer", Value::Int(4));
    let k = MH_KIND_STATIC;
    let r = judge(&mut ctx, long6, k, OBJ, "()I", "()I");
    assert!(
        message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
        "{r:?}"
    );
    // Through `invoke` at a wider site: still the `int` unboxing first.
    let r = judge(&mut ctx, long6, k, OBJ, "()I", "()J");
    assert!(
        message(&r).contains("Cannot cast java.lang.Long to java.lang.Integer"),
        "{r:?}"
    );
    let r = judge(&mut ctx, int4, k, OBJ, "()I", "()J");
    assert_eq!(unboxed(&r), Value::Long(4));
    // An adapter's `MH_DESC` is not authoritative: no inner conversion.
    assert!(judge(&mut ctx, long6, MH_KIND_DROP, OBJ, "()I", "()I").is_none());
    // A cast chain judges its own return.
    let chained = MH_MARK_CAST_CHAIN;
    let r = invoke_return_unbox_judged(
        &mut ctx,
        &Ok(Some(long6)),
        chained,
        k,
        OBJ,
        "()I",
        "()I",
        true,
    );
    assert!(r.is_none());
    // An `explicitCastArguments` stamp casts as JLS 5.5 does: `Long` 6
    // narrows to `int` 6, `null` is zero.
    let explicit = MH_MARK_EXPLICIT_CAST;
    let r = invoke_return_unbox_judged(
        &mut ctx,
        &Ok(Some(long6)),
        explicit,
        k,
        OBJ,
        "()I",
        "()I",
        true,
    );
    assert_eq!(unboxed(&r), Value::Int(6));
    let r = invoke_return_unbox_judged(
        &mut ctx,
        &Ok(Some(Value::Object(None))),
        explicit,
        k,
        OBJ,
        "()I",
        "()I",
        true,
    );
    assert_eq!(unboxed(&r), Value::Int(0));
}

/// What is never judged: a `void` or primitive declared return under
/// `invoke`, a reference target, a raw primitive, the switch off.
#[test]
fn i58_l4_the_return_unbox_leaves_other_shapes_alone() {
    let mut ctx = setup();
    let long6 = object(&mut ctx, "java/lang/Long", Value::Long(6));
    let k = MH_KIND_STATIC;
    assert!(judge(&mut ctx, Value::Object(None), k, "()V", "()V", "()I").is_none());
    assert!(judge(&mut ctx, long6, k, "()J", "()J", "()I").is_none());
    assert!(judge(&mut ctx, long6, k, OBJ, OBJ, "()Ljava/lang/Integer;").is_none());
    assert!(judge(&mut ctx, Value::Int(3), k, OBJ, OBJ, "()I").is_none());
    let off =
        invoke_return_unbox_judged(&mut ctx, &Ok(Some(long6)), 0, k, OBJ, OBJ, "()I", false);
    assert!(off.is_none());
    // The hot wrapper's byte test: an array or reference target never
    // reaches the shape test.
    assert!(invoke_return_unbox(
        &mut ctx,
        &Ok(Some(long6)),
        0,
        k,
        OBJ,
        Some(OBJ),
        Some("()[I")
    )
    .is_none());
    assert!(invoke_return_unbox(
        &mut ctx,
        &Ok(Some(long6)),
        0,
        k,
        OBJ,
        Some(OBJ),
        Some("()V")
    )
    .is_none());
    assert!(
        invoke_return_unbox(&mut ctx, &Ok(Some(long6)), 0, k, OBJ, None, Some("()I")).is_none()
    );
}
}

#[cfg(test)]
mod i58_l4_adapter_single_stamp_tests {
//! Interpreter round i2 wave 58, lane L4: ONE stamp over an ADAPTER kind is
//! judged like a cast chain (`CRATONVM_MH_ADAPTER_STAMP_CONVERTS`); over a
//! direct kind it is not (the doors and the leaf convert from `type`).
use super::*;

fn chain(links: &[(&str, bool)]) -> CastChain {
    CastChain {
        bound_refusal: None,
        links: links
            .iter()
            .map(|(d, e)| CastLink {
                explicit: *e,
                desc: (*d).to_string(),
            })
            .collect(),
    }
}

#[test]
fn i58_l4_one_stamp_over_an_adapter_is_judged() {
    let one = chain(&[("(JLjava/lang/Number;)J", true)]);
    assert!(!one.is_active(), "one link is no chain by itself");
    for kind in [MH_KIND_FILTER, MH_KIND_DROP, MH_KIND_INSERT, MH_KIND_GUARD] {
        assert!(
            adapter_kind_single_stamp_judged(kind, &one, true),
            "kind {kind}"
        );
        assert!(
            !adapter_kind_single_stamp_judged(kind, &one, false),
            "kind {kind}, off"
        );
    }
    for kind in [
        MH_KIND_STATIC,
        MH_KIND_VIRTUAL,
        MH_KIND_SPECIAL,
        MH_KIND_CONSTRUCTOR,
    ] {
        assert!(
            !adapter_kind_single_stamp_judged(kind, &one, true),
            "direct kind {kind}"
        );
    }
    let two = chain(&[
        ("(Ljava/lang/Object;I)J", false),
        ("(JLjava/lang/Number;)J", false),
    ]);
    assert!(two.is_active());
    assert!(
        !adapter_kind_single_stamp_judged(MH_KIND_FILTER, &two, true),
        "a chain already"
    );
    assert!(!adapter_kind_single_stamp_judged(
        MH_KIND_FILTER,
        &CastChain::default(),
        true
    ));
}
}
#[cfg(test)]
mod i59_l4_tests {
//! Interpreter round i2 wave 59, lane L4: the handle doors' class
//! initialization chains, the VarHandle own-wrapper `null`, and the one
//! return plan of the method-handle doors.
use super::*;
use cratonvm_native_api::ServedRaiseHopSite;

/// The static VarHandle's JDK method HotSpot lists, per access mode and
/// variable (JDK 25.0.3, `L4W59HandleClassInitFrames` `vh-static-*`).
#[test]
fn i59_l4_a_static_var_handle_names_its_family_and_mode() {
    assert_eq!(
        vh_static_mode_method("get", b'I'),
        Some((
            "java/lang/invoke/VarHandleInts$FieldStaticReadOnly",
            "(Ljava/lang/invoke/VarHandle;)I"
        ))
    );
    assert_eq!(
        vh_static_mode_method("set", b'I'),
        Some((
            "java/lang/invoke/VarHandleInts$FieldStaticReadWrite",
            "(Ljava/lang/invoke/VarHandle;I)V"
        ))
    );
    assert_eq!(
        vh_static_mode_method("compareAndSet", b'L'),
        Some((
            "java/lang/invoke/VarHandleReferences$FieldStaticReadWrite",
            "(Ljava/lang/invoke/VarHandle;Ljava/lang/Object;Ljava/lang/Object;)Z"
        ))
    );
    assert_eq!(
        vh_static_mode_method("compareAndExchangeAcquire", b'J'),
        Some((
            "java/lang/invoke/VarHandleLongs$FieldStaticReadWrite",
            "(Ljava/lang/invoke/VarHandle;JJ)J"
        ))
    );
    assert_eq!(
        vh_static_mode_method("getAndBitwiseOrRelease", b'Z'),
        Some((
            "java/lang/invoke/VarHandleBooleans$FieldStaticReadWrite",
            "(Ljava/lang/invoke/VarHandle;Z)Z"
        ))
    );
    assert_eq!(vh_static_mode_method("toMethodHandle", b'I'), None);
    assert_eq!(vh_static_mode_method("get", 0), None);
    // Unknown access: no chain, the plain initialization.
    assert!(vh_static_init_chain(VhStaticInit {
        mode: "",
        variable: 0
    })
    .is_none());
    let chain = vh_static_init_chain(VhStaticInit {
        mode: "get",
        variable: b'I',
    })
    .unwrap();
    assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden);
    assert_eq!(chain.hops[0].class, "java/lang/invoke/VarHandle");
    assert_eq!(chain.hops[0].name, "get");
    let names: Vec<&str> = chain.hops.iter().map(|h| h.name).collect();
    assert_eq!(
        names,
        [
            "get",
            "get",
            "target",
            "ensureInitialized",
            "initialize",
            "ensureClassInitialized",
            "ensureClassInitialized0"
        ]
    );
    assert_eq!(
        chain.hops.last().map(|h| h.site),
        Some(ServedRaiseHopSite::Raise)
    );
}

/// The three `DirectMethodHandle` barriers, as HotSpot 25.0.3 lists them.
#[test]
fn i59_l4_a_direct_handles_barrier_lists_hotspots_frames() {
    let names = |b| -> Vec<&'static str> {
        dmh_class_init_chain(b)
            .hops
            .iter()
            .map(|h| h.name)
            .collect()
    };
    assert_eq!(
        names(DmhInitBarrier::StaticMethod),
        [
            "invokeExact",
            "internalMemberNameEnsureInit",
            "ensureInitialized",
            "checkInitialized",
            "ensureClassInitialized",
            "ensureClassInitialized0"
        ]
    );
    assert_eq!(
        names(DmhInitBarrier::StaticField),
        [
            "invokeExact",
            "ensureInitialized",
            "ensureInitialized",
            "checkInitialized",
            "ensureClassInitialized",
            "ensureClassInitialized0"
        ]
    );
    assert_eq!(
        names(DmhInitBarrier::Constructor),
        ["invokeExact", "allocateInstance", "allocateInstance"]
    );
    for b in [
        DmhInitBarrier::StaticMethod,
        DmhInitBarrier::StaticField,
        DmhInitBarrier::Constructor,
    ] {
        let chain = dmh_class_init_chain(b);
        assert_eq!(chain.hops[0].site, ServedRaiseHopSite::Hidden, "{b:?}");
        assert_eq!(
            chain.hops.last().map(|h| h.site),
            Some(ServedRaiseHopSite::Raise),
            "{b:?}"
        );
        assert_eq!(
            chain.throwable_class,
            "java/lang/ExceptionInInitializerError"
        );
    }
}

/// The doors' plans pick the targets each door used to spell out.
#[test]
fn i59_l4_each_door_builds_its_own_return_plan() {
    let k = MH_KIND_STATIC;
    let leaf = "()J";
    let declared = Some("()D");
    let obj_site = Some("()Ljava/lang/Object;");
    let p = DoorReturn::invoke(k, leaf, declared, obj_site, 0, false);
    assert_eq!((p.cast_site, p.unbox_target), (obj_site, obj_site));
    assert_eq!(
        p.box_target, declared,
        "a reference site boxes the declared primitive"
    );
    let p = DoorReturn::invoke(k, leaf, Some("()J"), Some("()D"), 0, false);
    assert_eq!(
        p.box_target,
        Some("()D"),
        "a primitive site widens to itself"
    );
    let p = DoorReturn::exact(k, leaf, declared, 4, true);
    assert_eq!(
        (p.cast_site, p.unbox_target, p.box_target),
        (None, declared, declared)
    );
    assert!(p.explicit_stamp);
    assert_eq!(p.marks, 4);
    let p = DoorReturn::with_arguments(k, leaf, declared, 0, false, true);
    assert_eq!(p.box_target, declared);
    let p = DoorReturn::with_arguments(k, leaf, declared, 0, false, false);
    assert_eq!(p.box_target, None, "the leaf's own boxing");
    assert_eq!(p.unbox_target, declared);
}

/// Wave 60 (lane L4): the doors' tail tokens are projections of the plan.
/// A door with work after its dispatch (a return cast, a declared box)
/// arms no tail; an array return target counts as a cast since wave 60.
#[test]
fn i60_l4_the_tail_decision_is_a_projection_of_the_plan() {
    let k = MH_KIND_STATIC;
    let obj = "()Ljava/lang/Object;";
    let string = "()Ljava/lang/String;";
    let strings = "()[Ljava/lang/String;";
    let string_token = door_tail_token("Ljava/lang/String;");
    assert_ne!(string_token, 0);
    // `invoke`: the site's token, unless the outer cast has work.
    let p = DoorReturn::invoke(k, string, Some(string), Some(string), 0, false);
    assert_eq!(p.invoke_tail(), string_token);
    let p = DoorReturn::invoke(k, obj, Some(obj), Some(string), 0, false);
    assert_eq!(p.invoke_tail(), 0, "the outer cast to String works after");
    let p = DoorReturn::invoke(k, obj, Some(obj), Some(strings), 0, false);
    assert_eq!(p.invoke_tail(), 0, "an array site is cast too");
    let p = DoorReturn::invoke(k, obj, Some(obj), None, 0, false);
    assert_eq!(p.invoke_tail(), 0, "no site, no token");
    // `invokeExact`: the site's token when the declared return agrees.
    let p = DoorReturn::exact(k, string, Some(string), 0, false);
    assert_eq!(p.exact_tail(Some(string)), string_token);
    assert_eq!(p.exact_tail(Some("()I")), 0, "a disagreeing site");
    let p = DoorReturn::exact(k, obj, Some(strings), 0, false);
    assert_eq!(
        p.exact_tail(Some(strings)),
        0,
        "the inner array cast works after"
    );
    // `invokeWithArguments`: any work after the dispatch disarms it.
    let p = DoorReturn::with_arguments(k, "()J", Some("()D"), 0, false, true);
    assert!(
        p.with_arguments_works_after(),
        "a declared primitive to box"
    );
    let p = DoorReturn::with_arguments(k, obj, Some(string), 0, false, false);
    assert!(p.with_arguments_works_after(), "the inner cast");
    let p = DoorReturn::with_arguments(k, string, Some(string), 0, false, false);
    assert!(!p.with_arguments_works_after());
}
}
/// Interpreter round i3 wave 76, lane L4: `CRATONVM_MH_COMPAT_METHOD_TYPE_BYTECODE`.
/// The switch is set explicitly through a thread override; the end-to-end
/// rows are `L4W74MethodTypeIntern.java` (the `npe-*`
/// rows under `--compatible`) and `L4W76ReentryBench.java`.


/// Interpreter round i3 wave 79, lane L4: the defect fixes of the field-handle
/// and `MemberName` paths. The end-to-end rows are
/// `L4W79VolatileFieldHandle.java`,
/// `L4W74InheritedFieldSlot.java` and `L4W65FieldLinkSubclasses.java`.
#[cfg(test)]
mod i79_l4_defect_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    FieldMetadata, NativeClassAccess, NativeExceptionAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn field(
    name: &str,
    cid: ClassId,
    slot_index: usize,
    access_flags: u16,
    is_static: bool,
) -> FieldMetadata {
    FieldMetadata {
        name: name.to_string(),
        descriptor: "I".to_string(),
        access_flags,
        slot_index,
        declaring_class_id: cid,
        is_static,
    }
}

const VOLATILE: u16 = FIELD_ACC_VOLATILE as u16;

#[test]
fn the_volatile_bit_is_its_own_and_the_type_index_still_fits() {
    let others = [
        FL_TYPE_INDEX_MASK << 16,
        0xFFFF,
        FL_BOUND,
        FL_OWNER_ADMITS,
        FL_LINKED,
        FL_STATIC,
        FL_SETTER,
        FL_VALUE_CHECK,
        FL_MINT_SLOT,
        0x7 << FL_LINKS_SHIFT,
    ];
    for bits in others {
        assert_eq!(bits & FL_VOLATILE, 0, "{bits:#x} overlaps FL_VOLATILE");
    }
    // `MethodHandle.type` is declared below the synthetic window.
    assert!(MH_BASE <= FL_TYPE_INDEX_MASK as usize);
    assert!(MH_CLASS <= FL_TYPE_INDEX_MASK as usize);
    // A copy drops it with the link, keeping the minted slot.
    let linked = FL_LINKED | FL_VOLATILE | FL_MINT_SLOT | 4;
    assert_eq!(field_link_state_kept_by_copy(linked), FL_MINT_SLOT | 4);
}

#[test]
fn a_links_volatile_bit_is_read_from_the_field_it_resolves() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
    let sub = ctx.ensure_class_initialized("p/Sub").expect("p/Sub");
    ctx.set_superclass(sub, base);
    ctx.set_declared_fields(
        base,
        vec![
            field("v", base, 0, VOLATILE | 0x0001, false),
            field("p", base, 1, 0x0001, false),
            field("S", base, 0, VOLATILE | 0x0008, true),
            field("T", base, 1, 0x0008, true),
        ],
    );
    // `Sub` hides `p` with a volatile field of its own, slot 2.
    ctx.set_declared_fields(sub, vec![field("p", sub, 2, VOLATILE, false)]);

    // Declared by the start class, or inherited from its superclass.
    assert!(field_link_field_is_volatile(&ctx, base, "v", false, 0));
    assert!(field_link_field_is_volatile(&ctx, sub, "v", false, 0));
    assert!(!field_link_field_is_volatile(&ctx, base, "p", false, 1));
    // The slot decides between a hidden field and the one hiding it.
    assert!(field_link_field_is_volatile(&ctx, sub, "p", false, 2));
    assert!(!field_link_field_is_volatile(&ctx, sub, "p", false, 1));
    // A slot no declaration carries: the nearest of that name.
    assert!(field_link_field_is_volatile(&ctx, sub, "p", false, 9));
    // Statics by name and static-ness.
    assert!(field_link_field_is_volatile(&ctx, sub, "S", true, 7));
    assert!(!field_link_field_is_volatile(&ctx, base, "T", true, 1));
    assert!(
        !field_link_field_is_volatile(&ctx, base, "S", false, 0),
        "not an instance field"
    );
    // Nothing declares it (a lambda proxy's capture): plain.
    assert!(!field_link_field_is_volatile(&ctx, sub, "absent", false, 0));
}

#[test]
fn the_volatile_store_runs_the_store_once_either_way() {
    let mut runs = 0;
    assert_eq!(
        field_handle_volatile_store(true, || {
            runs += 1;
            7
        }),
        7
    );
    assert_eq!(
        field_handle_volatile_store(false, || {
            runs += 1;
            8
        }),
        8
    );
    assert_eq!(runs, 2);
    field_handle_volatile_load_fence(true);
    field_handle_volatile_load_fence(false);
}

fn linked_bound(
    ctx: &mut MockNativeContext,
    kind: i32,
    receiver: ObjectRef,
    class_raw: i32,
    extra_flags: i32,
    site: &str,
) -> ObjectRef {
    let mh = alloc_method_handle(ctx, "p/Cell", "cellValueZ", "()I", kind).unwrap();
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
fn a_volatile_link_still_serves_its_values() {
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(42));
    let raw = cell.as_u32() as i32;

    let getter = linked_bound(
        &mut ctx,
        MH_KIND_GETTER,
        r,
        raw,
        FL_BOUND | FL_VOLATILE,
        "()I",
    );
    let got = mh_field_link_fast(&mut ctx, getter, &[], Some("()I"));
    assert!(matches!(got, Some(Ok(Some(Value::Int(42))))));

    let setter = linked_bound(
        &mut ctx,
        MH_KIND_SETTER,
        r,
        raw,
        FL_BOUND | FL_SETTER | FL_VOLATILE,
        "(I)V",
    );
    let put = mh_field_link_fast(&mut ctx, setter, &[Value::Int(9)], Some("(I)V"));
    assert!(matches!(put, Some(Ok(Some(Value::Object(None))))));
    assert_eq!(ctx.get_field(r, 2), Value::Int(9));
}

#[test]
fn a_link_serves_only_its_declared_site_whatever_the_strict_switch() {
    // Wave 79 (`i71-L4-non-strict-invokeexact-serves-a-field-link-at-any-site`):
    // the site is compared with or without `CRATONVM_MH_STRICT_INVOKEEXACT`.
    let mut ctx = MockNativeContext::new();
    let cell = ctx.ensure_class_initialized("p/Cell").expect("p/Cell");
    let r = ctx.alloc_object(cell, 4);
    ctx.set_field(r, 2, Value::Int(5));
    let raw = cell.as_u32() as i32;
    let setter = linked_bound(
        &mut ctx,
        MH_KIND_SETTER,
        r,
        raw,
        FL_BOUND | FL_SETTER,
        "(I)V",
    );
    assert!(mh_field_link_fast(&mut ctx, setter, &[Value::Long(7)], Some("(J)V")).is_none());
    assert!(mh_field_link_fast(&mut ctx, setter, &[Value::Int(7)], None).is_none());
    assert_eq!(
        ctx.get_field(r, 2),
        Value::Int(5),
        "nothing stored at another site"
    );
    let getter = linked_bound(&mut ctx, MH_KIND_GETTER, r, raw, FL_BOUND, "()I");
    assert!(mh_field_link_fast(&mut ctx, getter, &[], Some("()J")).is_none());
}

#[test]
fn an_inherited_instance_field_resolves_to_its_declaring_class() {
    let mut ctx = MockNativeContext::new();
    let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
    let derived = ctx
        .ensure_class_initialized("p/Derived")
        .expect("p/Derived");
    let deeper = ctx.ensure_class_initialized("p/Deeper").expect("p/Deeper");
    ctx.set_superclass(derived, base);
    ctx.set_superclass(deeper, derived);
    ctx.set_declared_fields(
        base,
        vec![
            field("a", base, 0, 1, false),
            field("b", base, 1, 1, false),
            field("S", base, 0, 9, true),
        ],
    );
    ctx.set_declared_fields(derived, vec![field("c", derived, 2, 1, false)]);

    // Declared by the named class: no holder to write.
    assert_eq!(inherited_instance_field_holder(&ctx, derived, "c"), None);
    assert_eq!(inherited_instance_field_holder(&ctx, base, "b"), None);
    // Inherited: the declaring class, from one or two levels down.
    assert_eq!(
        inherited_instance_field_holder(&ctx, derived, "b"),
        Some(base)
    );
    assert_eq!(
        inherited_instance_field_holder(&ctx, deeper, "b"),
        Some(base)
    );
    assert_eq!(
        inherited_instance_field_holder(&ctx, deeper, "c"),
        Some(derived)
    );
    // A static of that name is not an instance field; nothing declares `z`.
    assert_eq!(inherited_instance_field_holder(&ctx, derived, "S"), None);
    assert_eq!(inherited_instance_field_holder(&ctx, derived, "z"), None);

    // The offset search finds the holder's slot, not the vmindex fallback.
    assert_eq!(
        mhn_instance_field_holder_from(&ctx, derived, "b"),
        Some((base, 1))
    );
    assert_eq!(
        mhn_instance_field_holder_from(&ctx, deeper, "a"),
        Some((base, 0))
    );
    assert_eq!(
        mhn_instance_field_holder_from(&ctx, derived, "c"),
        Some((derived, 2))
    );
    assert_eq!(mhn_instance_field_holder_from(&ctx, derived, "z"), None);
}

#[test]
fn the_id_refuses_a_receiver_whose_owner_name_is_ambiguous() {
    let mut ctx = MockNativeContext::new();
    // `q/Amb` named by no unique class (only `declare_second_copy` ids):
    // `class_id_by_name` answers `None`, as for a two-loader name.
    let owner = ctx.declare_second_copy("q/Amb");
    let _other = ctx.declare_second_copy("q/Amb");
    assert!(ctx.class_id_by_name("q/Amb").is_none());
    let s = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("String");
    let str_obj = ctx.alloc_object(s, 1);
    assert!(owner_refusal_the_name_cannot_judge(&ctx, s, owner, "q/Amb"));
    let verdict = owner_receiver_verdict_with(&mut ctx, str_obj, owner, "q/Amb", true);
    match verdict {
        Some(Some(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message },
        )))) => assert_eq!(message, "Cannot cast java.lang.String to q.Amb"),
        other => panic!("expected the owner's cast, got some={}", other.is_some()),
    }
    // Without the authority, the by-name judgement answers (as before).
    assert!(owner_receiver_verdict_with(&mut ctx, str_obj, owner, "q/Amb", false).is_none());
    // A unique name: the by-name cast still judges.
    let base = ctx.ensure_class_initialized("p/Base").expect("p/Base");
    assert!(!owner_refusal_the_name_cannot_judge(
        &ctx, s, base, "p/Base"
    ));
    // A lambda proxy's id is never judged here; nor an unnamed owner.
    assert!(!owner_refusal_the_name_cannot_judge(
        &ctx,
        ClassId::new(0x8000_0001),
        owner,
        "q/Amb"
    ));
    assert!(!owner_refusal_the_name_cannot_judge(&ctx, s, owner, ""));
    // The owner's own instance is admitted by id first.
    let own = ctx.alloc_object(owner, 1);
    assert!(matches!(
        owner_receiver_verdict_with(&mut ctx, own, owner, "q/Amb", true),
        Some(None)
    ));
}
}

/// Interpreter round i3 wave 79, lane L4: a field accessor `invoke`d at another
/// arity is `asType`'s refusal (page
/// `i70-orch-compatible-bound-getter-invoke-ignores-an-extra-argument`); the
/// end-to-end rows are the last five of
/// `L4W70LinkDerivation.java`.
#[cfg(test)]
mod i79_l4_field_arity_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

#[test]
fn a_field_accessor_refuses_another_arity() {
    if !mh_strict_exact_signature() {
        return;
    }
    let mut ctx = MockNativeContext::new();
    let bx = ctx.ensure_class_initialized("p/Box").expect("p/Box");
    let recv = ctx.alloc_object(bx, 2);

    // A bound instance getter: `()int`, its `MH_DESC` naming the receiver.
    let get =
        alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;)I", MH_KIND_GETTER).unwrap();
    ctx.set_field(get, MH_BOUND, Value::Object(Some(recv)));
    assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", Some("()I")).is_some());
    assert!(bound_handle_arity_refusal(&mut ctx, get, "()I", Some("()I")).is_none());
    // The same arity is a conversion, not this refusal's business.
    assert!(bound_handle_arity_refusal(&mut ctx, get, "()J", Some("()I")).is_none());
    // A `type` that is not the leaf minus the receiver is never judged.
    assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", Some("(Lp/Box;)I")).is_none());
    assert!(bound_handle_arity_refusal(&mut ctx, get, "(I)I", None).is_none());

    // Unbound: the `type` is the `MH_DESC`.
    let uget =
        alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;)I", MH_KIND_GETTER).unwrap();
    assert!(
        bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;I)I", Some("(Lp/Box;)I")).is_some()
    );
    assert!(
        bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;)I", Some("(Lp/Box;)I")).is_none()
    );
    assert!(bound_handle_arity_refusal(&mut ctx, uget, "(Lp/Box;I)I", Some("()I")).is_none());

    // A static getter and an instance setter.
    let sget = alloc_method_handle(&mut ctx, "p/Box", "S", "()I", MH_KIND_GETTER).unwrap();
    assert!(bound_handle_arity_refusal(&mut ctx, sget, "(I)I", Some("()I")).is_some());
    let set =
        alloc_method_handle(&mut ctx, "p/Box", "v", "(Lp/Box;I)V", MH_KIND_SETTER).unwrap();
    assert!(
        bound_handle_arity_refusal(&mut ctx, set, "(Lp/Box;)V", Some("(Lp/Box;I)V")).is_some()
    );
    assert!(
        bound_handle_arity_refusal(&mut ctx, set, "(Lp/Box;I)V", Some("(Lp/Box;I)V")).is_none()
    );
}
}

/// Interpreter round i3 wave 79, lane L4: the synthetic `MethodType` carrier of
/// `methodType(Class, Class[])` keeps a copy of the caller's array (page
/// `i76-L4-synthetic-method-type-keeps-the-callers-parameter-array`). The mock
/// answers `method_exists` false, so the JDK's factory chain declines and the
/// carrier is minted, as in a synthetic class library.
#[cfg(test)]
mod i79_l4_method_type_copy_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

#[test]
fn the_class_array_overload_keeps_a_copy_on_the_synthetic_carrier() {
    let mut ctx = MockNativeContext::new();
    let s = ctx
        .ensure_class_initialized("java/lang/String")
        .expect("String");
    let i = ctx
        .ensure_class_initialized("java/lang/Integer")
        .expect("Integer");
    let o = ctx
        .ensure_class_initialized("java/lang/Object")
        .expect("Object");
    let (s_m, i_m, o_m) = (
        ctx.get_class_mirror(s),
        ctx.get_class_mirror(i),
        ctx.get_class_mirror(o),
    );
    let arr = new_class_array(&mut ctx, 1);
    ctx.set_array_element(arr, 0, Value::Object(Some(s_m)));
    let Ok(Some(Value::Object(Some(mt)))) = make_method_type_copying(&mut ctx, o_m, Some(arr))
    else {
        panic!("a carrier");
    };
    let Value::Object(Some(kept)) = ctx.get_field(mt, 1) else {
        panic!("the carrier's parameter array");
    };
    assert!(
        kept != arr,
        "the carrier keeps a copy, not the caller's array"
    );
    assert_eq!(ctx.array_length(kept), 1);
    // The caller rewrites its array: the type does not move.
    ctx.set_array_element(arr, 0, Value::Object(Some(i_m)));
    assert_eq!(ctx.get_array_element(kept, 0), Value::Object(Some(s_m)));
    // No array: an empty one, as before.
    let Ok(Some(Value::Object(Some(none)))) = make_method_type_copying(&mut ctx, o_m, None)
    else {
        panic!("a carrier");
    };
    let Value::Object(Some(empty)) = ctx.get_field(none, 1) else {
        panic!("an empty parameter array");
    };
    assert_eq!(ctx.array_length(empty), 0);
}
}

/// Interpreter round i3 wave 80, lane L4: the flags `MethodHandleNatives.resolve`
/// writes for a lambda proxy's captured field
/// (`mhn_resolve_lambda_proxy_field`; page
/// `i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack`). The
/// end-to-end rows are `L4W61LambdaProxyHandleGaps.java`
/// (`findVarHandle*`) and `L4W61LambdaProxyFieldHandles.java`.
#[cfg(test)]
mod i80_l4_lambda_proxy_field_resolve_tests {
use super::*;

const IS_FIELD: i32 = 0x0004_0000;

#[test]
fn a_hidden_final_capture_gets_its_access_and_trusted_final() {
    // `getField` request: IS_FIELD | REF_getField << 24, no modifiers.
    let request = IS_FIELD | (1 << 24);
    let flags = mhn_hidden_field_flags(request, 1, 0x0012, true);
    assert_eq!(flags, IS_FIELD | (1 << 24) | 0x0012 | MN_TRUSTED_FINAL);
    // A non-final field of a hidden class: no trusted bit.
    assert_eq!(
        mhn_hidden_field_flags(request, 1, 0x0002, true),
        IS_FIELD | (1 << 24) | 0x0002
    );
    // A final field of an ordinary class: no trusted bit.
    assert_eq!(
        mhn_hidden_field_flags(request, 1, 0x0012, false),
        IS_FIELD | (1 << 24) | 0x0012
    );
    // A `putField` request keeps its kind; stale modifier bits are replaced.
    let put = IS_FIELD | (3 << 24) | 0x0001;
    assert_eq!(
        mhn_hidden_field_flags(put, 3, 0x0012, true),
        IS_FIELD | (3 << 24) | 0x0012 | MN_TRUSTED_FINAL
    );
}

#[test]
fn a_request_of_the_other_static_ness_is_rewritten_to_the_field_s_own() {
    assert_eq!(mhn_field_ref_kind(1, false), 1);
    assert_eq!(mhn_field_ref_kind(3, false), 3);
    assert_eq!(mhn_field_ref_kind(2, false), 1);
    assert_eq!(mhn_field_ref_kind(4, false), 3);
    assert_eq!(mhn_field_ref_kind(1, true), 2);
    assert_eq!(mhn_field_ref_kind(3, true), 4);
    assert_eq!(mhn_field_ref_kind(2, true), 2);
    assert_eq!(mhn_field_ref_kind(4, true), 4);
}
}

/// Interpreter round i3 wave 80, lane L4: the plan of
/// `CRATONVM_MH_REAL_FIELD_CONVERTING_SERVE` (`mh_field_converting_plan`) and
/// its widening. The end-to-end rows are
/// `L4W80FieldConvertingBench.java`.
#[cfg(test)]
mod i80_l4_field_converting_plan_tests {
use super::*;

const BOX: &str = "Lp/Box;";

fn plan(declared: &str, site: &str, is_static: bool) -> Option<FieldConvertingPlan> {
    mh_field_converting_plan(declared, site, is_static)
}

#[test]
fn the_bench_sites_are_served() {
    let d = format!("({BOX})I");
    assert_eq!(
        plan(&d, &format!("({BOX})Ljava/lang/Object;"), false),
        Some(FieldConvertingPlan {
            receiver_cast: None,
            ret: FieldConvertingReturn::Box
        })
    );
    assert_eq!(
        plan(&d, &format!("({BOX})J"), false),
        Some(FieldConvertingPlan {
            receiver_cast: None,
            ret: FieldConvertingReturn::Widen(b'J')
        })
    );
    assert_eq!(
        plan("()I", "()Ljava/lang/Object;", true),
        Some(FieldConvertingPlan {
            receiver_cast: None,
            ret: FieldConvertingReturn::Box
        })
    );
}

#[test]
fn a_wider_receiver_parameter_is_a_cast_to_the_declared_class() {
    let d = format!("({BOX})I");
    assert_eq!(
        plan(&d, "(Ljava/lang/Object;)I", false),
        Some(FieldConvertingPlan {
            receiver_cast: Some(BOX.to_string()),
            ret: FieldConvertingReturn::Same
        })
    );
    assert_eq!(
        plan(&d, "(Ljava/lang/Object;)Ljava/lang/Number;", false),
        Some(FieldConvertingPlan {
            receiver_cast: Some(BOX.to_string()),
            ret: FieldConvertingReturn::Box
        })
    );
    // A primitive receiver parameter is not a cast.
    assert_eq!(plan(&d, "(I)I", false), None);
}

#[test]
fn everything_else_declines() {
    let d = format!("({BOX})I");
    // Exact (the other lanes'), void, narrowing, unboxing-shaped, arity.
    assert_eq!(plan(&d, &d, false), None);
    assert_eq!(plan(&d, &format!("({BOX})V"), false), None);
    assert_eq!(plan(&d, &format!("({BOX})S"), false), None);
    assert_eq!(plan(&d, &format!("({BOX})Ljava/lang/String;"), false), None);
    assert_eq!(plan(&d, &format!("({BOX})Ljava/lang/Long;"), false), None);
    assert_eq!(plan(&d, "()I", false), None);
    assert_eq!(plan(&d, &format!("({BOX}I)I"), false), None);
    // `boolean` / `char` are no `Number`; `boolean` widens to nothing.
    assert_eq!(plan("()Z", "()Ljava/lang/Number;", true), None);
    assert_eq!(plan("()C", "()Ljava/lang/Number;", true), None);
    assert_eq!(plan("()Z", "()I", true), None);
    // A reference field: only an `Object` site.
    assert_eq!(
        plan("()Ljava/lang/String;", "()Ljava/lang/Object;", true),
        Some(FieldConvertingPlan {
            receiver_cast: None,
            ret: FieldConvertingReturn::Same
        })
    );
    assert_eq!(
        plan("()Ljava/lang/String;", "()Ljava/lang/CharSequence;", true),
        None
    );
    assert_eq!(plan("()Ljava/lang/Integer;", "()I", true), None);
}

#[test]
fn widening_follows_jls_5_1_2() {
    for (from, tos) in [
        (b'B', &b"SIJFD"[..]),
        (b'S', &b"IJFD"[..]),
        (b'C', &b"IJFD"[..]),
        (b'I', &b"JFD"[..]),
        (b'J', &b"FD"[..]),
        (b'F', &b"D"[..]),
    ] {
        for to in *b"ZBSCIJFD" {
            assert_eq!(
                field_converting_widens(from, to),
                tos.contains(&to),
                "{} -> {}",
                from as char,
                to as char
            );
        }
    }
    for to in *b"ZBSCIJFD" {
        assert!(!field_converting_widens(b'Z', to));
        assert!(!field_converting_widens(b'D', to));
    }
    assert!(matches!(
        field_converting_widen(Value::Int(-7), b'J'),
        Value::Long(-7)
    ));
    assert!(
        matches!(field_converting_widen(Value::Int(3), b'D'), Value::Double(d) if d == 3.0)
    );
    assert!(
        matches!(field_converting_widen(Value::Int(16_777_217), b'F'), Value::Float(f) if f == 16_777_216.0)
    );
    assert!(
        matches!(field_converting_widen(Value::Long(1 << 40), b'D'), Value::Double(d) if d == (1u64 << 40) as f64)
    );
    assert!(
        matches!(field_converting_widen(Value::Float(1.5), b'D'), Value::Double(d) if d == 1.5)
    );
    assert!(matches!(
        field_converting_widen(Value::Int(9), b'I'),
        Value::Int(9)
    ));
}
}

/// Interpreter round i3 wave 81, lane L4: [`mh_site_type_link_error`] rewrites
/// only a thrown `TypeNotPresentException`; the end-to-end rows are
/// `L4W81SiteTypeMissingClass.java` (an application
/// caller) and `L4W79BootSiteType.java` (a boot caller,
/// with `CRATONVM_MH_SITE_TYPE_BOOT_LOADER=1`).
#[cfg(test)]
mod i81_l4_site_type_link_error_tests {
use super::*;
use crate::test_utils::MockNativeContext;

#[test]
fn a_vm_internal_failure_passes_through_unchanged() {
    let mut ctx = MockNativeContext::new();
    for boot_caller in [false, true] {
        let failed = MethodCallFailed::InternalError(VmError::Internal {
            message: "site type".to_string(),
        });
        match mh_site_type_link_error(&mut ctx, failed, boot_caller) {
            MethodCallFailed::InternalError(VmError::Internal { message }) => {
                assert_eq!(message, "site type", "boot_caller={boot_caller}");
            }
            other => panic!("boot_caller={boot_caller}: rewritten to {other:?}"),
        }
    }
}
}

/// Interpreter round i3 wave 81, lane L4: the method-member resolve memo
/// (`CRATONVM_MHN_RESOLVE_METHOD_MEMO`, default off). Every switch is set
/// explicitly through a thread override. The end-to-end rows are
/// `L4W81MethodResolveMemo.java` (behaviour) and
/// `L4W81MethodFinderBench.java`.
#[cfg(test)]
mod i81_l4_method_resolve_memo_tests {
use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeHeapAccess, NativeInvokeAccess,
    NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::flags::with_thread_overrides;

fn key() -> MhnMethodKey {
    MhnMethodKey {
        objects: [0x1000, 0x2000, 0x3000],
        flags: 0x0501_0000,
        caller: 0x4000,
        mode: 0x3f,
        speculative: false,
    }
}

#[test]
fn the_memo_answers_only_its_own_member_caller_mode_and_vm() {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x8101);
    let epoch = ctx.gc_collection_count();
    let k = key();
    mhn_method_memo_put(&ctx, epoch, k, Some(0x0501_0002));
    assert_eq!(mhn_method_memo_get(&ctx, &k), Some(Some(0x0501_0002)));
    for (other, why) in [
        (
            MhnMethodKey {
                caller: 0x4008,
                ..k
            },
            "another caller",
        ),
        (MhnMethodKey { caller: 0, ..k }, "a null caller"),
        (MhnMethodKey { mode: 0x1f, ..k }, "another lookup mode"),
        (MhnMethodKey { mode: -1, ..k }, "the trusted mode"),
        (
            MhnMethodKey {
                speculative: true,
                ..k
            },
            "a speculative resolve",
        ),
        (
            MhnMethodKey {
                flags: 0x0601_0000,
                ..k
            },
            "another reference kind",
        ),
        (
            MhnMethodKey {
                objects: [0x1000, 0x2008, 0x3000],
                ..k
            },
            "another name",
        ),
        (
            MhnMethodKey {
                objects: [0x1000, 0x2000, 0x3008],
                ..k
            },
            "another type",
        ),
        (
            MhnMethodKey {
                objects: [0x1008, 0x2000, 0x3000],
                ..k
            },
            "another class",
        ),
    ] {
        assert_ne!(other, k, "{why}");
        assert_eq!(mhn_method_memo_get(&ctx, &other), None, "{why}");
    }
    // A found member that wrote no flags is remembered as such.
    let k2 = MhnMethodKey {
        caller: 0x5000,
        ..k
    };
    mhn_method_memo_put(&ctx, epoch, k2, None);
    assert_eq!(mhn_method_memo_get(&ctx, &k2), Some(None));
    let other_vm = MockNativeContext::new();
    other_vm.set_vm_identity(0x8102);
    assert_eq!(mhn_method_memo_get(&other_vm, &k), None, "another VM");
}

#[test]
fn nothing_is_stored_across_a_collection_or_for_vm_zero() {
    let ctx = MockNativeContext::new();
    ctx.set_vm_identity(0x8103);
    let k = MhnMethodKey {
        caller: 0x6000,
        ..key()
    };
    let stale_epoch = ctx.gc_collection_count().wrapping_add(1);
    mhn_method_memo_put(&ctx, stale_epoch, k, Some(1));
    assert_eq!(mhn_method_memo_get(&ctx, &k), None);
    let mock = MockNativeContext::new();
    let epoch = mock.gc_collection_count();
    mhn_method_memo_put(&mock, epoch, k, Some(1));
    assert_eq!(mhn_method_memo_get(&mock, &k), None, "vm 0");
}

#[test]
fn every_method_memo_slot_is_in_range() {
    for a in [0usize, 8, 0x1234_5678, usize::MAX] {
        for (mode, speculative) in [(0, false), (-1, true), (0x3f, false)] {
            let k = MhnMethodKey {
                objects: [a, a.wrapping_mul(3), a ^ 0x55],
                flags: i32::MIN,
                caller: a.rotate_left(5),
                mode,
                speculative,
            };
            assert!(mhn_method_memo_slot(&k) < MHN_METHOD_MEMO_SLOTS);
        }
    }
}

// `CRATONVM_MH_COMPAT_FINDER_NEST_ACCESS` (wave 81 rework): a kill
// switch; explicit words only (the default is not pinned).
}

#[cfg(test)]
mod i61_method_type_identity_tests {
use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};

#[test]
fn adaptation_identity_requires_the_same_class_mirrors() {
    let mut ctx = MockNativeContext::new();
    let mt_class = ctx
        .ensure_class_initialized("java/lang/invoke/MethodType")
        .unwrap();
    let mirror_class = ctx.ensure_class_initialized("java/lang/Class").unwrap();
    // Anonymous/hidden mirrors have no renderable descriptor. Distinct
    // mirrors must still force adaptation, as must a changed return class.
    let first = ctx.alloc_object(mirror_class, 4);
    let second = ctx.alloc_object(mirror_class, 4);
    // `MethodType.ptypes` is a `Class[]` (wave 96: was an untyped `Object[]`).
    let make = |ctx: &mut MockNativeContext, ret, param| {
        let params = ctx.new_ref_array(mirror_class, 1);
        ctx.set_array_element(params, 0, Value::Object(Some(param)));
        let mt = ctx.alloc_object(mt_class, 6);
        ctx.set_field_by_name(mt, "rtype", Value::Object(Some(ret)));
        ctx.set_field_by_name(mt, "ptypes", Value::Object(Some(params)));
        mt
    };
    let a = make(&mut ctx, first, first);
    let b = make(&mut ctx, first, first);
    let other_param = make(&mut ctx, first, second);
    let other_return = make(&mut ctx, second, first);
    assert!(method_types_same_classes(&ctx, a, a));
    assert!(method_types_same_classes(&ctx, a, b));
    assert!(!method_types_same_classes(&ctx, a, other_param));
    assert!(!method_types_same_classes(&ctx, a, other_return));
}
}

#[cfg(test)]
mod i94_l4_field_link_first_tests {
use super::*;
use crate::test_utils::MockNativeContext;

/// `CRATONVM_MH_FIELD_LINK_FIRST`'s early serve declines, with nothing
/// done, for a handle that is not a linked field accessor (here a fresh
/// static method handle) at either door, and for a missing site.
#[test]
fn field_link_first_declines_a_handle_that_is_not_a_linked_accessor() {
    let mut ctx = MockNativeContext::new();
    let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
    for door in [LinkingDoor::InvokeExact, LinkingDoor::Invoke] {
        assert!(
            mh_field_link_first(&mut ctx, h, &[Value::Int(1)], Some("(I)I"), door).is_none()
        );
        assert!(mh_field_link_first(&mut ctx, h, &[Value::Int(1)], None, door).is_none());
    }
}
}

#[cfg(test)]
mod jr21_reflect2_linker_key_tests {
use super::*;

/// `CRATONVM_JIT_MH_COMPILED_LINK_LEAN`'s one-`String` key is the key
/// [`mh_linked_invoker`] files the row under, byte for byte.
#[test]
fn the_lean_key_is_the_door_key() {
    for site in [
        "()Ljava/lang/Object;",
        "(Ljava/lang/Object;Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        "(II)I",
        "",
    ] {
        for caller in [1u32, 7, 4096, u32::MAX] {
            assert_eq!(
                jr21_reflect2_linker_key(site, caller),
                cratonvm_native_api::method_type_table::class_keyed(
                    &format!("linker:invokeExact:{site}"),
                    &[caller],
                ),
                "{site} {caller}"
            );
        }
    }
}
}

#[cfg(test)]
mod i95_l4_linker_basic_and_field_lane_tests {
use super::*;
use crate::test_utils::MockNativeContext;

/// The basic type the JDK linker's cached form is keyed by: references
/// and arrays erase to `Object`, subword primitives to `int` (the return
/// too), `long` / `double` take two slots, and a site past 253 slots (a
/// customized, uncached linker) has no key.
#[test]
fn the_linker_basic_descriptor_erases_as_method_type_basic_type() {
    assert_eq!(
        linker_basic_descriptor("(LBox;)I").as_deref(),
        Some("(Ljava/lang/Object;)I")
    );
    assert_eq!(
        linker_basic_descriptor("(LBox;ZBSC[I[[LBox;)Z").as_deref(),
        Some("(Ljava/lang/Object;IIIILjava/lang/Object;Ljava/lang/Object;)I")
    );
    assert_eq!(linker_basic_descriptor("(JD)V").as_deref(), Some("(JD)V"));
    assert_eq!(
        linker_basic_descriptor("()Ljava/lang/String;").as_deref(),
        Some("()Ljava/lang/Object;")
    );
    let wide = "J".repeat(127);
    assert!(linker_basic_descriptor(&format!("({wide})V")).is_none());
    let fits = "J".repeat(126);
    assert!(linker_basic_descriptor(&format!("({fits}I)V")).is_some());
    assert!(linker_basic_descriptor(&format!("({fits}II)V")).is_none());
    assert!(linker_basic_descriptor("(V)V").is_none());
    assert!(linker_basic_descriptor("not a descriptor").is_none());
}

/// `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER` never serves a synthetic
/// carrier (the linker never runs for one either) or a missing site.
#[test]
fn the_field_lane_declines_a_carrier_and_a_missing_site() {
    let mut ctx = MockNativeContext::new();
    let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
    let one = [Value::Int(1)];
    assert!(mh_real_field_before_linker(&mut ctx, h, &one, Some("(I)I"), 0).is_none());
    assert!(mh_real_field_before_linker(&mut ctx, h, &one, None, 0).is_none());
}

/// `CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER` (interpreter round i5 wave 102,
/// lane L4) declines the same: a synthetic carrier, a missing site.
#[test]
fn the_direct_lane_declines_a_carrier_and_a_missing_site() {
    let mut ctx = MockNativeContext::new();
    let h = alloc_method_handle(&mut ctx, "C", "m", "(I)I", MH_KIND_STATIC).unwrap();
    let one = [Value::Int(1)];
    assert!(mh_real_direct_before_linker(&mut ctx, h, &one, Some("(I)I"), 0).is_none());
    assert!(mh_real_direct_before_linker(&mut ctx, h, &one, None, 0).is_none());
}
}

