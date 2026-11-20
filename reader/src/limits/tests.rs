use super::*;

// -----------------------------------------------------------------
// checked_span — the `count * entry_size` multiplication.
//
// Each "must reject" case has a "must accept" twin at the adjacent
// value so a helper that rejected everything would fail the suite.
// -----------------------------------------------------------------

#[test]
fn checked_span_accepts_the_largest_u16_count_at_every_entry_size() {
    // Every real table in the format is a u2 count times one of the
    // sizes below. None of these products may overflow, on any host.
    for size in [
        EXCEPTIONS_ENTRY_SIZE,
        LINE_NUMBER_ENTRY_SIZE,
        INNER_CLASS_ENTRY_SIZE,
        LOCAL_VARIABLE_ENTRY_SIZE,
        METHOD_PARAMETER_ENTRY_SIZE,
        EXCEPTION_TABLE_ENTRY_SIZE,
        LOCALVAR_TARGET_ENTRY_SIZE,
    ] {
        let span = checked_span("test", MAX_U16_COUNT, size).expect("u2 count cannot overflow");
        assert_eq!(span, MAX_U16_COUNT * size);
    }
}

#[test]
fn checked_span_rejects_usize_max_times_two() {
    assert!(checked_span("test", usize::MAX, 2).is_err());
    // Off-by-one twin: the largest count that still fits at this
    // entry size must be accepted.
    assert!(checked_span("test", usize::MAX / 2, 2).is_ok());
}

#[test]
fn checked_span_rejects_half_max_plus_one() {
    // Exactly one past the boundary of the accepted twin above.
    assert!(checked_span("test", usize::MAX / 2 + 1, 2).is_err());
}

#[test]
fn checked_span_handles_zero_and_one() {
    assert_eq!(checked_span("test", 0, 8).unwrap(), 0);
    assert_eq!(checked_span("test", 1, 8).unwrap(), 8);
    // A zero entry size is degenerate but must not panic or divide.
    assert_eq!(checked_span("test", usize::MAX, 0).unwrap(), 0);
}

#[test]
fn checked_span_at_u32_and_i32_boundaries() {
    // A u4 wire length narrowed to usize, times a one-byte element:
    // must not overflow on a 64-bit host, and must be exact.
    let u32_max = u32::MAX as usize;
    assert_eq!(checked_span("test", u32_max, 1).unwrap(), u32_max);
    // i32::MAX / i32::MIN reinterpreted as unsigned wire values.
    assert_eq!(
        checked_span("test", i32::MAX as usize, 1).unwrap(),
        i32::MAX as usize
    );
    assert_eq!(
        checked_span("test", (i32::MIN as u32) as usize, 1).unwrap(),
        2_147_483_648
    );
}

// -----------------------------------------------------------------
// ensure_count_fits — count vs. remaining input length.
// -----------------------------------------------------------------

#[test]
fn count_exceeding_remaining_bytes_is_rejected_before_allocation() {
    // The headline case: 65 535 entries declared in a 40-byte file.
    assert!(ensure_count_fits("test", MAX_U16_COUNT, 1, 40).is_err());
    assert!(ensure_count_fits("test", MAX_U16_COUNT, 8, 40).is_err());
}

#[test]
fn count_exactly_filling_the_remaining_bytes_is_accepted() {
    // Must-accept twin at the exact boundary: 5 entries of 8 bytes in
    // exactly 40 bytes.
    assert!(ensure_count_fits("test", 5, 8, 40).is_ok());
    // One more entry does not fit.
    assert!(ensure_count_fits("test", 6, 8, 40).is_err());
    // One fewer trivially fits.
    assert!(ensure_count_fits("test", 4, 8, 40).is_ok());
}

#[test]
fn zero_count_always_fits_even_in_an_empty_buffer() {
    assert!(ensure_count_fits("test", 0, 8, 0).is_ok());
    // ...but one entry does not.
    assert!(ensure_count_fits("test", 1, 8, 0).is_err());
}

#[test]
fn count_fits_rejects_overflowing_product_rather_than_wrapping() {
    // usize::MAX entries of 2 bytes wraps to `usize::MAX - 1` if the
    // multiplication is unchecked, which would then compare as "fits"
    // against a large `remaining`. It must be an error instead.
    assert!(ensure_count_fits("test", usize::MAX, 2, usize::MAX).is_err());
}

#[test]
fn count_fits_treats_zero_entry_size_as_one_byte() {
    // A caller passing 0 must not divide/multiply by zero into an
    // "everything fits" verdict.
    assert!(ensure_count_fits("test", 100, 0, 10).is_err());
    assert!(ensure_count_fits("test", 10, 0, 10).is_ok());
}

// -----------------------------------------------------------------
// bounded_capacity — the actual reservation size.
// -----------------------------------------------------------------

