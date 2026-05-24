//! ARM64 (AArch64) JIT backend compilation pipeline.
//!
//! Translates JVM bytecode into a sequence of `Arm64Instruction` pseudo-ops that
//! represent the compilation result.  A separate encoding step (using `aarch64.rs`)
//! can later lower these to raw machine code bytes.
//!
//! ## Calling Convention (AAPCS64)
//!
//! - Integer args: X0-X7, return in X0
//! - Floating-point args: V0-V7, return in V0
//! - Callee-saved: X19-X28, FP (X29), LR (X30)
//! - Stack must be 16-byte aligned at all times
//!
//! ## Stack Layout (after prologue)
//!
//! ```text
//! [FP + 16]    = return address (saved LR)
//! [FP + 8]     = saved FP
//! [FP]         = <- FP points here
//! [FP - 8]     = local 0  (or in X19)
//! [FP - 16]    = local 1  (or in X20)
//! ...
//! [FP - N*8]   = spill slots / operand stack
//! ```

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Arm64Register
// ---------------------------------------------------------------------------

/// Lightweight register identifier for the backend pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Arm64Register(pub u8);

impl Arm64Register {
    // Argument / result registers
    pub const X0: Self = Self(0);
    pub const X1: Self = Self(1);
    pub const X2: Self = Self(2);
    pub const X3: Self = Self(3);
    pub const X4: Self = Self(4);
    pub const X5: Self = Self(5);
    pub const X6: Self = Self(6);
    pub const X7: Self = Self(7);

    // Scratch / temporary registers
    pub const X8: Self = Self(8);
    pub const X9: Self = Self(9);
    pub const X10: Self = Self(10);
    pub const X11: Self = Self(11);
    pub const X12: Self = Self(12);
    pub const X13: Self = Self(13);
    pub const X14: Self = Self(14);
    pub const X15: Self = Self(15);
    pub const X16: Self = Self(16); // IP0
    pub const X17: Self = Self(17); // IP1
    pub const X18: Self = Self(18); // Platform register

    // Callee-saved registers (for locals)
    pub const X19: Self = Self(19);
    pub const X20: Self = Self(20);
    pub const X21: Self = Self(21);
    pub const X22: Self = Self(22);
    pub const X23: Self = Self(23);
    pub const X24: Self = Self(24);
    pub const X25: Self = Self(25);
    pub const X26: Self = Self(26);
    pub const X27: Self = Self(27);
    pub const X28: Self = Self(28);

    // Special registers
    pub const FP: Self = Self(29);
    pub const LR: Self = Self(30);
    pub const SP: Self = Self(31);
    pub const XZR: Self = Self(31); // Context-dependent zero register

    // NEON SIMD registers (encoded as 32+n)
    pub const V0: Self = Self(32);
    pub const V1: Self = Self(33);
    pub const V2: Self = Self(34);
    pub const V3: Self = Self(35);
    pub const V4: Self = Self(36);
    pub const V5: Self = Self(37);
    pub const V6: Self = Self(38);
    pub const V7: Self = Self(39);

    /// Returns `true` if this is a callee-saved general-purpose register (X19-X28).
    pub fn is_callee_saved(self) -> bool {
        (19..=28).contains(&self.0)
    }

    /// Returns `true` if this register is used for integer argument passing.
    pub fn is_arg_reg(self) -> bool {
        self.0 <= 7
    }

    /// Raw numeric index.
    pub fn index(self) -> u8 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Arm64Condition
// ---------------------------------------------------------------------------

/// ARM64 condition codes mapping to the NZCV flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64Condition {
    /// Equal (Z=1)
    Eq,
    /// Not equal (Z=0)
    Ne,
    /// Signed less than (N!=V)
    Lt,
    /// Signed less or equal (Z=1 or N!=V)
    Le,
    /// Signed greater than (Z=0 and N=V)
    Gt,
    /// Signed greater or equal (N=V)
    Ge,
    /// Unsigned higher (C=1 and Z=0)
    Hi,
    /// Unsigned lower or same (C=0 or Z=1)
    Ls,
    /// Carry set / unsigned higher or same (C=1)
    Cs,
    /// Carry clear / unsigned lower (C=0)
    Cc,
    /// Always
    Al,
}

// ---------------------------------------------------------------------------
// Arm64Instruction
// ---------------------------------------------------------------------------

