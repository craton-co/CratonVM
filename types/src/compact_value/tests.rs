use super::*;

// -- Size assertion ------------------------------------------------------

#[test]
fn compact_value_is_8_bytes() {
    assert_eq!(std::mem::size_of::<CompactValue>(), 8);
}

// -- Int round-trips -----------------------------------------------------

#[test]
fn int_roundtrip_positive() {
    let cv = CompactValue::int(42);
    assert_eq!(cv.tag(), CompactTag::Int);
    assert_eq!(cv.as_int(), Some(42));
}

#[test]
fn int_roundtrip_zero() {
    let cv = CompactValue::int(0);
    assert_eq!(cv.tag(), CompactTag::Int);
    assert_eq!(cv.as_int(), Some(0));
}

#[test]
fn int_roundtrip_negative() {
    let cv = CompactValue::int(-1);
    assert_eq!(cv.as_int(), Some(-1));
    let cv = CompactValue::int(-999_999);
    assert_eq!(cv.as_int(), Some(-999_999));
}

#[test]
fn int_roundtrip_min_max() {
    let cv_min = CompactValue::int(i32::MIN);
    assert_eq!(cv_min.as_int(), Some(i32::MIN));
    let cv_max = CompactValue::int(i32::MAX);
    assert_eq!(cv_max.as_int(), Some(i32::MAX));
}

// -- Long round-trips ----------------------------------------------------

#[test]
fn long_roundtrip_basic() {
    let cv = CompactValue::long(123_456_789_012_345i64);
    assert_eq!(cv.as_long_unchecked(), 123_456_789_012_345i64);
}

#[test]
fn long_roundtrip_zero() {
    let cv = CompactValue::long(0);
    assert_eq!(cv.as_long_unchecked(), 0);
}

#[test]
fn long_roundtrip_negative() {
    let cv = CompactValue::long(-1);
    assert_eq!(cv.as_long_unchecked(), -1);
    let cv = CompactValue::long(-999_999_999_999i64);
    assert_eq!(cv.as_long_unchecked(), -999_999_999_999i64);
}

#[test]
fn long_roundtrip_min_max() {
    let cv_min = CompactValue::long(i64::MIN);
    assert_eq!(cv_min.as_long_unchecked(), i64::MIN);
    let cv_max = CompactValue::long(i64::MAX);
    assert_eq!(cv_max.as_long_unchecked(), i64::MAX);
}

// -- CRIT-1: `as_long()` must return `Some(_)` for every value that
// `to_value()` resolves to `Value::Long(_)`, including longs whose
// bit pattern collides with the NaN-tag space (e.g. `-1`, `i64::MIN`).
// Prior behavior returned `None` for those collisions because
// `as_long()` only handled the untagged path.

#[test]
fn as_long_handles_nan_tag_collisions() {
    // i64::MIN — high bit set, looks NaN-tagged.
    assert_eq!(CompactValue::long(i64::MIN).as_long(), Some(i64::MIN));
    // -1 — all-ones, the canonical collision case.
    assert_eq!(CompactValue::long(-1).as_long(), Some(-1));
    // 0 — untagged path; the baseline that always worked.
    assert_eq!(CompactValue::long(0).as_long(), Some(0));
}

#[test]
fn as_long_handles_positive_longs() {
    for v in [1_i64, 42, 1_000_000, 123_456_789_012_345, i64::MAX] {
        assert_eq!(
            CompactValue::long(v).as_long(),
            Some(v),
            "as_long() failed for {v}"
        );
    }
}

#[test]
fn as_long_handles_negative_longs() {
    for v in [-2_i64, -1000, -999_999_999_999, i64::MIN + 1] {
        assert_eq!(
            CompactValue::long(v).as_long(),
            Some(v),
            "as_long() failed for {v}"
        );
    }
}

/// `as_long()` and `to_value()` must agree: every bit pattern that
/// decodes to `Value::Long(x)` must also yield `Some(x)` from
/// `as_long()`.
#[test]
fn as_long_agrees_with_to_value_for_long_collisions() {
    for v in [
        i64::MIN,
        -1,
        0,
        1,
        i64::MAX,
        -42,
        i64::MIN + 1,
        i64::MAX - 1,
    ] {
        let cv = CompactValue::long(v);
        match cv.to_value() {
            Value::Long(x) => assert_eq!(
                cv.as_long(),
                Some(x),
                "as_long()/to_value() disagree for {v}"
            ),
            Value::Double(_) => assert_eq!(
                cv.as_long(),
                Some(v),
                "untagged long decoded as Double should still expose i64 via as_long()"
            ),
            other => panic!("unexpected Value variant for long {v}: {other:?}"),
        }
    }
}

/// `as_long()` must still return `None` for genuinely non-long tagged
/// values (Int, Float, Object, Null, Uninitialized, ReturnAddress).
#[test]
fn as_long_returns_none_for_non_longs() {
    assert_eq!(CompactValue::int(42).as_long(), None);
    assert_eq!(CompactValue::float(1.5).as_long(), None);
    assert_eq!(CompactValue::object(0x1000).as_long(), None);
    assert_eq!(CompactValue::null().as_long(), None);
    assert_eq!(CompactValue::uninitialized().as_long(), None);
    assert_eq!(CompactValue::return_address(7).as_long(), None);
}

// -- NaN-box collision: bit-exact round-trip -----------------------------
//
// `CompactValue::long` stores longs verbatim, including bit patterns
// that collide with the NaN-tagged sub-tag space (BC SM2 / LongArray
// bug: lossy re-tag was corrupting `0xfffd…` longs to `0xffff…`).
// Heap safety for slots whose bit pattern incidentally looks like
// `SUB_OBJECT` is now enforced by the GC root scanners filtering
// through `VmHeap::is_object_address`, not by the encoder.

/// Helper: a colliding long bit pattern with a chosen 3-bit would-be
/// sub-tag in bits 49-47 and a chosen 47-bit low payload.
fn collide_with_subtag(sub: u64, low: u64) -> i64 {
    (NANBOX_BITS | (sub << SUBTAG_SHIFT) | (low & PAYLOAD_MASK)) as i64
}

#[test]
fn long_collision_round_trip_exact_for_every_subtag() {
    // Bit-exact round-trip across every would-be sub-tag (0..=7) in the
    // colliding space, including SUB_OBJECT (2) — the exact BC SM2
    // regression case where the prior lossy re-tag was corrupting bits.
    for sub in 0..8u64 {
        for &low in &[0u64, PAYLOAD_MASK, 0x1_0000, 0x0BAD_BEEF] {
            // Skip a few ambiguous (sub, payload) pairs whose bit
            // pattern is identical to a real non-long value:
            //   * (sub=INT, payload < 2^32) — indistinguishable from a
            //     real int slot, where descriptor-J applies JVMS i2l
            //     widening (sign-extends the low 32 bits).
            //   * (sub=NULL/UNINIT, payload=0) — identical to
            //     `CompactValue::null()` / `uninitialized()`; J-decode
            //     honors JVMS §2.3 default 0L.
            // The verifier disambiguates in real bytecode (lstore never
            // writes a non-long into a J slot).
            if sub == SUB_INT && low >> 32 == 0 {
                continue;
            }
            if low == 0 && (sub == SUB_NULL || sub == SUB_UNINIT) {
                continue;
            }
            let v = collide_with_subtag(sub, low);
            let cv = CompactValue::long(v);
            assert_eq!(
                cv.as_long_unchecked(),
                v,
                "long must round-trip bit-exact for {v:#018x} (sub={sub})",
            );
            assert_eq!(
                cv.decode_by_descriptor(b'J'),
                Value::Long(v),
                "J-descriptor decode must round-trip bit-exact for {v:#018x}",
            );
        }
    }
}

/// Colliding longs whose natural sub-tag is already a long sub-tag
/// (bits 49-48 set) round-trip bit-exactly — this covers `-1`, `-2`
/// and every small-magnitude negative.
#[test]
fn long_natural_long_subtag_collisions_round_trip_exact() {
    for v in [
        -1i64,
        -2,
        -3,
        -1000,
        -999_999_999_999,
        -123_456,
        collide_with_subtag(SUB_LONG_LO, 0x12_3456),
        collide_with_subtag(SUB_LONG_HI, 0x7F_FFFF),
    ] {
        let cv = CompactValue::long(v);
        assert!(!cv.is_object());
        assert_eq!(cv.tag(), CompactTag::Long);
        assert_eq!(cv.as_long(), Some(v), "as_long mismatch for {v:#018x}");
        assert_eq!(cv.as_long_unchecked(), v);
        assert_eq!(cv.to_value(), Value::Long(v));
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(v));
        assert!(cv.is_category2());
    }
}

/// Non-colliding longs (the untagged fast path) — including the named
/// boundary values — round-trip bit-exactly and are never objects.
#[test]
fn long_non_colliding_round_trip_exact() {
    for v in [
        0i64,
        1,
        42,
        -42,
        i64::MAX,
        i64::MIN,
        i64::MIN + 1,
        i64::MAX - 1,
        123_456_789_012_345,
        0x0BAD_BEEF_DEAD_CAFEu64 as i64,
    ] {
        let cv = CompactValue::long(v);
        assert!(!cv.is_object(), "is_object() true for {v:#018x}");
        assert_ne!(cv.tag(), CompactTag::Object);
        assert_ne!(cv.tag(), CompactTag::ReturnAddress);
        assert_eq!(cv.as_long_unchecked(), v);
        assert_eq!(cv.as_long(), Some(v));
        assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(v));
    }
}

