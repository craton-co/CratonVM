// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ARM64 (AArch64) code emitter — Phase 7.2.
//!
//! This module contains the ARM64 instruction encoder and code generator,
//! mirroring `x64.rs` for the AArch64 instruction set. The target-independent
//! analysis passes (JIT scan, IR, optimization, scheduling) are shared;
//! only the final machine code emission differs per architecture.
//!
//! ## ARM64 Register Convention
//!
//! | Register | Role |
//! |----------|------|
//! | X0–X7 | Arguments / return values |
//! | X8 | Indirect result location |
//! | X9–X15 | Caller-saved temporaries |
//! | X16–X17 | Intra-procedure-call scratch (IP0/IP1) |
//! | X18 | Platform register (reserved on macOS) |
//! | X19–X28 | Callee-saved |
//! | X29 (FP) | Frame pointer |
//! | X30 (LR) | Link register (return address) |
//! | SP | Stack pointer |
//!
//! ## Key Differences from x86-64
//!
//! - Fixed-width 32-bit instructions (vs variable-length x86).
//! - No flags register for comparisons — use conditional branch forms directly.
//! - Separate instruction and data caches — must flush icache after writing code.
//! - W^X on Apple Silicon — handled by `platform::make_executable`.
//! - NEON (128-bit SIMD) instead of AVX2 (256-bit).

// ---------------------------------------------------------------------------
// Register definitions
// ---------------------------------------------------------------------------

/// An ARM64 general-purpose register (X0–X30), SP, or XZR.
///
/// The encoding value matches the 5-bit field used in ARM64 instructions.
/// SP and XZR share encoding 31 but are distinguished by context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum Reg {
    X0 = 0,
    X1 = 1,
    X2 = 2,
    X3 = 3,
    X4 = 4,
    X5 = 5,
    X6 = 6,
    X7 = 7,
    X8 = 8,
    X9 = 9,
    X10 = 10,
    X11 = 11,
    X12 = 12,
    X13 = 13,
    X14 = 14,
    X15 = 15,
    X16 = 16,
    X17 = 17,
    X18 = 18,
    X19 = 19,
    X20 = 20,
    X21 = 21,
    X22 = 22,
    X23 = 23,
    X24 = 24,
    X25 = 25,
    X26 = 26,
    X27 = 27,
    X28 = 28,
    X29 = 29, // FP
    X30 = 30, // LR
    /// Stack pointer (encoding 31, context-dependent).
    SP = 31,
}

/// Alias: XZR shares encoding 31 with SP but used in different contexts.
#[allow(dead_code)]
pub const XZR: Reg = Reg::SP; // encoding 31 — context determines SP vs XZR

// Convenience aliases
#[allow(dead_code)]
pub const FP: Reg = Reg::X29;
#[allow(dead_code)]
pub const LR: Reg = Reg::X30;

impl Reg {
    /// Return the 5-bit encoding value.
    #[inline]
    pub fn enc(self) -> u32 {
        self as u32
    }

    /// Construct a `Reg` from a raw 0..=31 encoding without using `transmute`.
    ///
    /// Returns `None` if `n` is out of range. Used by the ARM64 backend to
    /// convert register-allocator outputs (`Arm64Register`) into emitter
    /// `Reg` values without panicking on regalloc bugs — the JIT contract is
    /// "never panic in production; bail to interpreter instead" (C11).
    #[inline]
    pub fn from_u8(n: u8) -> Option<Reg> {
        match n {
            0 => Some(Reg::X0),
            1 => Some(Reg::X1),
            2 => Some(Reg::X2),
            3 => Some(Reg::X3),
            4 => Some(Reg::X4),
            5 => Some(Reg::X5),
            6 => Some(Reg::X6),
            7 => Some(Reg::X7),
            8 => Some(Reg::X8),
            9 => Some(Reg::X9),
            10 => Some(Reg::X10),
            11 => Some(Reg::X11),
            12 => Some(Reg::X12),
            13 => Some(Reg::X13),
            14 => Some(Reg::X14),
            15 => Some(Reg::X15),
            16 => Some(Reg::X16),
            17 => Some(Reg::X17),
            18 => Some(Reg::X18),
            19 => Some(Reg::X19),
            20 => Some(Reg::X20),
            21 => Some(Reg::X21),
            22 => Some(Reg::X22),
            23 => Some(Reg::X23),
            24 => Some(Reg::X24),
            25 => Some(Reg::X25),
            26 => Some(Reg::X26),
            27 => Some(Reg::X27),
            28 => Some(Reg::X28),
            29 => Some(Reg::X29),
            30 => Some(Reg::X30),
            31 => Some(Reg::SP),
            _ => None,
        }
    }
}

/// An ARM64 FP/SIMD register (D0–D31 for double, S0–S31 for single,
/// V0–V31 for NEON vector).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum FpReg {
    D0 = 0,
    D1 = 1,
    D2 = 2,
    D3 = 3,
    D4 = 4,
    D5 = 5,
    D6 = 6,
    D7 = 7,
    D8 = 8,
    D9 = 9,
    D10 = 10,
    D11 = 11,
    D12 = 12,
    D13 = 13,
    D14 = 14,
    D15 = 15,
    D16 = 16,
    D17 = 17,
    D18 = 18,
    D19 = 19,
    D20 = 20,
    D21 = 21,
    D22 = 22,
    D23 = 23,
    D24 = 24,
    D25 = 25,
    D26 = 26,
    D27 = 27,
    D28 = 28,
    D29 = 29,
    D30 = 30,
    D31 = 31,
}

impl FpReg {
    #[inline]
    pub fn enc(self) -> u32 {
        self as u32
    }