/// Backend IR instruction set for the ARM64 pipeline.
///
/// These are pseudo-instructions that map 1:1 to real ARM64 ops but carry
/// higher-level label references instead of raw offsets.
#[derive(Debug, Clone)]
pub enum Arm64Instruction {
    // -- Arithmetic --
    Add { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    AddImm { rd: Arm64Register, rn: Arm64Register, imm: i32 },
    Sub { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    SubImm { rd: Arm64Register, rn: Arm64Register, imm: i32 },
    Mul { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    SDiv { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Neg { rd: Arm64Register, rn: Arm64Register },
    Madd { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register, ra: Arm64Register },
    Msub { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register, ra: Arm64Register },

    // -- Logical --
    And { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Orr { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Eor { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Lsl { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Lsr { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },
    Asr { rd: Arm64Register, rn: Arm64Register, rm: Arm64Register },

    // -- Compare --
    Cmp { rn: Arm64Register, rm: Arm64Register },
    CmpImm { rn: Arm64Register, imm: i32 },
    Tst { rn: Arm64Register, rm: Arm64Register },

    // -- Move --
    Mov { rd: Arm64Register, rm: Arm64Register },
    MovImm { rd: Arm64Register, imm: i64 },
    MovK { rd: Arm64Register, imm: u16, shift: u8 },

    // -- Load / Store --
    Ldr { rt: Arm64Register, rn: Arm64Register, offset: i32 },
    Str { rt: Arm64Register, rn: Arm64Register, offset: i32 },
    Ldp { rt1: Arm64Register, rt2: Arm64Register, rn: Arm64Register, offset: i32 },
    Stp { rt1: Arm64Register, rt2: Arm64Register, rn: Arm64Register, offset: i32 },
    LdrLiteral { rt: Arm64Register, label: u32 },

    // -- Branch --
    B { label: u32 },
    BCond { cond: Arm64Condition, label: u32 },
    Bl { label: u32 },
    Br { rn: Arm64Register },
    Blr { rn: Arm64Register },
    Ret,
    Cbz { rt: Arm64Register, label: u32 },
    Cbnz { rt: Arm64Register, label: u32 },

    // -- Conversion --
    ScvtfDouble { vd: Arm64Register, rn: Arm64Register },
    FcvtzsInt { rd: Arm64Register, vn: Arm64Register },

    // -- FP move (bit-pattern transfer between GP and FP registers) --
    /// FMOV Vd, Xn — move GP register to FP register (bit-pattern, no conversion).
    FmovToFp { vd: Arm64Register, rn: Arm64Register },
    /// FMOV Xd, Vn — move FP register to GP register (bit-pattern, no conversion).
    FmovFromFp { rd: Arm64Register, vn: Arm64Register },
    /// FMOV Vd, Vn — move between FP registers.
    FmovFp { vd: Arm64Register, vn: Arm64Register },

    // -- FP negate --
    FnegDouble { vd: Arm64Register, vn: Arm64Register },

    // -- FP single-precision ops --
    FaddSingle { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FsubSingle { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FmulSingle { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FdivSingle { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FcmpSingle { vn: Arm64Register, vm: Arm64Register },
    FnegSingle { vd: Arm64Register, vn: Arm64Register },

    /// SCVTF Sd, Wn — convert 32-bit int to single-precision float.
    ScvtfSingle { vd: Arm64Register, rn: Arm64Register },
    /// FCVTZS Wd, Sn — convert single-precision float to 32-bit int (truncate toward zero).
    FcvtzsSingle { rd: Arm64Register, vn: Arm64Register },
    /// FCVT Dd, Sn — convert single to double.
    FcvtSingleToDouble { vd: Arm64Register, vn: Arm64Register },
    /// FCVT Sd, Dn — convert double to single.
    FcvtDoubleToSingle { vd: Arm64Register, vn: Arm64Register },

    /// LDR (FP) — load from [base + offset] to FP register.
    FpLdr { vt: Arm64Register, rn: Arm64Register, offset: i32, is_double: bool },
    /// STR (FP) — store FP register to [base + offset].
    FpStr { vt: Arm64Register, rn: Arm64Register, offset: i32, is_double: bool },

    // -- NEON SIMD (double precision) --
    FaddDouble { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FsubDouble { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FmulDouble { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FdivDouble { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    FcmpDouble { vn: Arm64Register, vm: Arm64Register },

    // -- NEON SIMD (integer vector, 4x32) --
    /// LD1 {Vt.4S}, [Xn] — load 128-bit vector from memory.
    NeonLd1_4s { vt: Arm64Register, rn: Arm64Register },
    /// ST1 {Vt.4S}, [Xn] — store 128-bit vector to memory.
    NeonSt1_4s { vt: Arm64Register, rn: Arm64Register },
    /// ADD Vd.4S, Vn.4S, Vm.4S — vector integer add (4x i32).
    NeonAdd4s { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },
    /// MUL Vd.4S, Vn.4S, Vm.4S — vector integer multiply (4x i32).
    NeonMul4s { vd: Arm64Register, vn: Arm64Register, vm: Arm64Register },

    // -- System --
    Nop,
    Brk { imm: u16 },

    // -- Pseudo-instructions --
    Label(u32),
    Comment(String),
    /// Raw 64-bit constant data embedded in the instruction stream (literal pool).
    /// The label is bound to the position of this data so that LdrLiteral can
    /// reference it.
    ConstantPoolEntry { label: u32, value: u64 },
}

// ---------------------------------------------------------------------------
// Arm64CallingConvention
// ---------------------------------------------------------------------------

/// AAPCS64 calling convention constants and helpers.
pub struct Arm64CallingConvention;

impl Arm64CallingConvention {
    /// Integer argument registers: X0-X7.
    pub const INT_ARG_REGS: &'static [Arm64Register] = &[
        Arm64Register::X0, Arm64Register::X1, Arm64Register::X2, Arm64Register::X3,
        Arm64Register::X4, Arm64Register::X5, Arm64Register::X6, Arm64Register::X7,
    ];

    /// Callee-saved registers: X19-X28.
    pub const CALLEE_SAVED: &'static [Arm64Register] = &[
        Arm64Register::X19, Arm64Register::X20, Arm64Register::X21, Arm64Register::X22,
        Arm64Register::X23, Arm64Register::X24, Arm64Register::X25, Arm64Register::X26,
        Arm64Register::X27, Arm64Register::X28,
    ];

    pub const RETURN_REG: Arm64Register = Arm64Register::X0;
    pub const FRAME_POINTER: Arm64Register = Arm64Register::FP;
    pub const LINK_REG: Arm64Register = Arm64Register::LR;
    pub const STACK_POINTER: Arm64Register = Arm64Register::SP;

    /// Stack alignment requirement (16 bytes on ARM64).
    pub const STACK_ALIGNMENT: usize = 16;

    /// Red-zone size (ARM64 AAPCS64 does not define a red zone).
    pub const RED_ZONE: usize = 0;

    /// Get the register for the n-th integer argument, if available.
    pub fn int_arg_reg(n: usize) -> Option<Arm64Register> {
        Self::INT_ARG_REGS.get(n).copied()
    }

    /// Map the n-th local variable to a callee-saved register, if available.
    pub fn local_reg(n: usize) -> Option<Arm64Register> {
        Self::CALLEE_SAVED.get(n).copied()
    }
}

// ---------------------------------------------------------------------------
// Arm64FrameLayout
// ---------------------------------------------------------------------------

/// Describes the stack frame geometry for a compiled method.
pub struct Arm64FrameLayout {
    /// Total frame size in bytes (16-byte aligned).
    pub frame_size: i32,
    /// Byte offset from FP where callee-saved registers are stored.
    pub callee_save_offset: i32,
    /// Byte offset from FP where spill slots begin.
    pub spill_offset: i32,
    /// Number of spill slots.
    pub num_spills: usize,
    /// Which callee-saved registers must be preserved.
    pub saved_regs: Vec<Arm64Register>,
    /// How many locals got a dedicated register.
    pub num_reg_locals: usize,
}

impl Arm64FrameLayout {
    /// Compute the frame layout given method metadata.
    ///
    /// The layout reserves 16 bytes for FP/LR (saved by STP in the prologue),
    /// then space for callee-saved registers, then spill slots.  Everything is
    /// rounded up to a 16-byte boundary.
    pub fn compute(_num_locals: usize, num_spills: usize, saved_regs: &[Arm64Register]) -> Self {
        let num_reg_locals = saved_regs.len();

        // FP/LR pair is saved separately (16 bytes).
        // Callee-saved regs: round count up to even for STP pairing.
        let num_saved = saved_regs.len();
        let callee_save_bytes = ((num_saved + 1) / 2) * 16; // pairs of 8-byte regs

        let spill_bytes = num_spills * 8;

        // Total = FP/LR (16) + callee-save area + spill area, aligned to 16.
        let raw = 16 + callee_save_bytes + spill_bytes;
        let frame_size = align_up(raw, 16) as i32;

        // Offsets are negative from FP.
        let callee_save_offset = -16 - callee_save_bytes as i32;
        let spill_offset = callee_save_offset - spill_bytes as i32;

        Self {
            frame_size,
            callee_save_offset,
            spill_offset,
            num_spills,
            saved_regs: saved_regs.to_vec(),
            num_reg_locals: num_reg_locals,
        }
    }
}

/// Round `value` up to the next multiple of `align`.
fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Arm64CodeBuffer
// ---------------------------------------------------------------------------

/// Accumulates `Arm64Instruction`s and manages labels.
pub struct Arm64CodeBuffer {
    instructions: Vec<Arm64Instruction>,
    labels: HashMap<u32, usize>,
    next_label: u32,
}

impl Arm64CodeBuffer {
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            labels: HashMap::new(),
            next_label: 0,
        }
    }

    /// Append an instruction.
    pub fn emit(&mut self, inst: Arm64Instruction) {
        self.instructions.push(inst);
    }

    /// Allocate a fresh label id.
    pub fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Bind `label` to the current instruction index and emit a `Label` pseudo-op.
    pub fn bind_label(&mut self, label: u32) {
        self.labels.insert(label, self.instructions.len());
        self.instructions.push(Arm64Instruction::Label(label));
    }

    pub fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    pub fn instructions(&self) -> &[Arm64Instruction] {
        &self.instructions
    }

    /// Every ARM64 instruction is 4 bytes (fixed-width encoding).
    pub fn estimated_size(&self) -> usize {
        self.instructions.len() * 4
    }
}

// ---------------------------------------------------------------------------
// Arm64CompileResult
// ---------------------------------------------------------------------------

/// Output of the compilation pipeline.
pub struct Arm64CompileResult {
    pub instructions: Vec<Arm64Instruction>,
    pub frame: Arm64FrameLayout,
    pub labels: HashMap<u32, usize>,
    pub success: bool,
    /// T1.1.3 — precise oop maps indexed by native PC offset.
    ///
    /// Each entry records the frame-slot offsets (relative to FP on
    /// AArch64, x86-64 uses RBP) that hold object references at a
    /// GC-triggering safepoint. The walker uses the same
    /// [`crate::OopMapEntry`] type as x64 so a single data format
    /// works across both backends.
    ///
    /// Populated at safepoint call sites during codegen (`new`,
    /// `anewarray`, `newarray`, method dispatch). When empty the
    /// `CompiledMethod`'s oop-map table is also empty and the GC
    /// root walker falls back to the conservative stack scan.
    pub oop_maps: Vec<crate::OopMapEntry>,
}

// ---------------------------------------------------------------------------
// Arm64Backend
// ---------------------------------------------------------------------------

/// The main ARM64 compilation pipeline.
///
/// Translates JVM bytecode into `Arm64Instruction` sequences using a
/// simulated operand stack (compile-time stack mapping).
pub struct Arm64Backend {
    buffer: Arm64CodeBuffer,
    frame: Option<Arm64FrameLayout>,
    /// Register (or spill slot) assignment for each local variable (GPR).
    local_regs: Vec<Option<Arm64Register>>,
    /// FP register assignment for float/double locals.
    /// When a float local has a dedicated FP register (D8-D15), it is stored here.
    float_local_regs: Vec<Option<Arm64Register>>,
    /// Simulated operand stack (tracks which register holds each stack slot).
    operand_stack: Vec<Arm64Register>,
    /// Simulated float operand stack (V registers).
    float_operand_stack: Vec<Arm64Register>,
    /// Next scratch register to hand out (cycles through X9-X15).
    scratch_cursor: u8,
    /// Next float scratch register to hand out (cycles through V0-V7).
    float_scratch_cursor: u8,
    /// Bytecode PC -> label mapping for branch targets.
    pc_labels: HashMap<usize, u32>,
    /// Label for the shared epilogue.
    epilogue_label: u32,
    /// Number of parameters for the current method (used for self-recursive calls).
    num_params: usize,
    /// Method invoke metadata: maps constant pool index to argument count.
    /// Populated by the caller (from resolved constant pool invoke entries).
    method_info: HashMap<u16, usize>,
    /// Set to true if a compilation error occurred (e.g. stack underflow).
    pub failed: bool,
    /// Spill map: register -> frame offset for spilled operand stack entries.
    spill_map: HashMap<Arm64Register, i32>,
    /// T1.1.3 — parallel oop-mark vector for `operand_stack`.
    ///
    /// `operand_stack_oop_marks[i] == true` means the register at
    /// `operand_stack[i]` currently holds an object reference. Pushed
    /// in lock-step with the operand stack by the opcode handlers;
    /// any push that isn't explicitly tagged defaults to `false` and
    /// the conservative-sweep fallback in
    /// `vm/src/jit/conservative_roots.rs::scan_one_frame_precise`
    /// catches anything we miss.
    operand_stack_oop_marks: Vec<bool>,
    /// T1.1.3 — collected oop maps, each keyed by the native PC
    /// offset (in the finalized instruction stream) of the
    /// instruction immediately after a safepoint call.
    pub oop_maps: Vec<crate::OopMapEntry>,
}

/// Scratch registers available for the operand stack (X9-X15, 7 regs).
const SCRATCH_REGS: [Arm64Register; 7] = [
    Arm64Register::X9,  Arm64Register::X10, Arm64Register::X11,
    Arm64Register::X12, Arm64Register::X13, Arm64Register::X14,
    Arm64Register::X15,
];

/// Float scratch registers for the float operand stack (V0-V7, 8 regs).
const FLOAT_SCRATCH_REGS: [Arm64Register; 8] = [
    Arm64Register::V0, Arm64Register::V1, Arm64Register::V2,
    Arm64Register::V3, Arm64Register::V4, Arm64Register::V5,
    Arm64Register::V6, Arm64Register::V7,
];

impl Arm64Backend {
    pub fn new() -> Self {
        Self {
            buffer: Arm64CodeBuffer::new(),
            frame: None,
            local_regs: Vec::new(),
            float_local_regs: Vec::new(),
            operand_stack: Vec::new(),
            float_operand_stack: Vec::new(),
            scratch_cursor: 0,
            float_scratch_cursor: 0,
            pc_labels: HashMap::new(),
            epilogue_label: 0,
            num_params: 0,
            method_info: HashMap::new(),
            failed: false,
            spill_map: HashMap::new(),
            operand_stack_oop_marks: Vec::new(),
            oop_maps: Vec::new(),
        }
    }

    /// T1.1.3 — mark the top of the operand stack as holding an
    /// object reference. Called by opcode handlers after any push
    /// that produces an oop (`aconst_null`, `aload*`, `aaload`,
    /// `new`, `anewarray`, `newarray`, object-returning
    /// `invoke*`). See the parallel `stack_oop_marks` mechanism in
    /// `jit/src/x64.rs` for the x86-64 counterpart.
    #[allow(dead_code)]
    fn mark_top_operand_as_oop(&mut self) {
        // Lazy sync: if the oop-marks vector is shorter than the
        // operand stack, pad with false entries. This happens when
        // a handler pushed without going through a marking helper;
        // the resulting map is a safe under-approximation and the
        // conservative sweep in the GC walker catches what we miss.
        while self.operand_stack_oop_marks.len() < self.operand_stack.len() {
            self.operand_stack_oop_marks.push(false);
        }
        if let Some(last) = self.operand_stack_oop_marks.last_mut() {
            *last = true;
        }
    }

    /// T1.1.3 — record an oop map at the current native PC.
    ///
    /// Called immediately after a safepoint-producing instruction
    /// (object-allocating helper call, method dispatch). Walks the
    /// parallel `operand_stack_oop_marks` + `spill_map` to find
    /// frame-slot offsets that currently hold oops, and emits an
    /// `OopMapEntry` keyed by the current `buffer.len()` position.
    ///
    /// Empty maps are skipped to keep per-method storage bounded:
    /// the GC walker falls back to conservative scanning for that
    /// frame, which is always a correct super-set of the precise
    /// coverage.
    #[allow(dead_code)]
    fn emit_oop_map_for_safepoint(&mut self) {
        if self.failed {
            return;
        }
        // Lazy resync per the mark helper above.
        while self.operand_stack_oop_marks.len() < self.operand_stack.len() {
            self.operand_stack_oop_marks.push(false);
        }
        self.operand_stack_oop_marks
            .truncate(self.operand_stack.len());

        // On AArch64 every instruction is 4 bytes fixed-width, so the
        // native PC offset of "the instruction after the safepoint"
        // equals `instruction_count * 4`. When the buffer is later
        // emitted by the runtime this multiplier may change (e.g. if
        // instructions are merged or re-encoded); in that case the
        // runtime is responsible for translating the recorded offset
        // via its post-emit relocation pass. For now we record the
        // instruction-count form.
        let native_pc = (self.buffer.instruction_count() * 4) as u32;
        let mut slots: Vec<i16> = Vec::new();
        // Walk spilled slots and emit their frame offsets.
        // AArch64 frame offsets are computed relative to FP; the
        // GC walker on aarch64 adds the entry SP (= FP at call time)
        // to each slot offset to read the qword.
        for (i, &mark) in self.operand_stack_oop_marks.iter().enumerate() {
            if !mark {
                continue;
            }
            let reg = match self.operand_stack.get(i) {
                Some(r) => *r,
                None => continue,
            };
            if let Some(&spill_off) = self.spill_map.get(&reg) {
                if let Ok(off16) = i16::try_from(spill_off) {
                    slots.push(off16);
                }
            }
            // Register-resident oops are preserved by the ABI
            // (X19-X28 are callee-saved on AAPCS64); the walker's
            // conservative fallback catches them since they remain
            // live in the caller's saved-register area during a
            // safepoint.
        }
        if slots.is_empty() {
            return;
        }
        self.oop_maps.push(crate::OopMapEntry {
            native_pc_offset: native_pc,
            frame_slot_offsets: slots,
        });
    }

    /// Allocate the next scratch register.
    ///
    /// When the operand stack depth exceeds the number of physical scratch
    /// registers (X9-X15), we spill the oldest live value to the stack frame
    /// before reusing its register, preventing silent data corruption.
    fn alloc_scratch(&mut self) -> Arm64Register {
        let r = SCRATCH_REGS[self.scratch_cursor as usize % SCRATCH_REGS.len()];
        // If we've wrapped around and this register is still live on the operand
        // stack, spill it to a frame spill slot before reusing.
        if self.scratch_cursor as usize >= SCRATCH_REGS.len() {
            if let Some(pos) = self.operand_stack.iter().position(|&reg| reg == r) {
                let frame = self.frame.as_ref().unwrap();
                let spill_slot = frame.num_reg_locals + pos;
                let offset = frame.spill_offset
                    + i32::try_from(spill_slot).unwrap_or(i32::MAX).saturating_mul(8);
                // Spill the register to its frame slot.
                self.buffer.emit(Arm64Instruction::Str {
                    rt: r,
                    rn: Arm64Register::FP,
                    offset,
                });
                // Record the spill so pop_operand can reload it.
                self.spill_map.insert(r, offset);
            }
        }
        self.scratch_cursor += 1;
        r
    }

    /// Push a value onto the simulated operand stack.
    fn push_operand(&mut self, reg: Arm64Register) {
        self.operand_stack.push(reg);
    }

    /// Pop the top of the simulated operand stack.
    /// If the register was spilled, emits a reload from the frame slot.
    /// Returns X0 as sentinel and sets `self.failed = true` on underflow.
    pub fn pop_operand(&mut self) -> Arm64Register {
        let reg = self.operand_stack.pop().unwrap_or_else(|| {
            self.failed = true;
            Arm64Register::X0
        });
        // If this register was spilled, reload it from the frame slot.
        if let Some(offset) = self.spill_map.remove(&reg) {
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            });
        }
        reg
    }

    /// Allocate the next float scratch register (round-robin over V0-V7).
    fn alloc_float_scratch(&mut self) -> Arm64Register {
        let r = FLOAT_SCRATCH_REGS[self.float_scratch_cursor as usize % FLOAT_SCRATCH_REGS.len()];
        self.float_scratch_cursor += 1;
        r
    }

    /// Push a value onto the simulated float operand stack.
    fn push_float_operand(&mut self, reg: Arm64Register) {
        self.float_operand_stack.push(reg);
    }

    /// Pop the top of the simulated float operand stack.
    /// Returns V0 as sentinel and sets `self.failed = true` on underflow.
    fn pop_float_operand(&mut self) -> Arm64Register {
        self.float_operand_stack.pop().unwrap_or_else(|| {
            self.failed = true;
            Arm64Register::V0
        })
    }

    /// Get or create a label for a bytecode PC.
    fn label_for_pc(&mut self, pc: usize) -> u32 {
        if let Some(&label) = self.pc_labels.get(&pc) {
            label
        } else {
            let label = self.buffer.new_label();
            self.pc_labels.insert(pc, label);
            label
        }
    }

    // -- Prologue / Epilogue ------------------------------------------------

    /// Emit the standard AAPCS64 prologue.
    fn emit_prologue(&mut self) {
        let frame = match self.frame.as_ref() {
            Some(f) => f,
            None => { self.failed = true; return; }
        };
        let frame_size = frame.frame_size;

        // Save FP and LR.
        self.buffer.emit(Arm64Instruction::Stp {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: -16,
        });

        // Set up frame pointer.
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::FP,
            rm: Arm64Register::SP,
        });

        // Allocate stack space.
        if frame_size > 16 {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: frame_size - 16, // 16 already consumed by STP above
            });
        }

        // Save callee-saved registers used for locals (in pairs).
        let saved = &frame.saved_regs.clone();
        let mut i = 0;
        while i + 1 < saved.len() {
            let offset = frame.callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Stp {
                rt1: saved[i],
                rt2: saved[i + 1],
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved.len() {
            let offset = frame.callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Str {
                rt: saved[i],
                rn: Arm64Register::FP,
                offset,
            });
        }
    }

    /// Emit the standard AAPCS64 epilogue.
    fn emit_epilogue(&mut self) {
        let frame = match self.frame.as_ref() {
            Some(f) => f,
            None => { self.failed = true; return; }
        };
        let callee_save_offset = frame.callee_save_offset;
        let saved = frame.saved_regs.clone();

        // Bind epilogue label.
        self.buffer.bind_label(self.epilogue_label);

        // Restore callee-saved registers.
        let mut i = 0;
        while i + 1 < saved.len() {
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldp {
                rt1: saved[i],
                rt2: saved[i + 1],
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved.len() {
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: saved[i],
                rn: Arm64Register::FP,
                offset,
            });
        }

        // Restore SP from FP, then restore FP/LR.
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::SP,
            rm: Arm64Register::FP,
        });
        self.buffer.emit(Arm64Instruction::Ldp {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Ret);
    }

    // -- Bytecode compilation -----------------------------------------------

    /// Compile a JVM method to ARM64 instructions.
    /// Compile a JVM bytecode method to ARM64 instructions.
    ///
    /// `method_info` maps constant pool indices (from invokestatic operands) to the
    /// number of arguments the target method expects.  When the map does not contain
    /// an entry for a given CP index the backend assumes a self-recursive call and
    /// uses `num_params` as the argument count.
    pub fn compile_method(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
    ) -> Arm64CompileResult {
        self.compile_method_with_info(num_locals, num_params, max_stack, bytecode, HashMap::new())
    }

    /// Like [`compile_method`] but accepts an explicit method-info map for invoke
    /// resolution.
    pub fn compile_method_with_info(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
    ) -> Arm64CompileResult {
        // Reset state.
        self.buffer = Arm64CodeBuffer::new();
        self.operand_stack.clear();
        self.float_operand_stack.clear();
        self.scratch_cursor = 0;
        self.float_scratch_cursor = 0;
        self.pc_labels.clear();
        self.local_regs.clear();
        self.float_local_regs.clear();
        self.spill_map.clear();
        self.num_params = num_params;
        self.method_info = method_info;

        // Run graph-coloring register allocation for ARM64.
        let alloc = super::regalloc::allocate_registers_arm64(
            bytecode, bytecode.len(), num_locals, num_params, &[],
        );

        // Build local_regs from GPR assignments (u8 register numbers → Arm64Register).
        for &a in &alloc.assignments {
            self.local_regs.push(a.map(Arm64Register));
        }
        // Pad if allocator returned fewer entries than num_locals
        while self.local_regs.len() < num_locals {
            self.local_regs.push(None);
        }

        // Build float_local_regs from FP assignments.
        // FP register numbers 8-15 map to callee-saved D8-D15.
        // We encode them as 32+n for the Arm64Register representation.
        for &a in &alloc.xmm_assignments {
            self.float_local_regs.push(a.map(|n| Arm64Register(32 + n)));
        }
        while self.float_local_regs.len() < num_locals {
            self.float_local_regs.push(None);
        }

        // Determine which callee-saved GPR regs we actually use.
        let saved_regs: Vec<Arm64Register> = alloc.used_callee_saved
            .iter()
            .map(|&n| Arm64Register(n))
            .collect();

        // Count spills: locals without a register + operand stack space.
        let gpr_spills = alloc.assignments.iter().filter(|a| a.is_none()).count();
        let fp_spills = alloc.xmm_assignments.iter()
            .enumerate()
            .filter(|(i, a)| a.is_none() && self.local_regs.get(*i).map_or(false, |g| g.is_none()))
            .count();
        let _ = fp_spills; // float spills use the same frame slots
        let num_spills = gpr_spills + max_stack;
        self.frame = Some(Arm64FrameLayout::compute(num_locals, num_spills, &saved_regs));

        self.epilogue_label = self.buffer.new_label();

        // Emit prologue.
        self.emit_prologue();

        // Copy incoming args to local registers.
        for i in 0..num_params.min(8) {
            if let Some(local_reg) = self.local_regs.get(i).copied().flatten() {
                let arg_reg = Arm64CallingConvention::INT_ARG_REGS[i];
                if arg_reg != local_reg {
                    self.buffer.emit(Arm64Instruction::Mov { rd: local_reg, rm: arg_reg });
                }
            }
        }

        // Walk bytecode.
        let mut pc = 0;
        let mut success = true;
        while pc < bytecode.len() {
            // Bind label if any branch targets this PC.
            if let Some(&label) = self.pc_labels.get(&pc) {
                if !self.buffer.labels.contains_key(&label) {
                    self.buffer.bind_label(label);
                }
            }

            let opcode = bytecode[pc];
            let start_pc = pc;
            pc += 1;

            match opcode {
                // iconst_m1 .. iconst_5
                0x02..=0x08 => {
                    let value = opcode as i32 - 3;
                    self.emit_iconst(value);
                }
                // bipush
                0x10 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let val = bytecode[pc] as i8 as i32;
                    pc += 1;
                    self.emit_iconst(val);
                }
                // sipush
                0x11 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let val = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    self.emit_iconst(val);
                }
                // iload_0 .. iload_3
                0x1a..=0x1d => self.emit_iload((opcode - 0x1a) as usize),
                // iload (wide index)
                0x15 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_iload(idx);
                }
                // istore_0 .. istore_3
                0x3b..=0x3e => self.emit_istore((opcode - 0x3b) as usize),
                // istore (wide index)
                0x36 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_istore(idx);
                }
                // iadd
                0x60 => self.emit_int_add(),
                // isub
                0x64 => self.emit_int_sub(),
                // imul
                0x68 => self.emit_int_mul(),
                // idiv
                0x6c => self.emit_int_div(),
                // ineg
                0x74 => self.emit_int_neg(),
                // iand
                0x7e => self.emit_int_and(),
                // ior
                0x80 => self.emit_int_or(),
                // ixor
                0x82 => self.emit_int_xor(),
                // ishl
                0x78 => self.emit_int_shl(),
                // ishr
                0x7a => self.emit_int_shr(),
                // if_icmpeq
                0x9f => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Eq, target);
                }
                // if_icmpne
                0xa0 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Ne, target);
                }
                // if_icmplt
                0xa1 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Lt, target);
                }
                // if_icmpge
                0xa2 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Ge, target);
                }
                // if_icmpgt
                0xa3 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Gt, target);
                }
                // if_icmple
                0xa4 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Le, target);
                }
                // goto
                0xa7 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::B { label });
                }
                // ireturn / lreturn
                0xac | 0xad => self.emit_return_int(),
                // freturn / dreturn — return FP value in V0
                0xae | 0xaf => {
                    let _val = self.pop_float_operand();
                    // V0 is already the return register; emit Ret
                    self.buffer.emit(Arm64Instruction::Ret);
                }
                // areturn
                0xb0 => self.emit_return_int(), // object ref is in GP reg
                // return (void)
                0xb1 => self.emit_return_void(),
                // aconst_null
                0x01 => {
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    self.push_operand(dst);
                    // T1.1.3 — null is a valid object reference.
                    self.mark_top_operand_as_oop();
                }
                // dup — duplicate top of stack
                0x59 => {
                    let top = self.pop_operand();
                    let dup = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup, rm: top });
                    self.push_operand(top);
                    self.push_operand(dup);
                }
                // pop
                0x57 => { let _ = self.pop_operand(); }
                // pop2
                0x58 => { let _ = self.pop_operand(); let _ = self.pop_operand(); }
                // swap
                0x5f => {
                    let a = self.pop_operand();
                    let b = self.pop_operand();
                    self.push_operand(a);
                    self.push_operand(b);
                }
                // ladd
                0x61 => self.emit_int_add(), // 64-bit add same instruction on ARM64
                // lsub
                0x65 => self.emit_int_sub(),
                // lmul
                0x69 => self.emit_int_mul(),
                // lneg
                0x75 => self.emit_int_neg(),
                // land
                0x7f => self.emit_int_and(),
                // lor
                0x81 => self.emit_int_or(),
                // lxor
                0x83 => self.emit_int_xor(),
                // lcmp — compare two longs, produce -1, 0, or 1
                0x94 => {
                    let b = self.pop_operand();
                    let a = self.pop_operand();
                    let dst = self.alloc_scratch();
                    // CMP a, b sets flags without overflow risk
                    self.buffer.emit(Arm64Instruction::Cmp { rn: a, rm: b });
                    // Default to 0 (equal)
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    let gt_label = self.buffer.new_label();
                    let end_label = self.buffer.new_label();
                    // If GT, set 1
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Gt, label: gt_label });
                    // If EQ, skip (already 0)
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: end_label });
                    // Otherwise LT: set -1
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
                    self.buffer.emit(Arm64Instruction::B { label: end_label });
                    self.buffer.bind_label(gt_label);
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
                    self.buffer.bind_label(end_label);
                    self.push_operand(dst);
                }
                // lload_0..lload_3
                0x1e..=0x21 => self.emit_iload((opcode - 0x1e) as usize),
                // lload
                0x16 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_iload(idx);
                }
                // lstore_0..lstore_3
                0x3f..=0x42 => self.emit_istore((opcode - 0x3f) as usize),
                // lstore
                0x37 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_istore(idx);
                }
                // ifXX (single operand branches)
                0x99 => { // ifeq
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
                }
                0x9a => { // ifne
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
                }
                // iflt (0x9b)
                0x9b => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Lt, label });
                }
                // ifge (0x9c)
                0x9c => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Ge, label });
                }
                // ifgt (0x9d)
                0x9d => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Gt, label });
                }
                // ifle (0x9e)
                0x9e => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Le, label });
                }
                // ifnull (0xc6)
                0xc6 => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
                }
                // ifnonnull (0xc7)
                0xc7 => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc+1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
                }
                // lconst_0, lconst_1
                0x09 => self.emit_lconst(0),
                0x0a => self.emit_lconst(1),
                // invokestatic — resolve argument count from method_info or
                // fall back to num_params for self-recursive calls.
                0xb8 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let cp_idx = ((bytecode[pc] as u16) << 8) | bytecode[pc + 1] as u16;
                    pc += 2;
                    let num_args = self.method_info
                        .get(&cp_idx)
                        .copied()
                        .unwrap_or(self.num_params);
                    self.emit_invoke(num_args);
                }
                // fconst_0
                0x0b => self.emit_fconst(0.0),
                // fconst_1
                0x0c => self.emit_fconst(1.0),
                // fconst_2
                0x0d => self.emit_fconst(2.0),
                // dconst_0
                0x0e => self.emit_fconst(0.0),
                // dconst_1
                0x0f => self.emit_fconst(1.0),
                // fload
                0x17 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fload(idx);
                }
                // dload
                0x18 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fload(idx);
                }
                // fload_0..fload_3
                0x22..=0x25 => self.emit_fload((opcode - 0x22) as usize),
                // dload_0..dload_3
                0x26..=0x29 => self.emit_fload((opcode - 0x26) as usize),
                // fstore
                0x38 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fstore(idx);
                }
                // dstore
                0x39 => {
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fstore(idx);
                }
                // fstore_0..fstore_3
                0x43..=0x46 => self.emit_fstore((opcode - 0x43) as usize),
                // dstore_0..dstore_3
                0x47..=0x4a => self.emit_fstore((opcode - 0x47) as usize),
                // fadd, dadd
                0x62 | 0x63 => self.emit_float_add(),
                // fsub, dsub
                0x66 | 0x67 => self.emit_float_sub(),
                // fmul, dmul
                0x6a | 0x6b => self.emit_float_mul(),
                // fdiv, ddiv
                0x6e | 0x6f => self.emit_float_div(),
                // frem, drem
                0x72 | 0x73 => self.emit_float_rem(),
                // fneg, dneg
                0x76 | 0x77 => self.emit_float_neg(),
                // i2l
                0x85 => {
                    // int to long: on 64-bit ARM, sign-extend 32-bit to 64-bit.
                    // For our backend, ints are already in 64-bit regs, so this is a no-op
                    // (values are sign-extended at load time).
                }
                // i2f (0x86)
                0x86 => self.emit_i2f(),
                // i2d (0x87)
                0x87 => self.emit_i2d(),
                // l2i (0x88)
                0x88 => self.emit_l2i(),
                // l2f (0x89)
                0x89 => self.emit_i2f(), // same as i2f on 64-bit
                // l2d (0x8a)
                0x8a => self.emit_i2d(), // same as i2d on 64-bit
                // f2i (0x8b)
                0x8b => self.emit_f2i(),
                // f2l (0x8c)
                0x8c => self.emit_f2i(), // same: fcvtzs to 64-bit
                // f2d (0x8d)
                0x8d => {
                    // float to double: in our backend both use double-precision V regs, no-op
                }
                // d2i (0x8e)
                0x8e => self.emit_f2i(),
                // d2l (0x8f)
                0x8f => self.emit_f2i(), // same: fcvtzs to 64-bit
                // d2f (0x90)
                0x90 => {
                    // double to float: in our backend both use double-precision V regs, no-op
                }
                // i2b (0x91) — int to byte (sign-extend low 8 bits)
                0x91 => {
                    let src = self.pop_operand();
                    let dst = self.alloc_scratch();
                    // SXTB: sign-extend byte to 64-bit using LSL+ASR pattern
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 56 });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl { rd: tmp, rn: src, rm: dst });
                    self.buffer.emit(Arm64Instruction::Asr { rd: tmp, rn: tmp, rm: dst });
                    self.push_operand(tmp);
                }
                // i2c (0x92) — int to char (zero-extend low 16 bits)
                0x92 => {
                    let src = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 0xFFFF });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::And { rd: tmp, rn: src, rm: dst });
                    self.push_operand(tmp);
                }
                // i2s (0x93) — int to short (sign-extend low 16 bits)
                0x93 => {
                    let src = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 48 });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl { rd: tmp, rn: src, rm: dst });
                    self.buffer.emit(Arm64Instruction::Asr { rd: tmp, rn: tmp, rm: dst });
                    self.push_operand(tmp);
                }
                // fcmpl (0x95), fcmpg (0x96), dcmpl (0x97), dcmpg (0x98)
                0x95 | 0x97 => self.emit_fcmp(true),  // NaN → -1
                0x96 | 0x98 => self.emit_fcmp(false), // NaN → 1

                // -- aload / astore (reference load/store, same as iload/istore on 64-bit) --
                0x19 => { // aload
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_iload(idx);
                    // T1.1.3 — aload always pushes an object reference.
                    self.mark_top_operand_as_oop();
                }
                0x2a..=0x2d => {
                    // aload_0..aload_3
                    self.emit_iload((opcode - 0x2a) as usize);
                    self.mark_top_operand_as_oop();
                }
                0x3a => { // astore
                    if pc >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_istore(idx);
                }
                0x4b..=0x4e => self.emit_istore((opcode - 0x4b) as usize), // astore_0..astore_3

                // -- iinc --
                0x84 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let idx = bytecode[pc] as usize;
                    let delta = bytecode[pc + 1] as i8 as i32;
                    pc += 2;
                    if let Some(Some(reg)) = self.local_regs.get(idx).copied() {
                        if delta >= 0 {
                            self.buffer.emit(Arm64Instruction::AddImm { rd: reg, rn: reg, imm: delta });
                        } else {
                            self.buffer.emit(Arm64Instruction::SubImm { rd: reg, rn: reg, imm: -delta });
                        }
                    } else {
                        // Spilled local: load, add, store back
                        let frame = self.frame.as_ref().unwrap();
                        let spill_index = self.spill_index_for(idx);
                        let offset = frame.spill_offset.saturating_add(
                            i32::try_from(spill_index).unwrap_or(i32::MAX).saturating_mul(8)
                        );
                        let tmp = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::Ldr { rt: tmp, rn: Arm64Register::FP, offset });
                        if delta >= 0 {
                            self.buffer.emit(Arm64Instruction::AddImm { rd: tmp, rn: tmp, imm: delta });
                        } else {
                            self.buffer.emit(Arm64Instruction::SubImm { rd: tmp, rn: tmp, imm: -delta });
                        }
                        self.buffer.emit(Arm64Instruction::Str { rt: tmp, rn: Arm64Register::FP, offset });
                    }
                }

                // -- iushr (logical shift right) --
                0x7c => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    // Mask to 32 bits first for unsigned shift
                    let mask = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm { rd: mask, imm: 0xFFFF_FFFF });
                    self.buffer.emit(Arm64Instruction::And { rd: dst, rn: val, rm: mask });
                    self.buffer.emit(Arm64Instruction::Lsr { rd: dst, rn: dst, rm: shift });
                    self.push_operand(dst);
                }

                // -- lushr (long unsigned shift right) --
                0x7d => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsr { rd: dst, rn: val, rm: shift });
                    self.push_operand(dst);
                }

                // -- lshl --
                0x79 => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl { rd: dst, rn: val, rm: shift });
                    self.push_operand(dst);
                }

                // -- lshr --
                0x7b => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Asr { rd: dst, rn: val, rm: shift });
                    self.push_operand(dst);
                }

                // -- ldiv --
                0x6d => self.emit_int_div(),

                // -- irem --
                0x70 => {
                    // a % b = a - (a / b) * b
                    let b = self.pop_operand();
                    let a = self.pop_operand();
                    let quot = self.alloc_scratch();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::SDiv { rd: quot, rn: a, rm: b });
                    self.buffer.emit(Arm64Instruction::Msub { rd: dst, rn: quot, rm: b, ra: a });
                    self.push_operand(dst);
                }

                // -- lrem --
                0x71 => {
                    let b = self.pop_operand();
                    let a = self.pop_operand();
                    let quot = self.alloc_scratch();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::SDiv { rd: quot, rn: a, rm: b });
                    self.buffer.emit(Arm64Instruction::Msub { rd: dst, rn: quot, rm: b, ra: a });
                    self.push_operand(dst);
                }

                // -- dup_x1 (0x5a) --
                0x5a => {
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let dup = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup, rm: val1 });
                    self.push_operand(dup);
                    self.push_operand(val2);
                    self.push_operand(val1);
                }

                // -- dup_x2 (0x5b) --
                0x5b => {
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let val3 = self.pop_operand();
                    let dup = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup, rm: val1 });
                    self.push_operand(dup);
                    self.push_operand(val3);
                    self.push_operand(val2);
                    self.push_operand(val1);
                }

                // -- dup2 (0x5c) --
                0x5c => {
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let dup1 = self.alloc_scratch();
                    let dup2 = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup1, rm: val1 });
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup2, rm: val2 });
                    self.push_operand(val2);
                    self.push_operand(val1);
                    self.push_operand(dup2);
                    self.push_operand(dup1);
                }

                // -- dup2_x1 (0x5d) --
                0x5d => {
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let val3 = self.pop_operand();
                    let dup1 = self.alloc_scratch();
                    let dup2 = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup1, rm: val1 });
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup2, rm: val2 });
                    self.push_operand(dup2);
                    self.push_operand(dup1);
                    self.push_operand(val3);
                    self.push_operand(val2);
                    self.push_operand(val1);
                }

                // -- dup2_x2 (0x5e) --
                0x5e => {
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let val3 = self.pop_operand();
                    let val4 = self.pop_operand();
                    let dup1 = self.alloc_scratch();
                    let dup2 = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup1, rm: val1 });
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup2, rm: val2 });
                    self.push_operand(dup2);
                    self.push_operand(dup1);
                    self.push_operand(val4);
                    self.push_operand(val3);
                    self.push_operand(val2);
                    self.push_operand(val1);
                }

                // -- tableswitch (0xaa) --
                0xaa => {
                    let index = self.pop_operand();
                    // Align to 4-byte boundary
                    while pc % 4 != 0 { pc += 1; }
                    if pc + 12 > bytecode.len() { success = false; break; }
                    let default_off = i32::from_be_bytes([bytecode[pc], bytecode[pc+1], bytecode[pc+2], bytecode[pc+3]]);
                    let low = i32::from_be_bytes([bytecode[pc+4], bytecode[pc+5], bytecode[pc+6], bytecode[pc+7]]);
                    let high = i32::from_be_bytes([bytecode[pc+8], bytecode[pc+9], bytecode[pc+10], bytecode[pc+11]]);
                    pc += 12;
                    let count = (high - low + 1).max(0) as usize;

                    // Range check: if index < low || index > high → default
                    let default_target = (start_pc as i32 + default_off) as usize;
                    let default_label = self.label_for_pc(default_target);

                    if low != 0 {
                        let adj = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::SubImm { rd: adj, rn: index, imm: low });
                        // adj holds the 0-based index
                        let bound = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm { rd: bound, imm: count as i64 });
                        self.buffer.emit(Arm64Instruction::Cmp { rn: adj, rm: bound });
                        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Cs, label: default_label });
                    } else {
                        let bound = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm { rd: bound, imm: count as i64 });
                        self.buffer.emit(Arm64Instruction::Cmp { rn: index, rm: bound });
                        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Cs, label: default_label });
                    }

                    // Emit linear chain of compares+branches for each case
                    for i in 0..count {
                        if pc + 4 > bytecode.len() { success = false; break; }
                        let off = i32::from_be_bytes([bytecode[pc], bytecode[pc+1], bytecode[pc+2], bytecode[pc+3]]);
                        pc += 4;
                        let target = (start_pc as i32 + off) as usize;
                        let case_label = self.label_for_pc(target);
                        let case_val = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm { rd: case_val, imm: (low + i as i32) as i64 });
                        self.buffer.emit(Arm64Instruction::Cmp { rn: index, rm: case_val });
                        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: case_label });
                    }
                    // Fall through to default
                    self.buffer.emit(Arm64Instruction::B { label: default_label });
                }

                // -- lookupswitch (0xab) --
                0xab => {
                    let key = self.pop_operand();
                    while pc % 4 != 0 { pc += 1; }
                    if pc + 8 > bytecode.len() { success = false; break; }
                    let default_off = i32::from_be_bytes([bytecode[pc], bytecode[pc+1], bytecode[pc+2], bytecode[pc+3]]);
                    let npairs = i32::from_be_bytes([bytecode[pc+4], bytecode[pc+5], bytecode[pc+6], bytecode[pc+7]]) as usize;
                    pc += 8;

                    let default_target = (start_pc as i32 + default_off) as usize;
                    let default_label = self.label_for_pc(default_target);

                    for _ in 0..npairs {
                        if pc + 8 > bytecode.len() { success = false; break; }
                        let match_val = i32::from_be_bytes([bytecode[pc], bytecode[pc+1], bytecode[pc+2], bytecode[pc+3]]);
                        let off = i32::from_be_bytes([bytecode[pc+4], bytecode[pc+5], bytecode[pc+6], bytecode[pc+7]]);
                        pc += 8;
                        let target = (start_pc as i32 + off) as usize;
                        let case_label = self.label_for_pc(target);
                        let cmp_val = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm { rd: cmp_val, imm: match_val as i64 });
                        self.buffer.emit(Arm64Instruction::Cmp { rn: key, rm: cmp_val });
                        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: case_label });
                    }
                    self.buffer.emit(Arm64Instruction::B { label: default_label });
                }

                // -- if_acmpeq (0xa5), if_acmpne (0xa6) --
                0xa5 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Eq, target);
                }
                0xa6 => {
                    if pc + 1 >= bytecode.len() { success = false; break; }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Ne, target);
                }

                // -- nop (0x00) --
                0x00 => {
                    self.buffer.emit(Arm64Instruction::Nop);
                }

                _ => {
                    // Unsupported opcode: mark failure AND emit trap instruction
                    // so execution does not silently fall through.
                    self.buffer.emit(Arm64Instruction::Comment(
                        format!("unsupported opcode 0x{:02x} at pc={}", opcode, start_pc),
                    ));
                    self.buffer.emit(Arm64Instruction::Brk { imm: 0 });
                    success = false;
                }
            }
        }

        // Emit epilogue.
        self.emit_epilogue();

        let frame = match self.frame.take() {
            Some(f) => f,
            None => return Arm64CompileResult {
                instructions: Vec::new(),
                frame: Arm64FrameLayout { frame_size: 0, callee_save_offset: 0, spill_offset: 0, num_spills: 0, saved_regs: Vec::new(), num_reg_locals: 0 },
                labels: HashMap::new(),
                success: false,
                oop_maps: Vec::new(),
            },
        };
        Arm64CompileResult {
            instructions: self.buffer.instructions.clone(),
            frame,
            labels: self.buffer.labels.clone(),
            success: success && !self.failed,
            // T1.1.3 — transfer the collected per-PC oop maps out of
            // the backend. When empty, the walker falls back to the
            // conservative stack scan for AArch64 frames, matching
            // the x64 behavior.
            oop_maps: std::mem::take(&mut self.oop_maps),
        }
    }

    // -- Arithmetic helpers -------------------------------------------------

    pub fn emit_int_add(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Add { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    pub fn emit_int_sub(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Sub { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    pub fn emit_int_mul(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Mul { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    pub fn emit_int_div(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        // Guard: if divisor is zero, trap (ArithmeticException).
        // CBZ rhs, trap_label; SDIV; B continue; trap: BRK
        let trap_label = self.buffer.new_label();
        let continue_label = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbz { rt: rhs, label: trap_label });
        self.buffer.emit(Arm64Instruction::SDiv { rd: dst, rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::B { label: continue_label });
        self.buffer.bind_label(trap_label);
        self.buffer.emit(Arm64Instruction::Brk { imm: 1 }); // ArithmeticException
        self.buffer.bind_label(continue_label);
        self.push_operand(dst);
    }

    pub fn emit_int_neg(&mut self) {
        let src = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Neg { rd: dst, rn: src });
        self.push_operand(dst);
    }

    fn emit_int_and(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::And { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    fn emit_int_or(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Orr { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    fn emit_int_xor(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Eor { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    fn emit_int_shl(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Lsl { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    fn emit_int_shr(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Asr { rd: dst, rn: lhs, rm: rhs });
        self.push_operand(dst);
    }

    // -- Compare / Branch ---------------------------------------------------

    pub fn emit_if_icmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        let label = self.label_for_pc(target_pc);
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    // -- Load / Store locals ------------------------------------------------

    pub fn emit_iload(&mut self, index: usize) {
        if let Some(Some(reg)) = self.local_regs.get(index).copied() {
            // Local lives in a callee-saved register: just push it.
            let dst = self.alloc_scratch();
            self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: reg });
            self.push_operand(dst);
        } else {
            // Spilled local: load from stack.
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index).unwrap_or(i32::MAX).saturating_mul(8)
            );
            let dst = self.alloc_scratch();
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: dst,
                rn: Arm64Register::FP,
                offset,
            });
            self.push_operand(dst);
        }
    }

    pub fn emit_istore(&mut self, index: usize) {
        let src = self.pop_operand();
        if let Some(Some(reg)) = self.local_regs.get(index) {
            self.buffer.emit(Arm64Instruction::Mov { rd: *reg, rm: src });
        } else {
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index).unwrap_or(i32::MAX).saturating_mul(8)
            );
            self.buffer.emit(Arm64Instruction::Str {
                rt: src,
                rn: Arm64Register::FP,
                offset,
            });
        }
    }

    // -- Constants ----------------------------------------------------------

    pub fn emit_iconst(&mut self, value: i32) {
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: i64::from(value) });
        self.push_operand(dst);
    }

    pub fn emit_lconst(&mut self, value: i64) {
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: value });
        self.push_operand(dst);
    }

    /// Load a 64-bit constant using the literal pool (LDR literal with inline data).
    ///
    /// Emits the pattern:  LDR Xt, pool_entry; B skip; .quad value; skip:
    /// This avoids the need for a post-code literal pool by placing the constant
    /// inline and branching over it.
    pub fn emit_ldr_literal(&mut self, rd: Arm64Register, value: u64) {
        let pool_label = self.buffer.new_label();
        let skip_label = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::LdrLiteral { rt: rd, label: pool_label });
        self.buffer.emit(Arm64Instruction::B { label: skip_label });
        self.buffer.emit(Arm64Instruction::ConstantPoolEntry { label: pool_label, value });
        self.buffer.bind_label(skip_label);
    }

    // -- Invoke -------------------------------------------------------------

    pub fn emit_invoke(&mut self, num_args: usize) {
        // Move arguments from operand stack into X0..X7.
        // Pop in reverse so that the first arg ends up in X0.
        let mut arg_regs = Vec::new();
        for _ in 0..num_args {
            arg_regs.push(self.pop_operand());
        }
        arg_regs.reverse();

        for (i, &src) in arg_regs.iter().enumerate() {
            if let Some(dst) = Arm64CallingConvention::int_arg_reg(i) {
                if src != dst {
                    self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: src });
                }
            }
        }

        // Emit an indirect call placeholder (real target patched later).
        let call_label = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Bl { label: call_label });

        // Result in X0; push onto operand stack.
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Mov { rd: dst, rm: Arm64Register::X0 });
        self.push_operand(dst);
    }

    // -- Float/Double helpers ------------------------------------------------

    /// Load a float/double constant onto the float operand stack.
    /// Uses MOVIMM to load the bit pattern into a GP register, then FMOV to V reg.
    pub fn emit_fconst(&mut self, value: f64) {
        let dst = self.alloc_float_scratch();
        if value == 0.0 {
            // FMOV from XZR: use MovImm 0 to GP, then ScvtfDouble
            let tmp = self.alloc_scratch();
            self.buffer.emit(Arm64Instruction::MovImm { rd: tmp, imm: 0 });
            self.buffer.emit(Arm64Instruction::ScvtfDouble { vd: dst, rn: tmp });
        } else {
            // Load integer representation and convert
            let tmp = self.alloc_scratch();
            self.buffer.emit(Arm64Instruction::MovImm { rd: tmp, imm: value as i64 });
            self.buffer.emit(Arm64Instruction::ScvtfDouble { vd: dst, rn: tmp });
        }
        self.push_float_operand(dst);
    }

    /// Load a float/double local variable onto the float operand stack.
    /// Locals are stored in GP registers; we convert to FP via ScvtfDouble
    /// or load from spill slot.
    pub fn emit_fload(&mut self, index: usize) {
        let dst = self.alloc_float_scratch();
        // Check if this float local has a dedicated FP register (from graph-coloring).
        if let Some(Some(fp_reg)) = self.float_local_regs.get(index).copied() {
            self.buffer.emit(Arm64Instruction::FmovFp { vd: dst, vn: fp_reg });
        } else if let Some(Some(gp_reg)) = self.local_regs.get(index).copied() {
            // Float local stored in a GP register: bit-pattern transfer to FP.
            self.buffer.emit(Arm64Instruction::FmovToFp { vd: dst, rn: gp_reg });
        } else {
            // Spilled local: load from stack directly into FP register.
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index).unwrap_or(i32::MAX).saturating_mul(8)
            );
            self.buffer.emit(Arm64Instruction::FpLdr {
                vt: dst,
                rn: Arm64Register::FP,
                offset,
                is_double: true,
            });
        }
        self.push_float_operand(dst);
    }

    /// Store the top of the float operand stack to a local variable.
    pub fn emit_fstore(&mut self, index: usize) {
        let src = self.pop_float_operand();
        // Check if this float local has a dedicated FP register.
        if let Some(Some(fp_reg)) = self.float_local_regs.get(index).copied() {
            self.buffer.emit(Arm64Instruction::FmovFp { vd: fp_reg, vn: src });
        } else if let Some(Some(gp_reg)) = self.local_regs.get(index) {
            // Float local stored in a GP register: bit-pattern transfer from FP.
            self.buffer.emit(Arm64Instruction::FmovFromFp { rd: *gp_reg, vn: src });
        } else {
            // Spilled: store FP register directly to stack.
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index).unwrap_or(i32::MAX).saturating_mul(8)
            );
            self.buffer.emit(Arm64Instruction::FpStr {
                vt: src,
                rn: Arm64Register::FP,
                offset,
                is_double: true,
            });
        }
    }

    /// Compute the spill slot index for a local that doesn't have a register.
    fn spill_index_for(&self, local_index: usize) -> usize {
        // Count how many locals before this one also lack a register (GPR and FP).
        let mut spill_idx = 0;
        for i in 0..local_index {
            let has_gpr = self.local_regs.get(i).map_or(false, |r| r.is_some());
            let has_fp = self.float_local_regs.get(i).map_or(false, |r| r.is_some());
            if !has_gpr && !has_fp {
                spill_idx += 1;
            }
        }
        spill_idx
    }

    /// Float/double add: pop two, emit FaddDouble, push result.
    pub fn emit_float_add(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FaddDouble { vd: dst, vn: lhs, vm: rhs });
        self.push_float_operand(dst);
    }

    /// Float/double sub: pop two, emit FsubDouble, push result.
    pub fn emit_float_sub(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble { vd: dst, vn: lhs, vm: rhs });
        self.push_float_operand(dst);
    }

    /// Float/double mul: pop two, emit FmulDouble, push result.
    pub fn emit_float_mul(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FmulDouble { vd: dst, vn: lhs, vm: rhs });
        self.push_float_operand(dst);
    }

    /// Float/double div: pop two, emit FdivDouble, push result.
    pub fn emit_float_div(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FdivDouble { vd: dst, vn: lhs, vm: rhs });
        self.push_float_operand(dst);
    }

    /// Float/double remainder: a - (a/b)*b pattern using fdiv, fmul, fsub.
    pub fn emit_float_rem(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let quotient = self.alloc_float_scratch();
        // quotient = lhs / rhs
        self.buffer.emit(Arm64Instruction::FdivDouble { vd: quotient, vn: lhs, vm: rhs });
        // Convert to integer and back to truncate: fcvtzs + scvtf
        let tmp_int = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::FcvtzsInt { rd: tmp_int, vn: quotient });
        let trunc = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::ScvtfDouble { vd: trunc, rn: tmp_int });
        // product = trunc * rhs
        let product = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FmulDouble { vd: product, vn: trunc, vm: rhs });
        // result = lhs - product
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble { vd: dst, vn: lhs, vm: product });
        self.push_float_operand(dst);
    }

    /// Float/double negate: FSUB from zero.
    pub fn emit_float_neg(&mut self) {
        let src = self.pop_float_operand();
        // Load zero into a V register
        let tmp = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: tmp, imm: 0 });
        let zero = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::ScvtfDouble { vd: zero, rn: tmp });
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble { vd: dst, vn: zero, vm: src });
        self.push_float_operand(dst);
    }

    /// i2f / i2d: pop int from GP stack, convert to float, push onto float stack.
    pub fn emit_i2f(&mut self) {
        let src = self.pop_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::ScvtfDouble { vd: dst, rn: src });
        self.push_float_operand(dst);
    }

    /// i2d: same as i2f in our backend (both use double-precision).
    pub fn emit_i2d(&mut self) {
        self.emit_i2f();
    }

    /// f2i / d2i: pop float from float stack, convert to int, push onto GP stack.
    pub fn emit_f2i(&mut self) {
        let src = self.pop_float_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::FcvtzsInt { rd: dst, vn: src });
        self.push_operand(dst);
    }

    /// l2i: truncate long to int (mask lower 32 bits).
    pub fn emit_l2i(&mut self) {
        let src = self.pop_operand();
        let dst = self.alloc_scratch();
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: mask, imm: 0xFFFF_FFFFi64 });
        self.buffer.emit(Arm64Instruction::And { rd: dst, rn: src, rm: mask });
        self.push_operand(dst);
    }

    /// Float compare: pop two floats, emit FcmpDouble + conditional set.
    /// `nan_minus`: if true, NaN produces -1 (fcmpl/dcmpl); if false, NaN produces 1 (fcmpg/dcmpg).
    pub fn emit_fcmp(&mut self, nan_minus: bool) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        self.buffer.emit(Arm64Instruction::FcmpDouble { vn: lhs, vm: rhs });
        // After FCMP, use conditional logic to produce -1, 0, or 1 on the int stack.
        // GT → 1, EQ → 0, LT → -1, unordered (NaN) → nan_minus ? -1 : 1
        let dst = self.alloc_scratch();
        // Start with 0
        self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
        // If GT, set to 1
        let gt_label = self.buffer.new_label();
        let end_label = self.buffer.new_label();
        let lt_label = self.buffer.new_label();
        let unord_label = self.buffer.new_label();

        // B.VS unordered (overflow flag set when NaN)
        self.buffer.emit(Arm64Instruction::Comment("fcmp result dispatch".into()));
        // For simplicity, use a series of conditional branches:
        // After FCMP: EQ means equal, GT means greater, LT(MI) means less, VS means unordered
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: end_label });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Gt, label: gt_label });
        // If we get here, it's LT or unordered
        if nan_minus {
            // Both LT and NaN produce -1
            self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
        } else {
            // LT produces -1, NaN produces 1 — check VS for NaN
            self.buffer.emit(Arm64Instruction::BCond {
                cond: Arm64Condition::Lt, label: lt_label,
            });
            // Unordered: NaN → 1
            self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
            self.buffer.bind_label(lt_label);
            self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
        }
        self.buffer.bind_label(gt_label);
        self.buffer.emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
        self.buffer.bind_label(end_label);
        // Bind unused labels to avoid issues
        if nan_minus {
            self.buffer.bind_label(lt_label);
            self.buffer.bind_label(unord_label);
        } else {
            self.buffer.bind_label(unord_label);
        }
        self.push_operand(dst);
    }

    // -- Return -------------------------------------------------------------

    pub fn emit_return_int(&mut self) {
        let src = self.pop_operand();
        if src != Arm64Register::X0 {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: Arm64Register::X0,
                rm: src,
            });
        }
        self.buffer.emit(Arm64Instruction::B { label: self.epilogue_label });
    }

    pub fn emit_return_void(&mut self) {
        self.buffer.emit(Arm64Instruction::B { label: self.epilogue_label });
    }

    // -----------------------------------------------------------------------
    // NEON vectorization helpers
    // -----------------------------------------------------------------------

    /// Emit a NEON-vectorized int-array sum loop.
    ///
    /// Generates code equivalent to:
    /// ```ignore
    /// int sum = 0;
    /// // vectorized portion: process 4 elements at a time using NEON
    /// int32x4_t vacc = {0, 0, 0, 0};
    /// for (int i = 0; i < (len & ~3); i += 4) {
    ///     vacc = vadd(vacc, vld1q_s32(&arr[i]));
    /// }
    /// sum = vacc[0] + vacc[1] + vacc[2] + vacc[3];
    /// // scalar tail
    /// for (int i = len & ~3; i < len; i++) {
    ///     sum += arr[i];
    /// }
    /// ```
    ///
    /// Arguments:
    /// - `arr_reg`: GP register holding the base pointer to the int array data
    /// - `len_reg`: GP register holding the array length
    /// - `result_reg`: GP register where the final sum will be stored
    pub fn emit_neon_array_sum(&mut self, arr_reg: Arm64Register, len_reg: Arm64Register, result_reg: Arm64Register) {
        let vacc = Arm64Register::V0; // accumulator vector
        let vdata = Arm64Register::V1; // loaded data vector
        let idx = self.alloc_scratch(); // loop counter
        let vec_len = self.alloc_scratch(); // len & ~3 (vectorized portion)
        let ptr = self.alloc_scratch(); // running pointer into array

        let vec_loop = self.buffer.new_label();
        let vec_done = self.buffer.new_label();
        let scalar_loop = self.buffer.new_label();
        let scalar_done = self.buffer.new_label();

        // Initialize accumulator vector to zero
        self.buffer.emit(Arm64Instruction::MovImm { rd: result_reg, imm: 0 });
        // Zero the vector accumulator (EOR Vd, Vd, Vd)
        self.buffer.emit(Arm64Instruction::Eor { rd: Arm64Register(vacc.0 - 32), rn: Arm64Register(vacc.0 - 32), rm: Arm64Register(vacc.0 - 32) });

        // vec_len = len & ~3 (round down to multiple of 4)
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: mask, imm: !3i64 });
        self.buffer.emit(Arm64Instruction::And { rd: vec_len, rn: len_reg, rm: mask });

        // ptr = arr_reg (starting pointer)
        self.buffer.emit(Arm64Instruction::Mov { rd: ptr, rm: arr_reg });
        // idx = 0
        self.buffer.emit(Arm64Instruction::MovImm { rd: idx, imm: 0 });

        // Skip vector loop if less than 4 elements
        self.buffer.emit(Arm64Instruction::Cmp { rn: vec_len, rm: Arm64Register::XZR });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: vec_done });

        // Vector loop
        self.buffer.bind_label(vec_loop);
        // Load 4 ints (16 bytes) from [ptr] into vdata
        self.buffer.emit(Arm64Instruction::NeonLd1_4s { vt: vdata, rn: ptr });
        // vacc += vdata
        self.buffer.emit(Arm64Instruction::NeonAdd4s { vd: vacc, vn: vacc, vm: vdata });
        // ptr += 16
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr, rn: ptr, imm: 16 });
        // idx += 4
        self.buffer.emit(Arm64Instruction::AddImm { rd: idx, rn: idx, imm: 4 });
        // if idx < vec_len → loop
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: vec_len });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Lt, label: vec_loop });

        self.buffer.bind_label(vec_done);

        // Horizontal reduce: sum the 4 lanes of vacc into result_reg.
        // We use ADDV to sum all lanes, but since we don't have ADDV encoded,
        // extract each lane via GP and add them.
        // For now, store vacc to stack, load the 4 ints, and add them.
        let _tmp_frame_off = -64i32; // Use a stack scratch area below FP
        // STR the vector to stack
        self.buffer.emit(Arm64Instruction::NeonSt1_4s { vt: vacc, rn: Arm64Register::SP });
        // Load the 4 elements and add them
        let t0 = self.alloc_scratch();
        let t1 = self.alloc_scratch();
        // Load first 2 as a pair, then next 2
        self.buffer.emit(Arm64Instruction::Ldr { rt: t0, rn: Arm64Register::SP, offset: 0 });
        self.buffer.emit(Arm64Instruction::Ldr { rt: t1, rn: Arm64Register::SP, offset: 8 });
        // Each 64-bit load holds 2x 32-bit ints. Split them:
        // Actually, for simplicity, use 32-bit loads. But our LDR is 64-bit.
        // Alternative: just use the 64-bit values and mask.
        // Let's use a simpler approach: store to [SP], load 4x 32-bit words via offset.
        // But we only have 64-bit LDR. Let's just add the two 64-bit halves and mask.
        // Actually, the cleanest approach: just add as 64-bit pairs.
        // t0 = arr[0] | (arr[1] << 32), t1 = arr[2] | (arr[3] << 32)
        // Extract low/high 32-bit from each:
        let lo0 = self.alloc_scratch();
        let hi0 = self.alloc_scratch();
        let lo1 = self.alloc_scratch();
        // Extract low 32 bits
        let mask32 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: mask32, imm: 0xFFFF_FFFF });
        self.buffer.emit(Arm64Instruction::And { rd: lo0, rn: t0, rm: mask32 });
        // Extract high 32 bits
        let shift32 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: shift32, imm: 32 });
        self.buffer.emit(Arm64Instruction::Lsr { rd: hi0, rn: t0, rm: shift32 });
        self.buffer.emit(Arm64Instruction::And { rd: lo1, rn: t1, rm: mask32 });
        let hi1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Lsr { rd: hi1, rn: t1, rm: shift32 });
        // Sum all 4 lanes
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: lo0, rm: hi0 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: lo1 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: hi1 });

        // Scalar tail: process remaining elements (idx..len)
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: len_reg });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Ge, label: scalar_done });

        self.buffer.bind_label(scalar_loop);
        // Load arr[idx] (4-byte int at ptr)
        let elem = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr { rt: elem, rn: ptr, offset: 0 });
        // Mask to 32 bits (array element is i32 but LDR loads 64 bits)
        self.buffer.emit(Arm64Instruction::And { rd: elem, rn: elem, rm: mask32 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: elem });
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr, rn: ptr, imm: 4 });
        self.buffer.emit(Arm64Instruction::AddImm { rd: idx, rn: idx, imm: 1 });
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: len_reg });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Lt, label: scalar_loop });

        self.buffer.bind_label(scalar_done);
    }

    /// Emit a NEON-vectorized dot product of two int arrays.
    ///
    /// Computes: sum(a[i] * b[i]) for i in 0..len
    pub fn emit_neon_dot_product(
        &mut self,
        arr_a_reg: Arm64Register,
        arr_b_reg: Arm64Register,
        len_reg: Arm64Register,
        result_reg: Arm64Register,
    ) {
        let vacc = Arm64Register::V0;
        let va = Arm64Register::V1;
        let vb = Arm64Register::V2;
        let vtmp = Arm64Register::V3;
        let idx = self.alloc_scratch();
        let vec_len = self.alloc_scratch();
        let ptr_a = self.alloc_scratch();
        let ptr_b = self.alloc_scratch();

        let vec_loop = self.buffer.new_label();
        let vec_done = self.buffer.new_label();
        let scalar_loop = self.buffer.new_label();
        let scalar_done = self.buffer.new_label();

        // Zero result and accumulator
        self.buffer.emit(Arm64Instruction::MovImm { rd: result_reg, imm: 0 });
        self.buffer.emit(Arm64Instruction::Eor {
            rd: Arm64Register(vacc.0 - 32),
            rn: Arm64Register(vacc.0 - 32),
            rm: Arm64Register(vacc.0 - 32),
        });

        // vec_len = len & ~3
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: mask, imm: !3i64 });
        self.buffer.emit(Arm64Instruction::And { rd: vec_len, rn: len_reg, rm: mask });

        self.buffer.emit(Arm64Instruction::Mov { rd: ptr_a, rm: arr_a_reg });
        self.buffer.emit(Arm64Instruction::Mov { rd: ptr_b, rm: arr_b_reg });
        self.buffer.emit(Arm64Instruction::MovImm { rd: idx, imm: 0 });

        self.buffer.emit(Arm64Instruction::Cmp { rn: vec_len, rm: Arm64Register::XZR });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: vec_done });

        self.buffer.bind_label(vec_loop);
        self.buffer.emit(Arm64Instruction::NeonLd1_4s { vt: va, rn: ptr_a });
        self.buffer.emit(Arm64Instruction::NeonLd1_4s { vt: vb, rn: ptr_b });
        // vtmp = va * vb (element-wise)
        self.buffer.emit(Arm64Instruction::NeonMul4s { vd: vtmp, vn: va, vm: vb });
        // vacc += vtmp
        self.buffer.emit(Arm64Instruction::NeonAdd4s { vd: vacc, vn: vacc, vm: vtmp });
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr_a, rn: ptr_a, imm: 16 });
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr_b, rn: ptr_b, imm: 16 });
        self.buffer.emit(Arm64Instruction::AddImm { rd: idx, rn: idx, imm: 4 });
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: vec_len });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Lt, label: vec_loop });

        self.buffer.bind_label(vec_done);

        // Horizontal reduce vacc
        self.buffer.emit(Arm64Instruction::NeonSt1_4s { vt: vacc, rn: Arm64Register::SP });
        let t0 = self.alloc_scratch();
        let t1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr { rt: t0, rn: Arm64Register::SP, offset: 0 });
        self.buffer.emit(Arm64Instruction::Ldr { rt: t1, rn: Arm64Register::SP, offset: 8 });
        let mask32 = self.alloc_scratch();
        let shift32 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm { rd: mask32, imm: 0xFFFF_FFFF });
        self.buffer.emit(Arm64Instruction::MovImm { rd: shift32, imm: 32 });
        let lo0 = self.alloc_scratch();
        let hi0 = self.alloc_scratch();
        let lo1 = self.alloc_scratch();
        let hi1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::And { rd: lo0, rn: t0, rm: mask32 });
        self.buffer.emit(Arm64Instruction::Lsr { rd: hi0, rn: t0, rm: shift32 });
        self.buffer.emit(Arm64Instruction::And { rd: lo1, rn: t1, rm: mask32 });
        self.buffer.emit(Arm64Instruction::Lsr { rd: hi1, rn: t1, rm: shift32 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: lo0, rm: hi0 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: lo1 });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: hi1 });

        // Scalar tail
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: len_reg });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Ge, label: scalar_done });

        self.buffer.bind_label(scalar_loop);
        let ea = self.alloc_scratch();
        let eb = self.alloc_scratch();
        let prod = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr { rt: ea, rn: ptr_a, offset: 0 });
        self.buffer.emit(Arm64Instruction::Ldr { rt: eb, rn: ptr_b, offset: 0 });
        self.buffer.emit(Arm64Instruction::And { rd: ea, rn: ea, rm: mask32 });
        self.buffer.emit(Arm64Instruction::And { rd: eb, rn: eb, rm: mask32 });
        self.buffer.emit(Arm64Instruction::Mul { rd: prod, rn: ea, rm: eb });
        self.buffer.emit(Arm64Instruction::Add { rd: result_reg, rn: result_reg, rm: prod });
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr_a, rn: ptr_a, imm: 4 });
        self.buffer.emit(Arm64Instruction::AddImm { rd: ptr_b, rn: ptr_b, imm: 4 });
        self.buffer.emit(Arm64Instruction::AddImm { rd: idx, rn: idx, imm: 1 });
        self.buffer.emit(Arm64Instruction::Cmp { rn: idx, rm: len_reg });
        self.buffer.emit(Arm64Instruction::BCond { cond: Arm64Condition::Lt, label: scalar_loop });

        self.buffer.bind_label(scalar_done);
    }
}

