use super::*;
use crate::test_utils::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{NativeClassAccess, NativeHeapAccess, NativeSystemAccess};

#[test]
fn nat21_only_one_dimensional_primitive_arrays_are_primitive_components() {
    let all = [
        ("[Z", b'Z', ArrayElementType::Boolean),
        ("[B", b'B', ArrayElementType::Byte),
        ("[C", b'C', ArrayElementType::Char),
        ("[S", b'S', ArrayElementType::Short),
        ("[I", b'I', ArrayElementType::Int),
        ("[J", b'J', ArrayElementType::Long),
        ("[F", b'F', ArrayElementType::Float),
        ("[D", b'D', ArrayElementType::Double),
    ];
    for (desc, letter, element) in all {
        assert_eq!(mh_iwa_primitive_component(desc), Some((letter, element)));
    }
    for not in [
        "[Ljava/lang/String;",
        "[[I",
        "[V",
        "[X",
        "[",
        "I",
        "",
        "Ljava/lang/Object;",
    ] {
        assert_eq!(mh_iwa_primitive_component(not), None, "{not}");
    }
}

#[test]
fn nat21_exact_unboxing_takes_the_matching_wrapper_and_declines_the_rest() {
    let mut ctx = MockNativeContext::new();
    let integer = ctx.declare_second_copy("java/lang/Integer");
    let short = ctx.declare_second_copy("java/lang/Short");
    let long = ctx.declare_second_copy("java/lang/Long");
    let double = ctx.declare_second_copy("java/lang/Double");
    let plain = ctx.declare_second_copy("p/NotABox");
    let i = ctx.alloc_object(integer, 1);
    ctx.set_field(i, 0, Value::Int(7));
    let s = ctx.alloc_object(short, 1);
    ctx.set_field(s, 0, Value::Int(5));
    let l = ctx.alloc_object(long, 1);
    ctx.set_field(l, 0, Value::Long(1 << 40));
    let d = ctx.alloc_object(double, 1);
    ctx.set_field(d, 0, Value::Double(2.5));
    let p = ctx.alloc_object(plain, 1);
    let o = |r| Value::Object(Some(r));

    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', o(i)), Some(Value::Int(7)));
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'S', o(s)), Some(Value::Int(5)));
    assert_eq!(
        mh_iwa_exact_unboxed(&mut ctx, b'J', o(l)),
        Some(Value::Long(1 << 40))
    );
    assert_eq!(
        mh_iwa_exact_unboxed(&mut ctx, b'D', o(d)),
        Some(Value::Double(2.5))
    );
    // A widening (`Short` into `int`, `Integer` into `long`) is the JDK's.
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', o(s)), None);
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'J', o(i)), None);
    // A narrowing or unrelated box fails in the JDK's body.
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', o(l)), None);
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', o(p)), None);
    // `null` (an NPE there), a raw primitive and an unknown letter decline.
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', Value::Object(None)), None);
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'I', Value::Int(7)), None);
    assert_eq!(mh_iwa_exact_unboxed(&mut ctx, b'V', o(i)), None);
}