/// `i64::MIN` / `i64::MAX` are NOT in the colliding space (bit 63 alone,
/// or bit 63 clear, does not satisfy the full NaN-box marker) — they take
/// the untagged fast path.  Documented invariant guard.
#[test]
fn long_min_max_are_non_colliding() {
    assert!(!is_nan_tagged(i64::MIN as u64));
    assert!(!is_nan_tagged(i64::MAX as u64));
    assert!(!CompactValue::long(i64::MIN).is_object());
    assert!(!CompactValue::long(i64::MAX).is_object());
    assert_eq!(CompactValue::long(i64::MIN).as_long_unchecked(), i64::MIN);
    assert_eq!(CompactValue::long(i64::MAX).as_long_unchecked(), i64::MAX);
}

/// A colliding long routed through the J descriptor must always decode
/// to `Value::Long(_)` regardless of its natural sub-tag (the operand
/// stack's verifier-derived typing disambiguates). `pop_static_field_value`
/// would reject any other variant. `SUB_INT` widens (i2l) rather than
/// reinterprets bits.
#[test]
fn long_collision_descriptor_decode_always_returns_long() {
    for sub in 0..8u64 {
        let v = collide_with_subtag(sub, 0x55_5555);
        let cv = CompactValue::long(v);
        assert!(
            matches!(cv.decode_by_descriptor(b'J'), Value::Long(_)),
            "J-descriptor decode must return Value::Long for sub={sub}",
        );
        // SUB_INT applies JVMS i2l widening (sign-extend the low 32
        // bits) rather than reinterpreting the full 64-bit pattern.
        let expected = if sub == SUB_INT {
            Value::Long((v as u64 & PAYLOAD_MASK) as u32 as i32 as i64)
        } else {
            Value::Long(v)
        };
        assert_eq!(
            cv.decode_by_descriptor(b'J'),
            expected,
            "J-descriptor decode mismatch for sub={sub}",
        );
    }
}

/// `int_tag_collision_long` distinguishes a real int (payload < 2^32)
/// from a long whose bits collide into the SUB_INT space (payload bits
/// 32-46 set). Drives the JIT/OSR local-slot transfer fix.
#[test]
fn int_tag_collision_long_discriminates() {
    const NANBOX: u64 = 0xFFFC_0000_0000_0000;
    // Genuine ints: payload < 2^32 → None (kept as int).
    for n in [0i32, 1, -1, 42, i32::MIN, i32::MAX] {
        assert_eq!(
            CompactValue::int(n).int_tag_collision_long(),
            None,
            "int {n}"
        );
    }
    // SUB_INT-pattern longs with payload bits 32-46 set → Some(raw bits).
    for &bits in &[
        NANBOX | (1u64 << 32),
        NANBOX | 0x7FFF_FFFF_FFFF,
        0xFFFC_0001_ABCD_1234u64,
        0xFFFC_5555_5555_5555u64,
    ] {
        assert_eq!(
            CompactValue::long(bits as i64).int_tag_collision_long(),
            Some(bits as i64),
            "collision long {bits:#018x}",
        );
    }
    // Non-Int tags → None.
    assert_eq!(CompactValue::long(-1).int_tag_collision_long(), None); // SUB_LONG_HI
    assert_eq!(CompactValue::long(i64::MIN).int_tag_collision_long(), None); // untagged
    assert_eq!(CompactValue::float(1.5).int_tag_collision_long(), None);
    assert_eq!(CompactValue::null().int_tag_collision_long(), None);
    // Residual ambiguous case: payload < 2^32 with SUB_INT pattern is
    // indistinguishable from a real int → None (documented limitation).
    assert_eq!(
        CompactValue::long(NANBOX as i64).int_tag_collision_long(),
        None
    );
}

/// MEDIUM (2026-06-17): `to_value()` / `as_int()` SUB_INT/SUB_FLOAT
/// long-collision guard. A primitive long whose verbatim bits collide
/// into the SUB_INT/SUB_FLOAT sub-tag space with payload bits 32..46 set
/// must NOT be truncated to a 32-bit Int/Float — it must be preserved
/// bit-exact as `Value::Long`, and `as_int()` must decline it.
#[test]
fn to_value_int_float_collision_preserves_long() {
    // Genuine ints / floats (32-bit payload) still decode normally and
    // round-trip via as_int / as_float.
    for n in [0i32, 1, -1, 42, i32::MIN, i32::MAX] {
        let cv = CompactValue::int(n);
        assert_eq!(cv.to_value(), Value::Int(n), "real int {n}");
        assert_eq!(cv.as_int(), Some(n), "as_int real int {n}");
    }
    for f in [0.0f32, 1.5, -3.25, f32::MAX] {
        assert_eq!(
            CompactValue::float(f).to_value(),
            Value::Float(f),
            "real float {f}"
        );
    }

    // Colliding longs: SUB_INT / SUB_FLOAT sub-tag with payload bits
    // 32..46 set. These cannot be real int/float slots, so to_value()
    // must preserve the full i64 bit pattern (not truncate to 32 bits),
    // and as_int() must return None for the SUB_INT case.
    for &low in &[
        1u64 << 32,
        0x7FFF_FFFF_FFFF,
        0x1_ABCD_1234,
        0x5555_5555_5555,
    ] {
        if low >> 32 == 0 {
            continue; // not actually a collision payload
        }
        for &sub in &[SUB_INT, SUB_FLOAT] {
            let v = collide_with_subtag(sub, low);
            let cv = CompactValue::long(v);
            assert_eq!(
                cv.to_value(),
                Value::Long(v),
                "collision long must be preserved bit-exact (sub={sub}, low={low:#x})",
            );
            // as_int must decline a SUB_INT-patterned collision long
            // rather than hand back a truncated low-32-bits int.
            if sub == SUB_INT {
                assert_eq!(
                    cv.as_int(),
                    None,
                    "as_int must decline SUB_INT collision long (low={low:#x})",
                );
            }
        }
    }
}

// -- Float round-trips ---------------------------------------------------

#[test]
fn float_roundtrip_basic() {
    let cv = CompactValue::float(3.14f32);
    assert_eq!(cv.tag(), CompactTag::Float);
    let f = cv.as_float().unwrap();
    assert_eq!(f.to_bits(), 3.14f32.to_bits());
}

#[test]
fn float_roundtrip_zero() {
    let cv = CompactValue::float(0.0f32);
    assert_eq!(cv.as_float().unwrap().to_bits(), 0.0f32.to_bits());
}

#[test]
fn float_roundtrip_negative_zero() {
    let cv = CompactValue::float(-0.0f32);
    assert_eq!(cv.as_float().unwrap().to_bits(), (-0.0f32).to_bits());
}

#[test]
fn float_roundtrip_nan() {
    let cv = CompactValue::float(f32::NAN);
    assert!(cv.as_float().unwrap().is_nan());
}

// -- Double round-trips --------------------------------------------------

#[test]
fn double_roundtrip_basic() {
    let cv = CompactValue::double(2.718_281_828_459_045);
    assert_eq!(cv.tag(), CompactTag::Double);
    let d = cv.as_double().unwrap();
    // Bit equality: a stored value that is read back has been through NO rounding step, so the round trip is bit-exact or the slot corrupted it.
    assert_eq!(d.to_bits(), 2.718_281_828_459_045f64.to_bits());
}

#[test]
fn double_roundtrip_zero() {
    let cv = CompactValue::double(0.0f64);
    assert_eq!(cv.as_double().unwrap().to_bits(), 0.0f64.to_bits());
}

#[test]
fn double_roundtrip_infinity() {
    let cv = CompactValue::double(f64::INFINITY);
    assert_eq!(cv.as_double().unwrap(), f64::INFINITY);
}

#[test]
fn double_roundtrip_neg_infinity() {
    let cv = CompactValue::double(f64::NEG_INFINITY);
    assert_eq!(cv.as_double().unwrap(), f64::NEG_INFINITY);
}

#[test]
fn double_canonical_nan() {
    // Storing a NaN should produce the canonical NaN or preserve the bits
    // if they don't collide with our tag space.
    let cv = CompactValue::double(f64::NAN);
    let d = cv.as_double().unwrap();
    assert!(d.is_nan());
}

// -- Object pointer round-trips ------------------------------------------

#[test]
fn object_roundtrip() {
    let ptr: u64 = 0x0000_1234_5678_ABC0;
    let cv = CompactValue::object(ptr);
    assert_eq!(cv.tag(), CompactTag::Object);
    assert_eq!(cv.as_object_ptr(), Some(ptr));
}

#[test]
fn object_large_pointer_47bit() {
    // Maximum 47-bit user-space pointer (bit 46 set, etc.)
    let ptr: u64 = 0x0000_7FFF_FFFF_FFF8; // 47-bit, 8-byte aligned
    let cv = CompactValue::object(ptr);
    assert_eq!(cv.as_object_ptr(), Some(ptr));
}

/// Pointer with bit 47+ set (out of 47-bit payload range) must panic via
/// the unconditional `assert!` rather than silently truncate the high
/// bits and produce a corrupted reference.  Regression test for the
/// "47-bit pointer truncation" vulnerability.
#[test]
#[should_panic(expected = "exceeds 47-bit address space")]
fn object_pointer_above_47bit_panics() {
    let ptr: u64 = 0x0001_0000_0000_0000; // bit 48 set — out of range
    let _ = CompactValue::object(ptr);
}

#[test]
#[should_panic(expected = "not a plausible heap object pointer")]
fn object_pointer_unaligned_panics() {
    let _ = CompactValue::object(0x1001);
}

#[test]
#[should_panic(expected = "not a plausible heap object pointer")]
fn object_pointer_null_page_panics() {
    let _ = CompactValue::object(0x8);
}