/// Describes a detected vectorizable loop pattern in JVM bytecode.
#[derive(Debug, Clone)]
pub enum NeonVectorizablePattern {
    /// Sum of int array elements: `for (i=0; i<len; i++) sum += arr[i]`
    IntArraySum {
        /// Local index of the array reference.
        array_local: usize,
        /// Local index of the accumulator.
        accum_local: usize,
        /// Local index of the loop counter.
        counter_local: usize,
    },
    /// Dot product of two int arrays: `for (i=0; i<len; i++) sum += a[i] * b[i]`
    IntDotProduct {
        array_a_local: usize,
        array_b_local: usize,
        accum_local: usize,
        counter_local: usize,
    },
}

/// Analyze bytecode for NEON-vectorizable patterns.
///
/// Returns a list of detected patterns that could be compiled to NEON code.
/// Currently detects:
/// - Int array sum loops (iaload + iadd accumulation pattern)
/// - Int dot product loops (two iaload + imul + iadd pattern)
pub fn detect_neon_patterns(bytecode: &[u8], _num_locals: usize) -> Vec<NeonVectorizablePattern> {
    let mut patterns = Vec::new();
    let code_len = bytecode.len();

    // Scan for backward branches (loop back-edges) to find loop bodies.
    let mut pc = 0;
    while pc < code_len {
        let op = bytecode[pc];

        // Look for backward branch (goto with negative offset or ifXX with negative offset)
        let (is_branch, offset_pos) = match op {
            0xa7 => (true, pc + 1), // goto
            0x99..=0xa6 | 0xc6 | 0xc7 => (true, pc + 1), // conditional branches
            _ => (false, 0),
        };

        if is_branch && offset_pos + 1 < code_len {
            let offset = i16::from_be_bytes([bytecode[offset_pos], bytecode[offset_pos + 1]]) as i32;
            if offset < 0 {
                // This is a backward branch — potential loop
                let loop_start = (pc as i32 + offset) as usize;
                let loop_end = pc;

                // Scan loop body for iaload + iadd pattern (array sum)
                let mut has_iaload = false;
                let mut has_iadd = false;
                let mut has_imul = false;
                let mut iaload_count = 0;

                let mut lpc = loop_start;
                while lpc < loop_end && lpc < code_len {
                    match bytecode[lpc] {
                        0x2e => { has_iaload = true; iaload_count += 1; } // iaload
                        0x60 => { has_iadd = true; }  // iadd
                        0x68 => { has_imul = true; }  // imul
                        _ => {}
                    }
                    lpc += super::regalloc::bc_len(bytecode, lpc);
                }

                if has_iaload && has_iadd && !has_imul && iaload_count == 1 {
                    // Simple array sum pattern detected
                    patterns.push(NeonVectorizablePattern::IntArraySum {
                        array_local: 0, // placeholder — full analysis would resolve these
                        accum_local: 0,
                        counter_local: 0,
                    });
                } else if iaload_count >= 2 && has_imul && has_iadd {
                    // Dot product pattern detected
                    patterns.push(NeonVectorizablePattern::IntDotProduct {
                        array_a_local: 0,
                        array_b_local: 0,
                        accum_local: 0,
                        counter_local: 0,
                    });
                }
            }
        }

        pc += match op {
            0x10 | 0x15..=0x19 | 0x36..=0x3a | 0xbc => 2,
            0x11 | 0x84 | 0x99..=0xa7 | 0xb2..=0xb8 | 0xbd | 0xc0 | 0xc1 | 0xc6 | 0xc7 => 3,
            0xbb => 3,
            0xc5 => 4,
            0xb9 => 5,
            _ => 1,
        };
    }

    patterns
}

