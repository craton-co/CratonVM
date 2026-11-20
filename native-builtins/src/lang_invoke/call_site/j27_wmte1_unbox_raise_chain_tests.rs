//! JIT round j27 wave 4, lane MHROWS2 (WMTE-1): the JDK frames HotSpot
//! lists above an `asType` unbox refusal the VM serves natively.
use super::*;
use cratonvm_native_api::ServedRaiseHopSite as Site;

fn shape(chain: &cratonvm_native_api::ServedRaiseChain) -> Vec<(&str, &str, Site)> {
    chain.hops.iter().map(|h| (h.class, h.name, h.site)).collect()
}

/// `asType(Object -> int)` of a `Long`: `unboxInteger(Object, false)` ->
/// `primitiveConversion` -> `Integer.class.cast(x)` raises
/// `Cannot cast java.lang.Long to java.lang.Integer` in `Class.cast`
/// (HotSpot 25: `unboxInteger:81, primitiveConversion:252, Class.cast`).
#[test]
fn unconvertible_unbox_lists_value_conversions_and_class_cast() {
    let chain = unbox_raise_chain("I", UnboxRaise::Unconvertible).expect("int unboxes");
    assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
    assert_eq!(
        shape(&chain),
        vec![
            ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
            (
                "sun/invoke/util/ValueConversions",
                "unboxInteger",
                Site::LastCall {
                    name: "primitiveConversion",
                    descriptor: Some(
                        "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;"
                    ),
                },
            ),
            (
                "sun/invoke/util/ValueConversions",
                "primitiveConversion",
                Site::LastCall {
                    name: "cast",
                    descriptor: Some("(Ljava/lang/Object;)Ljava/lang/Object;"),
                },
            ),
            ("java/lang/Class", "cast", Site::Raise),
        ]
    );
    assert_eq!(chain.hops[1].descriptor, "(Ljava/lang/Object;Z)I");
}

/// `asType(Object -> int)` of a `String`: `primitiveConversion`'s
/// `(Number) x` raises (HotSpot 25: `unboxInteger:81,
/// primitiveConversion:247`), a `checkcast`, not a construction.
#[test]
fn not_a_number_unbox_raises_at_the_number_checkcast() {
    let chain = unbox_raise_chain("Z", UnboxRaise::NotANumber).expect("boolean unboxes");
    assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
    assert_eq!(
        shape(&chain),
        vec![
            ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
            (
                "sun/invoke/util/ValueConversions",
                "unboxBoolean",
                Site::LastCall {
                    name: "primitiveConversion",
                    descriptor: Some(
                        "(Lsun/invoke/util/Wrapper;Ljava/lang/Object;Z)Ljava/lang/Number;"
                    ),
                },
            ),
            (
                "sun/invoke/util/ValueConversions",
                "primitiveConversion",
                Site::LocalCheckcast {
                    class_name: "java/lang/Number"
                },
            ),
        ]
    );
}

/// The null chain keeps its pre-j27 shape (wave 60, lane L3).
#[test]
fn null_unbox_raises_in_the_unbox_method() {
    let chain = unbox_raise_chain("J", UnboxRaise::NullReturn).expect("long unboxes");
    assert_eq!(chain.throwable_class, "java/lang/NullPointerException");
    assert_eq!(
        shape(&chain),
        vec![
            ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
            ("sun/invoke/util/ValueConversions", "unboxLong", Site::Raise),
        ]
    );
    assert_eq!(chain.hops[1].descriptor, "(Ljava/lang/Object;Z)J");
}

/// A real field handle's adapted receiver cast raises in `Class.cast`.
#[test]
fn reference_cast_lists_class_cast_above_the_invoker() {
    let chain = reference_cast_raise_chain();
    assert_eq!(chain.throwable_class, "java/lang/ClassCastException");
    assert_eq!(
        shape(&chain),
        vec![
            ("java/lang/invoke/MethodHandle", "invokeExact", Site::Hidden),
            ("java/lang/Class", "cast", Site::Raise),
        ]
    );
}

#[test]
fn a_reference_descriptor_has_no_unbox_chain() {
    assert!(unbox_raise_chain("Ljava/lang/Object;", UnboxRaise::Unconvertible).is_none());
    assert!(unbox_raise_chain("V", UnboxRaise::NullReturn).is_none());
}

/// MH16-10 (j27 wave 4, lane MHROWS2): the moving mock remaps the per-VM
/// `valueOf` caches as the VM's GC does (`gc_update_value_of_cache_refs`),
/// so a cached box asked for again after a move is the MOVED box, as on
/// the VM, and a moving test may box inside `-128..127`.
#[test]
fn the_moving_mock_forwards_the_value_of_cache() {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    let mut ctx = crate::test_utils::MockNativeContext::new();
    ctx.set_vm_identity(0x2710_4D16);
    let _ = ctx.ensure_class_initialized("java/lang/Integer");
    ctx.set_moving_heap_vm_rule(true);
    let boxed = |ctx: &mut crate::test_utils::MockNativeContext| {
        match crate::lang_math::native_integer_value_of(ctx, &[Value::Int(7)]) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("Integer.valueOf(7): {other:?}"),
        }
    };
    let first = boxed(&mut ctx);
    let pin = ctx.pin_native_root(first);
    // A GC point in both moving modes (its answer is the scripted help).
    let _ = ctx.reclaim_before_alloc_retry();
    let moved = ctx.read_native_pin(pin, first);
    assert_ne!(moved, first, "the reclaim must move the heap");
    assert_eq!(boxed(&mut ctx), moved, "valueOf(7) answers the moved canonical box");
}

/// The refusal the caller sees is unchanged: `Class.cast`'s wording.
#[test]
fn unconvertible_wrapper_still_refused_with_class_cast_wording() {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess};
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let _ = ctx.ensure_class_initialized("java/lang/Long");
    let boxed = match ctx.new_object("java/lang/Long") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("new_object Long: {other:?}"),
    };
    ctx.set_field(boxed, 0, Value::Long(6));
    let refused = mh_unbox_for_primitive(&mut ctx, "I", Value::Object(Some(boxed)), None);
    match refused {
        Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::ClassCastException { message, .. },
        ))) => assert_eq!(message, "Cannot cast java.lang.Long to java.lang.Integer"),
        other => panic!("not a ClassCastException: {other:?}"),
    }
}
