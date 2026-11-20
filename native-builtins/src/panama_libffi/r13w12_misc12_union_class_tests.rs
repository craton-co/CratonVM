//! Round 13 wave 12 (lane misc12): a by-value union's SysV eightbyte
//! classes (`r13w11-ffm6-union-ffi-type-is-always-integer-class`).
use super::*;
use crate::test_utils::{mock_ctx, MockNativeContext};
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::ClassId;

/// `union { double d; float f; }`: one SSE eightbyte, one `double`.
#[test]
fn a_union_of_double_and_float_is_one_double() {
    let classes = sysv_eightbyte_classes(8, &[(0, 8, true), (0, 4, true)]).unwrap();
    assert_eq!(classes, vec![Eightbyte::Sse]);
    assert_eq!(
        union_eightbyte_elems(8, 8, &classes),
        Some(vec![UnionElem::F64])
    );
}

/// An integer member anywhere in an eightbyte makes it INTEGER, and a
/// union with no SSE eightbyte keeps the byte struct.
#[test]
fn an_integer_member_wins_its_eightbyte() {
    // union { float f; int i; }
    let classes = sysv_eightbyte_classes(4, &[(0, 4, true), (0, 4, false)]).unwrap();
    assert_eq!(classes, vec![Eightbyte::Integer]);
    assert_eq!(union_eightbyte_elems(4, 4, &classes), None);
    // union { double d; long l; }
    let classes = sysv_eightbyte_classes(8, &[(0, 8, true), (0, 8, false)]).unwrap();
    assert_eq!(union_eightbyte_elems(8, 8, &classes), None);
}

/// Mixed and partial eightbytes, and the refusals.
#[test]
fn mixed_eightbytes_keep_their_size_and_the_limits_fall_back() {
    // union { struct { float x, y, z; } v; float f; }: 12 bytes, align 4.
    let leaves = [(0, 4, true), (4, 4, true), (8, 4, true), (0, 4, true)];
    let classes = sysv_eightbyte_classes(12, &leaves).unwrap();
    assert_eq!(classes, vec![Eightbyte::Sse, Eightbyte::Sse]);
    assert_eq!(
        union_eightbyte_elems(12, 4, &classes),
        Some(vec![UnionElem::F32, UnionElem::F32, UnionElem::F32])
    );
    // union { struct { double d; long l; } s; double e; }: 16 bytes.
    let leaves = [(0, 8, true), (8, 8, false), (0, 8, true)];
    let classes = sysv_eightbyte_classes(16, &leaves).unwrap();
    assert_eq!(classes, vec![Eightbyte::Sse, Eightbyte::Integer]);
    let elems = union_eightbyte_elems(16, 8, &classes).unwrap();
    assert_eq!(elems.len(), 9);
    assert_eq!(elems[0], UnionElem::F64);
    assert!(elems[1..].iter().all(|e| *e == UnionElem::Byte));
    // Larger than 16 bytes: MEMORY class, the byte struct stays.
    let classes = vec![Eightbyte::Sse; 3];
    assert_eq!(union_eightbyte_elems(24, 8, &classes), None);
    // A misaligned double: C would pass it in memory.
    assert_eq!(sysv_eightbyte_classes(12, &[(4, 8, true)]), None);
    // An all-float eightbyte in a byte-aligned union cannot be rebuilt.
    assert_eq!(union_eightbyte_elems(8, 1, &[Eightbyte::Sse]), None);
}

fn value_layout(ctx: &mut MockNativeContext, class: &str, size: i64) -> ObjectRef {
    let cid = ctx.ensure_class_initialized(class).unwrap();
    let l = ctx.alloc_object(cid, 4);
    ctx.set_field(l, 0, Value::Long(size));
    ctx.set_field(l, 1, Value::Long(size));
    ctx.set_field(l, 2, Value::Object(None));
    ctx.set_field(l, 3, Value::Object(None));
    l
}

/// The whole path on a union carrier: `unionLayout(JAVA_DOUBLE,
/// JAVA_FLOAT)` becomes a struct of ONE `FFI_TYPE_DOUBLE`.
#[test]
fn a_double_float_union_carrier_builds_one_double_element() {
    let mut ctx = mock_ctx();
    let d = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfDouble", 8);
    let f = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
    let cid = ctx
        .ensure_class_initialized("java/lang/foreign/UnionLayout")
        .unwrap();
    // A typed producer (`typed_array_producer_ratchet`).
    let members = ctx.new_ref_array(ClassId::new(1), 2);
    ctx.set_array_element(members, 0, Value::Object(Some(d)));
    ctx.set_array_element(members, 1, Value::Object(Some(f)));
    let u = ctx.alloc_object(cid, 4);
    ctx.set_field(u, 0, Value::Long(8));
    ctx.set_field(u, 1, Value::Long(8));
    ctx.set_field(u, 2, Value::Object(Some(members)));
    ctx.set_field(u, 3, Value::Object(None));

    let ty = union_sysv_eightbyte_ffi_type(&ctx, u, 8, 0).expect("an SSE union");
    // SAFETY: `ty` owns the struct type and its null-terminated elements.
    unsafe {
        let raw = &*ty.as_raw_ptr();
        assert!(!raw.elements.is_null());
        let first = *raw.elements;
        assert!(!first.is_null());
        assert_eq!((*first).type_ as u32, libffi::raw::FFI_TYPE_DOUBLE);
        assert!((*raw.elements.add(1)).is_null(), "exactly one element");
    }
}