// ---------------------------------------------------------------------------
// Machine code emission
// ---------------------------------------------------------------------------

/// Convert an `Arm64Register` (0..=31) to a `Reg` for the emitter.
///
/// Returns `None` if `r` is not a valid GP register encoding. The JIT
/// contract is "never panic in production; bail to interpreter instead"
/// (C11), so call sites either propagate the `None` upward or — when the
/// regalloc has already proven the register is a GP — use
/// `.expect("regalloc invariant: ...")` to surface a clear message if the
/// invariant is ever violated.
fn to_reg(r: Arm64Register) -> Option<crate::aarch64::Reg> {
    // In debug builds, surface the precondition immediately so misuse during
    // development is caught at the call site rather than far away in the
    // emitter.
    debug_assert!(r.0 <= 31, "to_reg called on non-GP register {}", r.0);
    crate::aarch64::Reg::from_u8(r.0)
}

/// Convert an `Arm64Register` (V0=32..V7=39) to an `FpReg` for the emitter.
///
/// Returns `None` if `r` is not a valid FP register encoding. See `to_reg`
/// for the rationale on returning `Option` instead of asserting (C11).
fn to_fpreg(r: Arm64Register) -> Option<crate::aarch64::FpReg> {
    debug_assert!(
        r.0 >= 32 && r.0 <= 39,
        "to_fpreg called on non-FP register {}",
        r.0
    );
    if r.0 < 32 {
        return None;
    }
    crate::aarch64::FpReg::from_u8(r.0 - 32)
}

