// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/// Upper bound on the number of entries in a `tableswitch.offsets` or
/// `lookupswitch.pairs` table. JVMS bounds bytecode at 65,535 bytes, so even
/// the largest legal switch table can only contain a few thousand entries —
/// 16,384 (64 KB of i32 offsets, or 128 KB of (i32, i32) pairs) is a safe
/// upper bound that rejects malicious headers like
/// `low = i32::MIN+1, high = i32::MAX` (which would otherwise pre-allocate
/// ~8.6 GB) while accepting any switch that can fit in a real method body.
///
/// Canonical value lives in [`crate::limits::MAX_SWITCH_ENTRIES`]; this
/// re-export is kept because downstream crates reference it by this path.
pub const MAX_SWITCH_ENTRIES: usize = crate::limits::MAX_SWITCH_ENTRIES;

/// Out-of-line payload of a `tableswitch`.
///
/// Held behind an `Arc` inside [`Instruction::Tableswitch`] so that the
/// `Instruction` enum stays a small fixed-size record with no owned `Vec` in
/// any variant. That is what lets a pre-decoded (quickened) instruction stream
/// be a flat array, and what makes executing a switch allocation-free: the
/// interpreter borrows the interned table instead of re-decoding (and
/// re-allocating) it on every execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSwitch {
    /// Branch offset taken when the key is outside `low..=high`.
    pub default: i32,
    /// Lowest key covered by `offsets`.
    pub low: i32,
    /// Highest key covered by `offsets`.
    pub high: i32,
    /// Branch offsets for keys `low..=high`, in order.
    pub offsets: Vec<i32>,
}

/// Out-of-line payload of a `lookupswitch`. See [`TableSwitch`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupSwitch {
    /// Branch offset taken when the key matches no pair.
    pub default: i32,
    /// `(key, branch offset)` pairs, sorted by key in the classfile.
    pub pairs: Vec<(i32, i32)>,
}

impl LookupSwitch {
    /// Branch offset for `key`, or [`Self::default`] when no pair matches.
    ///
    /// JVMS §6.5 (`lookupswitch`) requires the `match` values to appear "in
    /// increasing numerical order", so the ordinary case is a binary search —
    /// which matters because `lookupswitch` is what javac emits for the
    /// `hashCode()` arm of a string switch and for sparse `enum`/`int`
    /// switches, tables that routinely run to hundreds of entries. A linear
    /// scan makes every one of those a walk over the whole table.
    ///
    /// The linear fallback is not belt-and-braces, it is the correctness
    /// argument: this VM can run with verification skipped, and nothing else
    /// on the path proves the table is ordered. A binary-search *hit* is
    /// authoritative regardless of ordering (it only ever reports `Ok(i)` when
    /// `pairs[i].0 == key`), so the fallback is needed exactly for the
    /// unsorted-and-missed case, where it restores the old behaviour.
    #[inline]
    pub fn target(&self, key: i32) -> i32 {
        match self.pairs.binary_search_by_key(&key, |(k, _)| *k) {
            Ok(i) => self.pairs[i].1,
            Err(_) => self
                .pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, off)| *off)
                .unwrap_or(self.default),
        }
    }
}

/// A JVM bytecode instruction (JVM spec 6.5).
///
/// Each variant represents a single JVM instruction with its operands already decoded.
#[derive(Debug, Clone, PartialEq)]
#[allow(non_camel_case_types)]
pub enum Instruction {
    // Constants
    Nop,
    AconstNull,
    IconstM1,
    Iconst0,
    Iconst1,
    Iconst2,
    Iconst3,
    Iconst4,
    Iconst5,
    Lconst0,
    Lconst1,
    Fconst0,
    Fconst1,
    Fconst2,
    Dconst0,
    Dconst1,
    Bipush(i8),
    Sipush(i16),
    Ldc(u8),
    LdcW(u16),
    Ldc2W(u16),

    // Loads
    Iload(u16),
    Lload(u16),
    Fload(u16),
    Dload(u16),
    Aload(u16),
    Iaload,
    Laload,
    Faload,
    Daload,
    Aaload,
    Baload,
    Caload,
    Saload,