/// Round 13 wave 13 (lane ffm7): the AAPCS64 HFA rule for unions
/// (`r13w12-misc12-aarch64-union-hfa-travels-in-general-registers`).
#[test]
fn an_all_float_union_is_an_hfa_of_its_widest_member() {
    // union { float f; float g[2]; }: two floats.
    assert_eq!(
        union_hfa_elems(8, &[(0, 4, true), (0, 4, true), (4, 4, true)]),
        Some((UnionElem::F32, 2))
    );
    // union { double d; }: one double.
    assert_eq!(
        union_hfa_elems(8, &[(0, 8, true)]),
        Some((UnionElem::F64, 1))
    );
    // union { struct { double x, y, z, w; } v; double d; }: four doubles.
    let four = [
        (0, 8, true),
        (8, 8, true),
        (16, 8, true),
        (24, 8, true),
        (0, 8, true),
    ];
    assert_eq!(union_hfa_elems(32, &four), Some((UnionElem::F64, 4)));
}

/// What is not an HFA keeps the byte struct.
#[test]
fn a_mixed_or_integer_union_is_not_an_hfa() {
    // union { float f; double d; }: two base types.
    assert_eq!(union_hfa_elems(8, &[(0, 4, true), (0, 8, true)]), None);
    // union { float f; int i; }
    assert_eq!(union_hfa_elems(4, &[(0, 4, true), (0, 4, false)]), None);
    // Five floats: more than four elements.
    let five: Vec<(usize, usize, bool)> = (0..5).map(|k| (4 * k, 4, true)).collect();
    assert_eq!(union_hfa_elems(20, &five), None);
    // union { float f; } padded to 8: the second slot has no member.
    assert_eq!(union_hfa_elems(8, &[(0, 4, true)]), None);
    // No leaves at all.
    assert_eq!(union_hfa_elems(4, &[]), None);
}

/// The whole builder on a carrier, on every host (it is called on AArch64
/// only): `unionLayout(JAVA_FLOAT, sequenceLayout(2, JAVA_FLOAT))` becomes
/// a struct of exactly two `FFI_TYPE_FLOAT`.
#[test]
fn a_float_union_carrier_builds_two_float_elements() {
    let mut ctx = mock_ctx();
    let f = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
    let f2 = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
    let g = value_layout(&mut ctx, "java/lang/foreign/ValueLayout$OfFloat", 4);
    let scid = ctx
        .ensure_class_initialized("java/lang/foreign/StructLayout")
        .unwrap();
    // `struct { float; float; }` stands in for `float[2]`: same leaves.
    let struct_members = ctx.new_ref_array(ClassId::new(1), 2);
    ctx.set_array_element(struct_members, 0, Value::Object(Some(f2)));
    ctx.set_array_element(struct_members, 1, Value::Object(Some(g)));
    let pair = ctx.alloc_object(scid, 4);
    ctx.set_field(pair, 0, Value::Long(8));
    ctx.set_field(pair, 1, Value::Long(4));
    ctx.set_field(pair, 2, Value::Object(Some(struct_members)));
    ctx.set_field(pair, 3, Value::Object(None));
    let ucid = ctx
        .ensure_class_initialized("java/lang/foreign/UnionLayout")
        .unwrap();
    let members = ctx.new_ref_array(ClassId::new(1), 2);
    ctx.set_array_element(members, 0, Value::Object(Some(f)));
    ctx.set_array_element(members, 1, Value::Object(Some(pair)));
    let u = ctx.alloc_object(ucid, 4);
    ctx.set_field(u, 0, Value::Long(8));
    ctx.set_field(u, 1, Value::Long(4));
    ctx.set_field(u, 2, Value::Object(Some(members)));
    ctx.set_field(u, 3, Value::Object(None));

    let ty = union_aapcs64_hfa_ffi_type(&ctx, u, 8, 0).expect("an HFA union");
    // SAFETY: `ty` owns the struct type and its null-terminated elements.
    unsafe {
        let raw = &*ty.as_raw_ptr();
        assert!(!raw.elements.is_null());
        for k in 0..2 {
            let elem = *raw.elements.add(k);
            assert!(!elem.is_null());
            assert_eq!((*elem).type_ as u32, libffi::raw::FFI_TYPE_FLOAT);
        }
        assert!((*raw.elements.add(2)).is_null(), "exactly two elements");
    }
}