/// Shorthand for unwrapping `to_reg` at call sites where the register
/// allocator guarantees the register is a valid GPR. Centralising the
/// expect-message keeps the regalloc contract documented in one place.
#[inline]
fn r(reg: Arm64Register) -> crate::aarch64::Reg {
    to_reg(reg).expect("regalloc invariant: Arm64Register is a valid GPR (0..=31)")
}

/// Shorthand for unwrapping `to_fpreg` at call sites where the register
/// allocator guarantees the register is a valid FP register.
#[inline]
fn fp(reg: Arm64Register) -> crate::aarch64::FpReg {
    to_fpreg(reg).expect("regalloc invariant: Arm64Register is a valid FP reg (32..=39)")
}

/// Convert an `Arm64Condition` to an `aarch64::Cond`.
fn to_cond(c: &Arm64Condition) -> crate::aarch64::Cond {
    match c {
        Arm64Condition::Eq => crate::aarch64::Cond::EQ,
        Arm64Condition::Ne => crate::aarch64::Cond::NE,
        Arm64Condition::Lt => crate::aarch64::Cond::LT,
        Arm64Condition::Le => crate::aarch64::Cond::LE,
        Arm64Condition::Gt => crate::aarch64::Cond::GT,
        Arm64Condition::Ge => crate::aarch64::Cond::GE,
        Arm64Condition::Hi => crate::aarch64::Cond::HI,
        Arm64Condition::Ls => crate::aarch64::Cond::LS,
        Arm64Condition::Cs => crate::aarch64::Cond::HS,
        Arm64Condition::Cc => crate::aarch64::Cond::LO,
        Arm64Condition::Al => crate::aarch64::Cond::AL,
    }
}