    /// Construct an `FpReg` from a raw 0..=31 encoding without using `transmute`.
    ///
    /// Returns `None` if `n` is out of range. Used by the ARM64 backend to
    /// convert register-allocator outputs (`Arm64Register` with the FP-bias
    /// stripped) into emitter `FpReg` values without panicking on regalloc
    /// bugs — the JIT contract is "never panic in production; bail to
    /// interpreter instead" (C11).
    #[inline]
    pub fn from_u8(n: u8) -> Option<FpReg> {
        match n {
            0 => Some(FpReg::D0),
            1 => Some(FpReg::D1),
            2 => Some(FpReg::D2),
            3 => Some(FpReg::D3),
            4 => Some(FpReg::D4),
            5 => Some(FpReg::D5),
            6 => Some(FpReg::D6),
            7 => Some(FpReg::D7),
            8 => Some(FpReg::D8),
            9 => Some(FpReg::D9),
            10 => Some(FpReg::D10),
            11 => Some(FpReg::D11),
            12 => Some(FpReg::D12),
            13 => Some(FpReg::D13),
            14 => Some(FpReg::D14),
            15 => Some(FpReg::D15),
            16 => Some(FpReg::D16),
            17 => Some(FpReg::D17),
            18 => Some(FpReg::D18),
            19 => Some(FpReg::D19),
            20 => Some(FpReg::D20),
            21 => Some(FpReg::D21),
            22 => Some(FpReg::D22),
            23 => Some(FpReg::D23),
            24 => Some(FpReg::D24),
            25 => Some(FpReg::D25),
            26 => Some(FpReg::D26),
            27 => Some(FpReg::D27),
            28 => Some(FpReg::D28),
            29 => Some(FpReg::D29),
            30 => Some(FpReg::D30),
            31 => Some(FpReg::D31),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Condition codes
// ---------------------------------------------------------------------------

/// ARM64 condition codes matching the 4-bit encoding in the ISA.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
pub enum Cond {
    /// Equal (Z == 1)
    EQ = 0b0000,
    /// Not equal (Z == 0)
    NE = 0b0001,
    /// Carry set / unsigned higher or same (C == 1)
    HS = 0b0010,
    /// Carry clear / unsigned lower (C == 0)
    LO = 0b0011,
    /// Minus / negative (N == 1)
    MI = 0b0100,
    /// Plus / positive or zero (N == 0)
    PL = 0b0101,
    /// Overflow (V == 1)
    VS = 0b0110,
    /// No overflow (V == 0)
    VC = 0b0111,
    /// Unsigned higher (C == 1 && Z == 0)
    HI = 0b1000,
    /// Unsigned lower or same (C == 0 || Z == 1)
    LS = 0b1001,
    /// Signed greater or equal (N == V)
    GE = 0b1010,
    /// Signed less than (N != V)
    LT = 0b1011,
    /// Signed greater than (Z == 0 && N == V)
    GT = 0b1100,
    /// Signed less or equal (Z == 1 || N != V)
    LE = 0b1101,
    /// Always (unconditional)
    AL = 0b1110,
    /// Never (unconditional, rarely used)
    NV = 0b1111,
}

impl Cond {
    #[inline]
    pub fn enc(self) -> u32 {
        self as u32
    }
}

// Aliases
#[allow(dead_code)]
pub const CS: Cond = Cond::HS;
#[allow(dead_code)]
pub const CC: Cond = Cond::LO;

// ---------------------------------------------------------------------------
// NEON arrangement specifiers
// ---------------------------------------------------------------------------

/// NEON vector arrangement for LD1/ST1/vector-ALU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum VectorArrangement {
    S4,  // 4xS (i32x4 / f32x4)
    D2,  // 2xD (i64x2 / f64x2)
    H8,  // 8xH (i16x8)
    B16, // 16xB (i8x16)
}

// ---------------------------------------------------------------------------
// Emitter
// ---------------------------------------------------------------------------

/// ARM64 instruction emitter. Appends encoded instructions to an internal
/// byte buffer that can later be copied into executable memory.
pub struct Aarch64Emitter {
    code: Vec<u8>,
    /// Sticky flag set when a branch/patch target offset does not fit the
    /// instruction's immediate field (e.g. a B/BL beyond ±128 MB, a
    /// conditional/CBZ branch beyond ±1 MB, or a TBZ beyond ±32 KB).
    ///
    /// AArch64 branch immediates are masked into a fixed-width field, so an
    /// out-of-range offset would silently wrap to a wrong target and corrupt
    /// control flow. Rather than miscompile, the encoder/patcher records the
    /// overflow here (in addition to a hard `debug_assert!`) and the codegen
    /// driver bails to the interpreter — mirroring the x86-64 backend's
    /// `ExecutableBuffer::overflowed` rel8/rel32 `DisplacementOverflow`
    /// handling (`jit/src/lib.rs`).
    overflow: bool,
}

#[allow(dead_code)]
impl Aarch64Emitter {
    /// Create a new emitter with an empty code buffer.
    pub fn new() -> Self {
        Self {
            code: Vec::with_capacity(1024),
            overflow: false,
        }
    }

    /// Return a reference to the generated machine code.
    pub fn code(&self) -> &[u8] {
        &self.code
    }

    /// Whether any branch encoder or patch helper has seen an out-of-range
    /// offset since this emitter was created. When `true`, the generated code
    /// contains at least one truncated branch target and MUST NOT be executed;
    /// the codegen driver should discard it and fall back to the interpreter.
    ///
    /// The flag is sticky: once set it stays set, so a single check after all
    /// emission/patching is sufficient.
    ///
    /// The production reader is `aarch64_backend::emit_machine_code`, which
    /// checks it after the branch/literal patch loops and returns `None`
    /// (bail to the interpreter) when set. That check was missing until
    /// 2026-07-26 — this flag existed, was documented as the release-build
    /// protection, and had no caller outside these tests, so a release
    /// `aarch64` build emitted truncated branches as executable code.
    #[inline]
    pub fn overflowed(&self) -> bool {
        self.overflow
    }

    /// Record a branch-offset range overflow: trips a hard `debug_assert!`
    /// (loud failure in debug builds / tests) and sets the sticky
    /// [`overflowed`](Self::overflowed) flag so release builds bail to the
    /// interpreter instead of executing a wrong-target branch.
    #[inline]
    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    fn mark_branch_overflow(&mut self, kind: &str, delta: i64) {
        debug_assert!(
            false,
            "aarch64 {kind} branch offset {delta} out of range — would truncate to a wrong target"
        );
        self.overflow = true;
    }

    /// Current offset (byte position) in the code buffer.
    pub fn offset(&self) -> usize {
        self.code.len()
    }

    /// Append a 32-bit little-endian instruction word.
    pub fn emit_u32(&mut self, inst: u32) {
        self.code.extend_from_slice(&inst.to_le_bytes());
    }

    // -----------------------------------------------------------------------
    // Arithmetic (register forms) — 64-bit (X) and 32-bit (W)
    // -----------------------------------------------------------------------

    // Data-processing (shifted register) format:
    //   sf(1) opc(2) 01011 shift(2) 0 Rm(5) imm6(6) Rn(5) Rd(5)

    /// ADD Xd, Xn, Xm  (64-bit)
    pub fn add(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.add_shifted(rd, rn, rm, ShiftType::LSL, 0, true);
    }

    /// ADD Wd, Wn, Wm  (32-bit)
    pub fn add_w(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.add_shifted(rd, rn, rm, ShiftType::LSL, 0, false);
    }

    /// ADD with optional shift, parameterized by sf (true = 64-bit).
    fn add_shifted(&mut self, rd: Reg, rn: Reg, rm: Reg, shift: ShiftType, amount: u8, sf: bool) {
        let inst = dp_shifted_reg(sf, 0b00, 0b01011, shift, 0, rm, amount, rn, rd);
        self.emit_u32(inst);
    }

    /// ADDS Xd, Xn, Xm  (64-bit, sets flags)
    pub fn adds(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = dp_shifted_reg(true, 0b01, 0b01011, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    /// SUB Xd, Xn, Xm  (64-bit)
    pub fn sub(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.sub_shifted(rd, rn, rm, ShiftType::LSL, 0, true);
    }

    /// SUB Wd, Wn, Wm  (32-bit)
    pub fn sub_w(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.sub_shifted(rd, rn, rm, ShiftType::LSL, 0, false);
    }

    fn sub_shifted(&mut self, rd: Reg, rn: Reg, rm: Reg, shift: ShiftType, amount: u8, sf: bool) {
        let inst = dp_shifted_reg(sf, 0b10, 0b01011, shift, 0, rm, amount, rn, rd);
        self.emit_u32(inst);
    }

    /// SUBS Xd, Xn, Xm  (64-bit, sets flags)
    pub fn subs(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = dp_shifted_reg(true, 0b11, 0b01011, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    // -- ADD/SUB immediate --
    // sf(1) op(1) S(1) 100010 sh(1) imm12(12) Rn(5) Rd(5)

    /// ADD Xd, Xn, #imm12  (64-bit immediate)
    pub fn add_imm(&mut self, rd: Reg, rn: Reg, imm12: u16, shift12: bool) {
        let inst = addsub_imm(true, false, false, imm12, shift12, rn, rd);
        self.emit_u32(inst);
    }

    /// ADD Wd, Wn, #imm12  (32-bit immediate)
    pub fn add_imm_w(&mut self, rd: Reg, rn: Reg, imm12: u16, shift12: bool) {
        let inst = addsub_imm(false, false, false, imm12, shift12, rn, rd);
        self.emit_u32(inst);
    }

    /// SUB Xd, Xn, #imm12  (64-bit immediate)
    pub fn sub_imm(&mut self, rd: Reg, rn: Reg, imm12: u16, shift12: bool) {
        let inst = addsub_imm(true, true, false, imm12, shift12, rn, rd);
        self.emit_u32(inst);
    }

    /// SUB Wd, Wn, #imm12  (32-bit immediate)
    pub fn sub_imm_w(&mut self, rd: Reg, rn: Reg, imm12: u16, shift12: bool) {
        let inst = addsub_imm(false, true, false, imm12, shift12, rn, rd);
        self.emit_u32(inst);
    }

    // -- MUL / MADD / MSUB --
    // Data-processing (3 source): sf(1) 00 11011 000 Rm(5) o0(1) Ra(5) Rn(5) Rd(5)

    /// MUL Xd, Xn, Xm  (alias for MADD Xd, Xn, Xm, XZR)
    pub fn mul(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.madd(rd, rn, rm, XZR);
    }

    /// MUL Wd, Wn, Wm  (32-bit)
    pub fn mul_w(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        self.madd_w(rd, rn, rm, XZR);
    }

    /// MADD Xd, Xn, Xm, Xa  (Xd = Xa + Xn*Xm)
    pub fn madd(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        let inst = dp_3src(true, 0b000, rm, 0, ra, rn, rd);
        self.emit_u32(inst);
    }

    /// MADD Wd, Wn, Wm, Wa  (32-bit)
    pub fn madd_w(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        let inst = dp_3src(false, 0b000, rm, 0, ra, rn, rd);
        self.emit_u32(inst);
    }

    /// MSUB Xd, Xn, Xm, Xa  (Xd = Xa - Xn*Xm)
    pub fn msub(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        let inst = dp_3src(true, 0b000, rm, 1, ra, rn, rd);
        self.emit_u32(inst);
    }

    /// MSUB Wd, Wn, Wm, Wa  (32-bit)
    pub fn msub_w(&mut self, rd: Reg, rn: Reg, rm: Reg, ra: Reg) {
        let inst = dp_3src(false, 0b000, rm, 1, ra, rn, rd);
        self.emit_u32(inst);
    }

    /// SDIV Xd, Xn, Xm  (64-bit)
    pub fn sdiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        // sf=1, op=0b00, S=0, opcode2=0b000110 (11 = SDIV)
        // 1 0 0 11010110 Rm(5) 00001 1 Rn(5) Rd(5)
        let inst = dp_2src(true, rm, 0b000011, rn, rd);
        self.emit_u32(inst);
    }

    /// SDIV Wd, Wn, Wm  (32-bit)
    pub fn sdiv_w(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = dp_2src(false, rm, 0b000011, rn, rd);
        self.emit_u32(inst);
    }

    /// UDIV Xd, Xn, Xm  (64-bit)
    pub fn udiv(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = dp_2src(true, rm, 0b000010, rn, rd);
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // Logic (shifted register)
    // -----------------------------------------------------------------------
    // sf(1) opc(2) 01010 shift(2) N(1) Rm(5) imm6(6) Rn(5) Rd(5)

    /// AND Xd, Xn, Xm
    pub fn and(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = logic_shifted(true, 0b00, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    /// ORR Xd, Xn, Xm
    pub fn orr(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = logic_shifted(true, 0b01, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    /// EOR Xd, Xn, Xm
    pub fn eor(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = logic_shifted(true, 0b10, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    /// ANDS Xd, Xn, Xm  (AND + set flags)
    pub fn ands(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        let inst = logic_shifted(true, 0b11, ShiftType::LSL, 0, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    /// BIC Xd, Xn, Xm  (AND NOT)
    pub fn bic(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        // BIC = opc=00, N=1
        let inst = logic_shifted(true, 0b00, ShiftType::LSL, 1, rm, 0, rn, rd);
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // Shift (register) — aliases of data-processing (2 source)
    // -----------------------------------------------------------------------

    /// LSL Xd, Xn, Xm  (logical shift left, variable)
    pub fn lsl(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        // LSLV: opcode = 0b001000
        let inst = dp_2src(true, rm, 0b001000, rn, rd);
        self.emit_u32(inst);
    }

    /// LSR Xd, Xn, Xm  (logical shift right, variable)
    pub fn lsr(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        // LSRV: opcode = 0b001001
        let inst = dp_2src(true, rm, 0b001001, rn, rd);
        self.emit_u32(inst);
    }

    /// ASR Xd, Xn, Xm  (arithmetic shift right, variable)
    pub fn asr(&mut self, rd: Reg, rn: Reg, rm: Reg) {
        // ASRV: opcode = 0b001010
        let inst = dp_2src(true, rm, 0b001010, rn, rd);
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // Compare (aliases)
    // -----------------------------------------------------------------------

    /// CMP Xn, Xm  (alias for SUBS XZR, Xn, Xm)
    pub fn cmp(&mut self, rn: Reg, rm: Reg) {
        self.subs(XZR, rn, rm);
    }

    /// CMP Xn, #imm12
    pub fn cmp_imm(&mut self, rn: Reg, imm12: u16) {
        let inst = addsub_imm(true, true, true, imm12, false, rn, XZR);
        self.emit_u32(inst);
    }

    /// CMN Xn, Xm  (alias for ADDS XZR, Xn, Xm)
    pub fn cmn(&mut self, rn: Reg, rm: Reg) {
        self.adds(XZR, rn, rm);
    }

    /// TST Xn, Xm  (alias for ANDS XZR, Xn, Xm)
    pub fn tst(&mut self, rn: Reg, rm: Reg) {
        self.ands(XZR, rn, rm);
    }

    // -----------------------------------------------------------------------
    // Move (register + immediate)
    // -----------------------------------------------------------------------

    /// MOV Xd, Xm  (alias for ORR Xd, XZR, Xm)
    pub fn mov(&mut self, rd: Reg, rm: Reg) {
        self.orr(rd, XZR, rm);
    }

    /// MOVZ Xd, #imm16, LSL #shift  (move wide with zero)
    /// `shift` must be 0, 16, 32, or 48.
    pub fn movz(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(true, 0b10, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// MOVZ Wd, #imm16, LSL #shift  (32-bit)
    /// `shift` must be 0 or 16.
    pub fn movz_w(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(false, 0b10, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// MOVK Xd, #imm16, LSL #shift  (move wide with keep)
    pub fn movk(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(true, 0b11, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// MOVK Wd, #imm16, LSL #shift  (32-bit)
    pub fn movk_w(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(false, 0b11, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// MOVN Xd, #imm16, LSL #shift  (move wide with NOT)
    pub fn movn(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(true, 0b00, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// MOVN Wd, #imm16, LSL #shift  (32-bit)
    pub fn movn_w(&mut self, rd: Reg, imm16: u16, shift: u8) {
        let inst = move_wide(false, 0b00, shift, imm16, rd);
        self.emit_u32(inst);
    }

    /// Move an arbitrary 64-bit immediate into Xd using MOVZ + up to 3 MOVK.
    /// Generates the shortest possible sequence (1–4 instructions).
    pub fn mov_imm64(&mut self, rd: Reg, imm: u64) {
        let chunks: [u16; 4] = [
            (imm & 0xFFFF) as u16,
            ((imm >> 16) & 0xFFFF) as u16,
            ((imm >> 32) & 0xFFFF) as u16,
            ((imm >> 48) & 0xFFFF) as u16,
        ];

        let zero_count = chunks.iter().filter(|&&c| c == 0).count();
        let ffff_count = chunks.iter().filter(|&&c| c == 0xFFFF).count();

        if ffff_count > zero_count {
            // MOVN strategy: more efficient when most halfwords are 0xFFFF.
            // MOVN Xd, #inv_chunk, LSL#(first_pos) sets the register to the
            // bitwise NOT of (inv_chunk << first_pos), i.e. all 1s except
            // the chosen halfword. Then MOVK patches remaining non-0xFFFF chunks.
            let not_imm = !imm;
            let not_chunks: [u16; 4] = [
                (not_imm & 0xFFFF) as u16,
                ((not_imm >> 16) & 0xFFFF) as u16,
                ((not_imm >> 32) & 0xFFFF) as u16,
                ((not_imm >> 48) & 0xFFFF) as u16,
            ];

            let mut did_movn = false;
            for (i, (&chunk, &nc)) in chunks.iter().zip(not_chunks.iter()).enumerate() {
                if chunk == 0xFFFF {
                    continue;
                }
                if !did_movn {
                    self.movn(rd, nc, (i as u8) * 16);
                    did_movn = true;
                } else {
                    self.movk(rd, chunk, (i as u8) * 16);
                }
            }
            if !did_movn {
                // All chunks are 0xFFFF → value is u64::MAX (-1).
                self.movn(rd, 0, 0);
            }
        } else {
            // MOVZ strategy: start with first non-zero (or zero if all zero),
            // then MOVK remaining non-zero chunks.
            let mut first = true;
            for (i, &chunk) in chunks.iter().enumerate() {
                if chunk == 0 && !first {
                    continue;
                }
                if first {
                    self.movz(rd, chunk, (i as u8) * 16);
                    first = false;
                } else {
                    self.movk(rd, chunk, (i as u8) * 16);
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Load / Store
    // -----------------------------------------------------------------------

    /// LDR Xt, [Xn, Xm]  (register offset, 64-bit)
    pub fn ldr_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        // size=11, V=0, opc=01, 1 Rm option(011=LSL) S(0) 10 Rn Rt
        let inst = ldst_reg_offset(0b11, 0, 0b01, rm, 0b011, 0, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDR Xt, [Xn, #imm]  (unsigned offset, 64-bit)
    /// `imm` is in bytes and must be a multiple of 8, range 0..32760.
    pub fn ldr_imm(&mut self, rt: Reg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b11, 0, 0b01, imm >> 3, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDR Wt, [Xn, #imm]  (unsigned offset, 32-bit)
    /// `imm` is in bytes and must be a multiple of 4.
    pub fn ldr_imm_w(&mut self, rt: Reg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b10, 0, 0b01, imm >> 2, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDRB Wt, [Xn, #imm]  (unsigned offset, ZERO-EXTENDING BYTE load).
    ///
    /// `imm` is a byte count and is NOT scaled -- the byte form's `imm12` is in
    /// units of 1. The width matters: the safepoint flag this exists for is a
    /// Rust `AtomicBool`, i.e. exactly ONE byte, and the `GcBarrier` fields
    /// that follow it in memory (`gc_generation: AtomicU64`, ...) are not zero.
    /// Reading it with the 64-bit `ldr_imm` would fold those bytes into the
    /// test and make a safepoint poll fire whenever the generation counter is
    /// nonzero -- i.e. always, after the first collection.
    pub fn ldrb_imm(&mut self, rt: Reg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b00, 0, 0b01, imm, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Xt, [Xn, Xm]  (register offset, 64-bit)
    pub fn str_reg(&mut self, rt: Reg, rn: Reg, rm: Reg) {
        let inst = ldst_reg_offset(0b11, 0, 0b00, rm, 0b011, 0, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Xt, [Xn, #imm]  (unsigned offset, 64-bit)
    pub fn str_imm(&mut self, rt: Reg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b11, 0, 0b00, imm >> 3, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Wt, [Xn, #imm]  (unsigned offset, 32-bit)
    pub fn str_imm_w(&mut self, rt: Reg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b10, 0, 0b00, imm >> 2, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDUR Xt, [Xn, #simm9]  (unscaled signed offset, 64-bit, no writeback).
    ///
    /// Bug-fix (ARM64 BUG #1): plain offset load for negative or non-8-aligned
    /// offsets that fit in the 9-bit signed range (−256..=255). Unlike `ldr_pre`
    /// this does **not** mutate the base register `Xn`, so it is safe for
    /// frame-slot reloads addressed off FP/SP. `simm9` is in bytes (unscaled);
    /// the caller must ensure −256 <= simm9 <= 255.
    pub fn ldur(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        // size=11, V=0, opc=01 (LDUR, 64-bit)
        let inst = ldst_unscaled_imm(0b11, 0, 0b01, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STUR Xt, [Xn, #simm9]  (unscaled signed offset, 64-bit, no writeback).
    ///
    /// Bug-fix (ARM64 BUG #1): plain offset store counterpart to [`ldur`].
    pub fn stur(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        // size=11, V=0, opc=00 (STUR, 64-bit)
        let inst = ldst_unscaled_imm(0b11, 0, 0b00, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDR Xt, [Xn, #simm]!  (pre-index, 64-bit)
    pub fn ldr_pre(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        let inst = ldst_pre_post(0b11, 0, 0b01, simm9, true, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDR Xt, [Xn], #simm  (post-index, 64-bit)
    pub fn ldr_post(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        let inst = ldst_pre_post(0b11, 0, 0b01, simm9, false, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Xt, [Xn, #simm]!  (pre-index, 64-bit)
    pub fn str_pre(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        let inst = ldst_pre_post(0b11, 0, 0b00, simm9, true, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Xt, [Xn], #simm  (post-index, 64-bit)
    pub fn str_post(&mut self, rt: Reg, rn: Reg, simm9: i16) {
        let inst = ldst_pre_post(0b11, 0, 0b00, simm9, false, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDP Xt1, Xt2, [Xn, #imm]  (signed offset, 64-bit pair)
    /// `imm` must be a multiple of 8, range -512..504.
    pub fn ldp(&mut self, rt1: Reg, rt2: Reg, rn: Reg, imm: i16) {
        let inst = ldst_pair(0b10, 0, 1, 0b10, imm, rt2, rn, rt1);
        self.emit_u32(inst);
    }

    /// STP Xt1, Xt2, [Xn, #imm]  (signed offset, 64-bit pair)
    pub fn stp(&mut self, rt1: Reg, rt2: Reg, rn: Reg, imm: i16) {
        let inst = ldst_pair(0b10, 0, 0, 0b10, imm, rt2, rn, rt1);
        self.emit_u32(inst);
    }

    /// STP Xt1, Xt2, [Xn, #imm]!  (pre-index, 64-bit pair)
    pub fn stp_pre(&mut self, rt1: Reg, rt2: Reg, rn: Reg, imm: i16) {
        let inst = ldst_pair(0b10, 0, 0, 0b11, imm, rt2, rn, rt1);
        self.emit_u32(inst);
    }

    /// LDP Xt1, Xt2, [Xn], #imm  (post-index, 64-bit pair)
    pub fn ldp_post(&mut self, rt1: Reg, rt2: Reg, rn: Reg, imm: i16) {
        let inst = ldst_pair(0b10, 0, 1, 0b01, imm, rt2, rn, rt1);
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // Branches
    // -----------------------------------------------------------------------

    /// B <offset>  (unconditional branch, PC-relative)
    /// `offset` is in bytes from the current instruction, must be 4-byte aligned.
    /// Returns the offset of this instruction for later patching.
    pub fn b(&mut self, offset: i32) -> usize {
        let pos = self.offset();
        if !imm26_fits(offset as i64) {
            self.mark_branch_overflow("B", offset as i64);
        }
        let imm26 = ((offset >> 2) as u32) & 0x03FF_FFFF;
        let inst = 0b000101_00_0000_0000_0000_0000_0000_0000u32 | imm26;
        self.emit_u32(inst);
        pos
    }

    /// BL <offset>  (branch with link)
    pub fn bl(&mut self, offset: i32) -> usize {
        let pos = self.offset();
        if !imm26_fits(offset as i64) {
            self.mark_branch_overflow("BL", offset as i64);
        }
        let imm26 = ((offset >> 2) as u32) & 0x03FF_FFFF;
        let inst = 0b100101_00_0000_0000_0000_0000_0000_0000u32 | imm26;
        self.emit_u32(inst);
        pos
    }

    /// BR Xn  (branch to register)
    pub fn br(&mut self, rn: Reg) {
        // 1101011 0000 11111 000000 Rn 00000
        let inst = 0xD61F_0000 | (rn.enc() << 5);
        self.emit_u32(inst);
    }

    /// BLR Xn  (branch with link to register)
    pub fn blr(&mut self, rn: Reg) {
        // 1101011 0001 11111 000000 Rn 00000
        let inst = 0xD63F_0000 | (rn.enc() << 5);
        self.emit_u32(inst);
    }

    /// RET {Xn}  (return, defaults to X30/LR)
    pub fn ret(&mut self, rn: Reg) {
        // 1101011 0010 11111 000000 Rn 00000
        let inst = 0xD65F_0000 | (rn.enc() << 5);
        self.emit_u32(inst);
    }

    /// RET (to LR)
    pub fn ret_lr(&mut self) {
        self.ret(LR);
    }

    /// B.cond <offset>  (conditional branch)
    /// `offset` is in bytes, must be 4-aligned, range +/- 1MB.
    pub fn b_cond(&mut self, cond: Cond, offset: i32) -> usize {
        let pos = self.offset();
        if !imm19_fits(offset as i64) {
            self.mark_branch_overflow("B.cond", offset as i64);
        }
        let imm19 = ((offset >> 2) as u32) & 0x7FFFF;
        // 0101010 0 imm19(19) 0 cond(4)
        let inst = 0x5400_0000 | (imm19 << 5) | cond.enc();
        self.emit_u32(inst);
        pos
    }

    /// CBZ Xt, <offset>  (compare and branch if zero, 64-bit)
    pub fn cbz(&mut self, rt: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm19_fits(offset as i64) {
            self.mark_branch_overflow("CBZ", offset as i64);
        }
        let imm19 = ((offset >> 2) as u32) & 0x7FFFF;
        // sf=1, 011010 0 imm19 Rt
        let inst = 0xB400_0000 | (imm19 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// CBZ Wt, <offset>  (32-bit)
    pub fn cbz_w(&mut self, rt: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm19_fits(offset as i64) {
            self.mark_branch_overflow("CBZ (W)", offset as i64);
        }
        let imm19 = ((offset >> 2) as u32) & 0x7FFFF;
        let inst = 0x3400_0000 | (imm19 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// CBNZ Xt, <offset>  (compare and branch if not zero, 64-bit)
    pub fn cbnz(&mut self, rt: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm19_fits(offset as i64) {
            self.mark_branch_overflow("CBNZ", offset as i64);
        }
        let imm19 = ((offset >> 2) as u32) & 0x7FFFF;
        let inst = 0xB500_0000 | (imm19 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// CBNZ Wt, <offset>  (32-bit)
    pub fn cbnz_w(&mut self, rt: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm19_fits(offset as i64) {
            self.mark_branch_overflow("CBNZ (W)", offset as i64);
        }
        let imm19 = ((offset >> 2) as u32) & 0x7FFFF;
        let inst = 0x3500_0000 | (imm19 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// TBZ Xt, #bit, <offset>  (test bit and branch if zero)
    /// `bit` is 0..63, `offset` in bytes (4-aligned), range +/- 32KB.
    pub fn tbz(&mut self, rt: Reg, bit: u8, offset: i32) -> usize {
        let pos = self.offset();
        if !imm14_fits(offset as i64) {
            self.mark_branch_overflow("TBZ", offset as i64);
        }
        let imm14 = ((offset >> 2) as u32) & 0x3FFF;
        let b5 = ((bit as u32) >> 5) & 1;
        let b40 = (bit as u32) & 0x1F;
        // b5(1) 011011 0 b40(5) imm14(14) Rt(5)
        let inst = (b5 << 31) | 0x3600_0000 | (b40 << 19) | (imm14 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// TBNZ Xt, #bit, <offset>  (test bit and branch if not zero)
    pub fn tbnz(&mut self, rt: Reg, bit: u8, offset: i32) -> usize {
        let pos = self.offset();
        if !imm14_fits(offset as i64) {
            self.mark_branch_overflow("TBNZ", offset as i64);
        }
        let imm14 = ((offset >> 2) as u32) & 0x3FFF;
        let b5 = ((bit as u32) >> 5) & 1;
        let b40 = (bit as u32) & 0x1F;
        let inst = (b5 << 31) | 0x3700_0000 | (b40 << 19) | (imm14 << 5) | rt.enc();
        self.emit_u32(inst);
        pos
    }

    // -----------------------------------------------------------------------
    // Address generation
    // -----------------------------------------------------------------------

    /// ADR Xd, <offset>  (PC-relative, +/- 1MB)
    pub fn adr(&mut self, rd: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm21_fits(offset as i64) {
            self.mark_branch_overflow("ADR", offset as i64);
        }
        let imm = offset as u32;
        let immlo = imm & 0x3;
        let immhi = (imm >> 2) & 0x7FFFF;
        let inst = (immlo << 29) | 0x1000_0000 | (immhi << 5) | rd.enc();
        self.emit_u32(inst);
        pos
    }

    /// ADRP Xd, <offset>  (PC-relative page, +/- 4GB)
    pub fn adrp(&mut self, rd: Reg, offset: i32) -> usize {
        let pos = self.offset();
        if !imm21_fits(offset as i64) {
            self.mark_branch_overflow("ADRP", offset as i64);
        }
        let imm = offset as u32;
        let immlo = imm & 0x3;
        let immhi = (imm >> 2) & 0x7FFFF;
        let inst = (immlo << 29) | 0x9000_0000 | (immhi << 5) | rd.enc();
        self.emit_u32(inst);
        pos
    }

    /// Emit an ADR with a zero offset as a placeholder. Returns the byte offset
    /// of the instruction for later patching with `patch_adr`.
    pub fn load_label(&mut self, rd: Reg) -> usize {
        self.adr(rd, 0)
    }

    /// Emit a LDR Xt, <literal> instruction with a zero offset placeholder.
    /// Returns the byte offset for later patching with `patch_ldr_literal`.
    ///
    /// Encoding: `01 011 000 imm19 Rt` — loads a 64-bit value from PC + imm19*4.
    pub fn ldr_literal_x(&mut self, rt: Reg) -> usize {
        let pos = self.offset();
        // opc=01, V=0, imm19=0, Rt
        let inst: u32 = 0x5800_0000 | rt.enc();
        self.emit_u32(inst);
        pos
    }

    /// Append a raw 64-bit constant to the code buffer (for literal pool entries).
    /// Returns the byte offset of the constant.
    pub fn emit_u64_data(&mut self, value: u64) -> usize {
        let pos = self.offset();
        self.code.extend_from_slice(&value.to_le_bytes());
        pos
    }

    // -----------------------------------------------------------------------
    // Patch helpers
    // -----------------------------------------------------------------------

    /// Patch a B or BL instruction at `offset` to branch to `target`.
    /// Both `offset` and `target` are byte positions in the code buffer.
    pub fn patch_branch(&mut self, offset: usize, target: usize) {
        let delta64 = target as i64 - offset as i64;
        if !imm26_fits(delta64) {
            self.mark_branch_overflow("B/BL patch", delta64);
        }
        let delta = delta64 as i32;
        let imm26 = ((delta >> 2) as u32) & 0x03FF_FFFF;
        // Read existing opcode to preserve B vs BL distinction.
        let existing = u32::from_le_bytes([
            self.code[offset],
            self.code[offset + 1],
            self.code[offset + 2],
            self.code[offset + 3],
        ]);
        let top6 = existing & 0xFC00_0000;
        let patched = top6 | imm26;
        let bytes = patched.to_le_bytes();
        self.code[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Patch a B.cond / CBZ / CBNZ instruction at `offset` to target `target`.
    pub fn patch_bcond(&mut self, offset: usize, target: usize) {
        let delta64 = target as i64 - offset as i64;
        if !imm19_fits(delta64) {
            self.mark_branch_overflow("B.cond/CBZ patch", delta64);
        }
        let delta = delta64 as i32;
        let imm19 = ((delta >> 2) as u32) & 0x7FFFF;
        let existing = u32::from_le_bytes([
            self.code[offset],
            self.code[offset + 1],
            self.code[offset + 2],
            self.code[offset + 3],
        ]);
        // Clear the imm19 field (bits 23:5), preserve the rest.
        let patched = (existing & !0x00FF_FFE0) | (imm19 << 5);
        let bytes = patched.to_le_bytes();
        self.code[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Patch an ADR instruction at `offset` with the given target.
    pub fn patch_adr(&mut self, offset: usize, target: usize) {
        let delta64 = target as i64 - offset as i64;
        if !imm21_fits(delta64) {
            self.mark_branch_overflow("ADR patch", delta64);
        }
        let delta = delta64 as i32;
        let imm = delta as u32;
        let immlo = imm & 0x3;
        let immhi = (imm >> 2) & 0x7FFFF;
        let existing = u32::from_le_bytes([
            self.code[offset],
            self.code[offset + 1],
            self.code[offset + 2],
            self.code[offset + 3],
        ]);
        // Clear immhi (bits 23:5) and immlo (bits 30:29)
        let patched = (existing & 0x9F00_001F) | (immlo << 29) | (immhi << 5);
        let bytes = patched.to_le_bytes();
        self.code[offset..offset + 4].copy_from_slice(&bytes);
    }

    /// Patch a LDR (literal) instruction at `offset` so that it loads from `target`.
    /// The delta must be a multiple of 4 (both are instruction-aligned offsets).
    pub fn patch_ldr_literal(&mut self, offset: usize, target: usize) {
        let delta64 = target as i64 - offset as i64;
        if !imm19_fits(delta64) {
            self.mark_branch_overflow("LDR-literal patch", delta64);
        }
        let delta = delta64 as i32;
        let imm19 = ((delta >> 2) as u32) & 0x7FFFF;
        let existing = u32::from_le_bytes([
            self.code[offset],
            self.code[offset + 1],
            self.code[offset + 2],
            self.code[offset + 3],
        ]);
        // Clear imm19 field (bits 23:5), preserve opc/V/Rt.
        let patched = (existing & !0x00FF_FFE0) | (imm19 << 5);
        let bytes = patched.to_le_bytes();
        self.code[offset..offset + 4].copy_from_slice(&bytes);
    }

    // -----------------------------------------------------------------------
    // Floating-point instructions
    // -----------------------------------------------------------------------

    /// FMOV Dd, Dn  (double-precision register move)
    pub fn fmov_d(&mut self, rd: FpReg, rn: FpReg) {
        // 000 11110 01 1 0000 00 10000 Rn Rd
        let inst = 0x1E60_4000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FMOV Sd, Sn  (single-precision register move)
    pub fn fmov_s(&mut self, rd: FpReg, rn: FpReg) {
        // 000 11110 00 1 0000 00 10000 Rn Rd
        let inst = 0x1E20_4000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FMOV Dd, Xn  (general to FP, 64-bit)
    pub fn fmov_d_from_gp(&mut self, rd: FpReg, rn: Reg) {
        // 1 00 11110 01 1 00 111 000000 Rn Rd
        let inst = 0x9E67_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FMOV Xd, Dn  (FP to general, 64-bit)
    pub fn fmov_gp_from_d(&mut self, rd: Reg, rn: FpReg) {
        // 1 00 11110 01 1 00 110 000000 Rn Rd
        let inst = 0x9E66_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    // FP data-processing (2 source)
    // M(1) 0 S(1) 11110 ftype(2) 1 Rm(5) opcode(4) 10 Rn(5) Rd(5)

    fn fp_dp2(&mut self, double: bool, opcode: u32, rd: FpReg, rn: FpReg, rm: FpReg) {
        let ftype: u32 = if double { 0b01 } else { 0b00 };
        let inst = (0b00011110u32 << 24)
            | (ftype << 22)
            | (1 << 21)
            | (rm.enc() << 16)
            | (opcode << 12)
            | (0b10 << 10)
            | (rn.enc() << 5)
            | rd.enc();
        self.emit_u32(inst);
    }

    /// FADD Dd, Dn, Dm  (double)
    pub fn fadd_d(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(true, 0b0010, rd, rn, rm);
    }

    /// FADD Sd, Sn, Sm  (single)
    pub fn fadd_s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(false, 0b0010, rd, rn, rm);
    }

    /// FSUB Dd, Dn, Dm  (double)
    pub fn fsub_d(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(true, 0b0011, rd, rn, rm);
    }

    /// FSUB Sd, Sn, Sm  (single)
    pub fn fsub_s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(false, 0b0011, rd, rn, rm);
    }

    /// FMUL Dd, Dn, Dm  (double)
    pub fn fmul_d(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(true, 0b0000, rd, rn, rm);
    }

    /// FMUL Sd, Sn, Sm  (single)
    pub fn fmul_s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(false, 0b0000, rd, rn, rm);
    }

    /// FDIV Dd, Dn, Dm  (double)
    pub fn fdiv_d(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(true, 0b0001, rd, rn, rm);
    }

    /// FDIV Sd, Sn, Sm  (single)
    pub fn fdiv_s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        self.fp_dp2(false, 0b0001, rd, rn, rm);
    }

    /// FCMP Dn, Dm  (double)
    pub fn fcmp_d(&mut self, rn: FpReg, rm: FpReg) {
        // M=0 S=0 11110 01 1 Rm 00 1000 Rn 00 000
        let inst = 0x1E60_2000 | (rm.enc() << 16) | (rn.enc() << 5);
        self.emit_u32(inst);
    }

    /// FCMP Sn, Sm  (single)
    pub fn fcmp_s(&mut self, rn: FpReg, rm: FpReg) {
        let inst = 0x1E20_2000 | (rm.enc() << 16) | (rn.enc() << 5);
        self.emit_u32(inst);
    }

    /// SCVTF Dd, Xn  (signed int64 to double)
    pub fn scvtf_d_x(&mut self, rd: FpReg, rn: Reg) {
        // sf=1, 00 11110 01 1 00 010 000000 Rn Rd
        let inst = 0x9E62_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// SCVTF Sd, Wn  (signed int32 to single)
    pub fn scvtf_s_w(&mut self, rd: FpReg, rn: Reg) {
        // sf=0, 00 11110 00 1 00 010 000000 Rn Rd
        let inst = 0x1E22_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// SCVTF Dd, Wn  (signed int32 to double)
    pub fn scvtf_d_w(&mut self, rd: FpReg, rn: Reg) {
        let inst = 0x1E62_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FCVTZS Xd, Dn  (double to signed int64, round toward zero)
    pub fn fcvtzs_x_d(&mut self, rd: Reg, rn: FpReg) {
        // sf=1, 00 11110 01 1 11 000 000000 Rn Rd
        let inst = 0x9E78_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FCVTZS Wd, Sn  (single to signed int32, round toward zero)
    pub fn fcvtzs_w_s(&mut self, rd: Reg, rn: FpReg) {
        // sf=0, 00 11110 00 1 11 000 000000 Rn Rd
        let inst = 0x1E38_0000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FNEG Dd, Dn  (double negate)
    pub fn fneg_d(&mut self, rd: FpReg, rn: FpReg) {
        // 0 00 11110 01 1 0000 10 10000 Rn Rd
        let inst = 0x1E61_4000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FNEG Sd, Sn  (single negate)
    pub fn fneg_s(&mut self, rd: FpReg, rn: FpReg) {
        // 0 00 11110 00 1 0000 10 10000 Rn Rd
        let inst = 0x1E21_4000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FCVT Dd, Sn  (single to double)
    pub fn fcvt_d_s(&mut self, rd: FpReg, rn: FpReg) {
        // 0 00 11110 00 1 00 01 01 10000 Rn Rd
        let inst = 0x1E22_C000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// FCVT Sd, Dn  (double to single)
    pub fn fcvt_s_d(&mut self, rd: FpReg, rn: FpReg) {
        // 0 00 11110 01 1 00 00 01 10000 Rn Rd
        let inst = 0x1E62_4000 | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // FP load/store
    // -----------------------------------------------------------------------

    /// LDR Dt, [Xn, #imm]  (FP, 64-bit, unsigned offset)
    pub fn ldr_fp_d(&mut self, rt: FpReg, rn: Reg, imm: u16) {
        // size=11, V=1, opc=01
        let inst = ldst_unsigned_imm(0b11, 1, 0b01, imm >> 3, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR Dt, [Xn, #imm]  (FP, 64-bit, unsigned offset)
    pub fn str_fp_d(&mut self, rt: FpReg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b11, 1, 0b00, imm >> 3, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDR St, [Xn, #imm]  (FP, 32-bit, unsigned offset)
    pub fn ldr_fp_s(&mut self, rt: FpReg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b10, 1, 0b01, imm >> 2, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STR St, [Xn, #imm]  (FP, 32-bit, unsigned offset)
    pub fn str_fp_s(&mut self, rt: FpReg, rn: Reg, imm: u16) {
        let inst = ldst_unsigned_imm(0b10, 1, 0b00, imm >> 2, rn, rt.enc());
        self.emit_u32(inst);
    }

    // -- Unscaled FP load/store (LDUR/STUR, FP variants) --------------------
    //
    // Bug-fix (aarch64 parity audit 2026-08-01): the scaled unsigned-offset
    // forms above (`ldr_fp_d` … `str_fp_s`) take a `u16` byte offset and cannot
    // express a NEGATIVE displacement. Every frame slot on this backend is at a
    // negative offset from FP, so the FP spill/reload path was handing them
    // `offset as u16`, which reinterprets e.g. −24 as 65512 and then scales it
    // — a load/store 64 KiB *above* FP, deep inside the caller's frame.
    //
    // This is the FP twin of "ARM64 BUG #1", which was fixed for the GPR
    // `Ldr`/`Str` lowering (see `ldur`/`stur` above) and left unfixed here.
    // These four emitters are the unscaled, non-writeback (`imm9`, −256..=255)
    // forms the fix routes negative offsets through.

    /// LDUR Dt, [Xn, #simm9]  (FP 64-bit, unscaled signed offset, no writeback).
    pub fn ldur_fp_d(&mut self, rt: FpReg, rn: Reg, simm9: i16) {
        // size=11, V=1, opc=01
        let inst = ldst_unscaled_imm(0b11, 1, 0b01, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STUR Dt, [Xn, #simm9]  (FP 64-bit, unscaled signed offset, no writeback).
    pub fn stur_fp_d(&mut self, rt: FpReg, rn: Reg, simm9: i16) {
        // size=11, V=1, opc=00
        let inst = ldst_unscaled_imm(0b11, 1, 0b00, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// LDUR St, [Xn, #simm9]  (FP 32-bit, unscaled signed offset, no writeback).
    pub fn ldur_fp_s(&mut self, rt: FpReg, rn: Reg, simm9: i16) {
        // size=10, V=1, opc=01
        let inst = ldst_unscaled_imm(0b10, 1, 0b01, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    /// STUR St, [Xn, #simm9]  (FP 32-bit, unscaled signed offset, no writeback).
    pub fn stur_fp_s(&mut self, rt: FpReg, rn: Reg, simm9: i16) {
        // size=10, V=1, opc=00
        let inst = ldst_unscaled_imm(0b10, 1, 0b00, simm9, rn, rt.enc());
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // NEON basics (i32x4)
    // -----------------------------------------------------------------------

    /// LD1 {Vt.4S}, [Xn]  (load single structure, no offset)
    pub fn ld1_4s(&mut self, vt: FpReg, rn: Reg) {
        // 0 Q(1) 001100 0 1 0 00000 opcode(4) size(2) Rn(5) Rt(5)
        // Q=1 (128-bit), L=1, opcode=0111 (LD1 single struct, 1 reg), size=10 (32-bit)
        // 0 1 0011000 1 0 00000 0111 10 Rn Rt
        let inst = 0x4C40_7800 | (rn.enc() << 5) | vt.enc();
        self.emit_u32(inst);
    }

    /// ST1 {Vt.4S}, [Xn]  (store single structure, no offset)
    pub fn st1_4s(&mut self, vt: FpReg, rn: Reg) {
        // Same format with L=0
        // 0 1 0011000 0 0 00000 0111 10 Rn Rt
        let inst = 0x4C00_7800 | (rn.enc() << 5) | vt.enc();
        self.emit_u32(inst);
    }

    /// ADD Vd.4S, Vn.4S, Vm.4S  (NEON integer vector add)
    pub fn add_v4s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        // 0 Q(1) 0 01110 size(2) 1 Rm(5) opcode(5) 1 Rn(5) Rd(5)
        // Q=1, U=0, size=10 (32-bit), opcode=10000 (ADD)
        let inst = 0x4EA0_8400 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    /// MUL Vd.4S, Vn.4S, Vm.4S  (NEON integer vector multiply)
    pub fn mul_v4s(&mut self, rd: FpReg, rn: FpReg, rm: FpReg) {
        // 0 Q(1) 0 01110 size(2) 1 Rm(5) opcode(5) 1 Rn(5) Rd(5)
        // Q=1, U=0, size=10 (32-bit), opcode=10011 (MUL)
        let inst = 0x4EA0_9C00 | (rm.enc() << 16) | (rn.enc() << 5) | rd.enc();
        self.emit_u32(inst);
    }

    // -----------------------------------------------------------------------
    // System instructions
    // -----------------------------------------------------------------------

    /// NOP
    pub fn nop(&mut self) {
        self.emit_u32(0xD503_201F);
    }

    /// BRK #imm16  (breakpoint)
    pub fn brk(&mut self, imm16: u16) {
        let inst = 0xD420_0000 | ((imm16 as u32) << 5);
        self.emit_u32(inst);
    }

    /// SVC #imm16  (supervisor call)
    pub fn svc(&mut self, imm16: u16) {
        let inst = 0xD400_0001 | ((imm16 as u32) << 5);
        self.emit_u32(inst);
    }

    /// DSB (data synchronization barrier)
    /// `option`: 0b1111 = SY (full system), 0b1011 = ISH, etc.
    pub fn dsb(&mut self, option: u8) {
        // 1101010100 0 00 011 0011 CRm(4) 1 00 11111
        let inst = 0xD503_309F | ((option as u32 & 0xF) << 8);
        self.emit_u32(inst);
    }

    /// ISB (instruction synchronization barrier)
    pub fn isb(&mut self) {
        // ISB SY = 0xD5033FDF
        self.emit_u32(0xD503_3FDF);
    }

    /// DMB (data memory barrier)
    pub fn dmb(&mut self, option: u8) {
        let inst = 0xD503_30BF | ((option as u32 & 0xF) << 8);
        self.emit_u32(inst);
    }
}

// ---------------------------------------------------------------------------
// Shift type encoding (for shifted register forms)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[allow(dead_code)]
enum ShiftType {
    LSL = 0b00,
    LSR = 0b01,
    ASR = 0b10,
    ROR = 0b11, // only for logic ops
}

// ---------------------------------------------------------------------------
// Branch-offset range checks
// ---------------------------------------------------------------------------
//
// AArch64 PC-relative branch immediates are masked into a fixed-width field,
// so an offset that does not fit silently wraps to a wrong target. These
// helpers report whether a *byte* offset is encodable, so the encoders/patchers
// can flag overflow (see `Aarch64Emitter::mark_branch_overflow`) instead of
// emitting a truncated branch.

/// `B`/`BL` imm26: byte offset must be 4-aligned and within ±128 MB.
/// The field stores `offset >> 2`, a 26-bit signed value, so the byte range is
/// −2^27 ..= 2^27 − 4.
#[inline]
fn imm26_fits(delta: i64) -> bool {
    (delta & 0b11) == 0 && (-(1 << 27)..(1 << 27)).contains(&delta)
}

/// `B.cond`/`CBZ`/`CBNZ`/`LDR(literal)` imm19: 4-aligned, within ±1 MB.
#[inline]
fn imm19_fits(delta: i64) -> bool {
    (delta & 0b11) == 0 && (-(1 << 20)..(1 << 20)).contains(&delta)
}

/// `TBZ`/`TBNZ` imm14: 4-aligned, within ±32 KB.
#[inline]
fn imm14_fits(delta: i64) -> bool {
    (delta & 0b11) == 0 && (-(1 << 15)..(1 << 15)).contains(&delta)
}

/// `ADR`/`ADRP` imm21: a 21-bit signed value (unscaled for ADR; pages for
/// ADRP). Range −2^20 ..= 2^20 − 1.
#[inline]
fn imm21_fits(delta: i64) -> bool {
    (-(1 << 20)..(1 << 20)).contains(&delta)
}

// ---------------------------------------------------------------------------
// Encoding helpers (free functions, not exported)
// ---------------------------------------------------------------------------

/// Data-processing (shifted register).
/// sf(1) opc(2) fixed(5) shift(2) N(1) Rm(5) imm6(6) Rn(5) Rd(5)
fn dp_shifted_reg(
    sf: bool,
    opc: u32,
    fixed: u32,
    shift: ShiftType,
    n: u32,
    rm: Reg,
    imm6: u8,
    rn: Reg,
    rd: Reg,
) -> u32 {
    ((sf as u32) << 31)
        | (opc << 29)
        | (fixed << 24)
        | ((shift as u32) << 22)
        | ((n & 1) << 21)
        | (rm.enc() << 16)
        | ((imm6 as u32 & 0x3F) << 10)
        | (rn.enc() << 5)
        | rd.enc()
}

/// Data-processing (2 source).
/// sf(1) 0 S(1) 11010110 Rm(5) opcode(6) Rn(5) Rd(5)
fn dp_2src(sf: bool, rm: Reg, opcode: u32, rn: Reg, rd: Reg) -> u32 {
    ((sf as u32) << 31)
        | (0b0_11010110u32 << 21)
        | (rm.enc() << 16)
        | ((opcode & 0x3F) << 10)
        | (rn.enc() << 5)
        | rd.enc()
}

/// Data-processing (3 source).
/// sf(1) op54(2) 11011 op31(3) Rm(5) o0(1) Ra(5) Rn(5) Rd(5)
fn dp_3src(sf: bool, op31: u32, rm: Reg, o0: u32, ra: Reg, rn: Reg, rd: Reg) -> u32 {
    ((sf as u32) << 31)
        | (0b00_11011u32 << 24)
        | ((op31 & 0x7) << 21)
        | (rm.enc() << 16)
        | ((o0 & 1) << 15)
        | (ra.enc() << 10)
        | (rn.enc() << 5)
        | rd.enc()
}

/// Logic (shifted register).
/// sf(1) opc(2) 01010 shift(2) N(1) Rm(5) imm6(6) Rn(5) Rd(5)
fn logic_shifted(
    sf: bool,
    opc: u32,
    shift: ShiftType,
    n: u32,
    rm: Reg,
    imm6: u8,
    rn: Reg,
    rd: Reg,
) -> u32 {
    ((sf as u32) << 31)
        | ((opc & 0x3) << 29)
        | (0b01010u32 << 24)
        | ((shift as u32) << 22)
        | ((n & 1) << 21)
        | (rm.enc() << 16)
        | ((imm6 as u32 & 0x3F) << 10)
        | (rn.enc() << 5)
        | rd.enc()
}

/// Add/subtract immediate.
/// sf(1) op(1) S(1) 100010 sh(1) imm12(12) Rn(5) Rd(5)
fn addsub_imm(
    sf: bool,
    sub: bool,
    set_flags: bool,
    imm12: u16,
    shift12: bool,
    rn: Reg,
    rd: Reg,
) -> u32 {
    ((sf as u32) << 31)
        | ((sub as u32) << 30)
        | ((set_flags as u32) << 29)
        | (0b100010u32 << 23)
        | ((shift12 as u32) << 22)
        | ((imm12 as u32 & 0xFFF) << 10)
        | (rn.enc() << 5)
        | rd.enc()
}

/// Move wide (MOVZ/MOVK/MOVN).
/// sf(1) opc(2) 100101 hw(2) imm16(16) Rd(5)
fn move_wide(sf: bool, opc: u32, shift: u8, imm16: u16, rd: Reg) -> u32 {
    let hw = (shift / 16) as u32;
    ((sf as u32) << 31)
        | ((opc & 0x3) << 29)
        | (0b100101u32 << 23)
        | ((hw & 0x3) << 21)
        | ((imm16 as u32) << 5)
        | rd.enc()
}

/// Load/store register (unsigned offset).
/// size(2) 1 1 1 V(1) 01 opc(2) imm12(12) Rn(5) Rt(5)
fn ldst_unsigned_imm(size: u32, v: u32, opc: u32, imm12: u16, rn: Reg, rt: u32) -> u32 {
    ((size & 0x3) << 30)
        | (0b111u32 << 27)
        | ((v & 1) << 26)
        | (0b01u32 << 24)
        | ((opc & 0x3) << 22)
        | ((imm12 as u32 & 0xFFF) << 10)
        | (rn.enc() << 5)
        | (rt & 0x1F)
}

/// Load/store register (register offset).
/// size(2) 1 1 1 V(1) 00 opc(2) 1 Rm(5) option(3) S(1) 10 Rn(5) Rt(5)
fn ldst_reg_offset(
    size: u32,
    v: u32,
    opc: u32,
    rm: Reg,
    option: u32,
    s: u32,
    rn: Reg,
    rt: u32,
) -> u32 {
    ((size & 0x3) << 30)
        | (0b111u32 << 27)
        | ((v & 1) << 26)
        | (0b00u32 << 24)
        | ((opc & 0x3) << 22)
        | (1u32 << 21)
        | (rm.enc() << 16)
        | ((option & 0x7) << 13)
        | ((s & 1) << 12)
        | (0b10u32 << 10)
        | (rn.enc() << 5)
        | (rt & 0x1F)
}

/// Load/store register (unscaled signed immediate — LDUR/STUR).
/// size(2) 111 V(1) 00 opc(2) 0 imm9(9) 00 Rn(5) Rt(5)
///
/// Bug-fix (ARM64 BUG #1, frame-slot corruption): this is the *unscaled*,
/// *non-writeback* addressing form. Unlike `ldst_pre_post`, the two bits at
/// positions 11:10 are `00` (not `idx,1`), so the base register `Rn` is **not**
/// mutated — the access is a plain `[Rn + #simm9]` with no side effect. The
/// 9-bit signed immediate covers −256..=255 in single-byte units, with **no**
/// 8-byte alignment requirement (unlike the scaled `ldst_unsigned_imm` form).
/// This is what frame-slot spills/reloads at negative FP offsets must use.
fn ldst_unscaled_imm(size: u32, v: u32, opc: u32, simm9: i16, rn: Reg, rt: u32) -> u32 {
    let imm9 = (simm9 as u32) & 0x1FF;
    ((size & 0x3) << 30)
        | (0b111u32 << 27)
        | ((v & 1) << 26)
        | (0b00u32 << 24)
        | ((opc & 0x3) << 22)
        | (0u32 << 21)
        | (imm9 << 12)
        // bits 11:10 = 00 → unscaled, no index/writeback (distinguishes from
        // the pre/post-index form which sets bit 10).
        | (rn.enc() << 5)
        | (rt & 0x1F)
}

/// Load/store register (pre/post-index).
/// size(2) 111 V(1) 00 opc(2) 0 imm9(9) idx(1) 1 Rn(5) Rt(5)
/// idx: 1 = pre-index, 0 = post-index
fn ldst_pre_post(size: u32, v: u32, opc: u32, simm9: i16, pre: bool, rn: Reg, rt: u32) -> u32 {
    let imm9 = (simm9 as u32) & 0x1FF;
    ((size & 0x3) << 30)
        | (0b111u32 << 27)
        | ((v & 1) << 26)
        | (0b00u32 << 24)
        | ((opc & 0x3) << 22)
        | (0u32 << 21)
        | (imm9 << 12)
        | ((pre as u32) << 11)
        | (1u32 << 10)
        | (rn.enc() << 5)
        | (rt & 0x1F)
}

/// Load/store pair.
/// opc(2) V(1) 0 encoding(2) L(1) imm7(7) Rt2(5) Rn(5) Rt(5)
///
/// encoding: 00=non-temporal, 01=post-index, 10=signed-offset, 11=pre-index
/// For 64-bit (opc=10), imm7 is signed and scaled by 8.
fn ldst_pair(
    opc: u32,
    v: u32,
    l: u32,
    encoding: u32,
    imm: i16,
    rt2: Reg,
    rn: Reg,
    rt1: Reg,
) -> u32 {
    // Scale: for opc=10 (64-bit), divide by 8
    let imm7 = ((imm / 8) as u32) & 0x7F;
    ((opc & 0x3) << 30)
        | ((v & 1) << 26)
        | (0b101u32 << 27)
        | ((encoding & 0x3) << 23)
        | ((l & 1) << 22)
        | (imm7 << 15)
        | (rt2.enc() << 10)
        | (rn.enc() << 5)
        | rt1.enc()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- Reg::from_u8 / FpReg::from_u8 (C11) --------------------------------

    #[test]
    fn reg_from_u8_round_trips_all_valid_encodings() {
        for n in 0..=31u8 {
            let r = Reg::from_u8(n).expect("0..=31 must round-trip");
            assert_eq!(r as u8, n, "Reg::from_u8({n}) must encode back to {n}");
        }
    }

    #[test]
    fn reg_from_u8_rejects_out_of_range() {
        for n in [32u8, 40, 64, 100, 200, 255] {
            assert!(Reg::from_u8(n).is_none(), "Reg::from_u8({n}) must be None");
        }
    }

    #[test]
    fn fpreg_from_u8_round_trips_all_valid_encodings() {
        for n in 0..=31u8 {
            let r = FpReg::from_u8(n).expect("0..=31 must round-trip");
            assert_eq!(r as u8, n, "FpReg::from_u8({n}) must encode back to {n}");
        }
    }

    #[test]
    fn fpreg_from_u8_rejects_out_of_range() {
        for n in [32u8, 40, 64, 100, 200, 255] {
            assert!(
                FpReg::from_u8(n).is_none(),
                "FpReg::from_u8({n}) must be None"
            );
        }
    }

    /// Helper: read the last emitted 32-bit instruction word.
    fn last_inst(e: &Aarch64Emitter) -> u32 {
        let c = e.code();
        let len = c.len();
        u32::from_le_bytes([c[len - 4], c[len - 3], c[len - 2], c[len - 1]])
    }

    /// Helper: read instruction at a specific byte offset.
    fn inst_at(e: &Aarch64Emitter, off: usize) -> u32 {
        let c = e.code();
        u32::from_le_bytes([c[off], c[off + 1], c[off + 2], c[off + 3]])
    }

    // -- Arithmetic ---------------------------------------------------------

    #[test]
    fn test_add_x0_x1_x2() {
        let mut e = Aarch64Emitter::new();
        e.add(Reg::X0, Reg::X1, Reg::X2);
        // ADD X0, X1, X2: sf=1, opc=00, 01011 00 0 Rm=00010 000000 Rn=00001 Rd=00000
        // 1 00 01011 00 0 00010 000000 00001 00000
        // = 0x8B020020
        assert_eq!(last_inst(&e), 0x8B02_0020);
    }

    #[test]
    fn test_add_w_regs() {
        let mut e = Aarch64Emitter::new();
        e.add_w(Reg::X3, Reg::X4, Reg::X5);
        // sf=0: 0 00 01011 00 0 00101 000000 00100 00011
        // = 0x0B050083
        assert_eq!(last_inst(&e), 0x0B05_0083);
    }

    #[test]
    fn test_sub_x0_x1_x2() {
        let mut e = Aarch64Emitter::new();
        e.sub(Reg::X0, Reg::X1, Reg::X2);
        // SUB X0, X1, X2: sf=1, opc=10, ...
        // = 0xCB020020
        assert_eq!(last_inst(&e), 0xCB02_0020);
    }

    #[test]
    fn test_mul_x0_x1_x2() {
        let mut e = Aarch64Emitter::new();
        e.mul(Reg::X0, Reg::X1, Reg::X2);
        // MADD X0, X1, X2, XZR
        // sf=1 00 11011 000 Rm=00010 o0=0 Ra=11111 Rn=00001 Rd=00000
        // 1 00 11011 000 00010 0 11111 00001 00000
        // = 0x9B027C20
        assert_eq!(last_inst(&e), 0x9B02_7C20);
    }

    #[test]
    fn test_sdiv() {
        let mut e = Aarch64Emitter::new();
        e.sdiv(Reg::X0, Reg::X1, Reg::X2);
        // 1 0 0 11010110 00010 000011 00001 00000
        // = 0x9AC20C20
        assert_eq!(last_inst(&e), 0x9AC2_0C20);
    }

    #[test]
    fn test_add_imm() {
        let mut e = Aarch64Emitter::new();
        e.add_imm(Reg::SP, Reg::SP, 16, false);
        // sf=1 op=0 S=0 100010 sh=0 imm12=000000010000 Rn=11111 Rd=11111
        // 1 0 0 100010 0 000000010000 11111 11111
        // = 0x910043FF
        assert_eq!(last_inst(&e), 0x9100_43FF);
    }

    // -- Logic --------------------------------------------------------------

    #[test]
    fn test_and_orr_eor() {
        let mut e = Aarch64Emitter::new();
        e.and(Reg::X0, Reg::X1, Reg::X2);
        assert_eq!(last_inst(&e), 0x8A02_0020);

        e.orr(Reg::X0, Reg::X1, Reg::X2);
        assert_eq!(last_inst(&e), 0xAA02_0020);

        e.eor(Reg::X0, Reg::X1, Reg::X2);
        assert_eq!(last_inst(&e), 0xCA02_0020);
    }

    // -- Move ---------------------------------------------------------------

    #[test]
    fn test_movz() {
        let mut e = Aarch64Emitter::new();
        e.movz(Reg::X0, 0x1234, 0);
        // sf=1 10 100101 00 0001001000110100 00000
        // = 0xD2824680
        assert_eq!(last_inst(&e), 0xD282_4680);
    }

    #[test]
    fn test_movk() {
        let mut e = Aarch64Emitter::new();
        e.movk(Reg::X0, 0xABCD, 16);
        // sf=1 11 100101 01 imm16 Rd
        // = 0xF2A579A0 (hw=1)
        let inst = last_inst(&e);
        // Verify opc bits: 11 = MOVK
        assert_eq!((inst >> 29) & 0x3, 0b11);
        // Verify hw = 1 (shift=16)
        assert_eq!((inst >> 21) & 0x3, 1);
        // Verify Rd = 0
        assert_eq!(inst & 0x1F, 0);
    }

    #[test]
    fn test_mov_imm64_simple() {
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X0, 42);
        // Should be a single MOVZ X0, #42, LSL #0
        assert_eq!(e.code().len(), 4); // single instruction
        let inst = inst_at(&e, 0);
        // Check it's MOVZ with imm=42
        assert_eq!((inst >> 29) & 0x3, 0b10); // MOVZ opc
        assert_eq!((inst >> 5) & 0xFFFF, 42);
        assert_eq!(inst & 0x1F, 0); // Rd = X0
    }

    #[test]
    fn test_mov_imm64_two_chunks() {
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X1, 0x0000_0000_DEAD_BEEF);
        // chunk0 = 0xBEEF, chunk1 = 0xDEAD, chunk2 = 0, chunk3 = 0
        // Should be: MOVZ X1, #0xBEEF, LSL#0 + MOVK X1, #0xDEAD, LSL#16
        assert_eq!(e.code().len(), 8); // two instructions

        let i0 = inst_at(&e, 0);
        assert_eq!((i0 >> 29) & 0x3, 0b10); // MOVZ
        assert_eq!((i0 >> 5) & 0xFFFF, 0xBEEF);
        assert_eq!(i0 & 0x1F, 1); // X1

        let i1 = inst_at(&e, 4);
        assert_eq!((i1 >> 29) & 0x3, 0b11); // MOVK
        assert_eq!((i1 >> 5) & 0xFFFF, 0xDEAD);
    }

    #[test]
    fn test_mov_imm64_all_ones() {
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X0, u64::MAX);
        // All chunks 0xFFFF, should use MOVN X0, #0, LSL#0
        assert_eq!(e.code().len(), 4);
        let inst = inst_at(&e, 0);
        assert_eq!((inst >> 29) & 0x3, 0b00); // MOVN opc
    }

    #[test]
    fn test_mov_imm64_full_pattern() {
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X2, 0x1111_2222_3333_4444);
        // All four chunks non-zero: needs MOVZ + 3x MOVK = 16 bytes
        assert_eq!(e.code().len(), 16);
    }

    // -- Branch -------------------------------------------------------------

    #[test]
    fn test_b_forward() {
        let mut e = Aarch64Emitter::new();
        e.b(0x100);
        let inst = last_inst(&e);
        // B: 000101 imm26
        assert_eq!((inst >> 26) & 0x3F, 0b000101);
        // imm26 = 0x100 / 4 = 0x40
        assert_eq!(inst & 0x03FF_FFFF, 0x40);
    }

    #[test]
    fn test_bl() {
        let mut e = Aarch64Emitter::new();
        e.bl(0x200);
        let inst = last_inst(&e);
        assert_eq!((inst >> 26) & 0x3F, 0b100101);
        assert_eq!(inst & 0x03FF_FFFF, 0x80);
    }

    #[test]
    fn test_br_blr_ret() {
        let mut e = Aarch64Emitter::new();
        e.br(Reg::X16);
        assert_eq!(last_inst(&e), 0xD61F_0200);

        e.blr(Reg::X8);
        assert_eq!(last_inst(&e), 0xD63F_0100);

        e.ret_lr();
        assert_eq!(last_inst(&e), 0xD65F_03C0);
    }

    #[test]
    fn test_b_cond() {
        let mut e = Aarch64Emitter::new();
        e.b_cond(Cond::EQ, 0x10);
        let inst = last_inst(&e);
        // 0101010 0 imm19 0 cond
        assert_eq!(inst & 0xF, Cond::EQ.enc());
        // imm19 = 0x10/4 = 4, shifted left by 5
        assert_eq!((inst >> 5) & 0x7FFFF, 4);
    }

    #[test]
    fn test_cbz_cbnz() {
        let mut e = Aarch64Emitter::new();
        e.cbz(Reg::X5, 0x20);
        let inst = last_inst(&e);
        assert_eq!(inst & 0x1F, 5); // Rt = X5
        assert_eq!((inst >> 24) & 0xFF, 0xB4); // CBZ 64-bit prefix

        e.cbnz(Reg::X3, 0x10);
        let inst = last_inst(&e);
        assert_eq!(inst & 0x1F, 3);
        assert_eq!((inst >> 24) & 0xFF, 0xB5); // CBNZ 64-bit prefix
    }

    #[test]
    fn test_tbz_tbnz() {
        let mut e = Aarch64Emitter::new();
        // TBZ X0, #5, +8
        e.tbz(Reg::X0, 5, 8);
        let inst = last_inst(&e);
        assert_eq!(inst & 0x1F, 0); // Rt = X0
                                    // bit 5 in b40 field
        assert_eq!((inst >> 19) & 0x1F, 5);
        // imm14 = 8/4 = 2
        assert_eq!((inst >> 5) & 0x3FFF, 2);

        // TBNZ X1, #63, +0xC
        e.tbnz(Reg::X1, 63, 0xC);
        let inst = last_inst(&e);
        // b5=1 (bit 31), b40=31
        assert_eq!((inst >> 31) & 1, 1);
        assert_eq!((inst >> 19) & 0x1F, 31);
    }

    // -- Load / Store -------------------------------------------------------

    #[test]
    fn test_ldr_str_imm() {
        let mut e = Aarch64Emitter::new();
        // LDR X0, [X1, #8]
        e.ldr_imm(Reg::X0, Reg::X1, 8);
        let inst = last_inst(&e);
        // size=11, V=0, opc=01 → top byte 0xF9
        assert_eq!((inst >> 22) & 0x3FF, 0b11_111_0_01_01);
        // imm12 = 8/8 = 1
        assert_eq!((inst >> 10) & 0xFFF, 1);
        assert_eq!((inst >> 5) & 0x1F, 1); // Rn = X1
        assert_eq!(inst & 0x1F, 0); // Rt = X0

        // STR X2, [SP, #16]
        e.str_imm(Reg::X2, Reg::SP, 16);
        let inst = last_inst(&e);
        assert_eq!((inst >> 10) & 0xFFF, 2); // 16/8=2
        assert_eq!((inst >> 5) & 0x1F, 31); // SP
        assert_eq!(inst & 0x1F, 2); // X2
    }

    #[test]
    fn test_ldur_stur_unscaled() {
        // Bug-fix (ARM64 BUG #1): unscaled, non-writeback offset access.
        // LDUR X0, [X1, #-8]:
        //   size=11 V=0 opc=01, imm9=0x1F8 (-8), Rn=1, Rt=0, bits11:10=00
        //   = 0xF85F8020
        let mut e = Aarch64Emitter::new();
        e.ldur(Reg::X0, Reg::X1, -8);
        assert_eq!(last_inst(&e), 0xF85F_8020);

        // STUR X2, [SP, #-16]:
        //   size=11 V=0 opc=00, imm9=0x1F0 (-16), Rn=31, Rt=2, bits11:10=00
        //   = 0xF81F03E2
        e.stur(Reg::X2, Reg::SP, -16);
        assert_eq!(last_inst(&e), 0xF81F_03E2);

        // Crucially the unscaled form must NOT set the writeback/index bit
        // (bit 10) — that bit being 1 would mutate the base register.
        let inst = last_inst(&e);
        assert_eq!(
            (inst >> 10) & 0x3,
            0b00,
            "LDUR/STUR must be unscaled (bits 11:10 == 00)"
        );
        // And it is distinct from the unsigned-offset form (bits 24 == 0).
        assert_eq!(
            (inst >> 24) & 1,
            0,
            "LDUR/STUR is not the scaled unsigned-offset form"
        );
    }

    /// Unscaled FP load/store — the encodings the negative-frame-offset FP
    /// spill path depends on.
    ///
    /// Each expected word is the GPR LDUR/STUR word from
    /// `test_ldur_stur_unscaled` with bit 26 (V) set, which is exactly what
    /// the ARM ARM "Load/store register (unscaled immediate)" table says
    /// distinguishes the SIMD&FP variant from the general-purpose one.
    #[test]
    fn test_ldur_stur_fp_unscaled() {
        let mut e = Aarch64Emitter::new();

        // LDUR D0, [X1, #-8] = 0xFC5F8020 (GPR form 0xF85F8020 | V bit)
        e.ldur_fp_d(FpReg::D0, Reg::X1, -8);
        assert_eq!(last_inst(&e), 0xFC5F_8020);

        // STUR D2, [SP, #-16] = 0xFC1F03E2 (GPR form 0xF81F03E2 | V bit)
        e.stur_fp_d(FpReg::D2, Reg::SP, -16);
        assert_eq!(last_inst(&e), 0xFC1F_03E2);

        // LDUR S0, [X1, #-4] = 0xBC5FC020
        e.ldur_fp_s(FpReg::D0, Reg::X1, -4);
        assert_eq!(last_inst(&e), 0xBC5F_C020);

        // STUR S3, [X29, #-4] = 0xBC1FC3A3
        e.stur_fp_s(FpReg::D3, Reg::X29, -4);
        assert_eq!(last_inst(&e), 0xBC1F_C3A3);
    }

    /// The unscaled FP forms must not set the index/writeback bit — if bit 10
    /// were 1 the instruction would become pre/post-index and would MUTATE the
    /// base register, corrupting FP on every FP spill (the exact shape of
    /// ARM64 BUG #1 on the GPR side).
    #[test]
    fn test_fp_unscaled_has_no_writeback() {
        for i in 0..4u32 {
            let mut e = Aarch64Emitter::new();
            match i {
                0 => e.ldur_fp_d(FpReg::D5, Reg::X29, -32),
                1 => e.stur_fp_d(FpReg::D5, Reg::X29, -32),
                2 => e.ldur_fp_s(FpReg::D5, Reg::X29, -32),
                _ => e.stur_fp_s(FpReg::D5, Reg::X29, -32),
            }
            let inst = last_inst(&e);
            assert_eq!(
                (inst >> 10) & 0x3,
                0b00,
                "FP LDUR/STUR variant {i} must be unscaled/non-writeback"
            );
            assert_eq!(
                (inst >> 26) & 1,
                1,
                "variant {i} must set the V (SIMD&FP) bit"
            );
            assert_eq!((inst >> 24) & 1, 0, "variant {i} is not the scaled form");
        }
    }

    /// Barrier encodings, checked against the ARM ARM's `CRm` option table.
    ///
    /// These are the primitives any future aarch64 publication/volatile/monitor
    /// lowering has to reach for — nothing in the backend emits them today (see
    /// `docs/jit/aarch64-parity.md`), so pinning their bytes now is the cheapest
    /// way to keep them trustworthy until something does.
    #[test]
    fn test_barrier_encodings() {
        let mut e = Aarch64Emitter::new();

        e.dsb(0b1111); // DSB SY
        assert_eq!(last_inst(&e), 0xD503_3F9F);
        e.dsb(0b1011); // DSB ISH
        assert_eq!(last_inst(&e), 0xD503_3B9F);

        e.dmb(0b1111); // DMB SY
        assert_eq!(last_inst(&e), 0xD503_3FBF);
        // The barrier encoding is `base | CRm << 8`, so CRm lands in the third
        // hex digit from the right: DMB's base is D50330BF, and ISH (CRm=1011)
        // is D5033BBF, not D50333BF. The two literals below were written with
        // that nibble one position out — an easy transposition, and precisely
        // why these bytes are pinned against the ARM ARM rather than against
        // the emitter that produced them.
        e.dmb(0b1011); // DMB ISH
        assert_eq!(last_inst(&e), 0xD503_3BBF);
        e.dmb(0b1010); // DMB ISHST
        assert_eq!(last_inst(&e), 0xD503_3ABF);

        e.isb(); // ISB SY
        assert_eq!(last_inst(&e), 0xD503_3FDF);
    }

    #[test]
    fn test_ldp_stp() {
        let mut e = Aarch64Emitter::new();
        // STP X29, X30, [SP, #-16]!
        e.stp_pre(Reg::X29, Reg::X30, Reg::SP, -16);
        let inst = last_inst(&e);
        // opc=10, V=0, encoding=01 (pre-index), L=0
        assert_eq!((inst >> 30) & 0x3, 0b10); // opc
        assert_eq!((inst >> 22) & 1, 0); // L=0 (store)
        assert_eq!(inst & 0x1F, 29); // Rt1 = X29
        assert_eq!((inst >> 10) & 0x1F, 30); // Rt2 = X30

        // LDP X29, X30, [SP], #16
        e.ldp_post(Reg::X29, Reg::X30, Reg::SP, 16);
        let inst = last_inst(&e);
        assert_eq!((inst >> 22) & 1, 1); // L=1 (load)
    }

    // -- Patch --------------------------------------------------------------

    #[test]
    fn test_patch_branch() {
        let mut e = Aarch64Emitter::new();
        let bpos = e.b(0); // placeholder
        e.nop();
        e.nop();
        let target = e.offset();
        e.patch_branch(bpos, target);

        let inst = inst_at(&e, bpos);
        // Target is 12 bytes from bpos → imm26 = 12/4 = 3
        assert_eq!(inst & 0x03FF_FFFF, 3);
        assert_eq!((inst >> 26) & 0x3F, 0b000101); // still B
    }

    #[test]
    fn test_patch_bcond() {
        let mut e = Aarch64Emitter::new();
        let bpos = e.b_cond(Cond::NE, 0);
        e.nop();
        e.nop();
        e.nop();
        let target = e.offset();
        e.patch_bcond(bpos, target);

        let inst = inst_at(&e, bpos);
        assert_eq!(inst & 0xF, Cond::NE.enc()); // condition preserved
                                                // delta = 16 bytes → imm19 = 4
        assert_eq!((inst >> 5) & 0x7FFFF, 4);
    }

    // -- FP -----------------------------------------------------------------

    #[test]
    fn test_fadd_d() {
        let mut e = Aarch64Emitter::new();
        e.fadd_d(FpReg::D0, FpReg::D1, FpReg::D2);
        let inst = last_inst(&e);
        // 0 0 0 11110 01 1 Rm=00010 0010 10 Rn=00001 Rd=00000
        // = 0x1E622820
        assert_eq!(inst, 0x1E62_2820);
    }

    #[test]
    fn test_fsub_fmul_fdiv() {
        let mut e = Aarch64Emitter::new();
        e.fsub_d(FpReg::D3, FpReg::D4, FpReg::D5);
        let inst = last_inst(&e);
        assert_eq!((inst >> 12) & 0xF, 0b0011); // FSUB opcode

        e.fmul_d(FpReg::D6, FpReg::D7, FpReg::D8);
        let inst = last_inst(&e);
        assert_eq!((inst >> 12) & 0xF, 0b0000); // FMUL opcode

        e.fdiv_d(FpReg::D9, FpReg::D10, FpReg::D11);
        let inst = last_inst(&e);
        assert_eq!((inst >> 12) & 0xF, 0b0001); // FDIV opcode
    }

    #[test]
    fn test_fcmp() {
        let mut e = Aarch64Emitter::new();
        e.fcmp_d(FpReg::D0, FpReg::D1);
        let inst = last_inst(&e);
        // 00011110 01 1 00001 00 1000 00000 00 000
        assert_eq!(inst, 0x1E61_2000);
    }

    #[test]
    fn test_scvtf_fcvtzs() {
        let mut e = Aarch64Emitter::new();
        e.scvtf_d_x(FpReg::D0, Reg::X1);
        let inst = last_inst(&e);
        assert_eq!(inst, 0x9E62_0020);

        e.fcvtzs_x_d(Reg::X2, FpReg::D3);
        let inst = last_inst(&e);
        assert_eq!(inst, 0x9E78_0062);
    }

    // -- System -------------------------------------------------------------

    #[test]
    fn test_nop() {
        let mut e = Aarch64Emitter::new();
        e.nop();
        assert_eq!(last_inst(&e), 0xD503_201F);
    }

    #[test]
    fn test_brk() {
        let mut e = Aarch64Emitter::new();
        e.brk(0);
        assert_eq!(last_inst(&e), 0xD420_0000);
        e.brk(1);
        assert_eq!(last_inst(&e), 0xD420_0020);
    }

    #[test]
    fn test_ret_lr() {
        let mut e = Aarch64Emitter::new();
        e.ret_lr();
        // RET X30 = 0xD65F03C0
        assert_eq!(last_inst(&e), 0xD65F_03C0);
    }

    // -- CMP / MOV ----------------------------------------------------------

    #[test]
    fn test_cmp() {
        let mut e = Aarch64Emitter::new();
        e.cmp(Reg::X1, Reg::X2);
        // SUBS XZR, X1, X2
        let inst = last_inst(&e);
        assert_eq!(inst & 0x1F, 31); // Rd = XZR
        assert_eq!((inst >> 5) & 0x1F, 1); // Rn = X1
        assert_eq!((inst >> 16) & 0x1F, 2); // Rm = X2
    }

    #[test]
    fn test_mov_reg() {
        let mut e = Aarch64Emitter::new();
        e.mov(Reg::X0, Reg::X1);
        // ORR X0, XZR, X1
        let inst = last_inst(&e);
        assert_eq!(inst & 0x1F, 0); // Rd = X0
        assert_eq!((inst >> 5) & 0x1F, 31); // Rn = XZR
        assert_eq!((inst >> 16) & 0x1F, 1); // Rm = X1
    }

    // -- Comprehensive mov_imm64 patterns -----------------------------------

    #[test]
    fn test_mov_imm64_zero() {
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X0, 0);
        assert_eq!(e.code().len(), 4); // single MOVZ
        let inst = inst_at(&e, 0);
        assert_eq!((inst >> 29) & 0x3, 0b10); // MOVZ
        assert_eq!((inst >> 5) & 0xFFFF, 0);
    }

    #[test]
    fn test_mov_imm64_negative_one_pattern() {
        // 0xFFFFFFFF_FFFF0000 has chunks [0x0000, 0xFFFF, 0xFFFF, 0xFFFF]
        // Three 0xFFFF chunks → MOVN strategy better.
        let mut e = Aarch64Emitter::new();
        e.mov_imm64(Reg::X5, 0xFFFF_FFFF_FFFF_0000);
        // Should use MOVN with chunk0's NOT, then no MOVKs needed since all
        // other chunks are 0xFFFF.
        // MOVN X5, #0xFFFF, LSL#0 → sets bits 0..15 to NOT(0xFFFF) = 0x0000, rest to 1
        // Actually: MOVN X5, #0, LSL#0 → NOT(0) = 0xFFFF_FFFF_FFFF_FFFF, but
        // we need 0xFFFF_FFFF_FFFF_0000. So MOVN X5, #0xFFFF, LSL#0.
        // NOT of (0x000000000000FFFF) = 0xFFFFFFFFFFFF0000. Correct!
        assert_eq!(e.code().len(), 4); // single instruction
    }

    // -- Branch-offset range checks ----------------------------------------

    #[test]
    fn test_imm_fits_boundaries() {
        // imm26 (B/BL): ±128 MB, 4-aligned.
        assert!(imm26_fits(0));
        assert!(imm26_fits((1 << 27) - 4));
        assert!(imm26_fits(-(1 << 27)));
        assert!(!imm26_fits(1 << 27)); // just out of range
        assert!(!imm26_fits(-(1 << 27) - 4));
        assert!(!imm26_fits(2)); // not 4-aligned

        // imm19 (B.cond/CBZ/CBNZ/LDR-literal): ±1 MB.
        assert!(imm19_fits((1 << 20) - 4));
        assert!(imm19_fits(-(1 << 20)));
        assert!(!imm19_fits(1 << 20));
        assert!(!imm19_fits(-(1 << 20) - 4));
        assert!(!imm19_fits(1)); // not 4-aligned

        // imm14 (TBZ/TBNZ): ±32 KB.
        assert!(imm14_fits((1 << 15) - 4));
        assert!(imm14_fits(-(1 << 15)));
        assert!(!imm14_fits(1 << 15));
        assert!(!imm14_fits(-(1 << 15) - 4));

        // imm21 (ADR/ADRP): 21-bit signed, unscaled.
        assert!(imm21_fits((1 << 20) - 1));
        assert!(imm21_fits(-(1 << 20)));
        assert!(!imm21_fits(1 << 20));
        assert!(!imm21_fits(-(1 << 20) - 1));
    }

    #[test]
    fn test_in_range_branch_does_not_overflow() {
        let mut e = Aarch64Emitter::new();
        e.b(0x100);
        e.b_cond(Cond::EQ, -0x40);
        e.tbz(Reg::X0, 5, 8);
        e.cbz(Reg::X3, (1 << 20) - 4);
        assert!(
            !e.overflowed(),
            "in-range branches must not set the overflow flag"
        );
    }

    #[test]
    fn test_in_range_patch_does_not_overflow() {
        let mut e = Aarch64Emitter::new();
        let p = e.b(0);
        // Patch to a nearby in-range target.
        e.patch_branch(p, p + 0x20);
        assert!(!e.overflowed());
    }

    // `mark_branch_overflow` does two things on an out-of-range branch: it
    // trips a `debug_assert!` (loud panic in debug/test builds) AND sets the
    // sticky `overflowed` flag (always) so release builds discard the buffer
    // and bail to the interpreter. `debug_assert!` is compiled OUT under
    // `--release`, so a release test run sees no panic — these tests must
    // check the mechanism that is actually active for the build: the panic in
    // debug, the sticky flag in release. (Previously they only asserted the
    // panic and so spuriously FAILED under `cargo test --release`.)
    fn assert_branch_overflow_detected<F>(emit: F)
    where
        F: Fn(&mut Aarch64Emitter),
    {
        if cfg!(debug_assertions) {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {})); // silence the panic message
            let emit_ref = &emit;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut e = Aarch64Emitter::new();
                emit_ref(&mut e);
            }));
            std::panic::set_hook(prev);
            assert!(
                result.is_err(),
                "expected an out-of-range branch offset to trip debug_assert!"
            );
        } else {
            let mut e = Aarch64Emitter::new();
            emit(&mut e);
            assert!(
                e.overflowed(),
                "expected an out-of-range branch offset to set the sticky overflowed flag"
            );
        }
    }

    #[test]
    fn test_b_cond_beyond_1mb_overflows() {
        // > ±1 MB conditional branch must not silently truncate.
        assert_branch_overflow_detected(|e| {
            e.b_cond(Cond::NE, 1 << 20);
        });
    }

    #[test]
    fn test_tbz_beyond_32kb_overflows() {
        // > ±32 KB test-bit branch must not silently truncate.
        assert_branch_overflow_detected(|e| {
            e.tbz(Reg::X0, 3, 1 << 15);
        });
    }

    #[test]
    fn test_b_beyond_128mb_overflows() {
        assert_branch_overflow_detected(|e| {
            e.b(1 << 27);
        });
    }

    #[test]
    fn test_patch_branch_out_of_range_overflows() {
        assert_branch_overflow_detected(|e| {
            let p = e.b(0);
            e.patch_branch(p, p + (1 << 27));
        });
    }
}
