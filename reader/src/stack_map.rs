// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! StackMapTable parser (JVM spec 4.7.4).
//!
//! The StackMapTable attribute is used during bytecode verification (Pass 3, JVM spec 4.10.1)
//! for classes with version >= 51 (Java 7+). It contains typed frames at branch targets
//! that the verifier checks against to ensure type safety.
//!
//! Each frame describes the local variable types and operand stack types at a specific
//! bytecode offset. Frames use delta encoding: the offset of each frame is relative to
//! the previous frame (or the start of the method for the first frame).

use crate::class_reader_error::ClassReaderError;
// Preallocation is bounded by the input length via `bounded_capacity`; the
// limit constants live in `crate::limits` (see
// `docs/security/reader/limits.md`).
use crate::limits::{bounded_capacity, MAX_STACK_MAP_ENTRIES};

// ---------------------------------------------------------------------------
// Verification type info — the type tags used in StackMapTable
// ---------------------------------------------------------------------------

/// A verification type as used in StackMapTable frames (JVM spec 4.7.4).
///
/// These describe the type of a single local variable slot or stack entry
/// at a specific bytecode offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationTypeInfo {
    /// `Top` — an undefined/unusable slot (e.g. second half of long/double).
    Top,
    /// `Integer` — int, short, byte, char, or boolean.
    Integer,
    /// `Float` — float.
    Float,
    /// `Double` — double (occupies two slots).
    Double,
    /// `Long` — long (occupies two slots).
    Long,
    /// `Null` — the null reference.
    Null,
    /// `UninitializedThis` — the uninitialized `this` reference in a constructor.
    UninitializedThis,
    /// `Object` — a reference to an instance of the class at `cpool_index`.
    Object { cpool_index: u16 },
    /// `Uninitialized` — a reference to an uninitialized object created by `new` at `offset`.
    Uninitialized { offset: u16 },
}

// Tag constants (JVM spec 4.7.4)
const ITEM_TOP: u8 = 0;
const ITEM_INTEGER: u8 = 1;
const ITEM_FLOAT: u8 = 2;
const ITEM_DOUBLE: u8 = 3;
const ITEM_LONG: u8 = 4;
const ITEM_NULL: u8 = 5;
const ITEM_UNINITIALIZED_THIS: u8 = 6;
const ITEM_OBJECT: u8 = 7;
const ITEM_UNINITIALIZED: u8 = 8;

// ---------------------------------------------------------------------------
// Stack map frames
// ---------------------------------------------------------------------------

/// A single frame in the StackMapTable (JVM spec 4.7.4).
///
/// Each frame type encodes the local variables and operand stack at a specific
/// bytecode offset. The offset is encoded as a delta from the previous frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StackMapFrame {
    /// `same_frame` (tags 0-63): same locals as previous, empty stack.
    /// offset_delta = frame_type
    SameFrame { offset_delta: u16 },

    /// `same_locals_1_stack_item_frame` (tags 64-127): same locals, one stack entry.
    /// offset_delta = frame_type - 64
    SameLocals1StackItem {
        offset_delta: u16,
        stack: VerificationTypeInfo,
    },

    /// `same_locals_1_stack_item_frame_extended` (tag 247): same locals, one stack entry.
    /// offset_delta is explicit u16.
    SameLocals1StackItemExtended {
        offset_delta: u16,
        stack: VerificationTypeInfo,
    },

    /// `chop_frame` (tags 248-250): remove last k locals, empty stack.
    /// k = 251 - frame_type, offset_delta is explicit u16.
    ChopFrame { offset_delta: u16, chopped: u8 },

    /// `same_frame_extended` (tag 251): same locals as previous, empty stack.
    /// offset_delta is explicit u16.
    SameFrameExtended { offset_delta: u16 },

    /// `append_frame` (tags 252-254): append k new locals, empty stack.
    /// k = frame_type - 251, offset_delta is explicit u16.
    AppendFrame {
        offset_delta: u16,
        locals: Vec<VerificationTypeInfo>,
    },

    /// `full_frame` (tag 255): explicit full list of locals and stack.
    FullFrame {
        offset_delta: u16,
        locals: Vec<VerificationTypeInfo>,
        stack: Vec<VerificationTypeInfo>,
    },
}

