use super::coerce_static_to_descriptor;
use super::typed_default_static_slots;
use crate::types::Value;

/// The defect this guards: `Unsafe`'s nine `ARRAY_*_BASE_OFFSET` fields are
/// declared `J`, the fixup hands them an integer literal, and a slot
/// holding `Value::Int` where a `long` belongs reads back as garbage from
/// JIT-compiled code — `Arrays.equals(long[],long[])` answered `true` for
/// unequal arrays. Assert on the VARIANT, not just the numeric value: an
/// `assert_eq!(as_i64(...), 16)` passes just as happily on the broken one.
#[test]
fn a_long_field_gets_a_long_even_when_the_call_site_writes_an_int() {
    assert!(matches!(
        coerce_static_to_descriptor("J", Value::Int(16)),
        Some(Value::Long(16))
    ));
    assert!(matches!(
        coerce_static_to_descriptor("J", Value::Long(16)),
        Some(Value::Long(16))
    ));
}

#[test]
fn the_narrow_integral_descriptors_stay_int() {
    // `ARRAY_*_INDEX_SCALE` (I), `String.LATIN1`/`UTF16` (B),
    // `UnsafeConstants.BIG_ENDIAN` (Z) all share this arm.
    for d in ["I", "S", "B", "C", "Z"] {
        assert!(
            matches!(
                coerce_static_to_descriptor(d, Value::Int(4)),
                Some(Value::Int(4))
            ),
            "descriptor {d}"
        );
        // A long-typed literal narrows rather than being refused: the call
        // sites are integer constants, not user input.
        assert!(
            matches!(
                coerce_static_to_descriptor(d, Value::Long(4)),
                Some(Value::Int(4))
            ),
            "descriptor {d}"
        );
    }
}

#[test]
fn floating_descriptors_convert_and_references_pass_through() {
    assert!(matches!(
        coerce_static_to_descriptor("F", Value::Int(2)),
        Some(Value::Float(f)) if f == 2.0
    ));
    assert!(matches!(
        coerce_static_to_descriptor("D", Value::Int(2)),
        Some(Value::Double(d)) if d == 2.0
    ));
    assert!(matches!(
        coerce_static_to_descriptor("Ljava/lang/Object;", Value::Object(None)),
        Some(Value::Object(None))
    ));
}

/// The whole point of [`typed_default_static_slots`]: a `J`/`D` slot must
/// NOT come back as `Value::Int(0)`.
///
/// `StaticsBlock::new` zero-fills with `Int(0)`, which the interpreter
/// widens and JIT-compiled code reads as garbage — it takes the load width
/// from the descriptor and pulls 8 bytes over a 4-byte payload. Asserting
/// the WIDTH (the `Value` variant), not the numeric value, is what makes
/// this test able to fail: every arm below is zero either way.
#[test]
fn wide_static_slots_default_to_their_descriptor_width_not_int_zero() {
    let descriptors = ["J", "I", "D", "F", "Ljava/lang/Object;", "[B", "Z"];
    let slots = typed_default_static_slots(&descriptors, descriptors.len());
    assert!(
        matches!(slots[0], Value::Long(0)),
        "J must be Long, got {:?}",
        slots[0]
    );
    assert!(matches!(slots[1], Value::Int(0)));
    assert!(
        matches!(slots[2], Value::Double(d) if d == 0.0),
        "D must be Double"
    );
    assert!(matches!(slots[3], Value::Float(f) if f == 0.0));
    assert!(matches!(slots[4], Value::Object(None)));
    assert!(matches!(slots[5], Value::Object(None)));
    assert!(matches!(slots[6], Value::Int(0)));
}

/// The block is historically sized by the class's TOTAL field count while
/// slot indices are the STATIC-field enumeration order, so the tail is
/// slack. It must stay allocated (callers index into it) and must not run
/// off the end when there are fewer descriptors than slots.
#[test]
fn the_slack_tail_past_the_static_descriptors_is_kept_and_zeroed() {
    let slots = typed_default_static_slots(&["J"], 4);
    assert_eq!(slots.len(), 4);
    assert!(matches!(slots[0], Value::Long(0)));
    assert!(matches!(slots[3], Value::Int(0)));
    // Fewer slots than descriptors must truncate, not panic.
    assert_eq!(typed_default_static_slots(&["J", "D", "I"], 1).len(), 1);
    assert!(typed_default_static_slots::<&str>(&[], 0).is_empty());
}

/// A mismatch the fixup cannot repair must be REFUSED, not written: a
/// mistyped slot is worse than the well-typed zero a swallowed `<clinit>`
/// leaves behind, and the caller turns `None` into a visible shortfall in
/// the `populated (n/18)` warning.
#[test]
fn an_impossible_coercion_is_refused() {
    assert!(coerce_static_to_descriptor("Ljava/lang/String;", Value::Int(16)).is_none());
    assert!(coerce_static_to_descriptor("[I", Value::Long(16)).is_none());
    assert!(coerce_static_to_descriptor("J", Value::Object(None)).is_none());
}
