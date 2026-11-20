#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

use super::*;
use cratonvm_native_api::NativeCallback;

#[test]
fn handlers_match_native_callback_signature() {
    let hash_code: NativeCallback = intrinsic_record_hash_code;
    let equals: NativeCallback = intrinsic_record_equals;
    assert_ne!(hash_code as usize, equals as usize);
}

#[test]
fn primitive_hash_matches_wrapper_semantics() {
    assert_eq!(primitive_hash(&Value::Int(7)), 7);
    assert_eq!(primitive_hash(&Value::Long(1)), 1);
    // Long.hashCode(0x1_0000_0000) == (high ^ low) == 1
    assert_eq!(primitive_hash(&Value::Long(1 << 32)), 1);
    assert_eq!(primitive_hash(&Value::Object(None)), 0);
    assert_eq!(
        primitive_hash(&Value::Float(1.5f32)),
        1.5f32.to_bits() as i32
    );
}

#[test]
fn primitives_equal_uses_bit_semantics_for_floats() {
    assert!(primitives_equal(
        &Value::Double(f64::NAN),
        &Value::Double(f64::NAN)
    ));
    assert!(!primitives_equal(&Value::Double(0.0), &Value::Double(-0.0)));
    assert!(primitives_equal(&Value::Object(None), &Value::Object(None)));
    assert!(!primitives_equal(&Value::Int(1), &Value::Long(1)));
}

/// Every expectation below is the verbatim OpenJDK 25.0.3+9 answer for the
/// javac-generated body, captured by the `RecProbe` differential (G9-1).
///
/// The discriminating input is a NaN with a NON-canonical payload, which is
/// the only place `floatToIntBits` and `floatToRawIntBits` differ. Reached
/// from ordinary bytecode through `Float.intBitsToFloat`.
#[test]
fn a_nan_component_hashes_and_compares_by_canonical_bits() {
    // record F(float v) {}  new F(intBitsToFloat(0x7F800001)).hashCode()
    let odd_nan = f32::from_bits(0x7F80_0001);
    assert!(odd_nan.is_nan());
    assert_eq!(primitive_hash(&Value::Float(odd_nan)), 2_143_289_344);
    assert_eq!(primitive_hash(&Value::Float(f32::NAN)), 2_143_289_344);
    // record D(double v) {}  new D(longBitsToDouble(0x7FF0000000000001L))
    let odd_dnan = f64::from_bits(0x7FF0_0000_0000_0001);
    assert!(odd_dnan.is_nan());
    assert_eq!(primitive_hash(&Value::Double(odd_dnan)), 2_146_959_360);
    assert_eq!(primitive_hash(&Value::Double(f64::NAN)), 2_146_959_360);

    // Two NaNs with different payloads ARE equal components on HotSpot.
    assert!(primitives_equal(
        &Value::Float(odd_nan),
        &Value::Float(f32::NAN)
    ));
    assert!(primitives_equal(
        &Value::Double(odd_dnan),
        &Value::Double(f64::NAN)
    ));

    // Controls: the non-NaN answers must not have moved. `-0.0` keeps its
    // sign bit in both the hash and the comparison.
    assert_eq!(primitive_hash(&Value::Float(1.5f32)), 1_069_547_520);
    assert_eq!(primitive_hash(&Value::Float(-0.0f32)), i32::MIN);
    assert_eq!(primitive_hash(&Value::Double(-0.0f64)), i32::MIN);
    assert!(!primitives_equal(
        &Value::Float(0.0f32),
        &Value::Float(-0.0f32)
    ));
    assert!(!primitives_equal(&Value::Double(0.0), &Value::Double(-0.0)));
}

/// `Boolean.hashCode`'s two constants, which is what a `boolean` record
/// component hashes to — measured, `new Z(false).hashCode() == 1237`.
///
/// The full path needs a live class (`is_boolean_component` reads the
/// record's descriptors), which a leaf-crate unit test cannot build; what
/// is pinned here is the constant pair and the guard's own arithmetic,
/// i.e. that `primitive_hash` is still the INT answer for `0`/`1` so the
/// two are genuinely different values and the branch is not a no-op.
/// Interpreter round i6 wave 117, lane L4: the `boolean`-component memo is
/// per VM. Two memos (two VMs) never answer each other's class ids, a
/// filed class answers per component (past its count: not `boolean`), and
/// an unfiled one is a miss.
#[test]
fn boolean_component_memo_is_per_vm() {
    let vm_a = BooleanComponents::new();
    let vm_b = BooleanComponents::new();
    vm_a.file(7, vec![false, true].into_boxed_slice());
    assert_eq!(vm_a.lookup(7, 0), Some(false));
    assert_eq!(vm_a.lookup(7, 1), Some(true));
    assert_eq!(vm_a.lookup(7, 2), Some(false));
    assert_eq!(vm_a.lookup(8, 0), None);
    assert_eq!(vm_b.lookup(7, 1), None, "another VM's class 7 is not this one's");
    vm_b.file(7, vec![true].into_boxed_slice());
    assert_eq!(vm_b.lookup(7, 0), Some(true));
    assert_eq!(vm_a.lookup(7, 0), Some(false));
}

