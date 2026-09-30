// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Central resource limits for the class-file reader, plus the checked
//! arithmetic helpers that enforce them.
//!
//! # Why one module
//!
//! `reader` is the first code an untrusted `.class` file reaches. Every
//! count, length and index in the wire format is attacker-controlled, and
//! most of them feed either an allocation (`Vec::with_capacity`), a
//! multiplication (`count * entry_size`), or a recursive descent. Before
//! this module the limits were scattered as private `const`s across
//! `attribute.rs`, `class_reader.rs`, `field_type.rs`, `instruction.rs`,
//! `signature.rs` and `stack_map.rs`, with three separate definitions of
//! the same 1024-element preallocation cap and two of the 65 535 code
//! length. Collecting them here means a limit can be audited, tested and
//! changed in exactly one place.
//!
//! # Two kinds of limit
//!
//! **JVMS-mandated** limits come from the class file format itself: the
//! spec either states them outright (255 array dimensions, 65 535 code
//! bytes) or implies them through the width of the wire field (a `u2`
//! count can never exceed 65 535). Rejecting past them is *required* for
//! conformance — HotSpot raises `ClassFormatError` at the same points.
//!
//! **Defensive** limits have no basis in the spec. They exist purely to
//! bound the work an adversarial input can make the parser do:
//! preallocation caps, recursion depths, and the switch-table entry cap.
//! They are set far above anything `javac` emits, so a legitimate class
//! never sees them.
//!
//! Every constant below is tagged with which kind it is.
//!
//! # The rule these helpers implement
//!
//! A count read off the wire must be checked against the **bytes that
//! actually remain in the input** before it is used to size an
//! allocation. A 40-byte class file that declares 65 535 StackMapTable
//! frames must be rejected while parsing the header, not after reserving
//! several megabytes that the very next read will fail on. Since every
//! entry of every table in the format costs at least one byte, a declared
//! count greater than the remaining byte count is provably a lie, and the
//! parser can say so before touching the allocator.
//!
//! Nothing here panics: every rejection is a [`ClassReaderError`].

use crate::class_reader_error::ClassReaderError;

// ---------------------------------------------------------------------------
// JVMS-mandated limits
// ---------------------------------------------------------------------------

/// JVMS §4.1: `constant_pool_count` is a `u2`, so the pool can never
/// declare more than 65 535 slots. Index 0 is a reserved sentinel, so the
/// number of *real* entries is at most `MAX_CONSTANT_POOL_COUNT - 1`.
pub const MAX_CONSTANT_POOL_COUNT: u16 = u16::MAX;

/// Smallest number of bytes any single `cp_info` structure can occupy:
/// a `u1` tag plus a `u2` payload (`CONSTANT_Class`, `CONSTANT_String`,
/// `CONSTANT_MethodType`, `CONSTANT_Module`, `CONSTANT_Package`, and an
/// empty `CONSTANT_Utf8`). Long/Double are the only entries that cover two
/// slots, and they cost 9 bytes for those two — 4.5 bytes per slot, still
/// above this floor. So `slots * MIN_CONSTANT_POOL_ENTRY_BYTES` is a sound
/// lower bound on the bytes a declared pool must occupy.
pub const MIN_CONSTANT_POOL_ENTRY_BYTES: usize = 3;

/// JVMS §4.3.2: an array type may have at most 255 dimensions. Enforced in
/// [`crate::field_type::FieldType::parse_partial`].
pub const MAX_ARRAY_DIMENSIONS: usize = 255;

/// JVMS §4.7.3: `code_length` must be greater than zero and less than
/// 65 536. The field on the wire is a `u4`, so the check is a real
/// narrowing check, not a tautology.
pub const MAX_CODE_LENGTH: usize = 65_535;

/// The largest value any `u2` count in the format can take. Used as the
/// ceiling for "this count came from a `u2`, so it is already bounded"
/// assertions in the boundary tests.
pub const MAX_U16_COUNT: usize = u16::MAX as usize;