/// The checked constructor returns `None` instead of panicking for
/// out-of-range pointers.
#[test]
fn try_from_pointer_rejects_above_47bit() {
    let ptr: u64 = 0x0001_0000_0000_0000;
    assert!(CompactValue::try_from_pointer(ptr).is_none());
}

#[test]
fn try_from_pointer_rejects_null() {
    assert!(CompactValue::try_from_pointer(0).is_none());
}

#[test]
fn try_from_pointer_rejects_unaligned() {
    assert!(CompactValue::try_from_pointer(0x1001).is_none());
}

#[test]
fn try_from_pointer_rejects_null_page() {
    assert!(CompactValue::try_from_pointer(0x8).is_none());
    assert!(CompactValue::try_from_pointer(0xff8).is_none());
}

#[test]
fn try_from_pointer_accepts_valid_pointer() {
    let ptr: u64 = 0x0000_1234_5678_ABC0;
    let cv = CompactValue::try_from_pointer(ptr).expect("valid 47-bit pointer");
    assert_eq!(cv.as_object_ptr(), Some(ptr));
}

#[test]
fn try_from_pointer_accepts_max_47bit() {
    let ptr: u64 = 0x0000_7FFF_FFFF_FFF8;
    let cv = CompactValue::try_from_pointer(ptr).expect("max 47-bit pointer");
    assert_eq!(cv.as_object_ptr(), Some(ptr));
}

// -- Null ----------------------------------------------------------------

#[test]
fn null_roundtrip() {
    let cv = CompactValue::null();
    assert_eq!(cv.tag(), CompactTag::Null);
    assert!(cv.is_null());
    assert!(!cv.is_uninitialized());
    assert_eq!(cv.as_int(), None);
    assert_eq!(cv.as_object_ptr(), None);
}

// -- Uninitialized -------------------------------------------------------

#[test]
fn uninitialized_roundtrip() {
    let cv = CompactValue::uninitialized();
    assert_eq!(cv.tag(), CompactTag::Uninitialized);
    assert!(cv.is_uninitialized());
    assert!(!cv.is_null());
}

// -- ReturnAddress -------------------------------------------------------

#[test]
fn return_address_roundtrip() {
    let cv = CompactValue::return_address(999);
    assert_eq!(cv.tag(), CompactTag::ReturnAddress);
    assert_eq!(cv.as_return_address(), Some(999));
}

#[test]
fn return_address_zero() {
    let cv = CompactValue::return_address(0);
    assert_eq!(cv.as_return_address(), Some(0));
}

#[test]
fn return_address_max() {
    let cv = CompactValue::return_address(u32::MAX);
    assert_eq!(cv.as_return_address(), Some(u32::MAX));
}

// -- Tag extraction correctness ------------------------------------------

#[test]
fn tag_extraction_all_types() {
    assert_eq!(CompactValue::int(0).tag(), CompactTag::Int);
    assert_eq!(CompactValue::float(0.0).tag(), CompactTag::Float);
    assert_eq!(CompactValue::double(0.0).tag(), CompactTag::Double);
    assert_eq!(CompactValue::object(0x1000).tag(), CompactTag::Object);
    assert_eq!(CompactValue::null().tag(), CompactTag::Null);
    assert_eq!(
        CompactValue::uninitialized().tag(),
        CompactTag::Uninitialized
    );
    assert_eq!(
        CompactValue::return_address(0).tag(),
        CompactTag::ReturnAddress
    );
    // Long is untagged — tag() returns Double for untagged values.
    // The caller must know from JVM context that it is a Long.
    // We verify as_long_unchecked works correctly instead.
    let cv = CompactValue::long(42);
    assert_eq!(cv.as_long_unchecked(), 42);
}

// -- Cross-type rejection ------------------------------------------------

#[test]
fn int_not_extractable_as_float() {
    let cv = CompactValue::int(42);
    assert_eq!(cv.as_float(), None);
    assert_eq!(cv.as_double(), None);
    assert_eq!(cv.as_object_ptr(), None);
    assert!(!cv.is_null());
}

#[test]
fn double_not_extractable_as_int() {
    let cv = CompactValue::double(1.0);
    assert_eq!(cv.as_int(), None);
    assert_eq!(cv.as_float(), None);
    assert_eq!(cv.as_object_ptr(), None);
}

// -- From<&Value> conversion ---------------------------------------------

#[test]
fn from_value_int() {
    let v = Value::Int(42);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_int(), Some(42));
}

#[test]
fn from_value_long() {
    let v = Value::Long(i64::MAX);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_long_unchecked(), i64::MAX);
}

#[test]
fn from_value_float() {
    let v = Value::Float(1.5);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_float(), Some(1.5f32));
}

#[test]
fn from_value_double() {
    let v = Value::Double(2.5);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_double(), Some(2.5));
}

#[test]
fn from_value_null() {
    let v = Value::Object(None);
    let cv = CompactValue::from(&v);
    assert!(cv.is_null());
}

#[test]
fn from_value_uninit() {
    let v = Value::Uninitialized;
    let cv = CompactValue::from(&v);
    assert!(cv.is_uninitialized());
}

#[test]
fn from_value_retaddr() {
    let v = Value::ReturnAddress(123);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_return_address(), Some(123));
}

#[test]
fn from_value_object_ref() {
    let fake_ptr = 0x1234_5678_ABC0_u64 as *mut u8;
    let obj = unsafe { ObjectRef::from_raw(fake_ptr) };
    let v = Value::Object(Some(obj));
    let cv = CompactValue::from(&v);
    assert_eq!(cv.as_object_ptr(), Some(0x1234_5678_ABC0));
}

// -- to_value round-trip -------------------------------------------------

#[test]
fn to_value_int_roundtrip() {
    let v = Value::Int(-42);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.to_value(), v);
}

#[test]
fn to_value_double_roundtrip() {
    let v = Value::Double(std::f64::consts::PI);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.to_value().as_double(), Some(std::f64::consts::PI));
}

#[test]
fn to_value_as_long_roundtrip() {
    let v = Value::Long(i64::MIN);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.to_value_as_long(), v);
}

#[test]
fn to_value_null_roundtrip() {
    let v = Value::Object(None);
    let cv = CompactValue::from(&v);
    assert!(cv.to_value().is_null());
}

#[test]
fn to_value_uninit_roundtrip() {
    let v = Value::Uninitialized;
    let cv = CompactValue::from(&v);
    assert_eq!(cv.to_value(), v);
}

#[test]
fn to_value_retaddr_roundtrip() {
    let v = Value::ReturnAddress(456);
    let cv = CompactValue::from(&v);
    assert_eq!(cv.to_value(), v);
}

// -- Equality ------------------------------------------------------------

#[test]
fn equality_same_values() {
    assert_eq!(CompactValue::int(10), CompactValue::int(10));
    assert_eq!(CompactValue::null(), CompactValue::null());
    assert_eq!(CompactValue::uninitialized(), CompactValue::uninitialized());
    assert_eq!(CompactValue::long(77), CompactValue::long(77));
    assert_eq!(CompactValue::double(1.0), CompactValue::double(1.0));
}

#[test]
fn inequality_different_values() {
    assert_ne!(CompactValue::int(1), CompactValue::int(2));
    assert_ne!(CompactValue::null(), CompactValue::uninitialized());
    assert_ne!(CompactValue::int(0), CompactValue::float(0.0));
}

// -- T10.6 API additions ------------------------------------------------

#[test]
fn from_bits_to_bits_roundtrip() {
    let cv = CompactValue::int(42);
    let bits = cv.to_bits();
    let round = CompactValue::from_bits(bits);
    assert_eq!(round.as_int(), Some(42));
}

#[test]
fn from_value_ctor_matches_from_ref() {
    let v = Value::Int(99);
    let a = CompactValue::from_value(v);
    let b = CompactValue::from(&v);
    assert_eq!(a, b);
}

#[test]
fn from_value_owned() {
    let v = Value::Double(3.25);
    let a: CompactValue = v.into();
    assert_eq!(a.as_double(), Some(3.25));
}

#[test]
fn zero_is_all_bits_zero() {
    let z = CompactValue::zero();
    assert_eq!(z.to_bits(), 0);
    // 0 is untagged → decoded as Double(0.0)
    assert_eq!(z.tag(), CompactTag::Double);
}

#[test]
fn is_object_discriminates() {
    assert!(CompactValue::object(0x1000).is_object());
    assert!(!CompactValue::null().is_object());
    assert!(!CompactValue::int(0).is_object());
    assert!(!CompactValue::double(1.0).is_object());
}

#[test]
fn update_object_ptr_rewrites_pointer() {
    let mut cv = CompactValue::object(0x1000);
    assert!(cv.update_object_ptr(0x2000).is_ok());
    assert_eq!(cv.as_object_ptr(), Some(0x2000));
}

#[test]
fn update_object_ptr_noop_for_non_object() {
    let mut cv = CompactValue::int(42);
    // Non-object slot: even an out-of-range pointer is silently ignored —
    // there is no reference to corrupt.
    assert!(cv.update_object_ptr(0x9999).is_ok());
    assert_eq!(cv.as_int(), Some(42));
}

// -- MED audit 2026-05-24: hardened `update_object_ptr` -----------------