// ---------------------------------------------------------------------------
// StackMapTable — the parsed attribute
// ---------------------------------------------------------------------------

/// A parsed StackMapTable attribute (JVM spec 4.7.4).
///
/// Contains all frames declared for a method's Code attribute. The verifier
/// uses these to type-check branch targets and exception handlers.
#[derive(Debug, Clone)]
pub struct StackMapTable {
    pub entries: Vec<StackMapFrame>,
}

impl StackMapTable {
    /// Parse a StackMapTable from raw attribute bytes.
    ///
    /// The input `data` is the attribute_info data (everything after attribute_name_index
    /// and attribute_length), which starts with `number_of_entries: u16`.
    pub fn parse(data: &[u8]) -> Result<Self, ClassReaderError> {
        let mut pos = 0;

        let number_of_entries = read_u16(data, &mut pos)?;
        // Cap pre-allocation to prevent malicious class files from causing
        // huge allocations. `MAX_STACK_MAP_ENTRIES` (65 535) is the maximum
        // number of bytecode offsets in a Code attribute, so it is the
        // absolute ceiling — but on its own it is a *weak* bound: a
        // two-byte header can still ask for 65 535 `StackMapFrame`s (each
        // carrying two `Vec`s, ~56 bytes) from an otherwise empty
        // attribute, i.e. several megabytes reserved before the first
        // frame byte is even read.
        //
        // C2 remediation: reserve from what the input can actually hold.
        // The shortest legal frame is one byte (`same_frame`, tags 0..=63),
        // so `remaining` bytes can hold at most `remaining` frames.
        // Under-reserving is free — the `Vec` grows on demand — while
        // over-reserving is the whole attack.
        let remaining = data.len().saturating_sub(pos);
        const MIN_FRAME_BYTES: usize = 1; // same_frame is a bare tag byte
        let mut entries = Vec::with_capacity(bounded_capacity(
            (number_of_entries as usize).min(MAX_STACK_MAP_ENTRIES),
            MIN_FRAME_BYTES,
            remaining,
        ));

        for _ in 0..number_of_entries {
            let frame = parse_frame(data, &mut pos)?;
            entries.push(frame);
        }

        Ok(StackMapTable { entries })
    }

    /// Compute the absolute bytecode offsets for each frame.
    ///
    /// The first frame's absolute offset is `offset_delta`. Each subsequent
    /// frame's absolute offset is `previous_absolute + offset_delta + 1`.
    /// (The `+1` accounts for the implicit increment per JVM spec.)
    ///
    /// Round 7 audit fix (MED #8 / round-4 #6): the accumulation is
    /// performed in `u32` arithmetic so a malformed StackMapTable whose
    /// running absolute offset overshoots `u16::MAX` (65 535) is
    /// detected as `InvalidClassData` instead of silently wrapping. The
    /// JVM spec caps bytecode at 65 535 bytes (Code attribute uses a
    /// `u4` length, but branch targets are `u2`), so any frame offset
    /// > `u16::MAX` indicates a corrupt or maliciously-crafted class
    /// file. Each absolute offset is bounded-checked before being
    /// downcast back to `u16` for the returned vec, preserving the
    /// caller-visible type.
    ///
    /// Round 8 reader HIGH fix: the `p + delta + 1` accumulator is
    /// computed with `saturating_add` so a debug-build run on a
    /// maliciously-crafted file whose deltas push the running total
    /// past `u32::MAX` (theoretically impossible given `delta <=
    /// u16::MAX`, but the explicit saturating form removes any chance
    /// of a debug-build overflow panic firing before the explicit
    /// `> u16::MAX` bound check rejects the input). The saturated
    /// `u32::MAX` value still trips the bound check and produces the
    /// same `InvalidClassData` error a release build would surface.
    pub fn absolute_offsets(&self) -> Result<Vec<u16>, ClassReaderError> {
        let mut offsets = Vec::with_capacity(self.entries.len());
        let mut prev: Option<u32> = None;

        for (idx, entry) in self.entries.iter().enumerate() {
            let delta = frame_offset_delta(entry) as u32;
            let absolute: u32 = match prev {
                None => delta,
                Some(p) => p.saturating_add(delta).saturating_add(1),
            };
            if absolute > u16::MAX as u32 {
                return Err(ClassReaderError::InvalidClassData {
                    message: format!(
                        "StackMapTable: absolute offset {absolute} at frame {idx} exceeds u16::MAX (65535) — corrupt class file",
                    ),
                });
            }
            offsets.push(absolute as u16);
            prev = Some(absolute);
        }

        Ok(offsets)
    }
}