/// Interpreter round i6 wave 122, lane L4 (`i39-L4`, items 1-2): a class's
/// row holds the `boolean` flags and the fast-path answer apart. Filing one
/// half leaves the other a miss, `(0, 0, 0)` is a real filed answer, and
/// two VMs' tables stay apart.
#[test]
fn record_layout_rows_file_their_halves_apart() {
    let vm_a = BooleanComponents::new();
    let vm_b = BooleanComponents::new();
    vm_a.file_fast_path(7, (1 | 2, 0, 2));
    assert_eq!(vm_a.fast_path(7), Some((3, 0, 2)));
    assert_eq!(vm_a.lookup(7, 0), None, "the boolean half is not filed yet");
    vm_a.file(7, vec![true, false].into_boxed_slice());
    assert_eq!(vm_a.lookup(7, 0), Some(true));
    assert_eq!(vm_a.fast_path(7), Some((3, 0, 2)), "filing booleans keeps the fast half");
    vm_a.file_fast_path(9, (0, 0, 0));
    assert_eq!(vm_a.fast_path(9), Some((0, 0, 0)));
    assert_eq!(vm_b.fast_path(7), None);
}

/// A context with no per-VM slot (the mock) has no memo: the components
/// are read per call, and nothing is filed anywhere.
#[test]
fn a_context_without_the_slot_reads_per_call() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    assert!(boolean_components(&ctx).is_none());
    assert!(!is_boolean_component(
        &mut ctx,
        cratonvm_types::ClassId::new(3),
        0
    ));
    assert!(boolean_components(&ctx).is_none());
}

/// Interpreter round i7 wave 135, lane L3 (`i134-R2`,
/// `CRATONVM_RECORD_BOXED_COMPONENTS`): a boxed primitive's hash and
/// `equals` from its `value` slot are the wrapper's (`Long.hashCode`,
/// `Boolean.hashCode`, `Double.equals` by `doubleToLongBits`), a different
/// class is never equal, and a slot that does not read back as the
/// primitive declines (`None`: the caller dispatches as before).
#[test]
fn i135_l3_boxed_components_answer_as_the_wrappers() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let long_class = cratonvm_types::ClassId::new(41);
    let int_class = cratonvm_types::ClassId::new(42);
    let bool_class = cratonvm_types::ClassId::new(43);
    let double_class = cratonvm_types::ClassId::new(44);
    let boxed = |ctx: &mut crate::test_utils::MockNativeContext, class, value| {
        let obj = ctx.alloc_object(class, 1);
        ctx.set_field(obj, 0, value);
        obj
    };
    // `Long.valueOf(0x1_0000_0002L).hashCode()` is `1 ^ 2 == 3`.
    let l1 = boxed(&mut ctx, long_class, Value::Long(0x1_0000_0002));
    assert_eq!(boxed_hash(&mut ctx, l1, BOXED_PRIMITIVE, 0), Some(3));
    let l2 = boxed(&mut ctx, long_class, Value::Long(0x1_0000_0002));
    let i2 = boxed(&mut ctx, int_class, Value::Int(2));
    assert_eq!(boxed_equals(&mut ctx, l1, l2, 0), Some(true));
    assert_eq!(
        boxed_equals(&mut ctx, l1, i2, 0),
        Some(false),
        "Long.equals(Integer) is false"
    );
    let t = boxed(&mut ctx, bool_class, Value::Int(1));
    let f = boxed(&mut ctx, bool_class, Value::Int(0));
    let boolean = BOXED_PRIMITIVE | BOXED_BOOLEAN;
    assert_eq!(boxed_hash(&mut ctx, t, boolean, 0), Some(BOOLEAN_HASH_TRUE));
    assert_eq!(boxed_hash(&mut ctx, f, boolean, 0), Some(BOOLEAN_HASH_FALSE));
    assert_eq!(boxed_equals(&mut ctx, t, f, 0), Some(false));
    // `Double.valueOf(odd NaN).equals(Double.NaN)` is true; `0.0` vs `-0.0` false.
    let odd_nan = Value::Double(f64::from_bits(0x7FF0_0000_0000_0001));
    let n1 = boxed(&mut ctx, double_class, odd_nan);
    let n2 = boxed(&mut ctx, double_class, Value::Double(f64::NAN));
    assert_eq!(boxed_equals(&mut ctx, n1, n2, 0), Some(true));
    assert_eq!(boxed_hash(&mut ctx, n1, BOXED_PRIMITIVE, 0), Some(2_146_959_360));
    let z1 = boxed(&mut ctx, double_class, Value::Double(0.0));
    let z2 = boxed(&mut ctx, double_class, Value::Double(-0.0));
    assert_eq!(boxed_equals(&mut ctx, z1, z2, 0), Some(false));
    // A slot that is not a primitive declines.
    let odd = boxed(&mut ctx, long_class, Value::Object(None));
    assert_eq!(boxed_hash(&mut ctx, odd, BOXED_PRIMITIVE, 0), None);
    assert_eq!(boxed_hash(&mut ctx, l1, boolean, 0), None, "a Boolean slot must be an int");
    assert_eq!(boxed_equals(&mut ctx, odd, odd, 0), None);
}

#[test]
fn boolean_component_hash_constants() {
    assert_eq!(BOOLEAN_HASH_TRUE, 1231);
    assert_eq!(BOOLEAN_HASH_FALSE, 1237);
    assert_eq!(primitive_hash(&Value::Int(1)), 1);
    assert_eq!(primitive_hash(&Value::Int(0)), 0);
    assert_ne!(primitive_hash(&Value::Int(1)), BOOLEAN_HASH_TRUE);
    assert_ne!(primitive_hash(&Value::Int(0)), BOOLEAN_HASH_FALSE);
    // Every other int value is the same under both wrappers, which is why
    // the guard is `0 | 1` and not "every Int component".
    for v in [-1i32, 2, 7, i32::MIN, i32::MAX] {
        assert_eq!(primitive_hash(&Value::Int(v)), v);
    }
}