/// `update_object_ptr` with a pointer that has bits set above bit 46
/// must reject with `PointerOutOfRange` rather than silently masking the
/// high bits — symmetric to the constructor `object()` which panics on
/// the same condition. The slot is left unchanged so the caller can
/// recover or escalate.
#[test]
fn update_object_ptr_rejects_above_47bit() {
    let original = 0x0000_1234_5678_ABC0;
    let mut cv = CompactValue::object(original);
    let bad: u64 = 0x0001_0000_0000_0000; // bit 48 set — out of range
    match cv.update_object_ptr(bad) {
        Err(CompactValueError::PointerOutOfRange { ptr }) => assert_eq!(ptr, bad),
        other => panic!("expected PointerOutOfRange, got {other:?}"),
    }
    // Slot untouched on failure.
    assert_eq!(cv.as_object_ptr(), Some(original));
}

#[test]
fn update_object_ptr_rejects_implausible_pointer() {
    let original = 0x0000_1234_5678_ABC0;
    for bad in [0x1001u64, 0x8, 0xff8] {
        let mut cv = CompactValue::object(original);
        match cv.update_object_ptr(bad) {
            Err(CompactValueError::InvalidObjectPointer { ptr }) => assert_eq!(ptr, bad),
            other => panic!("expected InvalidObjectPointer, got {other:?}"),
        }
        assert_eq!(cv.as_object_ptr(), Some(original));
    }
}

/// `update_object_ptr` round-trips an in-range pointer — the canonical
/// GC compaction path.
#[test]
fn update_object_ptr_round_trips_in_range() {
    let mut cv = CompactValue::object(0x0000_1000);
    // Maximum-magnitude in-range pointer.
    let new_ptr: u64 = 0x0000_7FFF_FFFF_FFF8;
    cv.update_object_ptr(new_ptr)
        .expect("47-bit pointer must succeed");
    assert_eq!(cv.as_object_ptr(), Some(new_ptr));
    assert!(cv.is_object());
    assert_eq!(cv.tag(), CompactTag::Object);
}

/// The `_unchecked` variant masks-and-stores in release builds; on a
/// happy-path in-range pointer it behaves identically to the checked
/// API. Used by the GC scanner where the address space is bounded by
/// construction.
#[test]
fn update_object_ptr_unchecked_round_trips_in_range() {
    let mut cv = CompactValue::object(0x0000_1000);
    cv.update_object_ptr_unchecked(0x0000_2000);
    assert_eq!(cv.as_object_ptr(), Some(0x0000_2000));
}

/// `as_long_unchecked` returns the original i64 bit-exactly for every
/// i64 value — including every NaN-box collision sub-tag, since
/// `CompactValue::long` now stores verbatim.
#[test]
fn as_long_unchecked_returns_original_i64() {
    // Untagged fast path — original equals stored bits.
    for v in [0i64, 1, 2, 42, -42, 100, i64::MIN, i64::MAX] {
        let cv = CompactValue::long(v);
        assert_eq!(cv.as_long_unchecked(), v, "untagged long {v}");
        assert_eq!(cv.as_long_bits_unchecked(), v);
    }
    // Natural `SUB_LONG_LO`/`SUB_LONG_HI` collisions (small-magnitude
    // negatives whose top sub-tag bits are already 110 or 111).
    for v in [-1i64, -2, -3, -1000, -123_456_789_012_345] {
        let cv = CompactValue::long(v);
        assert_eq!(cv.as_long_unchecked(), v, "natural-collision long {v}");
        assert_eq!(cv.as_long_bits_unchecked(), v);
    }
    // Every collision sub-tag (0..=7) round-trips bit-exact — including
    // SUB_OBJECT (2), the BC SM2 regression case that motivated the
    // lossless verbatim encoding.
    for sub in 0u64..8 {
        let v = collide_with_subtag(sub, 0x12_3456);
        let cv = CompactValue::long(v);
        assert_eq!(cv.as_long_unchecked(), v, "collision sub={sub}");
        assert_eq!(cv.as_long_bits_unchecked(), v);
    }
}

/// Round-trip sanity for the new `as_long_bits_unchecked` name — same
/// value as `as_long_unchecked` for every long pattern. Guards against
/// accidental divergence if either method's body is edited.
#[test]
fn as_long_bits_unchecked_matches_as_long_unchecked() {
    for v in [
        0i64,
        1,
        -1,
        42,
        -42,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        i64::MAX - 1,
        -123_456_789_012_345,
    ] {
        let cv = CompactValue::long(v);
        assert_eq!(cv.as_long_unchecked(), cv.as_long_bits_unchecked());
    }
    // Also for the rare re-tag bit patterns.
    for sub in 0u64..8 {
        let v = collide_with_subtag(sub, 0x55_5555);
        let cv = CompactValue::long(v);
        assert_eq!(cv.as_long_unchecked(), cv.as_long_bits_unchecked());
    }
}

#[test]
fn repr_transparent_layout_matches_u64() {
    // Critical for the Vec<CompactValue> ↔ Vec<u64> transmute used by
    // the operand-stack pool integration.
    assert_eq!(
        std::mem::size_of::<CompactValue>(),
        std::mem::size_of::<u64>()
    );
    assert_eq!(
        std::mem::align_of::<CompactValue>(),
        std::mem::align_of::<u64>()
    );
}

#[test]
fn to_value_null_ptr_subobject_decodes_as_long() {
    let _guard = super::degrade_counter_test_lock();
    // BC SM2 fix (2026-05-28): `CompactValue::long` stores long bits
    // verbatim, so a slot with SUB_OBJECT bits and a null payload
    // could equally be a primitive long whose pattern landed here.
    // Real object slots are never null (`CompactValue::object` panics
    // on null) and never SUB_OBJECT-with-payload-zero, so `to_value`
    // returns `Value::Long` for safety — preserving the long bits
    // bit-exact rather than discarding them as `Object(None)`.
    let raw = make_tagged(SUB_OBJECT, 0);
    let cv = CompactValue::from_bits(raw);
    match cv.to_value() {
        Value::Long(x) => assert_eq!(x as u64, raw),
        other => panic!("expected Long, got {other:?}"),
    }
}

// ── T10.9.D regression tests for direct CompactValue hot path ────────

#[test]
fn t10_9_d_iadd_isub_imul_round_trip() {
    // Simulate a small loop of int arithmetic through CompactValue
    // push/pop to assert that the direct hot-path math is stable.
    let mut x: i32 = 0;
    for _ in 0..10 {
        // x = x + 1
        let a = CompactValue::int(x);
        let b = CompactValue::int(1);
        x = a.as_int().unwrap().wrapping_add(b.as_int().unwrap());
        // x = x * 2
        let a = CompactValue::int(x);
        let b = CompactValue::int(2);
        x = a.as_int().unwrap().wrapping_mul(b.as_int().unwrap());
        // x = x - 3
        let a = CompactValue::int(x);
        let b = CompactValue::int(3);
        x = a.as_int().unwrap().wrapping_sub(b.as_int().unwrap());
    }
    // Closed form: after each iter, x = 2*(x+1) - 3 = 2x - 1.
    // Starting at 0: -1, -3, -7, -15, -31, -63, -127, -255, -511, -1023.
    assert_eq!(x, -1023);
}

#[test]
fn t10_9_d_lload_lstore_preserves_sign() {
    // Round-trip a signed long through the CompactValue helpers; this
    // guards against a mis-shifted payload decode on the hot path.
    //
    // Note: large-magnitude negative longs can have an upper bit
    // pattern that collides with the NaN-boxed tag; CompactValue does
    // not canonicalise long bits (only doubles), so the JVM context
    // that guarantees "this slot is a long" allows as_long_unchecked
    // to reproduce the exact i64 value without regard to tag().
    for original in [
        -1i64,
        -2i64,
        -1000i64,
        -123_456_789_012_345i64,
        i64::MIN,
        i64::MIN + 1,
        -123_456i64,
    ] {
        let cv = CompactValue::long(original);
        assert_eq!(
            cv.as_long_unchecked(),
            original,
            "round-trip failed for {original}"
        );
        // Through from_bits, the value survives too.
        let round = CompactValue::from_bits(cv.to_bits());
        assert_eq!(round.as_long_unchecked(), original);
    }
    // A small positive long should be category-2 and untagged:
    let cv_pos = CompactValue::long(42);
    assert!(cv_pos.is_category2());
    assert_eq!(cv_pos.tag(), CompactTag::Double); // untagged → Double
}

#[test]
fn t10_9_d_fmul_fdiv_roundtrip() {
    // Float arithmetic through CompactValue: simulate a sequence
    // of fmul/fdiv on the hot path.
    let a = CompactValue::float(6.5f32);
    let b = CompactValue::float(2.0f32);
    // fmul: 6.5 * 2.0 = 13.0
    let prod = a.as_float().unwrap() * b.as_float().unwrap();
    let cv_prod = CompactValue::float(prod);
    assert!((cv_prod.as_float().unwrap() - 13.0f32).abs() < 1e-6);
    // fdiv: 13.0 / 4.0 = 3.25
    let c = CompactValue::float(4.0f32);
    let quot = cv_prod.as_float().unwrap() / c.as_float().unwrap();
    let cv_quot = CompactValue::float(quot);
    assert!((cv_quot.as_float().unwrap() - 3.25f32).abs() < 1e-6);
}

#[test]
fn t10_9_d_dup2_preserves_long_category() {
    // Simulate the dup2 operation on a Long (a single CompactValue in
    // the compact slot model).  The slot must remain category-2 and
    // the long value round-trip across bit-wise copies.
    let cv = CompactValue::long(0x0BAD_BEEF_DEAD_CAFEu64 as i64);
    assert!(cv.is_category2());
    // "Dup" the slot bit-for-bit and confirm both copies decode.
    let copy1 = cv;
    let copy2 = cv;
    assert_eq!(copy1.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFEu64 as i64);
    assert_eq!(copy2.as_long_unchecked(), 0x0BAD_BEEF_DEAD_CAFEu64 as i64);
    assert!(copy1.is_category2());
    assert!(copy2.is_category2());
}