/// Extract the offset_delta from any frame variant.
fn frame_offset_delta(frame: &StackMapFrame) -> u16 {
    match frame {
        StackMapFrame::SameFrame { offset_delta } => *offset_delta,
        StackMapFrame::SameLocals1StackItem { offset_delta, .. } => *offset_delta,
        StackMapFrame::SameLocals1StackItemExtended { offset_delta, .. } => *offset_delta,
        StackMapFrame::ChopFrame { offset_delta, .. } => *offset_delta,
        StackMapFrame::SameFrameExtended { offset_delta } => *offset_delta,
        StackMapFrame::AppendFrame { offset_delta, .. } => *offset_delta,
        StackMapFrame::FullFrame { offset_delta, .. } => *offset_delta,
    }
}

// ---------------------------------------------------------------------------
// Parsing helpers
// ---------------------------------------------------------------------------

fn read_u8(data: &[u8], pos: &mut usize) -> Result<u8, ClassReaderError> {
    if *pos >= data.len() {
        return Err(ClassReaderError::InvalidClassData {
            message: format!("StackMapTable: unexpected end of data at position {}", *pos),
        });
    }
    let val = data[*pos];
    *pos += 1;
    Ok(val)
}

fn read_u16(data: &[u8], pos: &mut usize) -> Result<u16, ClassReaderError> {
    // Round 11 audit fix (reader B2): use `checked_add` for `*pos + 2`
    // instead of the bare `*pos + 2 > data.len()` compare. `pos` currently
    // starts at 0 and only advances by small bounded reads, so it cannot
    // reach near `usize::MAX` today — but a bare add wraps if a future
    // caller ever seeds `pos` from an external offset, silently passing the
    // bounds check and indexing OOB. Matches the `checked_add` hardening the
    // rest of the crate (`buffer.rs::read_bytes`, `instruction.rs`)
    // standardised on. On overflow we surface the same `InvalidClassData`
    // end-of-data error a short buffer would produce.
    let end = match pos.checked_add(2) {
        Some(end) if end <= data.len() => end,
        _ => {
            return Err(ClassReaderError::InvalidClassData {
                message: format!("StackMapTable: unexpected end of data at position {}", *pos),
            });
        }
    };
    let val = u16::from_be_bytes([data[*pos], data[*pos + 1]]);
    *pos = end;
    Ok(val)
}

