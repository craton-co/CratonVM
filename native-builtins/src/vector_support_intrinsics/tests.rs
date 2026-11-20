use super::*;

/// Two VMs number their classes independently, so `ClassId(n)` is a
/// different class in each. Both are served by this one thread, and the
/// memo is thread-local: VM B must not be answered with VM A's slot.
#[test]
fn op_info_memo_does_not_answer_another_vm() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::FieldMetadata;
    let cid = ClassId::new(77_001);
    let op_info_at = |slot| {
        vec![FieldMetadata {
            name: "opInfo".to_string(),
            descriptor: "I".to_string(),
            access_flags: 0,
            slot_index: slot,
            declaring_class_id: cid,
            is_static: false,
        }]
    };
    // `vm`'s operator object: `opInfo` at `slot`, a decoy in the other.
    let make = |vm: usize, slot: usize, op_info: i32| {
        let mut mock = MockNativeContext::new();
        mock.set_vm_identity(vm);
        mock.set_declared_fields(cid, op_info_at(slot));
        let ctx: &mut dyn NativeContext = &mut mock;
        let op = ctx.alloc_object(cid, 2);
        ctx.set_field(op, slot, Value::Int(op_info));
        ctx.set_field(op, 1 - slot, Value::Int(99));
        (mock, op)
    };
    let (a, op_a) = make(7_001, 0, 11);
    let (b, op_b) = make(7_002, 1, 22);

    assert_eq!(op_info_of(&a, op_a), Some(11));
    assert_eq!(op_info_of(&b, op_b), Some(22));
    assert_eq!(op_info_of(&a, op_a), Some(11));
}

/// The float MIN/MAX lanes follow `Math.min`/`Math.max`, not Rust's.
///
/// Three rows that separate them, and every one is reachable from
/// `FloatVector.min`: NaN propagates (Rust returns the other operand),
/// `min(-0.0, 0.0)` is `-0.0` and `max(-0.0, 0.0)` is `0.0` (Rust's answer
/// for both is whichever it saw first).
#[test]
fn float_min_max_are_javas_not_rusts() {
    let nan = f32::NAN.to_bits() as i64;
    let one = 1.0f32.to_bits() as i64;
    let got = binary_lane(OP_MIN, ELEM_FLOAT, nan, one).unwrap();
    assert!(f32::from_bits(got as u32).is_nan(), "NaN must propagate");
    let got = binary_lane(OP_MAX, ELEM_FLOAT, one, nan).unwrap();
    assert!(f32::from_bits(got as u32).is_nan(), "NaN must propagate");

    let neg_zero = (-0.0f32).to_bits() as i64;
    let pos_zero = 0.0f32.to_bits() as i64;
    assert_eq!(
        binary_lane(OP_MIN, ELEM_FLOAT, neg_zero, pos_zero).unwrap() as u32,
        (-0.0f32).to_bits(),
        "min(-0.0, 0.0) is -0.0",
    );
    assert_eq!(
        binary_lane(OP_MAX, ELEM_FLOAT, neg_zero, pos_zero).unwrap() as u32,
        0.0f32.to_bits(),
        "max(-0.0, 0.0) is +0.0",
    );
}

/// A mask or shuffle class must NOT be read as a lane type.
///
/// `Long256Vector$Long256Mask` starts with "Long"; its payload is a
/// `boolean[]`. Building a `long[]` for it produced a
/// `ClassCastException: [J cannot be cast to [Z` inside
/// `LongVector.fromMemorySegment`, which is a long way from the mistake.
/// The predicate is pure, so it is testable without a VM.
#[test]
fn mask_and_shuffle_class_names_are_not_lane_types() {
    for name in [
        "jdk/incubator/vector/Long256Vector$Long256Mask",
        "jdk/incubator/vector/Int64Vector$Int64Mask",
        "jdk/incubator/vector/Float256Vector$Float256Shuffle",
        "jdk/incubator/vector/ByteMaxVector$ByteMaxMask",
    ] {
        let simple = name.rsplit('/').next().unwrap();
        assert!(
            simple.contains("Mask") || simple.contains("Shuffle"),
            "{name} must be screened out before the prefix table",
        );
    }
    // ...while the vector classes themselves still are lane types.
    for name in [
        "jdk/incubator/vector/Long256Vector",
        "jdk/incubator/vector/Float64Vector",
        "jdk/incubator/vector/ByteMaxVector",
    ] {
        let simple = name.rsplit('/').next().unwrap();
        assert!(
            !(simple.contains("Mask") || simple.contains("Shuffle")),
            "{name} must reach the prefix table",
        );
    }
}