// ---------------------------------------------------------------------------
// Per-entry wire sizes — the multipliers in `count * entry_size`
// ---------------------------------------------------------------------------

/// `exception_index_table[]` entry: one `u2` constant-pool index (JVMS §4.7.5).
pub const EXCEPTIONS_ENTRY_SIZE: usize = 2;
/// `line_number_table[]` entry: `start_pc` + `line_number` (JVMS §4.7.12).
pub const LINE_NUMBER_ENTRY_SIZE: usize = 4;
/// `classes[]` entry in `InnerClasses`: four `u2` fields (JVMS §4.7.6).
pub const INNER_CLASS_ENTRY_SIZE: usize = 8;
/// `local_variable_table[]` / `local_variable_type_table[]` entry: five
/// `u2` fields (JVMS §4.7.13, §4.7.14).
pub const LOCAL_VARIABLE_ENTRY_SIZE: usize = 10;
/// `parameters[]` entry in `MethodParameters`: two `u2` fields (JVMS §4.7.24).
pub const METHOD_PARAMETER_ENTRY_SIZE: usize = 4;
/// `exception_table[]` entry in `Code`: four `u2` fields (JVMS §4.7.3).
pub const EXCEPTION_TABLE_ENTRY_SIZE: usize = 8;
/// `table[]` entry in a `localvar_target` type-annotation target
/// (`target_type` 0x40/0x41): three `u2` fields (JVMS §4.7.20.1).
pub const LOCALVAR_TARGET_ENTRY_SIZE: usize = 6;

// ---------------------------------------------------------------------------
// Defensive limits — no JVMS basis, purely anti-DoS
// ---------------------------------------------------------------------------

/// Upper bound on any single `Vec::with_capacity` sized from a wire count.
///
/// Defensive. A `u2` count is already bounded at 65 535, but a table of
/// 65 535 fat entries is several megabytes of reservation that a two-byte
/// header can request. Capping the *reservation* at 1024 elements costs a
/// handful of `Vec` growth reallocations on the (vanishingly rare) class
/// with a bigger real table, and removes the amplification entirely.
pub const PREALLOC_CAP: usize = 1024;

/// Maximum nesting depth for the attribute-table recursion (`Code` inside
/// `Code`, `Record` component attributes inside `Record`).
///
/// Defensive. Real compilers never nest more than one or two levels.
pub const MAX_ATTRIBUTE_DEPTH: usize = 16;

/// Maximum nesting depth for the `annotation` ⇄ `element_value` mutual
/// recursion (`@`-valued and `[`-valued elements).
///
/// Defensive.
pub const MAX_ANNOTATION_DEPTH: usize = 256;

/// Maximum nesting depth for generic-signature parsing (nested type
/// arguments and array dimensions in `Signature` attributes).
///
/// Defensive.
pub const MAX_SIGNATURE_DEPTH: usize = 256;

/// Maximum number of entries a `tableswitch` / `lookupswitch` may declare.
///
/// Defensive. The wire fields are `s4`, so without a cap a single
/// four-byte `high` can request billions of entries. `code_length` is
/// itself capped at 65 535, so no legitimate switch can approach this.
pub const MAX_SWITCH_ENTRIES: usize = 16_384;

/// Maximum number of `StackMapTable` frames, one per bytecode offset.
///
/// Defensive, but derived from [`MAX_CODE_LENGTH`]: a frame must attach to
/// a distinct bytecode offset, and there are at most 65 535 of those.
pub const MAX_STACK_MAP_ENTRIES: usize = MAX_CODE_LENGTH;

// ---------------------------------------------------------------------------
// Checked helpers
// ---------------------------------------------------------------------------