/// Emit machine code bytes from the ARM64 pseudo-instruction sequence.
/// Uses `Aarch64Emitter` from `aarch64.rs` to encode each instruction.
pub fn emit_machine_code(result: &Arm64CompileResult) -> Option<Vec<u8>> {
    use crate::aarch64::Aarch64Emitter;

    if !result.success {
        return None;
    }

    let mut emitter = Aarch64Emitter::new();
    let mut label_offsets: HashMap<u32, usize> = HashMap::new();
    // (code_offset, label_id, is_cond) — is_cond distinguishes B from B.cond/CBZ/CBNZ
    let mut branch_patches: Vec<(usize, u32, bool)> = Vec::new();
    // (code_offset, label_id) for LDR literal instructions needing pool offset patching
    let mut literal_patches: Vec<(usize, u32)> = Vec::new();

    for inst in &result.instructions {
        match inst {
            Arm64Instruction::Label(id) => {
                label_offsets.insert(*id, emitter.offset());
            }
            Arm64Instruction::Comment(_) => { /* skip */ }
            Arm64Instruction::ConstantPoolEntry { label, value } => {
                // Bind the label to the current offset, then emit the raw 64-bit value.
                label_offsets.insert(*label, emitter.offset());
                emitter.emit_u64_data(*value);
            }

            // -- Arithmetic --
            Arm64Instruction::Add { rd, rn, rm } => {
                emitter.add(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::AddImm { rd, rn, imm } => {
                if *imm >= 0 {
                    emitter.add_imm(r(*rd), r(*rn), *imm as u16, false);
                } else {
                    emitter.sub_imm(r(*rd), r(*rn), (-*imm) as u16, false);
                }
            }
            Arm64Instruction::Sub { rd, rn, rm } => {
                emitter.sub(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::SubImm { rd, rn, imm } => {
                if *imm >= 0 {
                    emitter.sub_imm(r(*rd), r(*rn), *imm as u16, false);
                } else {
                    emitter.add_imm(r(*rd), r(*rn), (-*imm) as u16, false);
                }
            }
            Arm64Instruction::Mul { rd, rn, rm } => {
                emitter.mul(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::SDiv { rd, rn, rm } => {
                emitter.sdiv(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Neg { rd, rn } => {
                // NEG Xd, Xn = SUB Xd, XZR, Xn
                emitter.sub(r(*rd), crate::aarch64::Reg::SP, r(*rn));
            }
            Arm64Instruction::Madd { rd, rn, rm, ra } => {
                emitter.madd(r(*rd), r(*rn), r(*rm), r(*ra));
            }
            Arm64Instruction::Msub { rd, rn, rm, ra } => {
                emitter.msub(r(*rd), r(*rn), r(*rm), r(*ra));
            }

            // -- Logical --
            Arm64Instruction::And { rd, rn, rm } => {
                emitter.and(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Orr { rd, rn, rm } => {
                emitter.orr(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Eor { rd, rn, rm } => {
                emitter.eor(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Lsl { rd, rn, rm } => {
                emitter.lsl(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Lsr { rd, rn, rm } => {
                emitter.lsr(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::Asr { rd, rn, rm } => {
                emitter.asr(r(*rd), r(*rn), r(*rm));
            }

            // -- Compare --
            Arm64Instruction::Cmp { rn, rm } => {
                emitter.cmp(r(*rn), r(*rm));
            }
            Arm64Instruction::CmpImm { rn, imm } => {
                emitter.cmp_imm(r(*rn), *imm as u16);
            }
            Arm64Instruction::Tst { rn, rm } => {
                emitter.tst(r(*rn), r(*rm));
            }

            // -- Move --
            Arm64Instruction::Mov { rd, rm } => {
                emitter.mov(r(*rd), r(*rm));
            }
            Arm64Instruction::MovImm { rd, imm } => {
                emitter.mov_imm64(r(*rd), *imm as u64);
            }
            Arm64Instruction::MovK { rd, imm, shift } => {
                emitter.movk(r(*rd), *imm, *shift);
            }

            // -- Load / Store --
            Arm64Instruction::Ldr { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 {
                    emitter.ldr_imm(r(*rt), r(*rn), *offset as u16);
                } else {
                    emitter.ldr_pre(r(*rt), r(*rn), *offset as i16);
                }
            }
            Arm64Instruction::Str { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 {
                    emitter.str_imm(r(*rt), r(*rn), *offset as u16);
                } else {
                    emitter.str_pre(r(*rt), r(*rn), *offset as i16);
                }
            }
            Arm64Instruction::Ldp { rt1, rt2, rn, offset } => {
                emitter.ldp(r(*rt1), r(*rt2), r(*rn), *offset as i16);
            }
            Arm64Instruction::Stp { rt1, rt2, rn, offset } => {
                emitter.stp(r(*rt1), r(*rt2), r(*rn), *offset as i16);
            }
            Arm64Instruction::LdrLiteral { rt, label } => {
                // Emit a real LDR (literal) instruction. The offset to the
                // target label will be patched after all code is emitted.
                let pos = emitter.ldr_literal_x(r(*rt));
                literal_patches.push((pos, *label));
            }

            // -- Branch --
            Arm64Instruction::B { label } => {
                let pos = emitter.b(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::BCond { cond, label } => {
                let pos = emitter.b_cond(to_cond(cond), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Bl { label } => {
                let pos = emitter.bl(0);
                branch_patches.push((pos, *label, false));
            }
            Arm64Instruction::Br { rn } => {
                emitter.br(r(*rn));
            }
            Arm64Instruction::Blr { rn } => {
                emitter.blr(r(*rn));
            }
            Arm64Instruction::Ret => {
                emitter.ret_lr();
            }
            Arm64Instruction::Cbz { rt, label } => {
                let pos = emitter.cbz(r(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Cbnz { rt, label } => {
                let pos = emitter.cbnz(r(*rt), 0);
                branch_patches.push((pos, *label, true));
            }

            // -- FP move --
            Arm64Instruction::FmovToFp { vd, rn } => {
                emitter.fmov_d_from_gp(fp(*vd), r(*rn));
            }
            Arm64Instruction::FmovFromFp { rd, vn } => {
                emitter.fmov_gp_from_d(r(*rd), fp(*vn));
            }
            Arm64Instruction::FmovFp { vd, vn } => {
                emitter.fmov_d(fp(*vd), fp(*vn));
            }

            // -- FP negate --
            Arm64Instruction::FnegDouble { vd, vn } => {
                emitter.fneg_d(fp(*vd), fp(*vn));
            }

            // -- FP single-precision --
            Arm64Instruction::FaddSingle { vd, vn, vm } => {
                emitter.fadd_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubSingle { vd, vn, vm } => {
                emitter.fsub_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulSingle { vd, vn, vm } => {
                emitter.fmul_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivSingle { vd, vn, vm } => {
                emitter.fdiv_s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpSingle { vn, vm } => {
                emitter.fcmp_s(fp(*vn), fp(*vm));
            }
            Arm64Instruction::FnegSingle { vd, vn } => {
                emitter.fneg_s(fp(*vd), fp(*vn));
            }

            // -- Conversion --
            Arm64Instruction::ScvtfDouble { vd, rn } => {
                emitter.scvtf_d_x(fp(*vd), r(*rn));
            }
            Arm64Instruction::ScvtfSingle { vd, rn } => {
                emitter.scvtf_s_w(fp(*vd), r(*rn));
            }
            Arm64Instruction::FcvtzsInt { rd, vn } => {
                emitter.fcvtzs_x_d(r(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtzsSingle { rd, vn } => {
                emitter.fcvtzs_w_s(r(*rd), fp(*vn));
            }
            Arm64Instruction::FcvtSingleToDouble { vd, vn } => {
                emitter.fcvt_d_s(fp(*vd), fp(*vn));
            }
            Arm64Instruction::FcvtDoubleToSingle { vd, vn } => {
                emitter.fcvt_s_d(fp(*vd), fp(*vn));
            }

            // -- FP Load / Store --
            Arm64Instruction::FpLdr { vt, rn, offset, is_double } => {
                if *is_double {
                    emitter.ldr_fp_d(fp(*vt), r(*rn), *offset as u16);
                } else {
                    emitter.ldr_fp_s(fp(*vt), r(*rn), *offset as u16);
                }
            }
            Arm64Instruction::FpStr { vt, rn, offset, is_double } => {
                if *is_double {
                    emitter.str_fp_d(fp(*vt), r(*rn), *offset as u16);
                } else {
                    emitter.str_fp_s(fp(*vt), r(*rn), *offset as u16);
                }
            }

            // -- NEON FP (double) --
            Arm64Instruction::FaddDouble { vd, vn, vm } => {
                emitter.fadd_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FsubDouble { vd, vn, vm } => {
                emitter.fsub_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FmulDouble { vd, vn, vm } => {
                emitter.fmul_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FdivDouble { vd, vn, vm } => {
                emitter.fdiv_d(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::FcmpDouble { vn, vm } => {
                emitter.fcmp_d(fp(*vn), fp(*vm));
            }

            // -- NEON SIMD (integer vector, 4x32) --
            Arm64Instruction::NeonLd1_4s { vt, rn } => {
                emitter.ld1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonSt1_4s { vt, rn } => {
                emitter.st1_4s(fp(*vt), r(*rn));
            }
            Arm64Instruction::NeonAdd4s { vd, vn, vm } => {
                emitter.add_v4s(fp(*vd), fp(*vn), fp(*vm));
            }
            Arm64Instruction::NeonMul4s { vd, vn, vm } => {
                emitter.mul_v4s(fp(*vd), fp(*vn), fp(*vm));
            }

            // -- System --
            Arm64Instruction::Nop => {
                emitter.nop();
            }
            Arm64Instruction::Brk { imm } => {
                emitter.brk(*imm);
            }
        }
    }

    // Patch branches
    for &(offset, label, is_cond) in &branch_patches {
        if let Some(&target) = label_offsets.get(&label) {
            if is_cond {
                emitter.patch_bcond(offset, target);
            } else {
                emitter.patch_branch(offset, target);
            }
        }
        // If label not found, leave the placeholder (branch to self) — this can
        // happen for unresolved calls; the caller is expected to patch them.
    }

    // Patch LDR literal instructions to point to their target labels.
    // The label should reference a position containing a 64-bit constant
    // (e.g. a literal pool entry appended after all code).
    for &(offset, label) in &literal_patches {
        if let Some(&target) = label_offsets.get(&label) {
            emitter.patch_ldr_literal(offset, target);
        }
    }

    Some(emitter.code().to_vec())
}

// ---------------------------------------------------------------------------
// Arm64PeepholeOptimizer
// ---------------------------------------------------------------------------

/// ARM64-specific peephole optimizations applied after instruction selection.
pub struct Arm64PeepholeOptimizer;

impl Arm64PeepholeOptimizer {
    /// Apply all peephole optimizations.  Returns the total number of
    /// transformations applied.
    pub fn optimize(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let mut total = 0;
        total += Self::optimize_cbz(instructions);
        total += Self::optimize_zero_reg(instructions);
        total += Self::remove_identity_ops(instructions);
        total += Self::merge_load_store_pairs(instructions);
        total += Self::fuse_multiply_add(instructions);
        total
    }

    /// CMP Xn, XZR + B.EQ label -> CBZ Xn, label
    /// CMP Xn, XZR + B.NE label -> CBNZ Xn, label
    pub fn optimize_cbz(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i + 1 < instructions.len() {
            let do_opt = match (&instructions[i], &instructions[i + 1]) {
                (
                    Arm64Instruction::CmpImm { rn, imm: 0 },
                    Arm64Instruction::BCond { cond: Arm64Condition::Eq, label },
                ) => Some(Arm64Instruction::Cbz { rt: *rn, label: *label }),
                (
                    Arm64Instruction::CmpImm { rn, imm: 0 },
                    Arm64Instruction::BCond { cond: Arm64Condition::Ne, label },
                ) => Some(Arm64Instruction::Cbnz { rt: *rn, label: *label }),
                _ => None,
            };
            if let Some(replacement) = do_opt {
                instructions[i] = replacement;
                instructions.remove(i + 1);
                count += 1;
            } else {
                i += 1;
            }
        }
        count
    }

    /// MOV Xn, #0 -> use XZR where possible (replace with Mov from XZR).
    pub fn optimize_zero_reg(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let mut count = 0;
        for inst in instructions.iter_mut() {
            if let Arm64Instruction::MovImm { rd, imm: 0 } = inst {
                *inst = Arm64Instruction::Mov { rd: *rd, rm: Arm64Register::XZR };
                count += 1;
            }
        }
        count
    }

    /// Remove ADD Xn, Xn, #0 and SUB Xn, Xn, #0 (identity operations).
    pub fn remove_identity_ops(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let before = instructions.len();
        instructions.retain(|inst| {
            !matches!(
                inst,
                Arm64Instruction::AddImm { rd, rn, imm: 0 } if *rd == *rn
            ) && !matches!(
                inst,
                Arm64Instruction::SubImm { rd, rn, imm: 0 } if *rd == *rn
            )
        });
        before - instructions.len()
    }

    /// Merge consecutive LDR/STR with adjacent offsets into LDP/STP.
    pub fn merge_load_store_pairs(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i + 1 < instructions.len() {
            let merged = match (&instructions[i], &instructions[i + 1]) {
                (
                    Arm64Instruction::Ldr { rt: rt1, rn: rn1, offset: off1 },
                    Arm64Instruction::Ldr { rt: rt2, rn: rn2, offset: off2 },
                ) if rn1 == rn2 && *off2 == *off1 + 8 && rt1 != rt2 => {
                    Some(Arm64Instruction::Ldp {
                        rt1: *rt1, rt2: *rt2, rn: *rn1, offset: *off1,
                    })
                }
                (
                    Arm64Instruction::Str { rt: rt1, rn: rn1, offset: off1 },
                    Arm64Instruction::Str { rt: rt2, rn: rn2, offset: off2 },
                ) if rn1 == rn2 && *off2 == *off1 + 8 => {
                    Some(Arm64Instruction::Stp {
                        rt1: *rt1, rt2: *rt2, rn: *rn1, offset: *off1,
                    })
                }
                _ => None,
            };
            if let Some(replacement) = merged {
                instructions[i] = replacement;
                instructions.remove(i + 1);
                count += 1;
            } else {
                i += 1;
            }
        }
        count
    }

    /// MUL Xd, Xn, Xm followed by ADD Xa, Xd, Xr -> MADD Xa, Xn, Xm, Xr
    pub fn fuse_multiply_add(instructions: &mut Vec<Arm64Instruction>) -> usize {
        let mut count = 0;
        let mut i = 0;
        while i + 1 < instructions.len() {
            let fused = match (&instructions[i], &instructions[i + 1]) {
                (
                    Arm64Instruction::Mul { rd: mul_rd, rn: mul_rn, rm: mul_rm },
                    Arm64Instruction::Add { rd: add_rd, rn: add_rn, rm: add_rm },
                ) if mul_rd == add_rn => {
                    Some(Arm64Instruction::Madd {
                        rd: *add_rd, rn: *mul_rn, rm: *mul_rm, ra: *add_rm,
                    })
                }
                (
                    Arm64Instruction::Mul { rd: mul_rd, rn: mul_rn, rm: mul_rm },
                    Arm64Instruction::Add { rd: add_rd, rn: add_rn, rm: add_rm },
                ) if mul_rd == add_rm => {
                    Some(Arm64Instruction::Madd {
                        rd: *add_rd, rn: *mul_rn, rm: *mul_rm, ra: *add_rn,
                    })
                }
                _ => None,
            };
            if let Some(replacement) = fused {
                instructions[i] = replacement;
                instructions.remove(i + 1);
                count += 1;
            } else {
                i += 1;
            }
        }
        count
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- to_reg / to_fpreg tests (C11) --------------------------------------
    //
    // Note: `to_reg` / `to_fpreg` carry a `debug_assert!` precondition so
    // that misuse trips immediately during development. The Option return
    // is the production safety-net (debug_assert is a no-op in release
    // builds, so an out-of-range register encoding returns None instead of
    // panicking, letting the caller bail to the interpreter). We therefore
    // test the happy path here; the negative path is verified at the
    // underlying constructor level (Reg::from_u8 / FpReg::from_u8 in
    // aarch64.rs) where no debug-precondition exists.

    #[test]
    fn to_reg_accepts_all_gpr_encodings() {
        for n in 0..=31u8 {
            assert!(
                to_reg(Arm64Register(n)).is_some(),
                "to_reg({n}) should be Some for valid GPR"
            );
        }
    }

    #[test]
    fn to_fpreg_accepts_v0_through_v7() {
        for n in 32..=39u8 {
            assert!(
                to_fpreg(Arm64Register(n)).is_some(),
                "to_fpreg({n}) should be Some for valid FP reg"
            );
        }
    }

    // -- Arm64Register tests ------------------------------------------------

    #[test]
    fn register_callee_saved_x19_through_x28() {
        for i in 19..=28u8 {
            assert!(Arm64Register(i).is_callee_saved(), "X{} should be callee-saved", i);
        }
        assert!(!Arm64Register::X0.is_callee_saved());
        assert!(!Arm64Register::FP.is_callee_saved());
        assert!(!Arm64Register::LR.is_callee_saved());
    }

    #[test]
    fn register_arg_regs() {
        for i in 0..=7u8 {
            assert!(Arm64Register(i).is_arg_reg(), "X{} should be arg reg", i);
        }
        assert!(!Arm64Register::X8.is_arg_reg());
        assert!(!Arm64Register::X19.is_arg_reg());
    }

    #[test]
    fn register_index() {
        assert_eq!(Arm64Register::X0.index(), 0);
        assert_eq!(Arm64Register::FP.index(), 29);
        assert_eq!(Arm64Register::SP.index(), 31);
    }

    // -- Arm64Condition tests -----------------------------------------------

    #[test]
    fn condition_all_variants_defined() {
        // Verify all 11 variants are distinct and constructible.
        let conds = [
            Arm64Condition::Eq, Arm64Condition::Ne, Arm64Condition::Lt,
            Arm64Condition::Le, Arm64Condition::Gt, Arm64Condition::Ge,
            Arm64Condition::Hi, Arm64Condition::Ls, Arm64Condition::Cs,
            Arm64Condition::Cc, Arm64Condition::Al,
        ];
        assert_eq!(conds.len(), 11);
        // Check they're not all equal.
        assert_ne!(conds[0], conds[1]);
    }

    // -- CallingConvention tests --------------------------------------------

    #[test]
    fn calling_convention_int_arg_regs() {
        assert_eq!(Arm64CallingConvention::INT_ARG_REGS.len(), 8);
        assert_eq!(Arm64CallingConvention::int_arg_reg(0), Some(Arm64Register::X0));
        assert_eq!(Arm64CallingConvention::int_arg_reg(7), Some(Arm64Register::X7));
        assert_eq!(Arm64CallingConvention::int_arg_reg(8), None);
    }

    #[test]
    fn calling_convention_callee_saved_count() {
        assert_eq!(Arm64CallingConvention::CALLEE_SAVED.len(), 10);
    }

    #[test]
    fn calling_convention_local_reg_mapping() {
        assert_eq!(Arm64CallingConvention::local_reg(0), Some(Arm64Register::X19));
        assert_eq!(Arm64CallingConvention::local_reg(9), Some(Arm64Register::X28));
        assert_eq!(Arm64CallingConvention::local_reg(10), None);
    }

    #[test]
    fn calling_convention_stack_alignment_is_16() {
        assert_eq!(Arm64CallingConvention::STACK_ALIGNMENT, 16);
    }

    #[test]
    fn calling_convention_no_red_zone() {
        assert_eq!(Arm64CallingConvention::RED_ZONE, 0);
    }

    // -- FrameLayout tests --------------------------------------------------

    #[test]
    fn frame_layout_zero_locals() {
        let frame = Arm64FrameLayout::compute(0, 0, &[]);
        assert_eq!(frame.frame_size % 16, 0, "frame must be 16-byte aligned");
        assert_eq!(frame.num_spills, 0);
        assert_eq!(frame.num_reg_locals, 0);
    }

    #[test]
    fn frame_layout_five_locals() {
        let saved: Vec<_> = (0..5).map(|i| Arm64CallingConvention::CALLEE_SAVED[i]).collect();
        let frame = Arm64FrameLayout::compute(5, 2, &saved);
        assert_eq!(frame.num_reg_locals, 5);
        assert_eq!(frame.num_spills, 2);
        assert_eq!(frame.saved_regs.len(), 5);
    }

    #[test]
    fn frame_layout_16_byte_alignment() {
        for n in 0..20 {
            let num_saved = n.min(Arm64CallingConvention::CALLEE_SAVED.len());
            let saved: Vec<_> = (0..num_saved).map(|i| Arm64CallingConvention::CALLEE_SAVED[i]).collect();
            let frame = Arm64FrameLayout::compute(n, n, &saved);
            assert_eq!(frame.frame_size % 16, 0, "frame_size {} not aligned for n={}", frame.frame_size, n);
        }
    }

    // -- CodeBuffer tests ---------------------------------------------------

    #[test]
    fn code_buffer_emit_and_count() {
        let mut buf = Arm64CodeBuffer::new();
        assert_eq!(buf.instruction_count(), 0);
        buf.emit(Arm64Instruction::Nop);
        buf.emit(Arm64Instruction::Ret);
        assert_eq!(buf.instruction_count(), 2);
    }

    #[test]
    fn code_buffer_label_creation_and_binding() {
        let mut buf = Arm64CodeBuffer::new();
        let l1 = buf.new_label();
        let l2 = buf.new_label();
        assert_ne!(l1, l2);
        buf.emit(Arm64Instruction::Nop);
        buf.bind_label(l1);
        assert!(buf.labels.contains_key(&l1));
        assert_eq!(*buf.labels.get(&l1).unwrap(), 1); // after the Nop
    }

    #[test]
    fn code_buffer_estimated_size_4_bytes_per_instr() {
        let mut buf = Arm64CodeBuffer::new();
        for _ in 0..10 {
            buf.emit(Arm64Instruction::Nop);
        }
        assert_eq!(buf.estimated_size(), 40);
    }

    // -- Backend: prologue / epilogue tests ---------------------------------

    fn make_backend_with_method(num_locals: usize, num_params: usize, bytecode: &[u8]) -> Arm64CompileResult {
        let mut backend = Arm64Backend::new();
        backend.compile_method(num_locals, num_params, 4, bytecode)
    }

    #[test]
    fn backend_prologue_emits_stp_fp_lr() {
        let result = make_backend_with_method(0, 0, &[0xb1]); // return void
        let has_stp_fp_lr = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Stp { rt1, rt2, .. }
                if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
        ));
        assert!(has_stp_fp_lr, "prologue must save FP/LR with STP");
    }

    #[test]
    fn backend_epilogue_emits_ldp_fp_lr_and_ret() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        let has_ldp = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Ldp { rt1, rt2, .. }
                if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
        ));
        let has_ret = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Ret));
        assert!(has_ldp, "epilogue must restore FP/LR with LDP");
        assert!(has_ret, "epilogue must emit RET");
    }

    // -- Backend: instruction emission tests --------------------------------

    #[test]
    fn backend_iconst_emits_movimm() {
        // iconst_5 (opcode 0x08) then ireturn (0xac)
        let result = make_backend_with_method(0, 0, &[0x08, 0xac]);
        let _has_mov = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::MovImm { imm: 2, .. }
        ));
        // iconst_5 = opcode 0x08 - 3 = 5
        let has_mov5 = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::MovImm { imm: 5, .. }
        ));
        assert!(has_mov5, "iconst_5 should emit MovImm with imm=5");
    }

    #[test]
    fn backend_int_add_emits_add() {
        // iconst_1, iconst_2, iadd, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x60, 0xac]);
        let has_add = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Add { .. }));
        assert!(has_add, "iadd should emit Add instruction");
    }

    #[test]
    fn backend_int_sub_emits_sub() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x64, 0xac]);
        let has_sub = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Sub { .. }));
        assert!(has_sub, "isub should emit Sub instruction");
    }

    #[test]
    fn backend_int_mul_emits_mul() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x68, 0xac]);
        let has_mul = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Mul { .. }));
        assert!(has_mul, "imul should emit Mul instruction");
    }

    #[test]
    fn backend_int_div_emits_sdiv() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x6c, 0xac]);
        let has_div = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::SDiv { .. }));
        assert!(has_div, "idiv should emit SDiv instruction");
    }

    #[test]
    fn backend_int_neg_emits_neg() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x74, 0xac]);
        let has_neg = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Neg { .. }));
        assert!(has_neg, "ineg should emit Neg instruction");
    }

    #[test]
    fn backend_iload_from_register() {
        // iload_0 (0x1a) then ireturn (0xac), with 1 local
        let result = make_backend_with_method(1, 0, &[0x1a, 0xac]);
        // iload from a register local emits Mov from a callee-saved register.
        let has_mov_from_callee_saved = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Mov { rm, .. } if rm.is_callee_saved()
        ));
        assert!(has_mov_from_callee_saved, "iload_0 with register local should emit Mov from callee-saved reg");
    }

    #[test]
    fn backend_iload_from_spill_slot() {
        // 12 locals all loaded simultaneously to force spills (ARM64 has 10 callee-saved GPRs).
        // Load all 12, then return.
        let mut code = Vec::new();
        for i in 0..12u8 {
            code.push(0x15); // iload
            code.push(i);
        }
        code.push(0xac); // ireturn
        let result = make_backend_with_method(12, 12, &code);
        // At least 2 locals must be spilled → at least 2 Ldr from [FP + offset]
        let ldr_from_fp = result.instructions.iter().filter(|inst| matches!(
            inst,
            Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
        )).count();
        assert!(ldr_from_fp >= 2, "with 12 simultaneous locals, at least 2 should spill, got {ldr_from_fp} LDRs from FP");
    }

    #[test]
    fn backend_istore_to_register() {
        // iconst_1, istore_0, return
        let result = make_backend_with_method(1, 0, &[0x04, 0x3b, 0xb1]);
        let has_mov_to_callee_saved = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Mov { rd, .. } if rd.is_callee_saved()
        ));
        assert!(has_mov_to_callee_saved, "istore_0 with register local should emit Mov to callee-saved reg");
    }

    #[test]
    fn backend_return_int_uses_x0() {
        // iconst_3, ireturn
        let result = make_backend_with_method(0, 0, &[0x06, 0xac]);
        let has_mov_x0 = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
        ));
        assert!(has_mov_x0, "ireturn should move result to X0");
    }

    #[test]
    fn backend_return_void_emits_branch_to_epilogue() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        let has_b = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::B { .. }));
        assert!(has_b, "return void should emit B to epilogue");
    }

    #[test]
    fn compile_result_success_flag() {
        let good = make_backend_with_method(0, 0, &[0xb1]); // return void
        assert!(good.success, "simple void return should succeed");

        let bad = make_backend_with_method(0, 0, &[0xFF]); // invalid opcode
        assert!(!bad.success, "unsupported opcode should set success=false");
    }

    // -- Peephole optimizer tests -------------------------------------------

    #[test]
    fn peephole_cbz_optimization() {
        let mut instrs = vec![
            Arm64Instruction::CmpImm { rn: Arm64Register::X9, imm: 0 },
            Arm64Instruction::BCond { cond: Arm64Condition::Eq, label: 42 },
        ];
        let count = Arm64PeepholeOptimizer::optimize_cbz(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(matches!(instrs[0], Arm64Instruction::Cbz { rt, label: 42 } if rt == Arm64Register::X9));
    }

    #[test]
    fn peephole_cbnz_optimization() {
        let mut instrs = vec![
            Arm64Instruction::CmpImm { rn: Arm64Register::X10, imm: 0 },
            Arm64Instruction::BCond { cond: Arm64Condition::Ne, label: 7 },
        ];
        let count = Arm64PeepholeOptimizer::optimize_cbz(&mut instrs);
        assert_eq!(count, 1);
        assert!(matches!(instrs[0], Arm64Instruction::Cbnz { rt, label: 7 } if rt == Arm64Register::X10));
    }

    #[test]
    fn peephole_remove_identity_add_zero() {
        let mut instrs = vec![
            Arm64Instruction::AddImm { rd: Arm64Register::X9, rn: Arm64Register::X9, imm: 0 },
            Arm64Instruction::Nop,
        ];
        let count = Arm64PeepholeOptimizer::remove_identity_ops(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(matches!(instrs[0], Arm64Instruction::Nop));
    }

    #[test]
    fn peephole_keep_non_identity_add() {
        let mut instrs = vec![
            Arm64Instruction::AddImm { rd: Arm64Register::X9, rn: Arm64Register::X9, imm: 4 },
        ];
        let count = Arm64PeepholeOptimizer::remove_identity_ops(&mut instrs);
        assert_eq!(count, 0);
        assert_eq!(instrs.len(), 1);
    }

    #[test]
    fn peephole_merge_ldr_pair() {
        let mut instrs = vec![
            Arm64Instruction::Ldr { rt: Arm64Register::X9, rn: Arm64Register::FP, offset: -16 },
            Arm64Instruction::Ldr { rt: Arm64Register::X10, rn: Arm64Register::FP, offset: -8 },
        ];
        let count = Arm64PeepholeOptimizer::merge_load_store_pairs(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(matches!(
            instrs[0],
            Arm64Instruction::Ldp { rt1, rt2, rn, offset: -16 }
                if rt1 == Arm64Register::X9 && rt2 == Arm64Register::X10 && rn == Arm64Register::FP
        ));
    }

    #[test]
    fn peephole_merge_str_pair() {
        let mut instrs = vec![
            Arm64Instruction::Str { rt: Arm64Register::X19, rn: Arm64Register::FP, offset: -32 },
            Arm64Instruction::Str { rt: Arm64Register::X20, rn: Arm64Register::FP, offset: -24 },
        ];
        let count = Arm64PeepholeOptimizer::merge_load_store_pairs(&mut instrs);
        assert_eq!(count, 1);
        assert!(matches!(instrs[0], Arm64Instruction::Stp { .. }));
    }

    #[test]
    fn peephole_fuse_mul_add_to_madd() {
        let mut instrs = vec![
            Arm64Instruction::Mul { rd: Arm64Register::X9, rn: Arm64Register::X10, rm: Arm64Register::X11 },
            Arm64Instruction::Add { rd: Arm64Register::X12, rn: Arm64Register::X9, rm: Arm64Register::X13 },
        ];
        let count = Arm64PeepholeOptimizer::fuse_multiply_add(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(matches!(
            instrs[0],
            Arm64Instruction::Madd { rd, rn, rm, ra }
                if rd == Arm64Register::X12
                && rn == Arm64Register::X10
                && rm == Arm64Register::X11
                && ra == Arm64Register::X13
        ));
    }

    #[test]
    fn peephole_optimize_returns_total_count() {
        let mut instrs = vec![
            Arm64Instruction::AddImm { rd: Arm64Register::X9, rn: Arm64Register::X9, imm: 0 },
            Arm64Instruction::MovImm { rd: Arm64Register::X10, imm: 0 },
            Arm64Instruction::Nop,
        ];
        let total = Arm64PeepholeOptimizer::optimize(&mut instrs);
        // identity add removed (1) + zero mov -> xzr (1) = 2
        assert!(total >= 2, "expected at least 2 optimizations, got {}", total);
    }

    #[test]
    fn peephole_zero_reg_optimization() {
        let mut instrs = vec![
            Arm64Instruction::MovImm { rd: Arm64Register::X9, imm: 0 },
        ];
        let count = Arm64PeepholeOptimizer::optimize_zero_reg(&mut instrs);
        assert_eq!(count, 1);
        assert!(matches!(
            instrs[0],
            Arm64Instruction::Mov { rd, rm }
                if rd == Arm64Register::X9 && rm == Arm64Register::XZR
        ));
    }

    #[test]
    fn arm64_stack_alignment_is_16() {
        assert_eq!(Arm64CallingConvention::STACK_ALIGNMENT, 16);
    }

    // -- Additional integration-level tests ---------------------------------

    #[test]
    fn compile_simple_add_method() {
        // int add(int a, int b) { return a + b; }
        // Bytecode: iload_0, iload_1, iadd, ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert!(result.success);
        let has_add = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Add { .. }));
        assert!(has_add);
    }

    #[test]
    fn compile_bipush() {
        // bipush 42, ireturn
        let result = make_backend_with_method(0, 0, &[0x10, 42, 0xac]);
        assert!(result.success);
        let has_42 = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::MovImm { imm: 42, .. }
        ));
        assert!(has_42);
    }

    // -- Float/double operation tests ------------------------------------------

    #[test]
    fn backend_float_add_emits_fadd() {
        // fconst_1 (0x0c), fconst_1 (0x0c), fadd (0x62), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
        assert!(result.success, "float add should succeed");
        let has_fadd = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FaddDouble { .. }));
        assert!(has_fadd, "fadd should emit FaddDouble");
    }

    #[test]
    fn backend_float_sub_emits_fsub() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x66, 0xb1]);
        assert!(result.success);
        let has_fsub = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FsubDouble { .. }));
        assert!(has_fsub);
    }

    #[test]
    fn backend_float_mul_emits_fmul() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x6a, 0xb1]);
        assert!(result.success);
        let has_fmul = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FmulDouble { .. }));
        assert!(has_fmul);
    }

    #[test]
    fn backend_float_div_emits_fdiv() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x6e, 0xb1]);
        assert!(result.success);
        let has_fdiv = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FdivDouble { .. }));
        assert!(has_fdiv);
    }

    #[test]
    fn backend_type_conversion_i2f() {
        // iconst_1 (0x04), i2f (0x86), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x04, 0x86, 0xb1]);
        assert!(result.success, "i2f conversion should succeed");
        let has_scvtf = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::ScvtfDouble { .. }));
        assert!(has_scvtf, "i2f should emit ScvtfDouble");
    }

    #[test]
    fn backend_emit_machine_code_produces_bytes() {
        let result = make_backend_with_method(0, 0, &[0xb1]); // return void
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(code.is_some(), "should produce machine code bytes");
        let bytes = code.unwrap();
        assert!(!bytes.is_empty(), "machine code should not be empty");
        // ARM64 instructions are 4 bytes each
        assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
    }

    #[test]
    fn backend_emit_simple_add_produces_code() {
        // int add(int a, int b) { return a + b; }
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(code.is_some());
        let bytes = code.unwrap();
        assert!(bytes.len() >= 16, "add method should produce at least a few instructions");
    }

    #[test]
    fn backend_float_neg_emits_fsub() {
        // fconst_1 (0x0c), fneg (0x76), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x0c, 0x76, 0xb1]);
        assert!(result.success, "float neg should succeed");
        // fneg uses FsubDouble (zero - value)
        let has_fsub = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FsubDouble { .. }));
        assert!(has_fsub, "fneg should emit FsubDouble");
    }

    #[test]
    fn backend_float_rem_emits_instructions() {
        // fconst_1, fconst_1, frem (0x72), return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x72, 0xb1]);
        assert!(result.success, "float rem should succeed");
        // frem uses fdiv + fcvtzs + scvtf + fmul + fsub
        let has_fdiv = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FdivDouble { .. }));
        assert!(has_fdiv, "frem should emit FdivDouble as part of remainder pattern");
    }

    #[test]
    fn backend_fcmp_emits_fcmpdouble() {
        // fconst_1, fconst_1, fcmpl (0x95), return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x95, 0xb1]);
        assert!(result.success, "fcmpl should succeed");
        let has_fcmp = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::FcmpDouble { .. }));
        assert!(has_fcmp, "fcmpl should emit FcmpDouble");
    }

    #[test]
    fn backend_emit_machine_code_with_float_ops() {
        // fconst_1, fconst_1, fadd, return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(code.is_some(), "float operations should produce machine code");
        let bytes = code.unwrap();
        assert_eq!(bytes.len() % 4, 0, "machine code must be 4-byte aligned");
    }

    // --- M28 fix tests: comparison opcodes, lcmp, idiv ---

    #[test]
    fn backend_iflt_opcode() {
        // iconst_1, iflt +3 (offset to return), return
        // 0x04=iconst_1, 0x9b=iflt, 00 06=offset +6 (to return), 0xb1=return
        let result = make_backend_with_method(0, 0, &[0x04, 0x9b, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "iflt should compile successfully");
    }

    #[test]
    fn backend_ifge_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9c, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifge should compile successfully");
    }

    #[test]
    fn backend_ifgt_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9d, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifgt should compile successfully");
    }

    #[test]
    fn backend_ifle_opcode() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x9e, 0x00, 0x06, 0xb1, 0xb1]);
        assert!(result.success, "ifle should compile successfully");
    }

    #[test]
    fn backend_lcmp_opcode() {
        // lconst_0, lconst_1, lcmp, ireturn
        // 0x09=lconst_0, 0x0a=lconst_1, 0x94=lcmp, 0xac=ireturn
        let result = make_backend_with_method(0, 0, &[0x09, 0x0a, 0x94, 0xac]);
        assert!(result.success, "lcmp should compile successfully");
    }

    #[test]
    fn backend_idiv_opcode() {
        // iconst_2, iconst_1, idiv, ireturn
        // 0x05=iconst_2, 0x04=iconst_1, 0x6c=idiv, 0xac=ireturn
        let result = make_backend_with_method(0, 0, &[0x05, 0x04, 0x6c, 0xac]);
        assert!(result.success, "idiv should compile successfully");
    }

    #[test]
    fn backend_ifeq_uses_label_not_raw_target() {
        // iconst_0, ifeq +5 (to second return), return, return
        // 0x03=iconst_0, 0x99=ifeq, 0x00 0x05=branch offset
        let result = make_backend_with_method(0, 0, &[0x03, 0x99, 0x00, 0x05, 0xb1, 0xb1]);
        assert!(result.success, "ifeq with label should compile successfully");
    }

    #[test]
    fn backend_bounds_check_on_truncated_bytecode() {
        // Truncated ifeq: only 2 bytes instead of 3 (opcode + 2 offset bytes)
        let result = make_backend_with_method(0, 0, &[0x99, 0x00]);
        // Should fail gracefully, not panic
        assert!(!result.success || result.instructions.is_empty(),
            "truncated branch should not produce valid code");
    }

    // ===================================================================
    // Phase 95 — ARM64 graph-coloring register allocator integration
    // ===================================================================

    #[test]
    fn p95_backend_uses_graph_coloring_for_locals() {
        // int f(int a, int b) { int c = a + b; return c; }
        // iload_0, iload_1, iadd, istore_2, iload_2, ireturn
        let result = make_backend_with_method(3, 2, &[0x1a, 0x1b, 0x60, 0x3d, 0x1c, 0xac]);
        assert!(result.success);
        // Graph coloring may assign 2 or 3 regs — local 2 might share a register
        // with local 0 or 1 since their live ranges don't fully overlap.
        let saved_count = result.frame.saved_regs.len();
        assert!(saved_count >= 2 && saved_count <= 3,
            "graph coloring should identify 2-3 used callee-saved regs, got {}",
            saved_count);
        for reg in &result.frame.saved_regs {
            assert!(reg.is_callee_saved(), "saved reg X{} should be callee-saved", reg.0);
        }
    }

    #[test]
    fn p95_backend_non_interfering_locals_can_share_nothing_extra() {
        // Locals used in sequence (no simultaneous live ranges):
        // iload_0; istore_2; iload_1; ireturn
        // Locals 0, 1 are params (live at entry), local 2 is temp.
        let result = make_backend_with_method(3, 2, &[0x1a, 0x3d, 0x1b, 0xac]);
        assert!(result.success);
        // All 3 locals should get registers (no spills needed).
        let saved_count = result.frame.saved_regs.len();
        assert!(saved_count <= 3 && saved_count >= 2,
            "expected 2-3 callee-saved regs, got {saved_count}");
    }

    #[test]
    fn p95_backend_spill_slot_for_11th_local() {
        // 11 locals, all loaded → 10 get regs, 1 spills.
        // Load local 10 (spilled) and return it.
        let result = make_backend_with_method(11, 0, &[0x15, 10, 0xac]);
        assert!(result.success);
        // Local 10 should be loaded from a frame spill slot (Ldr from FP).
        let has_spill_load = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
        ));
        assert!(has_spill_load, "11th local should spill to stack");
    }

    #[test]
    fn p95_backend_float_local_uses_fp_reg() {
        // fconst_1 (0x0c), fstore_0 (0x43), fload_0 (0x22), return void (0xb1)
        let result = make_backend_with_method(1, 0, &[0x0c, 0x43, 0x22, 0xb1]);
        assert!(result.success);
        // fload_0 should use FmovFp (FP reg to FP reg) since graph coloring assigns D8-D15.
        let has_fmov_fp = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::FmovFp { .. }
        ));
        assert!(has_fmov_fp,
            "float local with FP register should use FmovFp for loads");
    }

    // ===================================================================
    // Phase 95.2 — New bytecode compilation tests
    // ===================================================================

    #[test]
    fn p95_aload_astore_compiles() {
        // aload_0, astore_1, aload_1, areturn
        let result = make_backend_with_method(2, 1, &[0x2a, 0x4c, 0x2b, 0xb0]);
        assert!(result.success, "aload/astore should compile");
    }

    #[test]
    fn p95_iinc_compiles() {
        // iload_0, iinc 0 5, iload_0, ireturn
        let result = make_backend_with_method(1, 1, &[0x1a, 0x84, 0x00, 0x05, 0x1a, 0xac]);
        assert!(result.success, "iinc should compile");
        // Should emit AddImm for the increment
        let has_add = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::AddImm { imm: 5, .. }
        ));
        assert!(has_add, "iinc +5 should emit AddImm with imm=5");
    }

    #[test]
    fn p95_iinc_negative_compiles() {
        // iinc 0 -1
        let result = make_backend_with_method(1, 1, &[0x84, 0x00, 0xFF, 0x1a, 0xac]);
        assert!(result.success, "iinc -1 should compile");
        let has_sub = result.instructions.iter().any(|inst| matches!(
            inst,
            Arm64Instruction::SubImm { imm: 1, .. }
        ));
        assert!(has_sub, "iinc -1 should emit SubImm with imm=1");
    }

    #[test]
    fn p95_iushr_compiles() {
        // iconst_1, iconst_1, iushr, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x04, 0x7c, 0xac]);
        assert!(result.success, "iushr should compile");
        let has_lsr = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Lsr { .. }));
        assert!(has_lsr, "iushr should emit Lsr");
    }

    #[test]
    fn p95_irem_compiles() {
        // iconst_5, iconst_2, irem, ireturn
        let result = make_backend_with_method(0, 0, &[0x08, 0x05, 0x70, 0xac]);
        assert!(result.success, "irem should compile");
        let has_msub = result.instructions.iter().any(|inst| matches!(inst, Arm64Instruction::Msub { .. }));
        assert!(has_msub, "irem should emit Msub (a - (a/b)*b)");
    }

    #[test]
    fn p95_dup_x1_compiles() {
        // iconst_1, iconst_2, dup_x1, pop, pop, pop, return
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x5a, 0x57, 0x57, 0x57, 0xb1]);
        assert!(result.success, "dup_x1 should compile");
    }

    #[test]
    fn p95_dup2_compiles() {
        // iconst_1, iconst_2, dup2, pop, pop, pop, pop, return
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x5c, 0x57, 0x57, 0x57, 0x57, 0xb1]);
        assert!(result.success, "dup2 should compile");
    }

    #[test]
    fn p95_tableswitch_compiles() {
        // iconst_1, tableswitch { low=0, high=2, default→8, 0→8, 1→8, 2→8 }, return
        // PC 0: iconst_1 (opcode 0x04)
        // PC 1: tableswitch (0xaa)
        // Padding to align to 4: 2 bytes (pc 2, 3)
        // PC 4: default offset: +7 → target PC 8
        // PC 8: low = 0
        // PC 12: high = 2
        // PC 16: offset[0] = +7 → target PC 8
        // PC 20: offset[1] = +7
        // PC 24: offset[2] = +7
        // PC 28: return
        let bytecode = &[
            0x04,       // 0: iconst_1
            0xaa,       // 1: tableswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
            0x00, 0x00, 0x00, 0x00, // 8: low = 0
            0x00, 0x00, 0x00, 0x02, // 12: high = 2
            0x00, 0x00, 0x00, 0x1b, // 16: case 0 → +27 → PC 28
            0x00, 0x00, 0x00, 0x1b, // 20: case 1 → +27
            0x00, 0x00, 0x00, 0x1b, // 24: case 2 → +27
            0xb1,       // 28: return
        ];
        let result = make_backend_with_method(0, 0, bytecode);
        assert!(result.success, "tableswitch should compile");
    }

    #[test]
    fn p95_lookupswitch_compiles() {
        // iconst_1, lookupswitch { npairs=2, default→done, 1→done, 42→done }, return
        let _bytecode = &[
            0x04,       // 0: iconst_1
            0xab,       // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1c, // 4: default → +28 → PC 29
            0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
            0x00, 0x00, 0x00, 0x01, // 12: match 1
            0x00, 0x00, 0x00, 0x1c, // 16: → +28
            0x00, 0x00, 0x00, 0x2a, // 20: match 42
            0x00, 0x00, 0x00, 0x1c, // 24: → +28
            0xb1, // 28: return (at PC 28 but target is 29, let me fix)
        ];
        // Target PC should be 1 + 28 = 29, but that's past the bytecode. Let me make it target PC 28.
        let bytecode2 = &[
            0x04,       // 0: iconst_1
            0xab,       // 1: lookupswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
            0x00, 0x00, 0x00, 0x02, // 8: npairs = 2
            0x00, 0x00, 0x00, 0x01, // 12: match 1
            0x00, 0x00, 0x00, 0x1b, // 16: → +27 → PC 28
            0x00, 0x00, 0x00, 0x2a, // 20: match 42
            0x00, 0x00, 0x00, 0x1b, // 24: → +27 → PC 28
            0xb1, // 28: return
        ];
        let result = make_backend_with_method(0, 0, bytecode2);
        assert!(result.success, "lookupswitch should compile");
    }

    #[test]
    fn p95_if_acmpeq_compiles() {
        // aconst_null, aconst_null, if_acmpeq +5, return, return
        let result = make_backend_with_method(0, 0, &[0x01, 0x01, 0xa5, 0x00, 0x05, 0xb1, 0xb1]);
        assert!(result.success, "if_acmpeq should compile");
    }

    #[test]
    fn p95_lshl_lushr_compiles() {
        // lconst_1, lconst_1, lshl, lconst_1, lushr, lreturn
        let result = make_backend_with_method(0, 0, &[0x0a, 0x0a, 0x79, 0x0a, 0x7d, 0xad]);
        assert!(result.success, "lshl/lushr should compile");
    }

    #[test]
    fn p95_nop_compiles() {
        let result = make_backend_with_method(0, 0, &[0x00, 0xb1]);
        assert!(result.success, "nop should compile");
    }

    #[test]
    fn p95_backend_fibonacci_compiles() {
        // Fibonacci-like: int fib(int n) with loop
        // local 0 = n (param), local 1 = a = 0, local 2 = b = 1, local 3 = tmp
        // istore_1(a=0), iconst_1, istore_2(b=1), iload_0, ifle done,
        // loop: iload_2, iload_1, iadd, istore_3, iload_2, istore_1, iload_3, istore_2,
        //       iinc 0 -1, iload_0, ifgt loop, done: iload_1, ireturn
        let bytecode: &[u8] = &[
            0x03,             // 0: iconst_0
            0x3c,             // 1: istore_1  (a = 0)
            0x04,             // 2: iconst_1
            0x3d,             // 3: istore_2  (b = 1)
            0x1a,             // 4: iload_0   (n)
            0x9e, 0x00, 0x14, // 5: ifle +20 → 25
            // loop body at PC 8:
            0x1c,             // 8: iload_2   (b)
            0x1b,             // 9: iload_1   (a)
            0x60,             // 10: iadd     (a+b)
            0x3e,             // 11: istore_3 (tmp = a+b)
            0x1c,             // 12: iload_2  (b)
            0x3c,             // 13: istore_1 (a = b)
            0x1d,             // 14: iload_3  (tmp)
            0x3d,             // 15: istore_2 (b = tmp)
            0x84, 0x00, 0xff, // 16: iinc 0, -1  (n--)
            0x1a,             // 19: iload_0  (n)
            0x9d, 0xff, 0xf3, // 20: ifgt -13 → 8
            // done at PC 23:
            0x1b,             // 23: iload_1  (a)
            0xac,             // 24: ireturn
        ];
        let result = make_backend_with_method(4, 1, bytecode);
        assert!(result.success, "fibonacci method should compile successfully");
        // All 4 locals should get registers (X19-X22 via graph coloring)
        assert!(result.frame.saved_regs.len() >= 3,
            "fibonacci needs at least 3 callee-saved regs, got {}", result.frame.saved_regs.len());
    }

    // ===================================================================
    // Phase 95.3 — NEON vectorization tests
    // ===================================================================

    #[test]
    fn p95_neon_array_sum_emits_vector_instructions() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        // Set up minimal frame
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        backend.emit_neon_array_sum(
            Arm64Register::X0,  // arr_reg
            Arm64Register::X1,  // len_reg
            Arm64Register::X2,  // result_reg
        );

        let instrs = backend.buffer.instructions();

        // Should contain NeonLd1_4s (vector load)
        let has_ld1 = instrs.iter().any(|i| matches!(i, Arm64Instruction::NeonLd1_4s { .. }));
        assert!(has_ld1, "NEON array sum should emit NeonLd1_4s");

        // Should contain NeonAdd4s (vector add)
        let has_add = instrs.iter().any(|i| matches!(i, Arm64Instruction::NeonAdd4s { .. }));
        assert!(has_add, "NEON array sum should emit NeonAdd4s");

        // Should contain NeonSt1_4s (for horizontal reduce via store)
        let has_st1 = instrs.iter().any(|i| matches!(i, Arm64Instruction::NeonSt1_4s { .. }));
        assert!(has_st1, "NEON array sum should emit NeonSt1_4s for horizontal reduce");

        // Should contain scalar tail loop
        let branch_count = instrs.iter().filter(|i| matches!(i, Arm64Instruction::BCond { .. })).count();
        assert!(branch_count >= 3, "should have vector loop + scalar tail branches, got {branch_count}");
    }

    #[test]
    fn p95_neon_dot_product_emits_mul_and_add() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        backend.emit_neon_dot_product(
            Arm64Register::X0,  // arr_a
            Arm64Register::X1,  // arr_b
            Arm64Register::X2,  // len
            Arm64Register::X3,  // result
        );

        let instrs = backend.buffer.instructions();

        // Should contain NeonMul4s (vector multiply)
        let has_mul = instrs.iter().any(|i| matches!(i, Arm64Instruction::NeonMul4s { .. }));
        assert!(has_mul, "NEON dot product should emit NeonMul4s");

        // Should contain NeonAdd4s (vector accumulate)
        let has_add = instrs.iter().any(|i| matches!(i, Arm64Instruction::NeonAdd4s { .. }));
        assert!(has_add, "NEON dot product should emit NeonAdd4s");

        // Should contain two NeonLd1_4s (one for each array)
        let ld1_count = instrs.iter().filter(|i| matches!(i, Arm64Instruction::NeonLd1_4s { .. })).count();
        assert_eq!(ld1_count, 2, "NEON dot product should load from two arrays");
    }

    #[test]
    fn p95_neon_pattern_detection_array_sum() {
        // Bytecode representing: for (i=0; i<len; i++) sum += arr[i]
        // iload_2 (array), iload_3 (index), iaload, iload_1 (sum), iadd,
        // istore_1 (sum), iinc 3 1, iload_3, iload_0(len), if_icmplt -12
        let bytecode = &[
            0x1c,             // 0: iload_2  (array)
            0x1d,             // 1: iload_3  (index)
            0x2e,             // 2: iaload
            0x1b,             // 3: iload_1  (sum)
            0x60,             // 4: iadd
            0x3c,             // 5: istore_1 (sum)
            0x84, 0x03, 0x01, // 6: iinc 3, 1
            0x1d,             // 9: iload_3  (index)
            0x1a,             // 10: iload_0 (len)
            0xa1, 0xff, 0xf5, // 11: if_icmplt -11 → 0
            0x1b,             // 14: iload_1  (sum)
            0xac,             // 15: ireturn
        ];
        let patterns = detect_neon_patterns(bytecode, 4);
        assert!(!patterns.is_empty(), "should detect array sum pattern");
        assert!(matches!(patterns[0], NeonVectorizablePattern::IntArraySum { .. }),
            "detected pattern should be IntArraySum");
    }

    #[test]
    fn p95_neon_pattern_detection_no_pattern() {
        // Simple bytecode with no loop — should detect nothing
        let bytecode = &[0x1a, 0xac]; // iload_0, ireturn
        let patterns = detect_neon_patterns(bytecode, 1);
        assert!(patterns.is_empty(), "no loop = no patterns");
    }

    #[test]
    fn p95_neon_machine_code_emission() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        // Emit a small NEON sequence
        backend.buffer.emit(Arm64Instruction::NeonLd1_4s { vt: Arm64Register::V0, rn: Arm64Register::X0 });
        backend.buffer.emit(Arm64Instruction::NeonAdd4s { vd: Arm64Register::V0, vn: Arm64Register::V0, vm: Arm64Register::V1 });
        backend.buffer.emit(Arm64Instruction::NeonMul4s { vd: Arm64Register::V2, vn: Arm64Register::V0, vm: Arm64Register::V1 });
        backend.buffer.emit(Arm64Instruction::NeonSt1_4s { vt: Arm64Register::V2, rn: Arm64Register::X1 });
        backend.buffer.emit(Arm64Instruction::Ret);

        let result = Arm64CompileResult {
            instructions: backend.buffer.instructions().to_vec(),
            frame: Arm64FrameLayout::compute(0, 0, &[]),
            labels: backend.buffer.labels.clone(),
            success: true,
            oop_maps: Vec::new(),
        };

        let code = emit_machine_code(&result);
        assert!(code.is_some(), "NEON instructions should produce machine code");
        let bytes = code.unwrap();
        assert_eq!(bytes.len(), 5 * 4, "5 instructions × 4 bytes each = 20 bytes");
        // All bytes should be non-zero (valid ARM64 encodings)
        assert!(bytes.iter().any(|&b| b != 0), "encoded bytes should be non-trivial");
    }
}