#[test]
fn capacity_never_exceeds_what_the_input_can_hold() {
    // 65 535 declared entries of 8 bytes each, but only 40 bytes of
    // input: reserve room for 5, not 65 535.
    assert_eq!(bounded_capacity(MAX_U16_COUNT, 8, 40), 5);
    // Same declaration with plenty of input is capped by PREALLOC_CAP.
    assert_eq!(bounded_capacity(MAX_U16_COUNT, 8, 1 << 30), PREALLOC_CAP);
    // A small honest count is used verbatim.
    assert_eq!(bounded_capacity(3, 8, 1 << 30), 3);
}

#[test]
fn capacity_is_zero_when_nothing_remains() {
    assert_eq!(bounded_capacity(MAX_U16_COUNT, 8, 0), 0);
    assert_eq!(bounded_capacity(MAX_U16_COUNT, 8, 7), 0);
    // Off-by-one twin: exactly one entry's worth of input.
    assert_eq!(bounded_capacity(MAX_U16_COUNT, 8, 8), 1);
}

#[test]
fn capacity_saturates_at_prealloc_cap_not_at_the_declared_count() {
    assert_eq!(
        bounded_capacity(usize::MAX, 1, usize::MAX),
        PREALLOC_CAP,
        "an absurd count must never drive the reservation"
    );
}

#[test]
fn capacity_handles_zero_entry_size_without_dividing_by_zero() {
    assert_eq!(bounded_capacity(10, 0, 10), 10);
}

// -----------------------------------------------------------------
// Width conversions.
// -----------------------------------------------------------------

#[test]
fn wire_len_conversion_is_exact_at_the_u32_boundaries() {
    assert_eq!(wire_len_to_usize("test", 0).unwrap(), 0);
    assert_eq!(wire_len_to_usize("test", 1).unwrap(), 1);
    assert_eq!(
        wire_len_to_usize("test", u32::MAX).unwrap(),
        u32::MAX as usize
    );
    // The bit pattern of i32::MIN read as an unsigned u4 length.
    assert_eq!(
        wire_len_to_usize("test", i32::MIN as u32).unwrap(),
        2_147_483_648
    );
    assert_eq!(
        wire_len_to_usize("test", i32::MAX as u32).unwrap(),
        2_147_483_647
    );
}

#[test]
fn checked_end_rejects_wraparound() {
    assert!(checked_end("test", usize::MAX, 1).is_err());
    // Exactly-representable end is the last accepted case: the largest
    // legal result is `usize::MAX` itself, so `start + len` may reach it
    // but not pass it.
    assert!(checked_end("test", usize::MAX - 1, 1).is_ok());
    assert!(checked_end("test", usize::MAX - 2, 2).is_ok());
    assert!(checked_end("test", usize::MAX - 1, 2).is_err());
    assert!(checked_end("test", usize::MAX - 1, 3).is_err());
    assert_eq!(checked_end("test", 10, 0).unwrap(), 10);
}

// -----------------------------------------------------------------
// The constants themselves.
// -----------------------------------------------------------------

#[test]
fn constants_are_internally_consistent() {
    assert_eq!(MAX_U16_COUNT, 65_535);
    assert_eq!(MAX_CONSTANT_POOL_COUNT as usize, MAX_U16_COUNT);
    assert_eq!(MAX_CODE_LENGTH, MAX_U16_COUNT);
    assert_eq!(MAX_STACK_MAP_ENTRIES, MAX_CODE_LENGTH);
    assert!(PREALLOC_CAP > 0 && PREALLOC_CAP <= MAX_U16_COUNT);
    assert!(MAX_ATTRIBUTE_DEPTH > 0);
    assert!(MAX_ANNOTATION_DEPTH > 0);
    assert!(MAX_SIGNATURE_DEPTH > 0);
    assert!(MAX_ARRAY_DIMENSIONS <= 255);
    // A switch table cannot legitimately exceed the code it lives in.
    assert!(MAX_SWITCH_ENTRIES <= MAX_CODE_LENGTH);
    assert!(MIN_CONSTANT_POOL_ENTRY_BYTES >= 3);
}

#[test]
fn constant_pool_floor_is_a_sound_lower_bound() {
    // Every cp_info tag's smallest encoding, in bytes per *slot*.
    // (tag byte + payload) / slots-occupied. The floor must not
    // exceed any of them, or a legitimate pool would be rejected.
    //
    // Utf8 (empty)      1 + 2          = 3 bytes / 1 slot
    // Class/String/
    //   MethodType/
    //   Module/Package  1 + 2          = 3 bytes / 1 slot
    // Integer/Float     1 + 4          = 5 bytes / 1 slot
    // Fieldref/…/
    //   NameAndType/
    //   Dynamic/…       1 + 4          = 5 bytes / 1 slot
    // MethodHandle      1 + 1 + 2      = 4 bytes / 1 slot
    // Long/Double       1 + 8 = 9      = 4.5 bytes / 2 slots
    let per_slot_floors = [3.0_f64, 3.0, 5.0, 5.0, 4.0, 4.5];
    for floor in per_slot_floors {
        assert!(
            (MIN_CONSTANT_POOL_ENTRY_BYTES as f64) <= floor,
            "constant-pool byte floor {MIN_CONSTANT_POOL_ENTRY_BYTES} would reject a legal pool of {floor} bytes/slot"
        );
    }
}
