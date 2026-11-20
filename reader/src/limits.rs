// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
mod tests;