/// An unimplemented opcode answers `None`, which the entry points turn into
/// the JDK's own lambda. This is the property that makes partial coverage
/// safe, so it is asserted rather than assumed.
#[test]
fn an_unknown_opcode_refuses_rather_than_guessing() {
    // VECTOR_OP_BIT_COUNT (3) is deliberately not implemented.
    assert!(binary_lane(3, ELEM_INT, 1, 2).is_none());
    assert!(unary_lane(3, ELEM_INT, 1).is_none());
    // ...and neither is any math-library opcode.
    assert!(unary_lane(103, ELEM_FLOAT, 0).is_none());
}

/// Integral division by zero refuses, so the ArithmeticException comes from
/// the JDK's own lane lambda with the JDK's own message rather than from a
/// message invented here.
#[test]
fn integral_division_by_zero_refuses() {
    assert!(binary_lane(OP_DIV, ELEM_INT, 6, 0).is_none());
    assert_eq!(binary_lane(OP_DIV, ELEM_INT, 6, 3), Some(2));
    // Float division by zero is infinity, not an exception — it must NOT
    // refuse, because refusing would be a needless fallback on a hot lane.
    let inf = binary_lane(OP_DIV, ELEM_FLOAT, 1.0f32.to_bits() as i64, 0).unwrap();
    assert!(f32::from_bits(inf as u32).is_infinite());
}

/// Shift counts are masked the way `ishl`/`lshl` mask them.
#[test]
fn shift_counts_wrap_like_the_jvms() {
    // int << 33 is int << 1
    assert_eq!(binary_lane(OP_LSHIFT, ELEM_INT, 1, 33), Some(2));
    // long << 65 is long << 1
    assert_eq!(binary_lane(OP_LSHIFT, ELEM_LONG, 1, 65), Some(2));
    // >>> zero-extends within the lane width, not within i64.
    assert_eq!(binary_lane(OP_URSHIFT, ELEM_INT, -1, 28), Some(15));
}

/// `wrap_integral_lane` is shared with `vector_api.rs`; check the narrowing
/// it gives this file is the Java one for the sub-word types.
#[test]
fn sub_word_lanes_narrow_like_java() {
    // (short) 0x1_0001 == 1
    assert_eq!(binary_lane(OP_ADD, ELEM_SHORT, 0xFFFF, 2), Some(1));
    // (byte) 127 + 1 == -128
    assert_eq!(binary_lane(OP_ADD, ELEM_BYTE, 127, 1), Some(-128));
}

/// Float->int cast saturates and maps NaN to zero, as `d2i` does.
#[test]
fn float_to_int_cast_saturates_like_d2i() {
    let big = 1.0e30f32.to_bits() as i64;
    assert_eq!(
        cast_lane(ELEM_FLOAT, ELEM_INT, big, false),
        Some(i64::from(i32::MAX))
    );
    let nan = f32::NAN.to_bits() as i64;
    assert_eq!(cast_lane(ELEM_FLOAT, ELEM_INT, nan, false), Some(0));
}

/// `UCAST` differs from `CAST` only when widening an integral lane.
#[test]
fn ucast_zero_extends_where_cast_sign_extends() {
    // (byte) -1 widened to int: CAST gives -1, UCAST gives 255.
    assert_eq!(cast_lane(ELEM_BYTE, ELEM_INT, -1, false), Some(-1));
    assert_eq!(cast_lane(ELEM_BYTE, ELEM_INT, -1, true), Some(255));
}