#[test]
fn t10_9_d_getfield_int_primitive() {
    // Simulate loading an int primitive from the heap (Value::Int)
    // and pushing it as a CompactValue on the stack.
    let heap_val = Value::Int(42);
    let cv = CompactValue::from_value(heap_val);
    assert_eq!(cv.as_int(), Some(42));
    assert_eq!(cv.tag(), CompactTag::Int);
    // Round-trip back to a Value for putfield-style paths.
    match cv.to_value() {
        Value::Int(v) => assert_eq!(v, 42),
        other => panic!("expected Int, got {other:?}"),
    }
}

#[test]
fn t10_9_d_fibonacci_smoke() {
    // Iterative fibonacci using CompactValue for every operand; fib(10)
    // = 55.  Stresses iadd/istore/iload through the compact helpers.
    let mut a = CompactValue::int(0);
    let mut b = CompactValue::int(1);
    for _ in 0..10 {
        let sum = a.as_int().unwrap().wrapping_add(b.as_int().unwrap());
        a = b;
        b = CompactValue::int(sum);
    }
    assert_eq!(a.as_int(), Some(55));
}

#[test]
fn is_category2_long_and_double() {
    // Both Long and Double are JVM category-2 types: is_category2
    // must return true for all "normal" cat-2 bit patterns.
    assert!(CompactValue::long(0).is_category2());
    assert!(CompactValue::long(1).is_category2());
    assert!(CompactValue::long(42).is_category2());
    assert!(CompactValue::long(i64::MAX).is_category2());
    // Negative longs that happen to have bits 49-47 = 111 also
    // decode as cat-2 via the SUB_LONG_HI branch.
    assert!(CompactValue::long(-1).is_category2());
    assert!(CompactValue::long(-2).is_category2());
    assert!(CompactValue::long(i64::MIN).is_category2());
    assert!(CompactValue::double(0.0).is_category2());
    assert!(CompactValue::double(std::f64::consts::PI).is_category2());

    // Non-cat2 types must all return false.
    assert!(!CompactValue::int(0).is_category2());
    assert!(!CompactValue::int(-1).is_category2());
    assert!(!CompactValue::float(1.5).is_category2());
    assert!(!CompactValue::null().is_category2());
    assert!(!CompactValue::uninitialized().is_category2());
    assert!(!CompactValue::object(0x1000).is_category2());
    assert!(!CompactValue::return_address(42).is_category2());
}

#[test]
fn to_value_unaligned_ptr_subobject_decodes_as_long() {
    let _guard = super::degrade_counter_test_lock();
    // BC SM2 fix (2026-05-28): a NaN-tagged slot with SUB_OBJECT bits
    // but an unaligned payload cannot be a real object reference
    // (`CompactValue::object` panics on unaligned pointers), so it
    // must be a long-bit-pattern collision. `to_value()` preserves the
    // long bits — replacing the prior `Object(None)` degrade that
    // would silently drop the value.
    let raw_unaligned = make_tagged(SUB_OBJECT, 0x1001);
    let cv = CompactValue::from_bits(raw_unaligned);
    match cv.to_value() {
        Value::Long(x) => assert_eq!(x as u64, raw_unaligned),
        other => panic!("expected Long, got {other:?}"),
    }
}

// -- Debug ---------------------------------------------------------------

#[test]
fn debug_format_smoke() {
    // Just ensure Debug doesn't panic for each variant.
    let _ = format!("{:?}", CompactValue::int(42));
    let _ = format!("{:?}", CompactValue::long(99));
    let _ = format!("{:?}", CompactValue::float(1.0));
    let _ = format!("{:?}", CompactValue::double(2.0));
    let _ = format!("{:?}", CompactValue::object(0x1000));
    let _ = format!("{:?}", CompactValue::null());
    let _ = format!("{:?}", CompactValue::uninitialized());
    let _ = format!("{:?}", CompactValue::return_address(0));
}

// ── T10.9.E decode_by_descriptor tests ───────────────────────────────

#[test]
fn decode_by_descriptor_j_roundtrips_small_long() {
    // This is the exact failure mode reported in Session 93: a small
    // long value (5) stored via CompactValue::long(5) is untagged,
    // and to_value() decodes it as Value::Double(2.47e-323). The
    // descriptor-aware decoder must yield Value::Long(5).
    let cv = CompactValue::long(5);
    // Control: raw to_value() still returns Double for untagged.
    match cv.to_value() {
        Value::Double(_) => {}
        other => {
            panic!("expected untagged long to decode as Double via to_value; got {other:?}")
        }
    }
    // Descriptor-aware decode picks the correct type.
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(5));
}

#[test]
fn decode_by_descriptor_j_handles_large_magnitude() {
    // Exercise values where the bit pattern has high bits collide with
    // the NaN-tag space; those use the SUB_LONG_LO/SUB_LONG_HI branch
    // inside decode_by_descriptor.
    for original in [
        0i64,
        1i64,
        42i64,
        -1i64,
        i64::MIN,
        i64::MAX,
        i64::MIN + 1,
        -123_456i64,
        123_456_789_012_345i64,
    ] {
        let cv = CompactValue::long(original);
        assert_eq!(
            cv.decode_by_descriptor(b'J'),
            Value::Long(original),
            "round-trip failed for {original}"
        );
    }
}

#[test]
fn decode_by_descriptor_d_preserves_doubles() {
    // Doubles must decode as Value::Double, never as Long — symmetric
    // to the long path.
    for val in [
        0.0f64,
        1.0,
        -1.0,
        std::f64::consts::PI,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let cv = CompactValue::double(val);
        match cv.decode_by_descriptor(b'D') {
            Value::Double(d) => assert_eq!(d, val),
            other => panic!("expected Double({val}), got {other:?}"),
        }
    }
}

#[test]
fn decode_by_descriptor_d_nan_is_canonical() {
    // A NaN double's bit pattern may collide with the tagged space —
    // CompactValue canonicalises it. decode_by_descriptor must still
    // return a Value::Double (NaN).
    let cv = CompactValue::double(f64::NAN);
    match cv.decode_by_descriptor(b'D') {
        Value::Double(d) => assert!(d.is_nan()),
        other => panic!("expected Double(NaN), got {other:?}"),
    }
}

#[test]
fn decode_by_descriptor_f_on_float_roundtrips() {
    let cv = CompactValue::float(std::f32::consts::E);
    match cv.decode_by_descriptor(b'F') {
        Value::Float(f) => assert_eq!(f.to_bits(), std::f32::consts::E.to_bits()),
        other => panic!("expected Float(E), got {other:?}"),
    }
}

/// An Int landing in an `F` slot must widen via JVMS i2f *numeric*
/// conversion, not a raw bit reinterpret.  Regression test for the
/// `f32::from_bits` bug: `Value::Int(1)` decoded as `b'F'` previously
/// produced the denormal `1.4e-45` (bit pattern 0x1) instead of `1.0`.
/// The sibling `b'D'` / `b'J'` Int branches already convert numerically.
#[test]
fn decode_by_descriptor_f_int_widens_numerically() {
    for v in [0i32, 1, -1, 42, -42, 100, i16::MAX as i32, -12345] {
        let cv = CompactValue::int(v);
        match cv.decode_by_descriptor(b'F') {
            Value::Float(f) => {
                assert_eq!(f, v as f32, "i2f numeric conversion expected for Int({v})",)
            }
            other => panic!("expected Float({}), got {other:?}", v as f32),
        }
    }
    // Mirror the sibling descriptors to confirm consistent semantics.
    assert_eq!(
        CompactValue::int(7).decode_by_descriptor(b'D'),
        Value::Double(7.0)
    );
    assert_eq!(
        CompactValue::int(7).decode_by_descriptor(b'F'),
        Value::Float(7.0)
    );
}

#[test]
fn decode_by_descriptor_i_int_roundtrips() {
    for v in [0, 1, -1, i32::MAX, i32::MIN, 42] {
        let cv = CompactValue::int(v);
        assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(v));
    }
}

#[test]
fn decode_by_descriptor_byte_short_char_boolean_use_int() {
    // JVMS: byte/short/char/boolean are represented as Int on the stack.
    let cv = CompactValue::int(127);
    assert_eq!(cv.decode_by_descriptor(b'B'), Value::Int(127));
    assert_eq!(cv.decode_by_descriptor(b'S'), Value::Int(127));
    assert_eq!(cv.decode_by_descriptor(b'C'), Value::Int(127));
    assert_eq!(cv.decode_by_descriptor(b'Z'), Value::Int(127));
}

#[test]
fn decode_by_descriptor_reference_keeps_object() {
    let ptr: u64 = 0x0000_1234_5678_ABC0;
    let _obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
    let cv = CompactValue::object(ptr);
    match cv.decode_by_descriptor(b'L') {
        Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
        other => panic!("expected Object(Some), got {other:?}"),
    }
    match cv.decode_by_descriptor(b'[') {
        Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
        other => panic!("expected Object(Some), got {other:?}"),
    }
}

#[test]
fn decode_by_descriptor_null_is_zero_for_primitive() {
    // A null slot in a primitive field decodes to the typed JVMS zero.
    let cv = CompactValue::null();
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(0));
    assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(0.0));
    assert_eq!(cv.decode_by_descriptor(b'F'), Value::Float(0.0));
    assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(0));
}

#[test]
fn decode_by_descriptor_uninit_is_zero_for_primitive() {
    let cv = CompactValue::uninitialized();
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(0));
    assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(0.0));
    assert_eq!(cv.decode_by_descriptor(b'F'), Value::Float(0.0));
    assert_eq!(cv.decode_by_descriptor(b'I'), Value::Int(0));
}