    // Stores
    Istore(u16),
    Lstore(u16),
    Fstore(u16),
    Dstore(u16),
    Astore(u16),
    Iastore,
    Lastore,
    Fastore,
    Dastore,
    Aastore,
    Bastore,
    Castore,
    Sastore,

    // Stack
    Pop,
    Pop2,
    Dup,
    DupX1,
    DupX2,
    Dup2,
    Dup2X1,
    Dup2X2,
    Swap,

    // Arithmetic
    Iadd,
    Ladd,
    Fadd,
    Dadd,
    Isub,
    Lsub,
    Fsub,
    Dsub,
    Imul,
    Lmul,
    Fmul,
    Dmul,
    Idiv,
    Ldiv,
    Fdiv,
    Ddiv,
    Irem,
    Lrem,
    Frem,
    Drem,
    Ineg,
    Lneg,
    Fneg,
    Dneg,
    Ishl,
    Lshl,
    Ishr,
    Lshr,
    Iushr,
    Lushr,
    Iand,
    Land,
    Ior,
    Lor,
    Ixor,
    Lxor,
    Iinc { index: u16, constant: i16 },

    // Conversions
    I2l,
    I2f,
    I2d,
    L2i,
    L2f,
    L2d,
    F2i,
    F2l,
    F2d,
    D2i,
    D2l,
    D2f,
    I2b,
    I2c,
    I2s,

    // Comparisons
    Lcmp,
    Fcmpl,
    Fcmpg,
    Dcmpl,
    Dcmpg,
    Ifeq(i16),
    Ifne(i16),
    Iflt(i16),
    Ifge(i16),
    Ifgt(i16),
    Ifle(i16),
    IfIcmpeq(i16),
    IfIcmpne(i16),
    IfIcmplt(i16),
    IfIcmpge(i16),
    IfIcmpgt(i16),
    IfIcmple(i16),
    IfAcmpeq(i16),
    IfAcmpne(i16),

    // Control
    Goto(i16),
    Jsr(i16),
    Ret(u16),
    Tableswitch(std::sync::Arc<TableSwitch>),
    Lookupswitch(std::sync::Arc<LookupSwitch>),
    Ireturn,
    Lreturn,
    Freturn,
    Dreturn,
    Areturn,
    Return,

    // References
    Getstatic(u16),
    Putstatic(u16),
    Getfield(u16),
    Putfield(u16),
    Invokevirtual(u16),
    Invokespecial(u16),
    Invokestatic(u16),
    Invokeinterface { index: u16, count: u8 },
    Invokedynamic(u16),
    New(u16),
    Newarray(u8),
    Anewarray(u16),
    Arraylength,
    Athrow,
    Checkcast(u16),
    Instanceof(u16),
    Monitorenter,
    Monitorexit,

    // Extended
    Wide, // Handled as a prefix during decoding — should not appear as a standalone instruction
    Multianewarray { index: u16, dimensions: u8 },
    Ifnull(i16),
    Ifnonnull(i16),
    GotoW(i32),
    JsrW(i32),
}

impl Instruction {
    // ── Bytecode decoder helpers ──

    fn byte_at(
        code: &[u8],
        index: usize,
    ) -> Result<u8, crate::class_reader_error::ClassReaderError> {
        code.get(index).copied().ok_or(
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { position: index },
        )
    }

    fn read_u8(
        code: &[u8],
        pc: &mut usize,
    ) -> Result<u8, crate::class_reader_error::ClassReaderError> {
        let val = Self::byte_at(code, *pc)?;
        *pc += 1;
        Ok(val)
    }

    fn read_i8(
        code: &[u8],
        pc: &mut usize,
    ) -> Result<i8, crate::class_reader_error::ClassReaderError> {
        Self::read_u8(code, pc).map(|v| v as i8)
    }

    fn read_u16(
        code: &[u8],
        pc: &mut usize,
    ) -> Result<u16, crate::class_reader_error::ClassReaderError> {
        // Audit fix (mirrors buffer.rs round-7 MED #7): compute the
        // high offset with `checked_add` before the bounds check via
        // `byte_at`, so a `*pc` near `usize::MAX` can't wrap to a small
        // index (silently succeeding) or panic in a debug build. On
        // overflow there can't be that many bytes left, so we surface
        // `UnexpectedEndOfData` — the same error `byte_at` returns OOB.
        let lo_idx = pc.checked_add(1).ok_or(
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { position: *pc },
        )?;
        let hi = Self::byte_at(code, *pc)? as u16;
        let lo = Self::byte_at(code, lo_idx)? as u16;
        *pc += 2;
        Ok((hi << 8) | lo)
    }