fn parse_verification_type(
    data: &[u8],
    pos: &mut usize,
) -> Result<VerificationTypeInfo, ClassReaderError> {
    let tag = read_u8(data, pos)?;
    match tag {
        ITEM_TOP => Ok(VerificationTypeInfo::Top),
        ITEM_INTEGER => Ok(VerificationTypeInfo::Integer),
        ITEM_FLOAT => Ok(VerificationTypeInfo::Float),
        ITEM_DOUBLE => Ok(VerificationTypeInfo::Double),
        ITEM_LONG => Ok(VerificationTypeInfo::Long),
        ITEM_NULL => Ok(VerificationTypeInfo::Null),
        ITEM_UNINITIALIZED_THIS => Ok(VerificationTypeInfo::UninitializedThis),
        ITEM_OBJECT => {
            let cpool_index = read_u16(data, pos)?;
            Ok(VerificationTypeInfo::Object { cpool_index })
        }
        ITEM_UNINITIALIZED => {
            let offset = read_u16(data, pos)?;
            Ok(VerificationTypeInfo::Uninitialized { offset })
        }
        _ => Err(ClassReaderError::InvalidClassData {
            message: format!("StackMapTable: invalid verification type tag: {tag}"),
        }),
    }
}

fn parse_verification_types(
    data: &[u8],
    pos: &mut usize,
    count: u16,
) -> Result<Vec<VerificationTypeInfo>, ClassReaderError> {
    // Cap pre-allocation — max_locals and max_stack are each u16, so 65535
    // is the absolute max. C2 remediation: also bound by the bytes that
    // remain. A `verification_type_info` is at least one tag byte, so a
    // `full_frame` claiming 65 535 locals in a 4-byte attribute reserves
    // nothing rather than a quarter-megabyte it can never fill.
    const MIN_VERIFICATION_TYPE_BYTES: usize = 1;
    let remaining = data.len().saturating_sub(*pos);
    let mut types = Vec::with_capacity(bounded_capacity(
        count as usize,
        MIN_VERIFICATION_TYPE_BYTES,
        remaining,
    ));
    for _ in 0..count {
        types.push(parse_verification_type(data, pos)?);
    }
    Ok(types)
}

fn parse_frame(data: &[u8], pos: &mut usize) -> Result<StackMapFrame, ClassReaderError> {
    let frame_type = read_u8(data, pos)?;

    match frame_type {
        // same_frame: tags 0-63
        0..=63 => Ok(StackMapFrame::SameFrame {
            offset_delta: frame_type as u16,
        }),

        // same_locals_1_stack_item_frame: tags 64-127
        64..=127 => {
            let stack = parse_verification_type(data, pos)?;
            Ok(StackMapFrame::SameLocals1StackItem {
                offset_delta: (frame_type - 64) as u16,
                stack,
            })
        }

        // Tags 128-246 are reserved and unused
        128..=246 => Err(ClassReaderError::InvalidClassData {
            message: format!("StackMapTable: reserved frame type tag: {frame_type}"),
        }),

        // same_locals_1_stack_item_frame_extended: tag 247
        247 => {
            let offset_delta = read_u16(data, pos)?;
            let stack = parse_verification_type(data, pos)?;
            Ok(StackMapFrame::SameLocals1StackItemExtended {
                offset_delta,
                stack,
            })
        }

        // chop_frame: tags 248-250
        248..=250 => {
            let offset_delta = read_u16(data, pos)?;
            let chopped = 251 - frame_type;
            Ok(StackMapFrame::ChopFrame {
                offset_delta,
                chopped,
            })
        }

        // same_frame_extended: tag 251
        251 => {
            let offset_delta = read_u16(data, pos)?;
            Ok(StackMapFrame::SameFrameExtended { offset_delta })
        }

        // append_frame: tags 252-254
        252..=254 => {
            let offset_delta = read_u16(data, pos)?;
            let num_new = (frame_type - 251) as u16;
            let locals = parse_verification_types(data, pos, num_new)?;
            Ok(StackMapFrame::AppendFrame {
                offset_delta,
                locals,
            })
        }

        // full_frame: tag 255
        255 => {
            let offset_delta = read_u16(data, pos)?;
            let num_locals = read_u16(data, pos)?;
            let locals = parse_verification_types(data, pos, num_locals)?;
            let num_stack = read_u16(data, pos)?;
            let stack = parse_verification_types(data, pos, num_stack)?;
            Ok(StackMapFrame::FullFrame {
                offset_delta,
                locals,
                stack,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