#[test]
fn decode_by_descriptor_int_widens_to_long() {
    // Upstream bytecode that forgot an i2l conversion landed an Int
    // on a J slot — descriptor-aware decode widens rather than
    // panics.
    let cv = CompactValue::int(-42);
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(-42));
}

#[test]
fn decode_by_descriptor_unknown_falls_through() {
    // An unrecognized descriptor byte preserves legacy `to_value`
    // behavior so the defensive fallback cannot regress callers
    // that accidentally pass something like 'V' (void).
    let cv = CompactValue::int(99);
    assert_eq!(cv.decode_by_descriptor(b'V'), cv.to_value());
}

#[test]
fn decode_by_descriptor_session93_reproduction() {
    // Exact bit pattern that broke ConcurrentHashMap.SIZECTL on the
    // KC16/KC26 boot path. Without descriptor awareness this slot
    // decodes as Value::Double(2.47e-323) — which `unsafe_offset`
    // previously fell through to 0 on, livelocking the CAS loop.
    let cv = CompactValue::long(5);
    // Not a tagged slot.
    assert!(!is_nan_tagged(cv.raw_bits()));
    // Raw to_value is Double — legacy behavior preserved.
    match cv.to_value() {
        Value::Double(_) => {}
        other => panic!("expected Double, got {other:?}"),
    }
    // Descriptor-aware decode returns Long(5) — the real fix.
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(5));
}

#[test]
fn decode_by_descriptor_d_with_int_widens() {
    let cv = CompactValue::int(7);
    assert_eq!(cv.decode_by_descriptor(b'D'), Value::Double(7.0));
}

#[test]
fn decode_by_descriptor_l_rejects_primitive() {
    // If a primitive is somehow stored in a reference slot, the
    // verifier would catch it pre-runtime — but defensively we
    // yield null so native code doesn't see a bogus pointer.
    let cv = CompactValue::int(99);
    assert!(matches!(cv.decode_by_descriptor(b'L'), Value::Object(None)));
}

// ── HIGH: NaN-box long↔object type-confusion — checked decoders ──────

// The lock that serialises every degrading test lives at module scope
// (`super::degrade_counter_test_lock`) so `crate::value`'s degrading tests
// can take the same one — see its doc comment.

#[test]
fn decode_by_descriptor_l_rejects_unseen_subobject_payload() {
    let _guard = super::degrade_counter_test_lock();
    let raw = make_tagged(SUB_OBJECT, 0x0000_6CCC_DDDD_E000);
    let cv = CompactValue::from_bits(raw);
    assert!(matches!(cv.decode_by_descriptor(b'L'), Value::Object(None)));
}

/// A primitive long whose bits land in the `SUB_OBJECT` space with an
/// aligned, non-null payload must not be decoded as a fabricated object by
/// the context-free `to_value`. `to_value_checked` with a heap predicate
/// that rejects the address also degrades it back to the bit-exact long
/// and counts the degradation.
/// Every SUB_OBJECT *encoder* must leave the payload decodable as a
/// reference by the context-free decoder.
///
/// The provenance bitmap (`crate::value::object_ref_payload_is_known`) is
/// what `to_value` consults to tell a real reference from a primitive long
/// whose bits collide with the SUB_OBJECT NaN-box pattern. Before these
/// four encoders recorded, they could mint a slot their own decoder
/// refused: `to_value` degraded the live reference to `Value::Long`, the
/// interpreter then marked the local `LKIND_LONG`, and
/// `Frame::scan_local_objects` skips LONG slots -- so the young sweep
/// reclaimed an object a running frame still held. That is the
/// `ClassId(0)` / `"result" is null` family in
/// `bug-h2-classid0-stale-address-family-FIXED.md`; two
/// sites (`Frame::update_object_refs`, `ValueStack::update_object_refs`)
/// had already been hand-patched with a `ObjectRef::from_raw` round-trip
/// for exactly this reason, which is the symptom of a missing invariant
/// one level down.
///
/// Addresses here are deliberately outside any test heap so the assertion
/// is about the ENCODER recording, not about some other test having
/// happened to touch the same 64-byte granule.
#[test]
fn every_sub_object_encoder_records_decodable_provenance_cv() {
    let _guard = degrade_counter_test_lock();
    let before = object_degradation_count();

    // 1. `object(raw)` -- the raw-pointer constructor.
    let a1 = 0x0000_5A5A_0001_0000u64;
    assert!(
        matches!(CompactValue::object(a1).to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a1),
        "object() minted a slot to_value() refuses",
    );

    // 2. `try_from_pointer(raw)` -- the fallible constructor.
    let a2 = 0x0000_5A5A_0002_0000u64;
    let cv2 = CompactValue::try_from_pointer(a2).expect("plausible payload");
    assert!(
        matches!(cv2.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a2),
        "try_from_pointer() minted a slot to_value() refuses",
    );

    // 3. `update_object_ptr` -- the checked GC relocation writer.
    let a3 = 0x0000_5A5A_0003_0000u64;
    let mut cv3 = CompactValue::object(a1);
    cv3.update_object_ptr(a3).expect("plausible payload");
    assert!(
        matches!(cv3.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a3),
        "update_object_ptr() left a relocated reference undecodable",
    );

    // 4. `update_object_ptr_unchecked` -- the GC compaction hot path.
    let a4 = 0x0000_5A5A_0004_0000u64;
    let mut cv4 = CompactValue::object(a1);
    cv4.update_object_ptr_unchecked(a4);
    assert!(
        matches!(cv4.to_value(), Value::Object(Some(o)) if o.as_ptr() as u64 == a4),
        "update_object_ptr_unchecked() left a relocated reference undecodable",
    );

    // None of the eight decodes above may have counted a degradation.
    assert_eq!(
        object_degradation_count() - before,
        0,
        "an encoded reference degraded to Long",
    );
}

#[test]
fn to_value_checked_degrades_fabricated_pointer_to_long() {
    let _guard = super::degrade_counter_test_lock();
    // Aligned (multiple of 8), non-null payload inside a SUB_OBJECT slot.
    let aligned = 0x0000_6BAD_CAFE_D000u64; // % 8 == 0, non-zero
    let raw = make_tagged(SUB_OBJECT, aligned);
    let cv = CompactValue::from_bits(raw);

    // Context-free path rejects the never-seen payload instead of
    // fabricating an object reference. Assert the DELTA rather than
    // resetting: the counter is process-wide, and zeroing it would
    // silently break whatever other test is mid-count.
    let base = object_degradation_count();
    match cv.to_value() {
        Value::Long(x) => assert_eq!(x as u64, raw),
        other => panic!("unchecked to_value should degrade to Long, got {other:?}"),
    }
    assert_eq!(object_degradation_count() - base, 1);

    // Checked path with a heap that says "not a live object" degrades to
    // the bit-exact long and records the reclassification.
    let base = object_degradation_count();
    match cv.to_value_checked(|_addr| false) {
        Value::Long(x) => assert_eq!(x as u64, raw),
        other => panic!("checked to_value should degrade to Long, got {other:?}"),
    }
    assert_eq!(object_degradation_count() - base, 1);
}

/// When the heap predicate confirms the address is live, the checked
/// decoder returns the same object reference as `to_value` and does NOT
/// count a degradation.
#[test]
fn to_value_checked_keeps_live_object() {
    let _guard = super::degrade_counter_test_lock();
    let aligned = 0x1234_5678_ABC0u64;
    let cv = CompactValue::object(aligned);
    let base = object_degradation_count();
    match cv.to_value_checked(|addr| addr == aligned) {
        Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, aligned),
        other => panic!("expected live Object, got {other:?}"),
    }
    assert_eq!(object_degradation_count() - base, 0);
}

/// `to_value_checked` is identical to `to_value` for every non-object
/// tag (and never invokes the heap closure for them).
#[test]
fn to_value_checked_matches_to_value_for_non_objects() {
    let samples = [
        CompactValue::int(-42),
        CompactValue::float(1.5),
        CompactValue::double(std::f64::consts::PI),
        CompactValue::null(),
        CompactValue::uninitialized(),
        CompactValue::return_address(7),
        CompactValue::long(5),  // untagged → Double
        CompactValue::long(-1), // SUB_LONG_HI collision
        CompactValue::long(i64::MIN),
    ];
    for cv in samples {
        // Closure must never be consulted for non-object slots.
        let checked = cv.to_value_checked(|_| panic!("heap closure called for non-object"));
        assert_eq!(checked, cv.to_value(), "mismatch for {cv:?}");
    }
}

/// `is_object_checked` returns true only when the payload is live and
/// counts a degradation when a SUB_OBJECT slot fails heap validation.
#[test]
fn is_object_checked_validates_against_heap() {
    let _guard = super::degrade_counter_test_lock();
    let aligned = 0x4000u64;
    let real = CompactValue::object(aligned);
    assert!(real.is_object()); // unchecked pattern test
    assert!(real.is_object_checked(|addr| addr == aligned));

    let base = object_degradation_count();
    // Same bit pattern, but the heap denies it → false + degradation.
    assert!(!real.is_object_checked(|_| false));
    assert_eq!(object_degradation_count() - base, 1);

    // Non-object slots short-circuit without consulting the heap.
    assert!(!CompactValue::int(1).is_object_checked(|_| panic!("called")));
    assert!(!CompactValue::long(-1).is_object_checked(|_| panic!("called")));
}