/// Compute `count * entry_size` with overflow rejected rather than wrapped.
///
/// Every table in the class file format is read as "a `u2`/`u4` count
/// followed by `count` fixed-size records", and the byte span of the
/// records is what gets handed to `ClassFileBuffer::read_bytes`. Most of
/// those counts are `u2`-bounded, so the product provably fits in a
/// `usize` on every supported host — but "provably" depends on the count's
/// width, which is exactly the sort of invariant that a later refactor
/// (e.g. widening a count to `u4`) breaks silently. On a 32-bit host a
/// wrapped product yields a *small* byte span, which `read_bytes` then
/// happily satisfies, and the parser reads a short prefix as if it were
/// the whole table.
///
/// Returns [`ClassReaderError::InvalidClassData`] on overflow — there can
/// never be that many bytes of input.
#[inline]
pub fn checked_span(
    label: &str,
    count: usize,
    entry_size: usize,
) -> Result<usize, ClassReaderError> {
    count
        .checked_mul(entry_size)
        .ok_or_else(|| ClassReaderError::InvalidClassData {
            message: format!(
                "{label}: entry count {count} times entry size {entry_size} overflows the address space"
            ),
        })
}

/// Reject a declared entry count that cannot possibly fit in the bytes
/// that remain in the input.
///
/// `min_entry_bytes` is the smallest number of bytes one entry can
/// occupy — for fixed-size records that is the record size; for
/// variable-size records (annotations, stack-map frames, constant-pool
/// entries) it is the shortest legal encoding. The check is therefore
/// conservative: it never rejects an input the full parse would accept, it
/// only rejects ones that are already provably truncated.
///
/// This is the check that turns "declare 65 535 entries in a 40-byte file"
/// from a multi-megabyte reservation followed by a read failure into an
/// immediate parse error.
#[inline]
pub fn ensure_count_fits(
    label: &str,
    count: usize,
    min_entry_bytes: usize,
    remaining: usize,
) -> Result<(), ClassReaderError> {
    let needed = checked_span(label, count, min_entry_bytes.max(1))?;
    if needed > remaining {
        return Err(ClassReaderError::InvalidClassData {
            message: format!(
                "{label}: declared count {count} needs at least {needed} bytes but only {remaining} remain"
            ),
        });
    }
    Ok(())
}

/// Preallocation size for a table of `count` entries whose shortest legal
/// encoding is `min_entry_bytes`, given `remaining` bytes of input.
///
/// The result is the minimum of three bounds:
///
/// 1. the declared `count` (never reserve more than was asked for),
/// 2. [`PREALLOC_CAP`] (never reserve an unbounded amount for one table),
/// 3. `remaining / min_entry_bytes` (never reserve room for entries the
///    input is too short to contain).
///
/// Bound 3 is what makes the reservation proportional to the *input*
/// rather than to a number the attacker typed into a two-byte field.
/// Under-reserving is always safe: the `Vec` grows on demand.
#[inline]
pub fn bounded_capacity(count: usize, min_entry_bytes: usize, remaining: usize) -> usize {
    let affordable = remaining / min_entry_bytes.max(1);
    count.min(PREALLOC_CAP).min(affordable)
}

/// Narrow a wire-format `u32` length to `usize` without a silent truncation.
///
/// On a 64-bit host this is infallible and compiles away. On a 32-bit host
/// `u32 as usize` is still lossless — but `usize::try_from` states the
/// intent, and the function is the single place to look if the reader is
/// ever ported to a 16-bit target or the field is widened to `u64`.
#[inline]
pub fn wire_len_to_usize(label: &str, value: u32) -> Result<usize, ClassReaderError> {
    usize::try_from(value).map_err(|_| ClassReaderError::InvalidClassData {
        message: format!("{label}: length {value} does not fit in a host usize"),
    })
}

/// Checked `start + len` for a byte range derived from wire values.
///
/// Used where a payload's absolute offset inside the shared class-file
/// buffer is computed as `body_offset + buf.position()` and then extended
/// by a declared length. Overflow here would wrap to a *small* end offset
/// that passes a naive bounds check.
#[inline]
pub fn checked_end(label: &str, start: usize, len: usize) -> Result<usize, ClassReaderError> {
    start
        .checked_add(len)
        .ok_or_else(|| ClassReaderError::InvalidClassData {
            message: format!("{label}: byte range {start}+{len} overflows the address space"),
        })
}

#[cfg(test)]
mod tests {
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
}