    fn read_i16(
        code: &[u8],
        pc: &mut usize,
    ) -> Result<i16, crate::class_reader_error::ClassReaderError> {
        Self::read_u16(code, pc).map(|v| v as i16)
    }

    fn read_i32(
        code: &[u8],
        pc: &mut usize,
    ) -> Result<i32, crate::class_reader_error::ClassReaderError> {
        // Audit fix (mirrors buffer.rs round-7 MED #7): validate the
        // highest offset (`*pc + 3`) with `checked_add` before any
        // `byte_at`, so a `*pc` near `usize::MAX` can't wrap to a small
        // index or panic in a debug build. On overflow there can't be
        // that many bytes left, so we surface `UnexpectedEndOfData` —
        // the same error `byte_at` returns when out of range.
        let b1_idx = pc.checked_add(1).ok_or(
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { position: *pc },
        )?;
        let b2_idx = pc.checked_add(2).ok_or(
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { position: *pc },
        )?;
        let b3_idx = pc.checked_add(3).ok_or(
            crate::class_reader_error::ClassReaderError::UnexpectedEndOfData { position: *pc },
        )?;
        let b0 = Self::byte_at(code, *pc)? as u32;
        let b1 = Self::byte_at(code, b1_idx)? as u32;
        let b2 = Self::byte_at(code, b2_idx)? as u32;
        let b3 = Self::byte_at(code, b3_idx)? as u32;
        *pc += 4;
        Ok(((b0 << 24) | (b1 << 16) | (b2 << 8) | b3) as i32)
    }