/// The unchecked `to_value` SUB_OBJECT degrade (null / unaligned payload)
/// bumps the observability counter so release-mode reclassifications are
/// countable rather than invisible.
#[test]
fn to_value_unchecked_degrade_increments_counter() {
    let _guard = super::degrade_counter_test_lock();
    let base = object_degradation_count();
    // Null payload SUB_OBJECT → Long, counted.
    let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0)).to_value();
    // Unaligned payload SUB_OBJECT → Long, counted.
    let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x1001)).to_value();
    let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x0000_6AAA_BBBB_C000)).to_value();
    assert_eq!(object_degradation_count() - base, 3);
}

// ── HIGH: null-page plausibility guard for the unchecked SUB_OBJECT decode ──

/// `object_payload_is_plausible` accepts only what a genuine
/// `CompactValue::object` could produce — non-null, 8-byte aligned, and at
/// or above the null-guard page — and rejects everything else.
#[test]
fn object_payload_plausibility_contract() {
    // Rejected: null, unaligned, and aligned-but-in-the-null-guard-page.
    assert!(!object_payload_is_plausible(0));
    assert!(!object_payload_is_plausible(4)); // unaligned
    assert!(!object_payload_is_plausible(0x1001)); // unaligned, above guard
    assert!(!object_payload_is_plausible(8)); // aligned but below guard
    assert!(!object_payload_is_plausible(NULL_GUARD_PAGE - 8)); // aligned, just under guard
                                                                // Accepted: the guard-page boundary is inclusive, and anything aligned
                                                                // above it.
    assert!(object_payload_is_plausible(NULL_GUARD_PAGE)); // 0x1000, aligned
    assert!(object_payload_is_plausible(0x1234_5678_ABC0));
    assert!(object_payload_is_plausible(PAYLOAD_MASK & !0b111)); // max aligned 47-bit
}

/// HIGH finding — crafted i64 attack: a primitive `long` whose verbatim
/// bits set `NANBOX_BITS`, the `SUB_OBJECT` sub-tag, and an **aligned,
/// non-null but null-page** payload must NOT fabricate an `ObjectRef`. The
/// hardened unchecked `to_value` now treats it as a long-bit-pattern
/// collision (the new null-page guard) and degrades it bit-exact, counting
/// the reclassification. Without the guard this slot would have decoded as
/// `Value::Object(Some(ObjectRef::from_raw(0x8)))` — a fabricated pointer.
#[test]
fn to_value_unchecked_null_page_subobject_degrades_to_long() {
    let _guard = super::degrade_counter_test_lock();
    for &aligned_low in &[0x8u64, 0x10, 0x100, NULL_GUARD_PAGE - 8] {
        // Aligned + non-null but inside the null-guard page → cannot be a
        // real heap reference.
        assert_eq!(aligned_low % 8, 0, "test payload must be 8-byte aligned");
        assert!(aligned_low != 0 && aligned_low < NULL_GUARD_PAGE);
        let raw = make_tagged(SUB_OBJECT, aligned_low);
        let cv = CompactValue::from_bits(raw);
        // The slot is bit-pattern-classified as an object (pure test)...
        assert!(cv.is_object());
        // ...but the hardened decoder refuses to fabricate a pointer.
        let base = object_degradation_count();
        match cv.to_value() {
            Value::Long(x) => assert_eq!(
                x as u64, raw,
                "null-page SUB_OBJECT long must round-trip bit-exact ({raw:#018x})",
            ),
            other => panic!(
                "null-page SUB_OBJECT payload {aligned_low:#x} must NOT fabricate \
                 an object; got {other:?}",
            ),
        }
        assert_eq!(
            object_degradation_count() - base,
            1,
            "null-page degrade must be counted ({raw:#018x})",
        );
    }
}

/// The crafted-i64 attack expressed as a real `i64` flowing through the
/// public `CompactValue::long` constructor (mirroring an `lxor`/`ladd`
/// result an attacker could `lstore`): a long whose bits form an aligned,
/// null-page SUB_OBJECT pattern is preserved as a long, never fabricated
/// into an object reference.
#[test]
fn crafted_long_with_null_page_object_bits_is_not_an_object() {
    let _guard = super::degrade_counter_test_lock();
    // Construct the exact colliding i64: NANBOX | (SUB_OBJECT<<47) | 0x8.
    let crafted = (NANBOX_BITS | (SUB_OBJECT << SUBTAG_SHIFT) | 0x8) as i64;
    let cv = CompactValue::long(crafted);
    // Bit-exact storage (verbatim long encoding).
    assert_eq!(cv.as_long_unchecked(), crafted);
    // The unchecked decode degrades to the bit-exact long, not an object.
    match cv.to_value() {
        Value::Long(x) => assert_eq!(x, crafted),
        other => panic!("crafted long must not decode as object; got {other:?}"),
    }
    // The descriptor-aware path (the type-safe route) also yields a long.
    assert_eq!(cv.decode_by_descriptor(b'J'), Value::Long(crafted));
    // And the heap-checked path degrades it without ever calling the heap
    // closure for an implausible payload.
    match cv.to_value_checked(|_| panic!("heap closure called for null-page payload")) {
        Value::Long(x) => assert_eq!(x, crafted),
        other => panic!("checked decode of crafted long must be Long; got {other:?}"),
    }
}

/// Regression guard: the hardening must not reject object pointers that
/// already crossed the unsafe `ObjectRef` construction boundary.
#[test]
fn to_value_unchecked_keeps_known_object() {
    for &ptr in &[NULL_GUARD_PAGE, 0x4000u64, 0x1234_5678_ABC0u64] {
        let _obj = unsafe { ObjectRef::from_raw(ptr as *mut u8) };
        let cv = CompactValue::object(ptr);
        match cv.to_value() {
            Value::Object(Some(o)) => assert_eq!(o.as_ptr() as u64, ptr),
            other => panic!("known object {ptr:#x} must decode as Object; got {other:?}"),
        }
    }
}

// ── The shared degradation sink: null exclusion and per-source breakdown ──

/// **The headline contract.** An ordinary Java null must never reach the
/// counter from a *raw-word* source, because
/// `plausible_heap_pointer(0) == false` puts it in the same cold arm as
/// genuine garbage — counting it would read in the millions on a clean run.
///
/// The asymmetry with the tagged sites below is deliberate and is the whole
/// reason there are two entry points; assert both halves in one test so
/// nobody "fixes" one of them into agreement with the other.
#[test]
fn null_ref_word_is_not_a_degradation_but_a_null_tagged_payload_is() {
    let _guard = super::degrade_counter_test_lock();

    // Raw-word source: null is ignored, non-null garbage is counted.
    let base_total = object_degradation_count();
    let base_jit = object_degradation_count_from(DegradationSource::Jit);
    assert!(
        !note_ref_word_degradation(0, DegradationSource::Jit, "test_null"),
        "a null reference word must not be reported as a degradation",
    );
    assert_eq!(
        object_degradation_count_from(DegradationSource::Jit) - base_jit,
        0,
        "note_ref_word_degradation counted an ordinary null",
    );
    assert!(
        note_ref_word_degradation(0x8D8D_8D8D, DegradationSource::Jit, "test_garbage"),
        "non-null implausible word must be reported as a degradation",
    );
    assert_eq!(
        object_degradation_count_from(DegradationSource::Jit) - base_jit,
        1,
    );
    assert_eq!(object_degradation_count() - base_total, 1);

    // Tagged source: a zero payload under SUB_OBJECT is an *unconstructible*
    // encoding (`object()` asserts non-null, `try_from_pointer` refuses it,
    // and a real null is SUB_NULL), so it is a collision and MUST count.
    let base_interp = object_degradation_count_from(DegradationSource::Interpreter);
    let _ = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0)).to_value();
    assert_eq!(
        object_degradation_count_from(DegradationSource::Interpreter) - base_interp,
        1,
        "SUB_OBJECT with a zero payload is a collision, not a Java null",
    );
}

/// The six in-file call sites must not fire for an *ordinary* null, which
/// in this encoding is `SUB_NULL` and never reaches a `SUB_OBJECT` arm.
/// This is what keeps the interpreter's counter honest without needing the
/// `raw == 0` filter the raw-word sources require.
#[test]
fn interpreter_sites_do_not_count_an_ordinary_null() {
    let _guard = super::degrade_counter_test_lock();
    let base = object_degradation_count();

    let null_slot = CompactValue::null();
    assert!(matches!(null_slot.to_value(), Value::Object(None)));
    assert!(matches!(
        null_slot.decode_by_descriptor(b'L'),
        Value::Object(None)
    ));
    assert!(matches!(
        null_slot.decode_by_descriptor(b'['),
        Value::Object(None)
    ));
    // The heap closure must never be consulted for a null slot either.
    assert!(matches!(
        null_slot.to_value_checked(|_| panic!("heap closure called for null slot")),
        Value::Object(None)
    ));
    assert!(!null_slot.is_object_checked(|_| panic!("heap closure called for null slot")));
    // `from_value(Object(None))` takes the dedicated null arm; `Object(Some)`
    // carries a `NonNull`, so the degrading arm cannot see a null at all.
    assert_eq!(
        CompactValue::from_value(Value::Object(None)).raw_bits(),
        null_slot.raw_bits(),
    );

    assert_eq!(
        object_degradation_count() - base,
        0,
        "an ordinary null was counted as a reference degradation",
    );
}