    /// Decode a single bytecode instruction from `code` at position `pc`.
    ///
    /// Returns the decoded instruction and the address of the next instruction.
    /// Reference: JVM spec §6.5 and the opcode table §7.
    pub fn decode(
        code: &[u8],
        pc: usize,
    ) -> Result<(Self, usize), crate::class_reader_error::ClassReaderError> {
        let opcode = Self::byte_at(code, pc)?;
        let mut next = pc + 1;

        let instr = match opcode {
            0x00 => Instruction::Nop,
            0x01 => Instruction::AconstNull,
            0x02 => Instruction::IconstM1,
            0x03 => Instruction::Iconst0,
            0x04 => Instruction::Iconst1,
            0x05 => Instruction::Iconst2,
            0x06 => Instruction::Iconst3,
            0x07 => Instruction::Iconst4,
            0x08 => Instruction::Iconst5,
            0x09 => Instruction::Lconst0,
            0x0a => Instruction::Lconst1,
            0x0b => Instruction::Fconst0,
            0x0c => Instruction::Fconst1,
            0x0d => Instruction::Fconst2,
            0x0e => Instruction::Dconst0,
            0x0f => Instruction::Dconst1,
            0x10 => Instruction::Bipush(Self::read_i8(code, &mut next)?),
            0x11 => Instruction::Sipush(Self::read_i16(code, &mut next)?),
            0x12 => Instruction::Ldc(Self::read_u8(code, &mut next)?),
            0x13 => Instruction::LdcW(Self::read_u16(code, &mut next)?),
            0x14 => Instruction::Ldc2W(Self::read_u16(code, &mut next)?),

            // Loads
            0x15 => Instruction::Iload(Self::read_u8(code, &mut next)? as u16),
            0x16 => Instruction::Lload(Self::read_u8(code, &mut next)? as u16),
            0x17 => Instruction::Fload(Self::read_u8(code, &mut next)? as u16),
            0x18 => Instruction::Dload(Self::read_u8(code, &mut next)? as u16),
            0x19 => Instruction::Aload(Self::read_u8(code, &mut next)? as u16),
            0x1a => Instruction::Iload(0),
            0x1b => Instruction::Iload(1),
            0x1c => Instruction::Iload(2),
            0x1d => Instruction::Iload(3),
            0x1e => Instruction::Lload(0),
            0x1f => Instruction::Lload(1),
            0x20 => Instruction::Lload(2),
            0x21 => Instruction::Lload(3),
            0x22 => Instruction::Fload(0),
            0x23 => Instruction::Fload(1),
            0x24 => Instruction::Fload(2),
            0x25 => Instruction::Fload(3),
            0x26 => Instruction::Dload(0),
            0x27 => Instruction::Dload(1),
            0x28 => Instruction::Dload(2),
            0x29 => Instruction::Dload(3),
            0x2a => Instruction::Aload(0),
            0x2b => Instruction::Aload(1),
            0x2c => Instruction::Aload(2),
            0x2d => Instruction::Aload(3),
            0x2e => Instruction::Iaload,
            0x2f => Instruction::Laload,
            0x30 => Instruction::Faload,
            0x31 => Instruction::Daload,
            0x32 => Instruction::Aaload,
            0x33 => Instruction::Baload,
            0x34 => Instruction::Caload,
            0x35 => Instruction::Saload,

            // Stores
            0x36 => Instruction::Istore(Self::read_u8(code, &mut next)? as u16),
            0x37 => Instruction::Lstore(Self::read_u8(code, &mut next)? as u16),
            0x38 => Instruction::Fstore(Self::read_u8(code, &mut next)? as u16),
            0x39 => Instruction::Dstore(Self::read_u8(code, &mut next)? as u16),
            0x3a => Instruction::Astore(Self::read_u8(code, &mut next)? as u16),
            0x3b => Instruction::Istore(0),
            0x3c => Instruction::Istore(1),
            0x3d => Instruction::Istore(2),
            0x3e => Instruction::Istore(3),
            0x3f => Instruction::Lstore(0),
            0x40 => Instruction::Lstore(1),
            0x41 => Instruction::Lstore(2),
            0x42 => Instruction::Lstore(3),
            0x43 => Instruction::Fstore(0),
            0x44 => Instruction::Fstore(1),
            0x45 => Instruction::Fstore(2),
            0x46 => Instruction::Fstore(3),
            0x47 => Instruction::Dstore(0),
            0x48 => Instruction::Dstore(1),
            0x49 => Instruction::Dstore(2),
            0x4a => Instruction::Dstore(3),
            0x4b => Instruction::Astore(0),
            0x4c => Instruction::Astore(1),
            0x4d => Instruction::Astore(2),
            0x4e => Instruction::Astore(3),
            0x4f => Instruction::Iastore,
            0x50 => Instruction::Lastore,
            0x51 => Instruction::Fastore,
            0x52 => Instruction::Dastore,
            0x53 => Instruction::Aastore,
            0x54 => Instruction::Bastore,
            0x55 => Instruction::Castore,
            0x56 => Instruction::Sastore,

            // Stack
            0x57 => Instruction::Pop,
            0x58 => Instruction::Pop2,
            0x59 => Instruction::Dup,
            0x5a => Instruction::DupX1,
            0x5b => Instruction::DupX2,
            0x5c => Instruction::Dup2,
            0x5d => Instruction::Dup2X1,
            0x5e => Instruction::Dup2X2,
            0x5f => Instruction::Swap,

            // Arithmetic
            0x60 => Instruction::Iadd,
            0x61 => Instruction::Ladd,
            0x62 => Instruction::Fadd,
            0x63 => Instruction::Dadd,
            0x64 => Instruction::Isub,
            0x65 => Instruction::Lsub,
            0x66 => Instruction::Fsub,
            0x67 => Instruction::Dsub,
            0x68 => Instruction::Imul,
            0x69 => Instruction::Lmul,
            0x6a => Instruction::Fmul,
            0x6b => Instruction::Dmul,
            0x6c => Instruction::Idiv,
            0x6d => Instruction::Ldiv,
            0x6e => Instruction::Fdiv,
            0x6f => Instruction::Ddiv,
            0x70 => Instruction::Irem,
            0x71 => Instruction::Lrem,
            0x72 => Instruction::Frem,
            0x73 => Instruction::Drem,
            0x74 => Instruction::Ineg,
            0x75 => Instruction::Lneg,
            0x76 => Instruction::Fneg,
            0x77 => Instruction::Dneg,
            0x78 => Instruction::Ishl,
            0x79 => Instruction::Lshl,
            0x7a => Instruction::Ishr,
            0x7b => Instruction::Lshr,
            0x7c => Instruction::Iushr,
            0x7d => Instruction::Lushr,
            0x7e => Instruction::Iand,
            0x7f => Instruction::Land,
            0x80 => Instruction::Ior,
            0x81 => Instruction::Lor,
            0x82 => Instruction::Ixor,
            0x83 => Instruction::Lxor,
            0x84 => {
                let index = Self::read_u8(code, &mut next)? as u16;
                let constant = Self::read_i8(code, &mut next)? as i16;
                Instruction::Iinc { index, constant }
            }

            // Conversions
            0x85 => Instruction::I2l,
            0x86 => Instruction::I2f,
            0x87 => Instruction::I2d,
            0x88 => Instruction::L2i,
            0x89 => Instruction::L2f,
            0x8a => Instruction::L2d,
            0x8b => Instruction::F2i,
            0x8c => Instruction::F2l,
            0x8d => Instruction::F2d,
            0x8e => Instruction::D2i,
            0x8f => Instruction::D2l,
            0x90 => Instruction::D2f,
            0x91 => Instruction::I2b,
            0x92 => Instruction::I2c,
            0x93 => Instruction::I2s,

            // Comparisons
            0x94 => Instruction::Lcmp,
            0x95 => Instruction::Fcmpl,
            0x96 => Instruction::Fcmpg,
            0x97 => Instruction::Dcmpl,
            0x98 => Instruction::Dcmpg,
            0x99 => Instruction::Ifeq(Self::read_i16(code, &mut next)?),
            0x9a => Instruction::Ifne(Self::read_i16(code, &mut next)?),
            0x9b => Instruction::Iflt(Self::read_i16(code, &mut next)?),
            0x9c => Instruction::Ifge(Self::read_i16(code, &mut next)?),
            0x9d => Instruction::Ifgt(Self::read_i16(code, &mut next)?),
            0x9e => Instruction::Ifle(Self::read_i16(code, &mut next)?),
            0x9f => Instruction::IfIcmpeq(Self::read_i16(code, &mut next)?),
            0xa0 => Instruction::IfIcmpne(Self::read_i16(code, &mut next)?),
            0xa1 => Instruction::IfIcmplt(Self::read_i16(code, &mut next)?),
            0xa2 => Instruction::IfIcmpge(Self::read_i16(code, &mut next)?),
            0xa3 => Instruction::IfIcmpgt(Self::read_i16(code, &mut next)?),
            0xa4 => Instruction::IfIcmple(Self::read_i16(code, &mut next)?),
            0xa5 => Instruction::IfAcmpeq(Self::read_i16(code, &mut next)?),
            0xa6 => Instruction::IfAcmpne(Self::read_i16(code, &mut next)?),

            // Control
            0xa7 => Instruction::Goto(Self::read_i16(code, &mut next)?),
            0xa8 => Instruction::Jsr(Self::read_i16(code, &mut next)?),
            0xa9 => Instruction::Ret(Self::read_u8(code, &mut next)? as u16),
            0xaa => {
                // tableswitch — requires 4-byte alignment padding
                let base_pc = pc;
                // Skip padding to align to 4-byte boundary
                while next % 4 != 0 {
                    next += 1;
                }
                let default = Self::read_i32(code, &mut next)?;
                let low = Self::read_i32(code, &mut next)?;
                let high = Self::read_i32(code, &mut next)?;
                if high < low {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!("tableswitch high ({high}) < low ({low})"),
                        },
                    );
                }
                let count_i64 = high as i64 - low as i64 + 1;
                if count_i64 > MAX_SWITCH_ENTRIES as i64 {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!(
                                "tableswitch entry count {count_i64} exceeds maximum {MAX_SWITCH_ENTRIES}"
                            ),
                        },
                    );
                }
                let count = count_i64 as usize;
                // Bound the reservation by what the method actually holds. A
                // malformed `high` within `MAX_SWITCH_ENTRIES` can otherwise
                // reserve up to 64 KB that the very next read fails on. Those
                // reads already return `UnexpectedEndOfData`, so this only
                // raises the identical error variant earlier (the reported
                // position moves to the head of the table).
                if count.saturating_mul(4) > code.len().saturating_sub(next) {
                    return Err(
                        crate::class_reader_error::ClassReaderError::UnexpectedEndOfData {
                            position: next,
                        },
                    );
                }
                let mut offsets = Vec::with_capacity(count);
                for _ in 0..count {
                    offsets.push(Self::read_i32(code, &mut next)?);
                }
                let _base_pc = base_pc; // retained for future offset validation
                Instruction::Tableswitch(std::sync::Arc::new(TableSwitch {
                    default,
                    low,
                    high,
                    offsets,
                }))
            }
            0xab => {
                // lookupswitch — requires 4-byte alignment padding
                while next % 4 != 0 {
                    next += 1;
                }
                let default = Self::read_i32(code, &mut next)?;
                let npairs_raw = Self::read_i32(code, &mut next)?;
                if npairs_raw < 0 {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!("lookupswitch npairs is negative: {npairs_raw}"),
                        },
                    );
                }
                if npairs_raw as i64 > MAX_SWITCH_ENTRIES as i64 {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!(
                                "lookupswitch npairs {npairs_raw} exceeds maximum {MAX_SWITCH_ENTRIES}"
                            ),
                        },
                    );
                }
                let npairs = npairs_raw as usize;
                // See the `tableswitch` note above: bound the reservation
                // (128 KB worst case here) by the bytes actually available.
                if npairs.saturating_mul(8) > code.len().saturating_sub(next) {
                    return Err(
                        crate::class_reader_error::ClassReaderError::UnexpectedEndOfData {
                            position: next,
                        },
                    );
                }
                let mut pairs = Vec::with_capacity(npairs);
                for _ in 0..npairs {
                    let key = Self::read_i32(code, &mut next)?;
                    let offset = Self::read_i32(code, &mut next)?;
                    pairs.push((key, offset));
                }
                Instruction::Lookupswitch(std::sync::Arc::new(LookupSwitch { default, pairs }))
            }
            0xac => Instruction::Ireturn,
            0xad => Instruction::Lreturn,
            0xae => Instruction::Freturn,
            0xaf => Instruction::Dreturn,
            0xb0 => Instruction::Areturn,
            0xb1 => Instruction::Return,

            // References
            0xb2 => Instruction::Getstatic(Self::read_u16(code, &mut next)?),
            0xb3 => Instruction::Putstatic(Self::read_u16(code, &mut next)?),
            0xb4 => Instruction::Getfield(Self::read_u16(code, &mut next)?),
            0xb5 => Instruction::Putfield(Self::read_u16(code, &mut next)?),
            0xb6 => Instruction::Invokevirtual(Self::read_u16(code, &mut next)?),
            0xb7 => Instruction::Invokespecial(Self::read_u16(code, &mut next)?),
            0xb8 => Instruction::Invokestatic(Self::read_u16(code, &mut next)?),
            0xb9 => {
                let index = Self::read_u16(code, &mut next)?;
                let count = Self::read_u8(code, &mut next)?;
                let reserved = Self::read_u8(code, &mut next)?;
                if reserved != 0 {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!(
                                "invokeinterface reserved byte must be 0, got {reserved}"
                            ),
                        },
                    );
                }
                Instruction::Invokeinterface { index, count }
            }
            0xba => {
                let index = Self::read_u16(code, &mut next)?;
                let reserved1 = Self::read_u8(code, &mut next)?;
                let reserved2 = Self::read_u8(code, &mut next)?;
                if reserved1 != 0 || reserved2 != 0 {
                    return Err(
                        crate::class_reader_error::ClassReaderError::InvalidClassData {
                            message: format!(
                            "invokedynamic reserved bytes must be 0, got {reserved1}, {reserved2}"
                        ),
                        },
                    );
                }
                Instruction::Invokedynamic(index)
            }
            0xbb => Instruction::New(Self::read_u16(code, &mut next)?),
            0xbc => Instruction::Newarray(Self::read_u8(code, &mut next)?),
            0xbd => Instruction::Anewarray(Self::read_u16(code, &mut next)?),
            0xbe => Instruction::Arraylength,
            0xbf => Instruction::Athrow,
            0xc0 => Instruction::Checkcast(Self::read_u16(code, &mut next)?),
            0xc1 => Instruction::Instanceof(Self::read_u16(code, &mut next)?),
            0xc2 => Instruction::Monitorenter,
            0xc3 => Instruction::Monitorexit,

            // Extended
            0xc4 => {
                // wide prefix — extends the following instruction's index to u16
                let wide_opcode = Self::read_u8(code, &mut next)?;
                match wide_opcode {
                    0x15 => Instruction::Iload(Self::read_u16(code, &mut next)?),
                    0x16 => Instruction::Lload(Self::read_u16(code, &mut next)?),
                    0x17 => Instruction::Fload(Self::read_u16(code, &mut next)?),
                    0x18 => Instruction::Dload(Self::read_u16(code, &mut next)?),
                    0x19 => Instruction::Aload(Self::read_u16(code, &mut next)?),
                    0x36 => Instruction::Istore(Self::read_u16(code, &mut next)?),
                    0x37 => Instruction::Lstore(Self::read_u16(code, &mut next)?),
                    0x38 => Instruction::Fstore(Self::read_u16(code, &mut next)?),
                    0x39 => Instruction::Dstore(Self::read_u16(code, &mut next)?),
                    0x3a => Instruction::Astore(Self::read_u16(code, &mut next)?),
                    0x84 => {
                        let index = Self::read_u16(code, &mut next)?;
                        let constant = Self::read_i16(code, &mut next)?;
                        Instruction::Iinc { index, constant }
                    }
                    0xa9 => Instruction::Ret(Self::read_u16(code, &mut next)?),
                    _ => {
                        return Err(
                            crate::class_reader_error::ClassReaderError::InvalidClassData {
                                message: format!("invalid wide opcode: 0x{wide_opcode:02x}"),
                            },
                        );
                    }
                }
            }
            0xc5 => {
                let index = Self::read_u16(code, &mut next)?;
                let dimensions = Self::read_u8(code, &mut next)?;
                Instruction::Multianewarray { index, dimensions }
            }
            0xc6 => Instruction::Ifnull(Self::read_i16(code, &mut next)?),
            0xc7 => Instruction::Ifnonnull(Self::read_i16(code, &mut next)?),
            0xc8 => Instruction::GotoW(Self::read_i32(code, &mut next)?),
            0xc9 => Instruction::JsrW(Self::read_i32(code, &mut next)?),

            _ => {
                return Err(
                    crate::class_reader_error::ClassReaderError::InvalidClassData {
                        message: format!("unknown opcode: 0x{opcode:02x} at pc {pc}"),
                    },
                );
            }
        };

        Ok((instr, next))
    }

    /// Returns true if this instruction is a branch (conditional or unconditional).
    pub fn is_branch(&self) -> bool {
        matches!(
            self,
            Instruction::Goto(_)
                | Instruction::GotoW(_)
                | Instruction::Jsr(_)
                | Instruction::JsrW(_)
                | Instruction::Ifeq(_)
                | Instruction::Ifne(_)
                | Instruction::Iflt(_)
                | Instruction::Ifge(_)
                | Instruction::Ifgt(_)
                | Instruction::Ifle(_)
                | Instruction::IfIcmpeq(_)
                | Instruction::IfIcmpne(_)
                | Instruction::IfIcmplt(_)
                | Instruction::IfIcmpge(_)
                | Instruction::IfIcmpgt(_)
                | Instruction::IfIcmple(_)
                | Instruction::IfAcmpeq(_)
                | Instruction::IfAcmpne(_)
                | Instruction::Ifnull(_)
                | Instruction::Ifnonnull(_)
                | Instruction::Tableswitch(_)
                | Instruction::Lookupswitch(_)
        )
    }

    /// Returns true if this instruction is a return instruction.
    pub fn is_return(&self) -> bool {
        matches!(
            self,
            Instruction::Return
                | Instruction::Ireturn
                | Instruction::Lreturn
                | Instruction::Freturn
                | Instruction::Dreturn
                | Instruction::Areturn
        )
    }
}

#[cfg(test)]
mod tests;