/// The total is *defined* as the sum of the per-source table, so a bump on
/// any source shows up in both, and the breakdown attributes it correctly.
#[test]
fn degradation_total_is_the_sum_of_the_per_source_breakdown() {
    let _guard = super::degrade_counter_test_lock();
    let before = object_degradation_breakdown();
    let before_total = object_degradation_count();
    assert_eq!(
        before.iter().copied().sum::<u64>(),
        before_total,
        "total must equal the sum of the breakdown",
    );

    for source in DegradationSource::ALL {
        note_object_degradation_from(source);
    }

    let after = object_degradation_breakdown();
    for source in DegradationSource::ALL {
        assert_eq!(
            after[source.index()] - before[source.index()],
            1,
            "source '{}' did not record exactly one event",
            source.name(),
        );
    }
    assert_eq!(
        object_degradation_count() - before_total,
        DegradationSource::COUNT as u64,
    );
    assert_eq!(
        after.iter().copied().sum::<u64>(),
        object_degradation_count()
    );
}

/// `note_object_degradation()` is the interpreter alias, so this crate's own
/// sites are attributed to `Interpreter` and never to a sibling source.
#[test]
fn in_file_sites_are_attributed_to_the_interpreter_source() {
    let _guard = super::degrade_counter_test_lock();
    let before = object_degradation_breakdown();

    // A never-seen aligned payload: the `to_value` SUB_OBJECT degrade arm.
    let cv = CompactValue::from_bits(make_tagged(SUB_OBJECT, 0x0000_7EEE_1111_8000));
    assert!(matches!(cv.to_value(), Value::Long(_)));
    // The heap-denied arm of `is_object_checked`.
    assert!(!cv.is_object_checked(|_| false));

    let after = object_degradation_breakdown();
    assert_eq!(
        after[DegradationSource::Interpreter.index()]
            - before[DegradationSource::Interpreter.index()],
        2,
    );
    assert_eq!(
        after[DegradationSource::Jit.index()] - before[DegradationSource::Jit.index()],
        0,
    );
    assert_eq!(
        after[DegradationSource::ArrayElement.index()]
            - before[DegradationSource::ArrayElement.index()],
        0,
    );
}

/// `DegradationSource` table invariants: indices are dense, distinct, and
/// within the tables' bounds, and every variant has a distinct name.
#[test]
fn degradation_source_table_is_dense_and_named() {
    for (i, source) in DegradationSource::ALL.into_iter().enumerate() {
        assert_eq!(source.index(), i, "ALL must be in index order");
        assert!(source.index() < DegradationSource::COUNT);
        assert!(!source.name().is_empty());
    }
    assert_eq!(DegradationSource::ALL.len(), DegradationSource::COUNT);
    assert_ne!(
        DegradationSource::Jit.name(),
        DegradationSource::ArrayElement.name(),
    );
}

/// The collapse set is exactly the negative quiet NaNs with mantissa bit 50
/// set, the counter sees every one of them, and it sees nothing else.
///
/// This is a characterisation test, not an aspiration: it pins the KNOWN
/// lossy case of the encoding so that a future change to `NANBOX_BITS`
/// cannot widen it unnoticed. If the encoding is ever made lossless, this
/// test is what has to be rewritten — deliberately, rather than a silent
/// count going up.
#[test]
fn the_nan_collapse_set_is_exactly_the_tag_pattern_and_is_counted() {
    // Not affected: every ordinary value, +/-0, the infinities, the
    // canonical quiet NaN, a POSITIVE payload NaN, and a negative NaN whose
    // mantissa bit 50 is CLEAR.
    let survivors: [u64; 8] = [
        0x3ff0_0000_0000_0000, // 1.0
        0xbff0_0000_0000_0000, // -1.0
        0x0000_0000_0000_0000, // +0.0
        0x8000_0000_0000_0000, // -0.0
        0x7ff0_0000_0000_0000, // +inf
        0xfff0_0000_0000_0000, // -inf
        0x7ff8_0000_0000_0001, // positive quiet NaN with a payload
        0xfff8_0000_0000_0000, // NEGATIVE quiet NaN, marker bit 50 clear
    ];
    let before = reset_nan_payload_collapse_count();
    let _ = before; // whatever earlier tests in this process did
    for bits in survivors {
        let cv = CompactValue::double(f64::from_bits(bits));
        assert_eq!(
            cv.0, bits,
            "{bits:#018x} must round-trip verbatim through CompactValue::double",
        );
    }
    assert_eq!(
        nan_payload_collapse_count(),
        0,
        "no survivor may be counted as a collapse",
    );

    // Affected: sign + exponent + quiet + marker all set. `0xFFFC…` is the
    // boundary; `0xFFFF_FFFF_FFFF_FFFF` is the pattern the commons-math NaN
    // census tripped over, and `Math.sqrt(-1.0)` widened from a float NaN
    // with mantissa bits 22 and 21 set lands here too.
    let collapsed: [u64; 4] = [
        0xFFFC_0000_0000_0000,
        0xFFFC_0000_0000_0001,
        0xFFFE_5E0E_8000_0000,
        0xFFFF_FFFF_FFFF_FFFF,
    ];
    for bits in collapsed {
        let cv = CompactValue::double(f64::from_bits(bits));
        assert_eq!(
            cv.0, CANONICAL_NAN,
            "{bits:#018x} collides with the tag space and must canonicalize",
        );
        // ...and it is still a NaN, which is why nothing above the encoding
        // notices: this is a payload loss, not a value change.
        assert!(f64::from_bits(cv.0).is_nan());
    }
    assert_eq!(
        nan_payload_collapse_count(),
        collapsed.len() as u64,
        "every collapse must be counted exactly once",
    );
    reset_nan_payload_collapse_count();
}

/// The other half of the pair: the SAME patterns `double()` collapses go
/// through `double_raw()` verbatim, and nothing is counted.
///
/// This is what the interpreter actually uses. Every store that reaches it
/// writes a `KIND_DOUBLE` / `LKIND_DOUBLE` / `SlotType::Double` mark beside
/// the slot, which is why dropping the canonicalization here is a fidelity
/// win rather than a type-confusion bug — see `double_raw`'s contract, and
/// `cratonvm_vm::runtime::frame`'s
/// `tag_colliding_double_local_round_trips_and_is_never_a_root` for the
/// GC half of the proof.
#[test]
fn double_raw_keeps_every_tag_colliding_payload_and_counts_nothing() {
    reset_nan_payload_collapse_count();
    // The four `double()` flattens, plus the boundary of the collision set
    // and the two `f2d` samples the census printed.
    let colliding: [u64; 7] = [
        0xFFFC_0000_0000_0000,
        0xFFFC_0000_0000_0001,
        0xFFFE_5E0E_8000_0000,
        0xFFFF_FFFF_FFFF_FFFF,
        0xFFFC_541A_8000_0000,
        0xFFFD_A846_C000_0000,
        0xFFFF_AF30_A000_0000,
    ];
    for bits in colliding {
        let cv = CompactValue::double_raw(f64::from_bits(bits));
        assert_eq!(
            cv.0, bits,
            "{bits:#018x} must round-trip verbatim through double_raw",
        );
        // The descriptor-aware read agrees, EXCEPT inside the one
        // sub-region a descriptor alone cannot separate: a `SUB_INT`
        // pattern whose payload is under 2^32 is bit-for-bit what
        // `CompactValue::int` produces for a small int, and a `SUB_NULL` /
        // `SUB_UNINIT` pattern with payload 0 is bit-for-bit a real null or
        // an unwritten slot. `decode_by_descriptor` has no kind mark to
        // consult, so it keeps answering `Int`-widened / `0.0d` there. Every
        // slot the interpreter actually stores a double in DOES carry that
        // mark, which is why the two exceptions below cost nothing at
        // runtime -- see `decode_arg_kind_aware` and `peek_kind_is_double`.
        let payload = bits & PAYLOAD_MASK;
        let sub = (bits >> SUBTAG_SHIFT) & SUBTAG_MASK;
        let descriptor_alone_is_ambiguous = (sub == SUB_INT && payload >> 32 == 0)
            || ((sub == SUB_NULL || sub == SUB_UNINIT) && payload == 0);
        if !descriptor_alone_is_ambiguous {
            match cv.decode_by_descriptor(b'D') {
                Value::Double(d) => assert_eq!(d.to_bits(), bits),
                other => panic!("D-descriptor decode of {bits:#018x} gave {other:?}"),
            }
        }
        // And `from_value_kinded` is the same encode by another door.
        assert_eq!(
            CompactValue::from_value_kinded(Value::Double(f64::from_bits(bits))).0,
            bits,
        );
    }
    assert_eq!(
        nan_payload_collapse_count(),
        0,
        "double_raw must never reach the collapse counter",
    );

    // Name the ambiguous sub-region explicitly: exactly the patterns a
    // *descriptor-only* decode still reads as something else. Both are
    // SUB_INT with a payload a real int could have had.
    for (bits, as_int) in [
        (0xFFFC_0000_0000_0000u64, 0i32),
        (0xFFFC_0000_0000_0001u64, 1i32),
    ] {
        assert_eq!(
            CompactValue::double_raw(f64::from_bits(bits)).0,
            bits,
            "the slot still holds the bits",
        );
        assert_eq!(
            CompactValue::int(as_int).0,
            bits,
            "…and they are bit-for-bit CompactValue::int({as_int}), which is \
             why a decoder with no kind mark cannot tell them apart",
        );
    }

    // Non-colliding doubles are byte-identical through both constructors,
    // so nothing that already worked changes shape.
    for bits in [
        0x3ff0_0000_0000_0000u64,
        0x7ff8_0000_0000_0000,
        0xfff8_0000_0000_0000,
        0x0000_0000_0000_0000,
    ] {
        let d = f64::from_bits(bits);
        assert_eq!(CompactValue::double(d).0, CompactValue::double_raw(d).0);
    }
    assert_eq!(nan_payload_collapse_count(), 0);
    reset_nan_payload_collapse_count();
}
