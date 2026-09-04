// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! ARM64 (AArch64) JIT backend compilation pipeline.
//!
//! Translates JVM bytecode into a sequence of `Arm64Instruction` pseudo-ops that
//! represent the compilation result.  A separate encoding step (using `aarch64.rs`)
//! can later lower these to raw machine code bytes.
//!
//! # SUPPORT STATUS — read before enabling this backend
//!
//! This is **not** a working second-tier JIT. It is an arithmetic-only
//! prototype. `ARCHITECTURE.md` calls it "partial coverage"; the audit
//! below (2026-07-26) is what that actually means. Do not ship an
//! `aarch64` build on the assumption that it is x64-equivalent.
//!
//! ## What it can compile
//!
//! Whole-method compilation only (no OSR). A method compiles **iff** every
//! one of its bytecodes is in the supported set below; a single unsupported
//! opcode sets `Arm64CompileResult::success = false`, which makes
//! `emit_machine_code` return `None` and `jit::try_compile` return `None`.
//! The VM then permanently bail-lists the method and interprets it. That
//! fallback is clean — an unsupported method is never mis-executed.
//!
//! Counts, for the 202 opcode values in `0x00..=0xc9`: **163** have a match
//! arm, **39** do not (table below). Of the 163, five arms exist but always
//! refuse the method — `invokestatic` (`0xb8`, no call-target resolution) and
//! `idiv`/`ldiv`/`irem`/`lrem` (see the safety notes) — so **158** opcodes
//! actually lower. Two more arms — `ldc`/`ldc_w` (`0x12`/`0x13`) and `ldc2_w`
//! (`0x14`) — also always refuse, for want of a constant pool, so the real
//! figure is **156**. For comparison, `x64.rs` has an arm for 193 of the same
//! 202 and lacks only `frem`, `drem`, `jsr`, `ret`, `wide`, `goto_w`, `jsr_w`.
//! (`pop2`, `dup2_x1` and `dup2_x2` were on that list until the commons-math
//! throughput fix and the `dup2_x2` fix added x64 arms for them — see
//! `fixed-suite-bugs/bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md`
//! and
//! `fixed-suite-bugs/jit/dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md`.
//! `x64::tests::scan_admitted_opcodes_are_lowered_or_declared` now fails if a
//! scan-admitted opcode ever loses its x64 arm again.)
//!
//! **The stack shuffles are category- and stack-aware as of 2026-08-18, and
//! were not before.** This backend keeps TWO simulated operand stacks —
//! `operand_stack` for int/long/reference and `float_operand_stack` for
//! float/double — and `pop`/`pop2`/`dup`/`dup_x1`/`dup_x2`/`dup2`/`dup2_x1`/
//! `dup2_x2`/`swap` each popped a FIXED number of entries from the first one.
//! A `float`/`double` operand is on the other stack, so those arms shuffled
//! unrelated integer values and left the FP value untouched — silently, with
//! no underflow, whenever the integer stack happened to be deep enough. And a
//! `long` is ONE entry here and TWO JVM slots, so every `pop2`/`dup2*`/
//! `dup_x2` form except the all-category-1 one touched the wrong number of
//! entries. All nine arms now consult [`Arm64Backend::int_stack_shuffle_entries`]
//! and refuse the method when the operands cannot be proven integer-stack
//! values of a known category.
//!
//! What lowers: constants (`*const_*`, `bipush`, `sipush` — the `ldc` family
//! has arms but refuses, there being no constant pool here), local load/store for
//! int/long/float/double/reference, `iinc`, int/long/float/double arithmetic
//! and bitwise ops **except division and remainder**, the numeric conversions
//! (`i2l` … `i2s`), `fcmp*`/`dcmp*`/`lcmp`, all
//! `if*`/`if_icmp*`/`if_acmp*`/`goto`, `tableswitch`, `lookupswitch`, the
//! stack shuffles (`dup*`, `swap`, `pop*`), `nop`, and all `*return`.
//!
//! ## What it CANNOT compile — the entire object model
//!
//! These 39 opcodes have **no lowering at all** and bail the method:
//!
//! | Area | Opcodes |
//! |------|---------|
//! | Array load | `0x2e..=0x35` (`iaload` … `saload`) |
//! | Array store | `0x4f..=0x56` (`iastore` … `sastore`) |
//! | Field access | `0xb2` `getstatic`, `0xb3` `putstatic`, `0xb4` `getfield`, `0xb5` `putfield` |
//! | Dispatch | `0xb6` `invokevirtual`, `0xb7` `invokespecial`, `0xb9` `invokeinterface`, `0xba` `invokedynamic` |
//! | Allocation | `0xbb` `new`, `0xbc` `newarray`, `0xbd` `anewarray`, `0xc5` `multianewarray`, `0xbe` `arraylength` |
//! | Exceptions | `0xbf` `athrow` |
//! | Type checks | `0xc0` `checkcast`, `0xc1` `instanceof` |
//! | Monitors | `0xc2` `monitorenter`, `0xc3` `monitorexit` |
//! | Misc | `0xc4` `wide`, `0xc8` `goto_w`, `0xa8`/`0xa9`/`0xc9` `jsr`/`ret`/`jsr_w` |
//!
//! `0xb8` `invokestatic` *has* a match arm, but [`Arm64Backend::emit_invoke`]
//! unconditionally sets `self.failed` — there is no call-target resolution on
//! this backend, so **no method containing any call of any kind compiles**.
//! In practice the admissible population is: leaf methods that touch no
//! object, no array, no field, no call, no monitor and no exception.
//!
//! Consequences worth stating plainly:
//! - **No inline caches.** x64 has MIC/PIC inline caches for
//!   `invokevirtual`/`invokeinterface`; this backend has neither those *nor*
//!   the generic slow-path helper, because it has no calls.
//! - **No inline TLAB bump allocation** (x64 has one) — there is no `new`.
//! - **No exception handling**, no `exception_table` consultation, no
//!   handler dispatch.
//!
//! ## Safety-critical gaps (these are the reason for the warning above)
//!
//! A full mechanism-by-mechanism comparison against the x86-64 backend lives in
//! `docs/jit/aarch64-parity.md`. The short version:
//!
//! - **GC safepoint polls: BUILT, and OPT-IN
//!   (`CRATONVM_JIT_ARM64_SAFEPOINTS`, default-OFF).** Updated 2026-09-03. x64
//!   emits a cooperative poll of `helpers.safepoint_flag_addr` at method entry
//!   and at every loop back-edge; this backend emitted none, and the header
//!   used to say it "cannot: no helper address is plumbed in, and taking a poll
//!   needs a CALL, which `emit_invoke` refuses". Both halves of that are now
//!   addressed: [`Arm64Backend::set_helpers`] plumbs the table in, and the poll
//!   emits its own `BLR` rather than going through `emit_invoke` (which refuses
//!   *bytecode* invokes because it has no call-target resolution — a different
//!   problem).
//!
//!   [`Arm64Backend::emit_safepoint_poll`] emits the x64 shape: materialize the
//!   flag address, `LDRB` **one byte** of it (the flag is an `AtomicBool`, and
//!   a 64-bit load would fold the `GcBarrier` counters after it into the test),
//!   `CBZ` past the slow path, spill the caller-saved operand registers, `BLR`,
//!   record the oop map at the return address, reload. It runs at method entry
//!   and at each loop header, and with it on
//!   [`Arm64Backend::label_for_pc`] no longer refuses backward branches — that
//!   refusal existed precisely because a compiled loop with no poll is a region
//!   a stop-the-world request can never interrupt, so **loops compile again**.
//!
//!   **It is default-OFF and that is deliberate.** No CI runner or developer
//!   host in this repository can EXECUTE aarch64, so the evidence for it is
//!   instruction-word assertions and pseudo-op structure — everything a
//!   non-aarch64 host can honestly prove, and not the same as "it works".
//!   Default-on would be publishing an unexecuted calling sequence into a GC's
//!   stop-the-world protocol. With it off, this backend is byte-identical to
//!   before: no poll, and backward branches still refused.
//! - **Oop maps: the WRITER works; there is no safepoint to call it at.**
//!   Updated 2026-09-03. [`Arm64Backend::mark_top_operand_as_oop`] is called
//!   from three opcode arms (`aconst_null`, `aload`, `aload_0..3`), so
//!   references really do flow through these frames. Its consumer,
//!   [`Arm64Backend::emit_oop_map_for_safepoint`], used to key its map as
//!   `instruction_count * 4` — wrong for this pseudo-op stream, since `Label`
//!   and `Comment` emit nothing, `ConstantPoolEntry` emits 8 bytes and
//!   `MovImm`/`AddImm`/`CmpImm` and out-of-range `Ldr`/`Str` expand to 1–4
//!   words — and the 2026-08-01 audit made it fail the method closed rather
//!   than let a caller inherit that.
//!
//!   It is now keyed the way that audit prescribed: off the ENCODER's byte
//!   offset. The compiler records an [`Arm64PendingOopMap`] against the
//!   pseudo-op INDEX of the instruction following the safepoint — a distinct
//!   type, so an unresolved PC cannot be mistaken for a resolved one — and
//!   [`emit_machine_code_with_oop_maps`] translates it once the encoder knows
//!   where each pseudo-op landed. A map it cannot place discards the method.
//!   [`publish_compiled_method`] then attaches the result to the artifact,
//!   which the `cfg`-gated caller previously did not do at all.
//!
//!   **The first caller arrived 2026-09-03**: the safepoint poll above records
//!   a map at its `BLR`'s return address, naming the operand slots it spilled.
//!   With polls off (the default) `pending_oop_maps` is still empty and the GC
//!   walker still takes its conservative fallback, exactly as before.
//!
//!   **Reference LOCALS are named too, as of 2026-09-03.** A frame-homed one
//!   is named where it already lives. A REGISTER-homed one (X19-X28) is stored
//!   to a home slot reserved for it, named, and reloaded after the call: those
//!   registers are callee-saved, so the value survives on its own, but it
//!   survives inside the CALLEE's saved-register area where only the
//!   conservative walk can see it -- and a conservative walk marks without
//!   being able to REWRITE. A relocating collector could not otherwise move an
//!   object whose only root was a register local. Which locals hold references
//!   comes from the flow-sensitive `compute_local_oop_masks` shared with x64,
//!   not from a whole-method approximation, because naming a primitive would
//!   hand a relocating collector a non-pointer to rewrite.
//!
//!   What remains before a MOVING collector could run here: there is still no
//!   safepoint-id slot, so `fully_oop_covered` stays false and
//!   `find_oop_map_for_pc` is the only reader that can select these maps; and
//!   an `astore` between two safepoints leaves the home slot stale, which is
//!   harmless only because each poll rewrites it before its own map is taken.
//! - **No deoptimization and no OSR.** Neither word appears in this file.
//!   There is no frame reconstruction, no uncommon-trap stub, no
//!   `osr_pc_to_native` table. There is nothing to tier down *from* (this is
//!   the only tier), so a deopt cannot occur — but equally, no speculative
//!   optimization may ever be added here without building that first.
//! - **No stack-overflow bang** in the prologue (x64 emits one). Frames that
//!   could step past the first guard page (>= 4096 bytes) are refused as of the
//!   2026-08-01 audit; smaller frames cannot skip the guard.
//! - **Float locals are not homed in FP registers.** `regalloc::ARM64_LOCAL_FPS`
//!   offers `D8`–`D15`, which AAPCS64 makes callee-saved, and this backend's
//!   prologue/epilogue save only GPRs — so homing a float local there destroyed
//!   the caller's copy. The allocator's FP assignments are ignored (2026-08-01);
//!   float locals live in frame slots or GPRs.
//! - **32-bit int ops are lowered to 64-bit X-form instructions.** `iadd`,
//!   `isub`, `imul`, `ineg`, `ishl`, `ishr`, `iand`, `ior`, `ixor` all use
//!   the same emitters as their `l*` counterparts, so JVM 32-bit wrapping
//!   does not happen (`Integer.MAX_VALUE + 1` yields `2147483648`, not
//!   `Integer.MIN_VALUE`) and `ishl`/`ishr` do not mask the shift amount to
//!   5 bits. `iushr` masks the *value* to 32 bits but not the shift.
//!   Fixing this needs W-form variants threaded through the whole operand
//!   pipeline (loads, compares, returns, `i2l`), which is a backend-wide
//!   type-discipline change and is **not** attempted piecemeal.
//! - **32-bit int ops are still lowered to 64-bit X-form** (see the entry
//!   above). This remains the largest *silent* correctness gap on this backend
//!   and is deliberately NOT fixed piecemeal; `docs/jit/aarch64-parity.md`
//!   records the shape a correct fix takes.
//! - **Loops were infinite self-branches** until the 2026-07-26 audit. Branch targets
//!   were discovered lazily as each branch was decoded, so a back-edge target
//!   (already walked past) never got a label bound, and the encoder left the
//!   displacement-0 placeholder — `B .`. Fixed by giving
//!   [`Arm64Backend::compile_method_with_info`] a discovery pass; any label
//!   that is still unbound now bails the method rather than emitting a
//!   self-branch. See `emit_machine_code`.
//! - **`idiv`/`ldiv`/`irem`/`lrem` bail** as of this audit. `idiv`'s
//!   divide-by-zero guard branched to `BRK #1`, which raises SIGTRAP — the
//!   process dies instead of throwing `ArithmeticException` (nothing in the
//!   VM converts SIGTRAP). `irem`/`lrem` had no zero check at all, and
//!   AArch64 `SDIV` by zero yields 0 rather than trapping, so `x % 0`
//!   silently returned `x`. Until a real exception path exists, these four
//!   opcodes are refused; see the `0x6c`/`0x6d`/`0x70`/`0x71` arms.
//!
//! ## Reachability
//!
//! `jit/src/lib.rs` dispatches here from exactly one place: the
//! `#[cfg(target_arch = "aarch64")]` block at the top of `try_compile_inner`,
//! which returns unconditionally (the IR pipeline and the x64 backend are
//! bypassed entirely on that target). Note that this is **not** the only
//! compile entry the VM uses: `vm/src/runtime/interpreter.rs` and
//! `vm/src/vm.rs` call `x64::compile*` directly from the eager first-call,
//! OSR and probe paths with no `target_arch` guard. Those sites are outside
//! this module and are not fixed here, but an `aarch64` build would emit
//! x86-64 bytes from them.
//!
//! Both this module and `aarch64.rs` are compiled unconditionally on every
//! host (`pub mod` in `lib.rs`, no `cfg`), so their unit tests — including
//! every instruction-encoding test — run in ordinary x86-64 CI. The code is
//! not rotting; it is simply far smaller in scope than its name suggests.
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
    Add {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AddImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    Sub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    Mul {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SDiv {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Neg {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    Madd {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },
    Msub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },

    // -- Logical --
    And {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Orr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Eor {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsl {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Asr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Compare --
    Cmp {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImm {
        rn: Arm64Register,
        imm: i32,
    },
    Tst {
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Move --
    Mov {
        rd: Arm64Register,
        rm: Arm64Register,
    },
    MovImm {
        rd: Arm64Register,
        imm: i64,
    },
    MovK {
        rd: Arm64Register,
        imm: u16,
        shift: u8,
    },

    // -- Load / Store --
    Ldr {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Zero-extending BYTE load. Distinct from [`Self::Ldr`] because the
    /// safepoint flag is a one-byte `AtomicBool` and the 64-bit form would
    /// fold the `GcBarrier` fields after it into the test.
    Ldrb {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Str {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Ldp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Stp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Pre-index STP with writeback: `STP rt1, rt2, [rn, #offset]!`.
    ///
    /// Bug-fix (ARM64 BUG #2): the AAPCS64-idiomatic prologue store. The base
    /// register `rn` is updated to `rn + offset` as part of the instruction,
    /// which both saves the pair AND allocates the (first 16 bytes of the)
    /// stack frame atomically. `offset` uses the small fixed −16, always inside
    /// the imm7 range, so it is robust for arbitrarily large frames (unlike a
    /// signed-offset STP at `frame_size-16`).
    StpPre {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Post-index LDP with writeback: `LDP rt1, rt2, [rn], #offset`.
    ///
    /// Bug-fix (ARM64 BUG #2): the matching epilogue restore. Loads the pair
    /// from `[rn]` then updates `rn = rn + offset`, mirroring [`StpPre`].
    LdpPost {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    LdrLiteral {
        rt: Arm64Register,
        label: u32,
    },

    // -- Branch --
    B {
        label: u32,
    },
    BCond {
        cond: Arm64Condition,
        label: u32,
    },
    Bl {
        label: u32,
    },
    Br {
        rn: Arm64Register,
    },
    Blr {
        rn: Arm64Register,
    },
    Ret,
    Cbz {
        rt: Arm64Register,
        label: u32,
    },
    Cbnz {
        rt: Arm64Register,
        label: u32,
    },

    // -- Conversion --
    ScvtfDouble {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    FcvtzsInt {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP move (bit-pattern transfer between GP and FP registers) --
    /// FMOV Vd, Xn — move GP register to FP register (bit-pattern, no conversion).
    FmovToFp {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FMOV Xd, Vn — move FP register to GP register (bit-pattern, no conversion).
    FmovFromFp {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FMOV Vd, Vn — move between FP registers.
    FmovFp {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP negate --
    FnegDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP single-precision ops --
    FaddSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpSingle {
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FnegSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// SCVTF Sd, Wn — convert 32-bit int to single-precision float.
    ScvtfSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FCVTZS Wd, Sn — convert single-precision float to 32-bit int (truncate toward zero).
    FcvtzsSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Dd, Sn — convert single to double.
    FcvtSingleToDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Sd, Dn — convert double to single.
    FcvtDoubleToSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// LDR (FP) — load from [base + offset] to FP register.
    FpLdr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },
    /// STR (FP) — store FP register to [base + offset].
    FpStr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },

    // -- NEON SIMD (double precision) --
    FaddDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpDouble {
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- NEON SIMD (integer vector, 4x32) --
    /// LD1 {Vt.4S}, [Xn] — load 128-bit vector from memory.
    NeonLd1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ST1 {Vt.4S}, [Xn] — store 128-bit vector to memory.
    NeonSt1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ADD Vd.4S, Vn.4S, Vm.4S — vector integer add (4x i32).
    NeonAdd4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    /// MUL Vd.4S, Vn.4S, Vm.4S — vector integer multiply (4x i32).
    NeonMul4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- System --
    Nop,
    Brk {
        imm: u16,
    },

    // -- Pseudo-instructions --
    Label(u32),
    Comment(String),
    /// Raw 64-bit constant data embedded in the instruction stream (literal pool).
    /// The label is bound to the position of this data so that LdrLiteral can
    /// reference it.
    ConstantPoolEntry {
        label: u32,
        value: u64,
    },
}

// ---------------------------------------------------------------------------
// Arm64CallingConvention
// ---------------------------------------------------------------------------

/// AAPCS64 calling convention constants and helpers.
pub struct Arm64CallingConvention;

impl Arm64CallingConvention {
    /// Integer argument registers: X0-X7.
    pub const INT_ARG_REGS: &'static [Arm64Register] = &[
        Arm64Register::X0,
        Arm64Register::X1,
        Arm64Register::X2,
        Arm64Register::X3,
        Arm64Register::X4,
        Arm64Register::X5,
        Arm64Register::X6,
        Arm64Register::X7,
    ];

    /// Callee-saved registers: X19-X28.
    pub const CALLEE_SAVED: &'static [Arm64Register] = &[
        Arm64Register::X19,
        Arm64Register::X20,
        Arm64Register::X21,
        Arm64Register::X22,
        Arm64Register::X23,
        Arm64Register::X24,
        Arm64Register::X25,
        Arm64Register::X26,
        Arm64Register::X27,
        Arm64Register::X28,
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
/// Does the aarch64 backend emit GC safepoint polls
/// (`CRATONVM_JIT_ARM64_SAFEPOINTS`, **default-OFF; opt-in**)?
///
/// # Why this one is opt-in when every sibling switch is default-on
///
/// It emits MACHINE CODE FOR AN ARCHITECTURE NO CI RUNNER OR DEVELOPER HOST
/// HERE CAN EXECUTE. Every other codegen switch in this workspace ships
/// default-on with a kill switch because a regression run can execute it and
/// say so; this one cannot be run at all until someone builds on an aarch64
/// host. The encodings below are asserted against known-good instruction words
/// and the structure is asserted against the emitted pseudo-op stream, which is
/// everything a non-aarch64 host can honestly prove -- and it is not the same
/// as "it works". Default-on would be publishing an unexecuted calling
/// sequence into a GC's stop-the-world protocol.
///
/// Turning it on does two things: a poll at method entry and at every loop
/// header, and -- because that is what the refusal was FOR -- it lifts
/// `label_for_pc`'s blanket refusal of backward branches, so loops compile
/// again. With it off, this backend is byte-identical to before.
pub(crate) fn arm64_safepoints_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ARM64_SAFEPOINTS")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

/// A safepoint's oop map as the COMPILER can know it: the frame slots are
/// final, but the PC is a PSEUDO-OP INDEX, not a byte offset.
///
/// The two cannot be the same value on this backend and that is the whole
/// reason this type exists. `Arm64Instruction` is a pseudo-op stream, not a
/// fixed-width one: `Label` and `Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, and `MovImm` / `AddImm` / `CmpImm` / out-of-range `Ldr`/`Str`
/// expand to one to four words (`mov_imm64`, `emit_addsub_imm_safe`,
/// `emit_addr_into_ip0`). So the `instruction_count * 4` the writer used to
/// record was wrong for any method containing one of those, and a map keyed by
/// a wrong PC is worse than no map -- the GC reads the WRONG FRAME SLOTS at a
/// real safepoint and either misses a live reference or rewrites a primitive.
///
/// Keeping the unresolved form in its own type means an unresolved PC cannot be
/// mistaken for a resolved one by a later reader: there is no `OopMapEntry`
/// anywhere until [`emit_machine_code_with_oop_maps`] has run the encoder and
/// can say what the byte offset actually is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm64PendingOopMap {
    /// Index into [`Arm64CompileResult::instructions`] of the pseudo-op that
    /// FOLLOWS the safepoint -- the same "return address" convention the x64
    /// backend uses for `OopMapEntry::native_pc_offset`.
    pub pseudo_index: u32,
    /// Frame-slot offsets (relative to FP) holding object references here.
    pub frame_slot_offsets: Vec<i16>,
}

pub struct Arm64CompileResult {
    pub instructions: Vec<Arm64Instruction>,
    pub frame: Arm64FrameLayout,
    pub labels: HashMap<u32, usize>,
    pub success: bool,
    /// T1.1.3 — this compilation's safepoint oop maps, PC-UNRESOLVED.
    ///
    /// Each entry records the frame-slot offsets (relative to FP on AArch64;
    /// x86-64 uses RBP) that hold object references at a GC-capable safepoint,
    /// keyed by pseudo-op index. [`emit_machine_code_with_oop_maps`] turns
    /// these into `crate::OopMapEntry` values keyed by real byte offsets --
    /// see [`Arm64PendingOopMap`] for why the compiler cannot do that itself.
    ///
    /// **Still empty in practice, for a reason that is no longer the writer.**
    /// The writer is correct now; what is missing is a SAFEPOINT to call it at.
    /// This backend lowers no allocation, no call and no monitor, and refuses
    /// back edges, so a compiled method contains no GC-capable point at all --
    /// see the "Safety-critical gaps" section of the module header. The first
    /// real safepoint on this backend inherits a correct map writer instead of
    /// the mis-keyed one that used to be here.
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
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
    /// Per-bci operand-stack kinds, for the stack-shuffle opcodes only.
    ///
    /// This backend keeps TWO simulated stacks — `operand_stack` for
    /// int/long/reference and `float_operand_stack` for float/double — and its
    /// shuffle arms only ever touched the first one, by a fixed number of
    /// entries. Both assumptions are wrong in general: `dup` of a `double`
    /// shuffles the wrong stack entirely, and every `pop2`/`dup2*` form except
    /// the all-category-1 one touches a different number of entries than the
    /// arm popped. See [`Arm64Backend::int_stack_shuffle_entries`].
    ///
    /// Populated with EMPTY metadata, which costs nothing here: the analysis
    /// needs field types, call arities and constant-pool tags, and this backend
    /// refuses every method containing a field access, a call of any kind, or
    /// any `ldc`, so no admissible method has a site the analysis would need
    /// them for.
    stack_kinds: crate::x64::stack_kinds::StackKindMap,
    /// Simulated float operand stack (V registers).
    float_operand_stack: Vec<Arm64Register>,
    /// Next scratch register to hand out (cycles through X9-X15).
    scratch_cursor: u8,
    /// Next float scratch register to hand out (cycles through V0-V7).
    float_scratch_cursor: u8,
    /// Bytecode PC -> label mapping for branch targets.
    pc_labels: HashMap<usize, u32>,
    /// Bytecode PC of the instruction currently being lowered.
    ///
    /// Read by [`Arm64Backend::label_for_pc`] to classify a branch target as
    /// forward or backward. A backward target is a loop back-edge, and this
    /// backend has no safepoint poll to put on one — see the "no GC safepoint
    /// polls" note in the module header and `docs/jit/aarch64-parity.md`.
    cur_bytecode_pc: usize,
    /// `max_stack` for this compilation, needed to place the safepoint home
    /// slots after the operand area.
    max_stack: usize,
    /// Per-bytecode-pc "must be oop" local masks, and whether the dataflow
    /// reached each pc. Shared with x64 (`compute_local_oop_masks`) -- the
    /// analysis is pure bytecode. Empty when unsupported (>64 locals), which
    /// this backend treats as "no claim" and falls back to the conservative
    /// scan for.
    local_oop_masks: Vec<u64>,
    local_oop_reached: Vec<bool>,
    /// Which parameter slots hold references on entry. The ENTRY poll answers
    /// from this: the dataflow's own bci-0 state is seeded with it, but the
    /// prologue poll runs before the walk, so it reads the seed directly.
    /// Zero unless [`Arm64Backend::set_method_descriptor`] was called.
    param_oop_mask: u64,
    /// Frame offsets of reference LOCALS at the safepoint being emitted, folded
    /// into the map by `emit_oop_map_for_safepoint`. Taken, not copied, so a
    /// site that stages them without emitting a map cannot leak them into a
    /// later safepoint.
    pending_local_oop_slots: Vec<i32>,
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
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Runtime helper addresses. Zeroed until [`Arm64Backend::set_helpers`] is
    /// called, and `safepoint_flag_addr == 0` is the same "not wired" contract
    /// the x64 backend uses: the poll emitter then emits nothing at all.
    helpers: crate::JitRuntimeHelpers,
    /// Bytecode PCs that are the target of a BACKWARD branch -- loop headers.
    /// Discovered by pass 1 (see `compile_method_with_info`) and read by pass 2,
    /// which emits a safepoint poll at each one.
    back_edge_targets: std::collections::HashSet<usize>,
    /// Whether this compilation emits safepoint polls, seeded from
    /// [`arm64_safepoints_enabled`] in `new()`.
    ///
    /// A FIELD rather than a direct call to that function, because the function
    /// latches a `OnceLock` for the life of the process and a test needs both
    /// arms in one binary. `set_safepoints_enabled` is the only other writer.
    safepoints_enabled: bool,
}

/// Scratch registers available for the operand stack (X9-X15, 7 regs).
const SCRATCH_REGS: [Arm64Register; 7] = [
    Arm64Register::X9,
    Arm64Register::X10,
    Arm64Register::X11,
    Arm64Register::X12,
    Arm64Register::X13,
    Arm64Register::X14,
    Arm64Register::X15,
];

/// Float scratch registers for the float operand stack (V0-V7, 8 regs).
const FLOAT_SCRATCH_REGS: [Arm64Register; 8] = [
    Arm64Register::V0,
    Arm64Register::V1,
    Arm64Register::V2,
    Arm64Register::V3,
    Arm64Register::V4,
    Arm64Register::V5,
    Arm64Register::V6,
    Arm64Register::V7,
];

impl Arm64Backend {
    pub fn new() -> Self {
        Self {
            buffer: Arm64CodeBuffer::new(),
            frame: None,
            local_regs: Vec::new(),
            float_local_regs: Vec::new(),
            operand_stack: Vec::new(),
            stack_kinds: crate::x64::stack_kinds::StackKindMap::default(),
            float_operand_stack: Vec::new(),
            scratch_cursor: 0,
            float_scratch_cursor: 0,
            pc_labels: HashMap::new(),
            cur_bytecode_pc: 0,
            max_stack: 0,
            local_oop_masks: Vec::new(),
            local_oop_reached: Vec::new(),
            param_oop_mask: 0,
            pending_local_oop_slots: Vec::new(),
            epilogue_label: 0,
            num_params: 0,
            method_info: HashMap::new(),
            failed: false,
            spill_map: HashMap::new(),
            operand_stack_oop_marks: Vec::new(),
            pending_oop_maps: Vec::new(),
            // SAFETY: `JitRuntimeHelpers` is a plain struct of `usize`
            // addresses; all-zero is its documented "nothing wired" state, and
            // `safepoint_flag_addr == 0` is what gates the poll emitter.
            helpers: unsafe { std::mem::zeroed() },
            back_edge_targets: std::collections::HashSet::new(),
            safepoints_enabled: arm64_safepoints_enabled(),
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

    /// Record this safepoint's oop map: the frame slots that hold object
    /// references right now, keyed so the encoder can give them a real PC.
    ///
    /// # Why this does not produce an `OopMapEntry`
    ///
    /// It cannot, yet. The PC an `OopMapEntry` needs is a BYTE OFFSET into the
    /// emitted code, and at compile time this backend has only a pseudo-op
    /// stream whose entries are not 4 bytes each -- `Label` and `Comment` emit
    /// nothing, `ConstantPoolEntry` emits 8 bytes, `MovImm`/`AddImm`/`CmpImm`
    /// and out-of-range `Ldr`/`Str` expand to one to four words. The 2026-08-01
    /// parity audit found this helper keying its map as
    /// `instruction_count * 4` and made it fail the method closed rather than
    /// let a first caller inherit a wrong PC, noting that the fix "is to key
    /// oop maps off the *encoder's* byte offset (`Aarch64Emitter::offset()` in
    /// `emit_machine_code`), not off the pseudo-op count -- then delete this
    /// guard".
    ///
    /// This is that fix. The map is recorded against the pseudo-op INDEX of the
    /// instruction that follows the safepoint, in an
    /// [`Arm64PendingOopMap`] that cannot be confused for a resolved one, and
    /// [`emit_machine_code_with_oop_maps`] translates it once the encoder knows
    /// where each pseudo-op landed. The guard is gone.
    ///
    /// # What is still missing, and it is not this
    ///
    /// A SAFEPOINT to call this from. The backend lowers no allocation, no call
    /// and no monitor and refuses back edges, so a compiled method contains no
    /// GC-capable point -- which is why this still has no production call site
    /// and `pending_oop_maps` is still empty in practice. What changed is that
    /// the first one will inherit a correct writer.
    ///
    /// This walks the OPERAND stack. Reference LOCALS reach the map through
    /// `pending_local_oop_slots`, staged by `emit_safepoint_poll`, which stores
    /// a register-homed one to a reserved home first -- see there for why a
    /// callee-saved register is not good enough for a relocating collector.
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

        // The pseudo-op that will FOLLOW this safepoint. `instruction_count()`
        // is `instructions.len()`, i.e. the index the next `emit` will occupy,
        // which is the same "return address" convention x64 records.
        let pseudo_index = match u32::try_from(self.buffer.instruction_count()) {
            Ok(n) => n,
            // A method with more than 4 billion pseudo-ops cannot occur, but a
            // silent truncation here would be a wrong PC again. Refuse.
            Err(_) => {
                self.failed = true;
                return;
            }
        };

        let mut slots: Vec<i16> = Vec::new();
        for (i, &mark) in self.operand_stack_oop_marks.iter().enumerate() {
            if !mark {
                continue;
            }
            let reg = match self.operand_stack.get(i) {
                Some(r) => *r,
                None => continue,
            };
            if let Some(&spill_off) = self.spill_map.get(&reg) {
                match i16::try_from(spill_off) {
                    Ok(off16) => {
                        if !slots.contains(&off16) {
                            slots.push(off16);
                        }
                    }
                    // A spill slot further than `i16` from FP. The x64 side
                    // counts this (`map_incomplete_cause::STACK_OFF_TOO_DEEP`)
                    // and marks the map incomplete; here there is no
                    // completeness channel yet, so refuse the method rather
                    // than publish a map that silently drops a live reference.
                    Err(_) => {
                        self.failed = true;
                        return;
                    }
                }
            }
        }
        // The reference LOCALS staged by `emit_safepoint_poll`. Taken, not
        // copied, so a site that stages them without emitting a map cannot leak
        // them into a later safepoint (the same discipline x64's Stage 3 uses).
        for off in std::mem::take(&mut self.pending_local_oop_slots) {
            match i16::try_from(off) {
                Ok(off16) => {
                    if !slots.contains(&off16) {
                        slots.push(off16);
                    }
                }
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        if slots.is_empty() {
            return;
        }
        self.pending_oop_maps.push(Arm64PendingOopMap {
            pseudo_index,
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
                // THE BASE IS THE FRAME-HOMED LOCAL COUNT, not `num_reg_locals`.
                // Those are complements: `num_reg_locals` counts the locals that
                // got a REGISTER, while the spill area holds the ones that did
                // not. Basing operands at the former aliased a local's slot
                // whenever fewer than half the locals were register-homed, and
                // ran past the reserved area into the callee-save slots when
                // more than half were. `num_spills = gpr_spills + max_stack` is
                // sized for this base.
                let base = self.local_spill_count();
                let frame = self.frame.as_ref().unwrap();
                let spill_slot = base + pos;
                let offset = frame.spill_offset
                    + i32::try_from(spill_slot)
                        .unwrap_or(i32::MAX)
                        .saturating_mul(8);
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

    /// Supply the runtime helper addresses this backend needs for a safepoint
    /// poll. Without it `safepoint_flag_addr` stays 0 and
    /// [`Self::emit_safepoint_poll`] emits nothing.
    pub fn set_helpers(&mut self, helpers: crate::JitRuntimeHelpers) {
        self.helpers = helpers;
    }

    /// Override the safepoint-poll decision for this compilation.
    ///
    /// Exists because [`arm64_safepoints_enabled`] latches a `OnceLock`, so a
    /// test binary can only ever observe one arm of it; both arms have to be
    /// reachable, since "off is byte-identical to before" is itself a claim
    /// that needs asserting.
    pub fn set_safepoints_enabled(&mut self, on: bool) {
        self.safepoints_enabled = on;
    }

    /// Seed the reference-parameter mask from this method's descriptor.
    ///
    /// Must be called BEFORE compiling: `compile_pass` feeds it to
    /// `compute_local_oop_masks` as the dataflow's entry state, and the ENTRY
    /// poll reads it directly (the prologue runs before the walk, so there is
    /// no bci to look up there).
    ///
    /// Without it the mask is 0, and a reference PARAMETER that is never
    /// `astore`d is never named -- covered by the conservative scan, but not
    /// precisely, which is the difference that matters to a relocating
    /// collector.
    pub fn set_method_descriptor(&mut self, descriptor: &str, is_static: bool) {
        self.param_oop_mask = crate::compute_param_oop_mask(descriptor, is_static);
    }

    /// Emit a cooperative GC safepoint poll.
    ///
    /// The x64 shape, transliterated (see `x64::Compiler::emit_safepoint_poll`):
    ///
    /// ```text
    ///     MOVZ/MOVK X16, #safepoint_flag_addr
    ///     LDRB      W17, [X16]          ; ONE byte -- the flag is an AtomicBool
    ///     CBZ       X17, skip           ; clear -> no safepoint requested
    ///     <spill live operand registers to frame slots>
    ///     MOVZ/MOVK X16, #safepoint_slow_path
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     <reload the spilled operand registers>
    ///   skip:
    /// ```
    ///
    /// # The register choice is the ABI's own answer
    ///
    /// X16/X17 are IP0/IP1, the intra-procedure-call scratch registers AAPCS64
    /// reserves for exactly this; they are caller-saved and hold no operand or
    /// local. Java locals live in X19-X28, which are callee-SAVED, so the call
    /// preserves them. The operand stack lives in X9-X15, which are caller-
    /// saved and would be destroyed -- hence the spill.
    ///
    /// # Why the spill and reload sit INSIDE the branch
    ///
    /// `spill_map` is a compile-time model that `pop_operand` consults to decide
    /// whether to reload. If the spill were emitted only on the taken path but
    /// recorded in `spill_map` unconditionally, then on the NOT-taken path
    /// (flag clear -- the overwhelmingly common case) `pop_operand` would emit a
    /// reload of a frame slot that was never written, reading garbage as a live
    /// value. So the entries are added for the duration of the map write and
    /// removed again, and the registers are restored before the join: the model
    /// on both paths is identical, which is the only way a compile-time model
    /// and a runtime branch can agree.
    ///
    /// # What the GC sees
    ///
    /// The oop map is recorded at the BLR's return address, naming the frame
    /// slots the spill just wrote -- the same convention x64 uses. Reference
    /// locals in callee-saved registers are NOT named: the callee spills
    /// X19-X28 into its own frame, which the conservative stack walk covers.
    /// That is sound for a MARKING collector and NOT for a relocating one,
    /// which cannot rewrite through a conservative scan -- so a moving
    /// collector on this backend needs register naming first. Same caveat as
    /// `emit_oop_map_for_safepoint`, restated here because this is the site
    /// that creates the exposure.
    fn emit_safepoint_poll(&mut self, entry: bool) {
        if self.failed || !self.safepoints_enabled {
            return;
        }
        // The "not wired" contract, identical to x64's: no flag address means
        // no poll code at all, rather than a call through a null pointer.
        if self.helpers.safepoint_flag_addr == 0 || self.helpers.safepoint_slow_path == 0 {
            return;
        }
        let skip = self.buffer.new_label();
        // Cast: a helper address is a real mapped pointer, always < i64::MAX.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_flag_addr as i64,
        });
        self.buffer.emit(Arm64Instruction::Ldrb {
            rt: Arm64Register::X17,
            rn: Arm64Register::X16,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: Arm64Register::X17,
            label: skip,
        });

        // Spill every live operand register that is not already spilled, and
        // remember which ones WE added so the removal below is exact.
        let mut added: Vec<Arm64Register> = Vec::new();
        let live: Vec<Arm64Register> = self.operand_stack.clone();
        for (depth, reg) in live.iter().enumerate() {
            if self.spill_map.contains_key(reg) || added.contains(reg) {
                continue;
            }
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                // No slot reserved for this depth. Publishing a poll whose
                // spill cannot be placed would leave a live reference in a
                // caller-saved register across a CALL, so refuse the method.
                self.failed = true;
                return;
            };
            self.buffer.emit(Arm64Instruction::Str {
                rt: *reg,
                rn: Arm64Register::FP,
                offset,
            });
            self.spill_map.insert(*reg, offset);
            added.push(*reg);
        }

        // NAME THE REFERENCE LOCALS.
        //
        // A frame-homed local is already in a slot, so it only has to be named.
        // A REGISTER-homed one is the case this exists for: X19-X28 are
        // callee-saved, so its value survives the call -- but it survives
        // inside the CALLEE's saved-register area, where only the conservative
        // walk can see it, and a conservative walk marks without being able to
        // REWRITE. A relocating collector therefore cannot move an object whose
        // only root is a register local. Storing it to a reserved home makes it
        // a nameable, rewritable root; the reload after the call is what carries
        // a moved object's new address back into the register.
        let mut reg_homed: Vec<(usize, i32)> = Vec::new();
        if let Some(mut mask) = self.oop_locals_at_current_pc(entry) {
            while mask != 0 {
                // Cast: count/index to usize
                let i = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                if i >= self.local_regs.len() {
                    continue;
                }
                if self.local_regs.get(i).copied().flatten().is_some() {
                    let (Some(off), Some(reg)) = (
                        self.safepoint_home_for_reg_local(i),
                        self.local_regs.get(i).copied().flatten(),
                    ) else {
                        // No home reserved: refuse rather than leave a live
                        // reference reachable only through a conservative scan
                        // while claiming to have named the frame.
                        self.failed = true;
                        return;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: reg,
                        rn: Arm64Register::FP,
                        offset: off,
                    });
                    self.pending_local_oop_slots.push(off);
                    reg_homed.push((i, off));
                } else {
                    // Frame-homed: already where the GC can read and rewrite it.
                    let Some(frame) = self.frame.as_ref() else {
                        self.failed = true;
                        return;
                    };
                    let slot = self.spill_index_for(i);
                    let Some(off) = i32::try_from(slot)
                        .ok()
                        .and_then(|n| n.checked_mul(8))
                        .and_then(|n| frame.spill_offset.checked_add(n))
                    else {
                        self.failed = true;
                        return;
                    };
                    self.pending_local_oop_slots.push(off);
                }
            }
        }

        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_slow_path as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        // At the return address, with the operand oops in frame slots.
        self.emit_oop_map_for_safepoint();

        // Reload every register-homed local the GC may have REWRITTEN. Without
        // this the frame slot carries the object's new address while the
        // register still holds the old one -- the map would be correct and the
        // running code would not.
        for (i, off) in &reg_homed {
            if let Some(reg) = self.local_regs.get(*i).copied().flatten() {
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: reg,
                    rn: Arm64Register::FP,
                    offset: *off,
                });
            }
        }

        // Restore, and put the compile-time model back exactly as it was.
        for reg in &added {
            if let Some(offset) = self.spill_map.remove(reg) {
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: *reg,
                    rn: Arm64Register::FP,
                    offset,
                });
            }
        }
        self.buffer.bind_label(skip);
    }

    /// The oop-local mask in force at the safepoint being emitted, or `None`
    /// when no claim can be made.
    ///
    /// `None` is a REFUSAL, not "no oop locals": the dataflow is empty above 64
    /// locals and unreached at pcs only an exception edge can arrive at, and
    /// treating either as "nothing live" is how a collector loses a root. The
    /// caller falls back to naming nothing, which leaves those frames to the
    /// conservative scan -- correct, just less precise.
    fn oop_locals_at_current_pc(&self, entry: bool) -> Option<u64> {
        if entry {
            // The prologue poll runs before the walk, so there is no bci to
            // look up; the live oops there are exactly the reference
            // parameters, which is what seeds the dataflow at bci 0.
            return Some(self.param_oop_mask);
        }
        if self.local_oop_masks.is_empty() {
            return None;
        }
        if !self
            .local_oop_reached
            .get(self.cur_bytecode_pc)
            .copied()
            .unwrap_or(false)
        {
            return None;
        }
        self.local_oop_masks.get(self.cur_bytecode_pc).copied()
    }

    /// Frame offset of the safepoint home reserved for register-homed local
    /// `index`, or `None` if it has no register (it lives in a spill slot
    /// already) or no home was reserved.
    ///
    /// Homes sit after the operand area: `local_spill_count() + max_stack + k`,
    /// where `k` numbers the register-homed locals in order. That is the tail
    /// `num_spills` was extended by, so a home can never collide with a local's
    /// slot or an operand's.
    fn safepoint_home_for_reg_local(&self, index: usize) -> Option<i32> {
        self.local_regs.get(index).copied().flatten()?;
        let k = (0..index)
            .filter(|&i| self.local_regs.get(i).copied().flatten().is_some())
            .count();
        let frame = self.frame.as_ref()?;
        let slot = self
            .local_spill_count()
            .checked_add(self.max_stack)?
            .checked_add(k)?;
        if slot >= frame.num_spills {
            return None;
        }
        let scaled = i32::try_from(slot).ok()?.checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// Frame offset of the spill slot for operand-stack depth `depth`, or
    /// `None` when that slot is outside the reserved spill area.
    ///
    /// MIRRORS `alloc_scratch` EXACTLY, sharing its base through
    /// [`Self::local_spill_count`]. The two are the only writers of `spill_map`
    /// and must agree about where a given depth lives, or the oop map names a
    /// slot nothing wrote.
    ///
    /// (An earlier version of this comment said the base was `num_reg_locals`
    /// because "the spill area holds the register-homed locals FIRST". That is
    /// backwards -- the spill area holds the locals that got NO register -- and
    /// copying `alloc_scratch` faithfully copied its bug. See
    /// `operand_spill_slots_do_not_alias_frame_homed_locals`.)
    ///
    /// `alloc_scratch` reaches this arithmetic only for a depth it has already
    /// proved live, so it can saturate; the poll can be asked about any depth,
    /// so it bound-checks against the reserved area and refuses instead.
    fn spill_offset_for_depth(&self, depth: usize) -> Option<i32> {
        let base = self.local_spill_count();
        let frame = self.frame.as_ref()?;
        let spill_slot = base.checked_add(depth)?;
        if spill_slot >= frame.num_spills {
            return None;
        }
        let scaled = i32::try_from(spill_slot).ok()?.checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// Run the shared operand-stack kind analysis over `bytecode`.
    ///
    /// See the `stack_kinds` field for why empty metadata is sufficient on
    /// this backend.
    fn analyze_stack_kinds(bytecode: &[u8]) -> crate::x64::stack_kinds::StackKindMap {
        use crate::x64::stack_kinds::{analyze, StackKindInputs};
        let refs = rustc_hash::FxHashSet::default();
        let inputs = StackKindInputs {
            field_types: rustc_hash::FxHashMap::default(),
            static_types: rustc_hash::FxHashMap::default(),
            calls: rustc_hash::FxHashMap::default(),
            ldc_refs: &refs,
            ldc_fp: &refs,
            ldc_resolved: &refs,
            handler_pcs: &[],
        };
        analyze(bytecode, bytecode.len(), &inputs)
    }

    /// How many `operand_stack` entries the stack-shuffle at `pc` may touch,
    /// or `None` when this backend must refuse the method.
    ///
    /// `want` is the number of TOP-OF-STACK entries the arm needs to be
    /// integer-stack values; the answer is `Some(n)` only when the analysis
    /// types all of them and none is a `float`/`double`.
    ///
    /// **Why a refusal and not a shuffle.** The shuffle arms below were written
    /// against a single stack of category-1 values, and this backend has
    /// neither property:
    ///
    ///   * A `float`/`double` operand lives on `float_operand_stack`. Popping
    ///     `operand_stack` for it takes an unrelated value — or underflows into
    ///     a caller's entry — and pushes the shuffled result onto a stack the
    ///     consuming arm will not read.
    ///   * A `long` is ONE entry here and TWO JVM slots, so every
    ///     `pop2`/`dup2`/`dup2_x1`/`dup2_x2`/`dup_x2` form except the
    ///     all-category-1 one touches a different number of entries than the
    ///     arm popped.
    ///
    /// Both produce a silently wrong operand stack, which this file's own
    /// `irem`/`lrem` note already calls worse than a bail: "A silent wrong
    /// answer is worse than a bail". This is the answer to the question the
    /// x64 `dup2_x2` page left open — whether that backend's unconditional
    /// four-pop was live here. It was, and so were four more arms.
    fn int_stack_shuffle_entries(&mut self, pc: usize, want: usize) -> Option<Vec<bool>> {
        let kinds = self.stack_kinds.get(pc)?;
        if kinds.len() < want {
            return None;
        }
        let mut cats = Vec::with_capacity(want);
        for k in kinds[kinds.len() - want..].iter().rev() {
            match k {
                crate::x64::stack_kinds::StackKind::Int
                | crate::x64::stack_kinds::StackKind::Ref => cats.push(false),
                crate::x64::stack_kinds::StackKind::Long => cats.push(true),
                // Float / Double live on the OTHER stack; Unknown is not a
                // guess this may make.
                _ => return None,
            }
        }
        // `cats[0]` is the top, `cats[1]` the entry below it, ...
        Some(cats)
    }

    /// The shared `dup2_x1` / `dup2_x2` shuffle:
    /// `[under.., group..] -> [group.., under.., group..]`, counted in
    /// `operand_stack` ENTRIES rather than JVM slots.
    ///
    /// Both opcodes differ only in how many entries each group is, and both
    /// resolve that from [`Arm64Backend::int_stack_shuffle_entries`] before
    /// calling here, so this routine never has to guess a category.
    fn emit_dup_group_over(&mut self, group_entries: usize, under_entries: usize) {
        // Pop top-down: `group[0]` is the topmost value.
        let mut group = Vec::with_capacity(group_entries);
        for _ in 0..group_entries {
            group.push(self.pop_operand());
        }
        let mut under = Vec::with_capacity(under_entries);
        for _ in 0..under_entries {
            under.push(self.pop_operand());
        }
        // One fresh scratch per duplicated entry. `alloc_scratch` round-robins,
        // so take them all before emitting to avoid a copy landing in a
        // register a later copy is about to overwrite.
        let copies: Vec<Arm64Register> = (0..group_entries).map(|_| self.alloc_scratch()).collect();
        for (copy, src) in copies.iter().zip(group.iter()) {
            self.buffer.emit(Arm64Instruction::Mov {
                rd: *copy,
                rm: *src,
            });
        }
        for copy in copies.into_iter().rev() {
            self.push_operand(copy);
        }
        for reg in under.into_iter().rev() {
            self.push_operand(reg);
        }
        for reg in group.into_iter().rev() {
            self.push_operand(reg);
        }
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
    ///
    /// Also the single chokepoint where a **loop back-edge** is detected. Every
    /// branch target on this backend — `goto`, `if*`, `if_icmp*`, `if_acmp*`,
    /// and every `tableswitch`/`lookupswitch` case and default — is resolved
    /// through here, so a target at or before the instruction currently being
    /// lowered is exactly the set of back-edges.
    ///
    /// **Why that refuses the method (aarch64 parity audit, 2026-08-01).**
    /// x86-64 emits a cooperative safepoint poll of `helpers.safepoint_flag_addr`
    /// at method entry and at every loop back-edge
    /// (`x64::Backend::emit_safepoint_poll` /
    /// `emit_safepoint_poll_prologue`, enabled by default via
    /// `x64::jit_safepoint_polls_enabled`). This backend emits none, and
    /// it cannot: no safepoint-helper address is plumbed into
    /// `compile_method_with_info`, and even if it were, taking the poll requires
    /// a CALL — which `emit_invoke` refuses to emit because there is no
    /// call-target resolution here.
    ///
    /// A compiled loop therefore contains no safepoint of any kind. A thread
    /// inside one never observes a stop-the-world request, so any GC that needs
    /// to stop it hangs the whole VM — and since the compilable population is
    /// "leaf pure-arithmetic methods", loops are the *typical* case, not an edge
    /// case. The module header already documented this hazard; it was
    /// documented but not defended, and a straight-line-only compiled body is
    /// the only shape that is actually safe to run. So: bail, and interpret.
    ///
    /// This is a liveness bail, deliberately conservative — it refuses reducible
    /// and irreducible back-edges alike, and refuses a backward `goto` even when
    /// the loop provably terminates, because "provably terminates" is not the
    /// property that matters. What matters is bounded time to the next
    /// safepoint, and without a poll there is no bound.
    fn label_for_pc(&mut self, pc: usize) -> u32 {
        if pc <= self.cur_bytecode_pc {
            if self.safepoints_enabled {
                // A loop header. The blanket refusal below existed because a
                // compiled loop with no poll in it is a region a
                // stop-the-world request can never interrupt -- so with polls
                // available, RECORD it and let pass 2 put one there.
                self.back_edge_targets.insert(pc);
            } else {
                self.failed = true;
            }
        }
        self.label_for_pc_unchecked(pc)
    }

    /// Label allocation without the back-edge check.
    ///
    /// Used only by the walk loop's pre-seed step, which binds a label at a PC
    /// the *discovery* pass already identified as a branch target. That step is
    /// not itself a branch, so running the back-edge test there would misfire
    /// (notably at `pc == 0`, where `cur_bytecode_pc` is still its initial `0`).
    /// The branch that created the target already went through
    /// [`label_for_pc`], so nothing is missed.
    fn label_for_pc_unchecked(&mut self, pc: usize) -> u32 {
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
            None => {
                self.failed = true;
                return;
            }
        };
        let frame_size = frame.frame_size;

        // Bug-fix (ARM64 BUG #2, broken prologue SP/frame geometry):
        //
        // The previous prologue emitted a *signed-offset* (non-writeback) STP
        // `[SP,#-16]`, which does NOT decrement SP, then `MOV FP,SP`, then
        // `SUB SP,SP,#(frame-16)` under the false assumption that "16 was
        // already consumed by STP". Because the STP never moved SP, the frame
        // ended up 16 bytes too small at the bottom and, for a minimal frame,
        // SP could sit *above* the saved FP/LR slots — corrupting the frame.
        //
        // New scheme (AAPCS64-idiomatic, FP at top of frame, all callee-save /
        // spill offsets remain NEGATIVE from FP exactly as `Arm64FrameLayout`
        // computes them — no layout change required):
        //
        //   1. STP FP, LR, [SP, #-16]!   (writeback) — saves the caller's
        //      FP/LR and moves SP to old_SP-16. Small fixed offset, always in
        //      imm7 range, so it is robust for arbitrarily large frames.
        //   2. SUB SP, SP, #(frame-16)   — allocate the rest of the frame;
        //      SP now = old_SP - frame_size (the bottom).
        //   3. ADD FP, SP, #frame        — FP = old_SP (the top). The saved
        //      caller FP/LR therefore live at [FP-16], matching
        //      `callee_save_offset = -16 - callee_save_bytes` and the spill
        //      slots below it.
        //
        // The matching epilogue reverses this exactly (see `emit_epilogue`).
        self.buffer.emit(Arm64Instruction::StpPre {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: -16,
        });

        // Allocate the remainder of the frame (the writeback STP already
        // consumed the first 16 bytes).
        if frame_size > 16 {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: frame_size - 16,
            });
        }

        // Point FP at the top of the frame (old_SP). After this, the saved
        // FP/LR pair from step 1 sits at [FP-16].
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::FP,
            rn: Arm64Register::SP,
            imm: frame_size,
        });

        // Save callee-saved registers used for locals (in pairs).
        //
        // PERF: the previous code cloned the entire `saved_regs` Vec
        // (`&frame.saved_regs.clone()`) once per compiled method purely to
        // satisfy the borrow checker — the loop body needs `&mut self` for
        // `self.buffer.emit(...)`, which cannot coexist with a live borrow of
        // `self.frame` held across the call. `Arm64Register` is `Copy`, so
        // instead of cloning the whole Vec we hoist the two scalars we need
        // (the callee-save base offset and the element count) out of the borrow,
        // then copy out each register by briefly re-borrowing `self.frame` per
        // access. No per-method heap allocation; the emission sequence is
        // byte-for-byte identical to before.
        let callee_save_offset = frame.callee_save_offset;
        let saved_len = frame.saved_regs.len();
        // `frame` is unused past this point — its borrow ends here, freeing the
        // `self.buffer.emit` calls below to take `&mut self`.

        let mut i = 0;
        while i + 1 < saved_len {
            let offset = callee_save_offset + (i as i32) * 8;
            // Re-borrow `self.frame` only to copy out the two `Copy` registers;
            // the borrow ends before `self.buffer.emit` is invoked.
            let frame = self.frame.as_ref().expect("frame present");
            let (rt1, rt2) = (frame.saved_regs[i], frame.saved_regs[i + 1]);
            self.buffer.emit(Arm64Instruction::Stp {
                rt1,
                rt2,
                rn: Arm64Register::FP,
                offset,
            });
            i += 2;
        }
        if i < saved_len {
            let offset = callee_save_offset + (i as i32) * 8;
            let rt = self.frame.as_ref().expect("frame present").saved_regs[i];
            self.buffer.emit(Arm64Instruction::Str {
                rt,
                rn: Arm64Register::FP,
                offset,
            });
        }

        // METHOD-ENTRY SAFEPOINT POLL, emitted from the END of the prologue --
        // after FP is established and the callee-saved registers are stored, so
        // the frame the poll's CALL runs on top of is complete and walkable.
        // The operand stack is empty here, so the poll spills nothing.
        self.emit_safepoint_poll(true);
    }

    /// Emit the standard AAPCS64 epilogue.
    fn emit_epilogue(&mut self) {
        let frame = match self.frame.as_ref() {
            Some(f) => f,
            None => {
                self.failed = true;
                return;
            }
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

        // Bug-fix (ARM64 BUG #2): reverse the writeback prologue exactly.
        //
        // The caller's FP/LR were saved at [FP-16] (see `emit_prologue`).
        //   1. SUB SP, FP, #16          — SP = old_SP-16, the address the
        //      post-index LDP loads from (mirror of the prologue's
        //      `STP ...,[SP,#-16]!`).
        //   2. LDP FP, LR, [SP], #16    — restore the caller's FP/LR, then
        //      SP = old_SP (caller's stack pointer fully restored).
        self.buffer.emit(Arm64Instruction::SubImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::FP,
            imm: 16,
        });
        self.buffer.emit(Arm64Instruction::LdpPost {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: 16,
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
    ///
    /// Runs [`Arm64Backend::compile_pass`] **twice**. The walk binds a label at
    /// bytecode pc `p` only if `p` is already known to be a branch target when
    /// the walk reaches it, and targets are discovered lazily, as each branch
    /// is decoded. For a forward branch that is fine — the branch is decoded
    /// before its target is reached. For a BACKWARD branch (i.e. every loop
    /// back-edge) it is not: the target pc was walked past before the label
    /// existed, so the label was created and never bound, and the encoder's
    /// patch loop left the placeholder — a displacement-0 branch, which on
    /// AArch64 is a branch to itself. Every loop this backend "compiled" was
    /// therefore an infinite self-branch.
    ///
    /// The first pass exists purely to discover the complete set of branch
    /// target PCs; its output is discarded. The second pass re-walks the same
    /// bytecode with that set in hand and binds a label at each target as it
    /// passes, so back-edges resolve. Reusing the real walk for discovery
    /// (rather than a separate scanner) means the two can never disagree about
    /// instruction boundaries — this module has no bytecode length table to
    /// keep in sync.
    pub fn compile_method_with_info(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
    ) -> Arm64CompileResult {
        // Discovered by pass 1 and read by pass 2, so `compile_pass` must NOT
        // clear it (it clears `pc_labels`, which is why the back-edge set has
        // to live outside that reset). Cleared HERE instead, per compile: a
        // reused backend would otherwise carry another method's loop headers
        // and emit polls at unrelated PCs.
        self.back_edge_targets.clear();

        // Pass 1 — discovery. Label ids allocated here are NOT reused: pass 2
        // resets the buffer (and with it the label-id counter), so it hands
        // `compile_pass` bytecode PCs and lets it allocate its own ids.
        drop(self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info.clone(),
            &[],
        ));
        let mut branch_targets: Vec<usize> = self.pc_labels.keys().copied().collect();
        branch_targets.sort_unstable();
        // `failed` is sticky and intentionally NOT cleared between the two
        // passes: whatever made pass 1 refuse the method (e.g. an unresolved
        // invoke) makes pass 2 refuse it identically, and carrying the flag
        // keeps the two passes' verdicts in lockstep.

        // Pass 2 — emission, with every back-edge target known up front.
        self.compile_pass(
            num_locals,
            num_params,
            max_stack,
            bytecode,
            method_info,
            &branch_targets,
        )
    }

    /// One walk of the bytecode. See [`compile_method_with_info`] for why this
    /// runs twice and what `branch_targets` carries between the passes: a
    /// sorted list of every bytecode PC that is the target of some branch,
    /// used to bind a label at each such PC as the walk passes it (the lazy
    /// `label_for_pc` discovery below cannot see backward branches in time).
    fn compile_pass(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
        branch_targets: &[usize],
    ) -> Arm64CompileResult {
        // Reset state.
        self.buffer = Arm64CodeBuffer::new();
        self.operand_stack.clear();
        self.float_operand_stack.clear();
        self.scratch_cursor = 0;
        self.float_scratch_cursor = 0;
        self.pc_labels.clear();
        self.cur_bytecode_pc = 0;
        self.local_regs.clear();
        self.float_local_regs.clear();
        self.spill_map.clear();
        self.num_params = num_params;
        self.method_info = method_info;
        self.stack_kinds = Self::analyze_stack_kinds(bytecode);
        self.max_stack = max_stack;
        self.pending_local_oop_slots.clear();
        // The same "must be oop" local dataflow x64 uses, seeded with this
        // method's reference parameters. A bit is set only when EVERY path
        // reaching that pc stored a reference there, which is what a
        // relocating collector needs: a false positive would have the GC
        // rewrite a primitive that happens to look like an address.
        let (lo_masks, lo_reached) = crate::x64::compute_local_oop_masks(
            bytecode,
            bytecode.len(),
            num_locals,
            self.param_oop_mask,
        );
        self.local_oop_masks = lo_masks;
        self.local_oop_reached = lo_reached;

        // Run graph-coloring register allocation for ARM64.
        let alloc = super::regalloc::allocate_registers_arm64(
            bytecode,
            bytecode.len(),
            num_locals,
            num_params,
            &[],
        );

        // Build local_regs from GPR assignments (u8 register numbers → Arm64Register).
        for &a in &alloc.assignments {
            self.local_regs.push(a.map(Arm64Register));
        }
        // Pad if allocator returned fewer entries than num_locals
        while self.local_regs.len() < num_locals {
            self.local_regs.push(None);
        }

        // Float/double locals get NO dedicated FP register on this backend.
        //
        // Bug-fix (aarch64 parity audit, 2026-08-01 — AAPCS64 violation):
        // `regalloc::ARM64_LOCAL_FPS` is `D8..D15`, and on AAPCS64 those are
        // precisely the **callee-saved** FP registers
        // (the low 64 bits of V8-V15 must be preserved across a call). This
        // backend's prologue/epilogue save and restore only the callee-saved
        // GPRs in `alloc.used_callee_saved` — `alloc.used_xmm_regs` is never
        // consulted and `Arm64FrameLayout` reserves no space for FP saves. So
        // the previous code, which homed float locals in D8-D15, silently
        // destroyed the *caller's* D8-D15 on every compiled method that used a
        // float or double local. The caller is either the interpreter (Rust,
        // compiled by LLVM, which very much does keep values in D8-D15 across
        // calls) or another compiled frame; either way it is corruption.
        //
        // Two ways to close it: save/restore the used FP registers in the
        // prologue/epilogue, or stop allocating them. The second is chosen here
        // — it is the change that cannot itself be wrong, and this backend
        // compiles nothing whose performance is worth the risk. Float locals now
        // live in a frame slot (via `FpLdr`/`FpStr`, whose negative-offset
        // lowering is fixed in `emit_machine_code`) or, when the allocator gave
        // the slot a GPR, in that GPR via `FmovToFp`/`FmovFromFp`.
        //
        // If FP homing is ever wanted back, the prerequisite is an FP
        // save/restore area in `Arm64FrameLayout::compute` plus prologue and
        // epilogue emission driven by `alloc.used_xmm_regs` — not a revert of
        // this loop. Asserted by `float_locals_never_use_callee_saved_fp_regs`.
        for _ in 0..num_locals {
            self.float_local_regs.push(None);
        }

        // Determine which callee-saved GPR regs we actually use.
        let saved_regs: Vec<Arm64Register> = alloc
            .used_callee_saved
            .iter()
            .map(|&n| Arm64Register(n))
            .collect();

        // Count spills: locals without a register + operand stack space.
        let gpr_spills = alloc.assignments.iter().filter(|a| a.is_none()).count();
        let fp_spills = alloc
            .xmm_assignments
            .iter()
            .enumerate()
            .filter(|(i, a)| a.is_none() && self.local_regs.get(*i).map_or(false, |g| g.is_none()))
            .count();
        let _ = fp_spills; // float spills use the same frame slots
        // One extra spill word per REGISTER-HOMED local, reserved only when
        // this compilation emits polls.
        //
        // A register-homed local has no frame slot at all on this backend --
        // `spill_index_for` numbers only the locals that got NO register -- so
        // there is nowhere for a safepoint to put it. The prologue's
        // callee-save slots cannot be borrowed either: those hold the CALLER's
        // values and the epilogue restores from them. Hence a dedicated home,
        // placed after the operand area. See `safepoint_home_for_reg_local`.
        let safepoint_homes = if self.safepoints_enabled {
            saved_regs.len()
        } else {
            0
        };
        let num_spills = gpr_spills + max_stack + safepoint_homes;
        let layout = Arm64FrameLayout::compute(num_locals, num_spills, &saved_regs);

        // Refuse frames that could step over the stack guard page.
        //
        // Bug-fix (aarch64 parity audit, 2026-08-01 — missing stack bang):
        // x86-64 probes every page the new frame crosses BEFORE moving RSP
        // (`x64::Backend::emit_stack_bang_before_frame_alloc`, page size
        // `x64::reg_encoding::STACK_BANG_PAGE_SIZE == 4096`), which converts
        // stack exhaustion
        // into a fault ON the guard page — recoverable, and reported as
        // `StackOverflowError`. This backend emits no bang at all: its prologue
        // is a bare `SUB SP, SP, #frame_size`. A frame larger than one guard
        // page can therefore move SP clean PAST the guard and land in unrelated
        // mapped memory, at which point the first spill store silently corrupts
        // whatever is there instead of trapping.
        //
        // `frame_size` here is attacker-influenced in the ordinary sense —
        // `max_locals` and `max_stack` come from the class file and are u16 —
        // so this is not theoretical: `num_spills = gpr_spills + max_stack`,
        // giving frames up to ~512 KiB. Until a bang exists, any frame that
        // could reach beyond the first guard page is refused.
        const AARCH64_GUARD_PAGE_BYTES: i32 = 4096;
        if layout.frame_size >= AARCH64_GUARD_PAGE_BYTES {
            self.failed = true;
        }
        self.frame = Some(layout);

        self.epilogue_label = self.buffer.new_label();

        // Emit prologue.
        self.emit_prologue();

        // Copy incoming args to local registers.
        for i in 0..num_params.min(8) {
            if let Some(local_reg) = self.local_regs.get(i).copied().flatten() {
                let arg_reg = Arm64CallingConvention::INT_ARG_REGS[i];
                if arg_reg != local_reg {
                    self.buffer.emit(Arm64Instruction::Mov {
                        rd: local_reg,
                        rm: arg_reg,
                    });
                }
            }
        }

        // Walk bytecode.
        let mut pc = 0;
        let mut success = true;
        while pc < bytecode.len() {
            // Pre-seed this PC's label if the discovery pass saw a branch to
            // it. Without this, only forward branches (whose target is reached
            // after the branch is decoded) ever get bound — see
            // `compile_method_with_info`.
            if branch_targets.binary_search(&pc).is_ok() {
                let _ = self.label_for_pc_unchecked(pc);
            }

            // Bind label if any branch targets this PC.
            if let Some(&label) = self.pc_labels.get(&pc) {
                if !self.buffer.labels.contains_key(&label) {
                    self.buffer.bind_label(label);
                }
            }

            // LOOP-HEADER SAFEPOINT POLL. Emitted after the label is bound, so
            // a back edge jumps to the label and lands on the poll -- one poll
            // per header regardless of how many branches target it, which is
            // why this is here and not at the ~11 branch sites. (A forward
            // branch to the same label also lands on it; an extra poll is
            // harmless.) `back_edge_targets` comes from pass 1, so a header is
            // known before pass 2 reaches it.
            if self.back_edge_targets.contains(&pc) {
                self.emit_safepoint_poll(false);
            }

            let opcode = bytecode[pc];
            let start_pc = pc;
            // Publish the instruction boundary before lowering, so every
            // `label_for_pc` call made by this opcode's arm can tell a forward
            // branch from a back-edge (see `label_for_pc`).
            self.cur_bytecode_pc = start_pc;
            pc += 1;

            match opcode {
                // iconst_m1 .. iconst_5
                0x02..=0x08 => {
                    let value = opcode as i32 - 3;
                    self.emit_iconst(value);
                }
                // bipush
                0x10 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let val = bytecode[pc] as i8 as i32;
                    pc += 1;
                    self.emit_iconst(val);
                }
                // sipush
                0x11 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let val = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    self.emit_iconst(val);
                }
                // iload_0 .. iload_3
                0x1a..=0x1d => self.emit_iload((opcode - 0x1a) as usize),
                // iload (wide index)
                0x15 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_iload(idx);
                }
                // istore_0 .. istore_3
                0x3b..=0x3e => self.emit_istore((opcode - 0x3b) as usize),
                // istore (wide index)
                0x36 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                // idiv — see the `division / remainder` note in the module
                // header. `emit_int_div`'s zero guard branches to `BRK #1`,
                // which raises SIGTRAP; nothing in the VM converts that into
                // an `ArithmeticException`, so a `x / 0` in compiled code
                // killed the process instead of throwing. Refuse the method
                // until a real exception path exists.
                0x6c => {
                    success = false;
                    break;
                }
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
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Eq, target);
                }
                // if_icmpne
                0xa0 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Ne, target);
                }
                // if_icmplt
                0xa1 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Lt, target);
                }
                // if_icmpge
                0xa2 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Ge, target);
                }
                // if_icmpgt
                0xa3 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Gt, target);
                }
                // if_icmple
                0xa4 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Le, target);
                }
                // goto
                0xa7 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    self.push_operand(dst);
                    // T1.1.3 — null is a valid object reference.
                    self.mark_top_operand_as_oop();
                }
                // dup — duplicate top of stack
                0x59 => {
                    // JVMS: category-1 only. A `double` on top would live on
                    // `float_operand_stack`, so duplicating `operand_stack`'s
                    // top copies an unrelated value.
                    match self.int_stack_shuffle_entries(start_pc, 1) {
                        Some(c) if !c[0] => {}
                        _ => {
                            success = false;
                            break;
                        }
                    }
                    let top = self.pop_operand();
                    let dup = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Mov { rd: dup, rm: top });
                    self.push_operand(top);
                    self.push_operand(dup);
                }
                // pop
                0x57 => {
                    // JVMS: category-1 only, and it must be on the int stack.
                    match self.int_stack_shuffle_entries(start_pc, 1) {
                        Some(c) if !c[0] => {}
                        _ => {
                            success = false;
                            break;
                        }
                    }
                    let _ = self.pop_operand();
                }
                // pop2
                0x58 => {
                    // FORM 2 is a single category-2 entry here, not two.
                    let Some(cats) = self.int_stack_shuffle_entries(start_pc, 1) else {
                        success = false;
                        break;
                    };
                    if cats[0] {
                        let _ = self.pop_operand();
                    } else {
                        match self.int_stack_shuffle_entries(start_pc, 2) {
                            Some(c) if !c[1] => {}
                            _ => {
                                success = false;
                                break;
                            }
                        }
                        let _ = self.pop_operand();
                        let _ = self.pop_operand();
                    }
                }
                // swap
                0x5f => {
                    // JVMS: both operands category-1.
                    match self.int_stack_shuffle_entries(start_pc, 2) {
                        Some(c) if !c[0] && !c[1] => {}
                        _ => {
                            success = false;
                            break;
                        }
                    }
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
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    let gt_label = self.buffer.new_label();
                    let end_label = self.buffer.new_label();
                    // If GT, set 1
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Gt,
                        label: gt_label,
                    });
                    // If EQ, skip (already 0)
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Eq,
                        label: end_label,
                    });
                    // Otherwise LT: set -1
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
                    self.buffer.emit(Arm64Instruction::B { label: end_label });
                    self.buffer.bind_label(gt_label);
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
                    self.buffer.bind_label(end_label);
                    self.push_operand(dst);
                }
                // lload_0..lload_3
                0x1e..=0x21 => self.emit_iload((opcode - 0x1e) as usize),
                // lload
                0x16 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_iload(idx);
                }
                // lstore_0..lstore_3
                0x3f..=0x42 => self.emit_istore((opcode - 0x3f) as usize),
                // lstore
                0x37 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_istore(idx);
                }
                // ifXX (single operand branches)
                0x99 => {
                    // ifeq
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
                }
                0x9a => {
                    // ifne
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
                }
                // iflt (0x9b)
                0x9b => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer
                        .emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Lt,
                        label,
                    });
                }
                // ifge (0x9c)
                0x9c => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer
                        .emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Ge,
                        label,
                    });
                }
                // ifgt (0x9d)
                0x9d => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer
                        .emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Gt,
                        label,
                    });
                }
                // ifle (0x9e)
                0x9e => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer
                        .emit(Arm64Instruction::CmpImm { rn: val, imm: 0 });
                    self.buffer.emit(Arm64Instruction::BCond {
                        cond: Arm64Condition::Le,
                        label,
                    });
                }
                // ifnull (0xc6)
                0xc6 => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
                    pc += 2;
                    let target = (start_pc as i32 + offset) as usize;
                    let label = self.label_for_pc(target);
                    self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
                }
                // ifnonnull (0xc7)
                0xc7 => {
                    let val = self.pop_operand();
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as i32;
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
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let cp_idx = ((bytecode[pc] as u16) << 8) | bytecode[pc + 1] as u16;
                    pc += 2;
                    let num_args = self
                        .method_info
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
                // ldc (0x12) / ldc_w (0x13) / ldc2_w (0x14)
                //
                // Bug-fix (AArch64 STUB, ldc/ldc2_w): these opcodes load a
                // constant identified by a constant-pool index, but this backend
                // is never handed the method's constant pool — `Arm64Backend`
                // carries only `method_info` (cp-index -> invoke arg count), not
                // the resolved constant values — so there is no way to recover
                // the int/long/float/double/String/Class constant here. We
                // therefore BAIL to the interpreter rather than guess.
                //
                // CROSS-FILE FOLLOW-UP: to JIT these, plumb the resolved
                // constant pool (or at least an index -> {i32,i64,f32,f64}
                // numeric-constant table) into `compile_method_with_info`, then
                // route numeric ldc/ldc2_w to `emit_iconst`/`emit_lconst`/
                // `emit_fconst` (the bit-exact emit_fconst added in this pass
                // handles arbitrary float/double values). String/Class/MethodType
                // constants still require runtime resolution and must keep
                // bailing. Until that plumbing exists, bailing is the only
                // correct option.
                0x12 | 0x13 => {
                    // ldc consumes 1 operand byte, ldc_w consumes 2.
                    let operand_len = if opcode == 0x12 { 1 } else { 2 };
                    if pc + operand_len > bytecode.len() {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::Comment(
                        "ldc/ldc_w: constant pool not available — bailing to interpreter".into(),
                    ));
                    self.buffer.emit(Arm64Instruction::Brk { imm: 0 });
                    success = false;
                    break;
                }
                0x14 => {
                    // ldc2_w consumes 2 operand bytes (long/double constant).
                    if pc + 2 > bytecode.len() {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::Comment(
                        "ldc2_w: constant pool not available — bailing to interpreter".into(),
                    ));
                    self.buffer.emit(Arm64Instruction::Brk { imm: 0 });
                    success = false;
                    break;
                }
                // fload
                0x17 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fload(idx);
                }
                // dload
                0x18 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_fstore(idx);
                }
                // dstore
                0x39 => {
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                //
                // Bug-fix (AArch64 STUB, frem/drem silent-wrong-result): the
                // `emit_float_rem` truncating implementation (`a - trunc(a/b)*b`
                // via an i64 round-trip with FCVTZS/SCVTF) is only correct while
                // |a/b| < 2^63. FCVTZS SATURATES to i64::MIN/MAX outside that
                // range, so the truncated quotient — and therefore the
                // remainder — is silently WRONG for large operands. Java's
                // frem/drem (JLS §15.17.3, fmod semantics) is exact across the
                // whole double range. Rather than emit a path that is wrong for
                // a real (if rare) input range, bail to the interpreter, which
                // computes the correct IEEE remainder. AArch64 is the secondary
                // backend; correctness over feature completeness. (Re-wiring
                // emit_float_rem would require an exact reduction loop — e.g.
                // repeated FRINT/scaled subtraction — not the saturating cast.)
                0x72 | 0x73 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "frem/drem: no exact lowering — bailing to interpreter".into(),
                    ));
                    self.buffer.emit(Arm64Instruction::Brk { imm: 0 });
                    success = false;
                    break;
                }
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
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 56 });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl {
                        rd: tmp,
                        rn: src,
                        rm: dst,
                    });
                    self.buffer.emit(Arm64Instruction::Asr {
                        rd: tmp,
                        rn: tmp,
                        rm: dst,
                    });
                    self.push_operand(tmp);
                }
                // i2c (0x92) — int to char (zero-extend low 16 bits)
                0x92 => {
                    let src = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm {
                        rd: dst,
                        imm: 0xFFFF,
                    });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::And {
                        rd: tmp,
                        rn: src,
                        rm: dst,
                    });
                    self.push_operand(tmp);
                }
                // i2s (0x93) — int to short (sign-extend low 16 bits)
                0x93 => {
                    let src = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 48 });
                    let tmp = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl {
                        rd: tmp,
                        rn: src,
                        rm: dst,
                    });
                    self.buffer.emit(Arm64Instruction::Asr {
                        rd: tmp,
                        rn: tmp,
                        rm: dst,
                    });
                    self.push_operand(tmp);
                }
                // fcmpl (0x95), fcmpg (0x96), dcmpl (0x97), dcmpg (0x98)
                0x95 | 0x97 => self.emit_fcmp(true),  // NaN → -1
                0x96 | 0x98 => self.emit_fcmp(false), // NaN → 1

                // -- aload / astore (reference load/store, same as iload/istore on 64-bit) --
                0x19 => {
                    // aload
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                0x3a => {
                    // astore
                    if pc >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    pc += 1;
                    self.emit_istore(idx);
                }
                0x4b..=0x4e => self.emit_istore((opcode - 0x4b) as usize), // astore_0..astore_3

                // -- iinc --
                0x84 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let idx = bytecode[pc] as usize;
                    let delta = bytecode[pc + 1] as i8 as i32;
                    pc += 2;
                    if let Some(Some(reg)) = self.local_regs.get(idx).copied() {
                        if delta >= 0 {
                            self.buffer.emit(Arm64Instruction::AddImm {
                                rd: reg,
                                rn: reg,
                                imm: delta,
                            });
                        } else {
                            self.buffer.emit(Arm64Instruction::SubImm {
                                rd: reg,
                                rn: reg,
                                imm: -delta,
                            });
                        }
                    } else {
                        // Spilled local: load, add, store back
                        let frame = self.frame.as_ref().unwrap();
                        let spill_index = self.spill_index_for(idx);
                        let offset = frame.spill_offset.saturating_add(
                            i32::try_from(spill_index)
                                .unwrap_or(i32::MAX)
                                .saturating_mul(8),
                        );
                        let tmp = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::Ldr {
                            rt: tmp,
                            rn: Arm64Register::FP,
                            offset,
                        });
                        if delta >= 0 {
                            self.buffer.emit(Arm64Instruction::AddImm {
                                rd: tmp,
                                rn: tmp,
                                imm: delta,
                            });
                        } else {
                            self.buffer.emit(Arm64Instruction::SubImm {
                                rd: tmp,
                                rn: tmp,
                                imm: -delta,
                            });
                        }
                        self.buffer.emit(Arm64Instruction::Str {
                            rt: tmp,
                            rn: Arm64Register::FP,
                            offset,
                        });
                    }
                }

                // -- iushr (logical shift right) --
                0x7c => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    // Mask to 32 bits first for unsigned shift
                    let mask = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::MovImm {
                        rd: mask,
                        imm: 0xFFFF_FFFF,
                    });
                    self.buffer.emit(Arm64Instruction::And {
                        rd: dst,
                        rn: val,
                        rm: mask,
                    });
                    self.buffer.emit(Arm64Instruction::Lsr {
                        rd: dst,
                        rn: dst,
                        rm: shift,
                    });
                    self.push_operand(dst);
                }

                // -- lushr (long unsigned shift right) --
                0x7d => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsr {
                        rd: dst,
                        rn: val,
                        rm: shift,
                    });
                    self.push_operand(dst);
                }

                // -- lshl --
                0x79 => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Lsl {
                        rd: dst,
                        rn: val,
                        rm: shift,
                    });
                    self.push_operand(dst);
                }

                // -- lshr --
                0x7b => {
                    let shift = self.pop_operand();
                    let val = self.pop_operand();
                    let dst = self.alloc_scratch();
                    self.buffer.emit(Arm64Instruction::Asr {
                        rd: dst,
                        rn: val,
                        rm: shift,
                    });
                    self.push_operand(dst);
                }

                // -- ldiv -- (refused; same reason as `idiv` above)
                0x6d => {
                    success = false;
                    break;
                }

                // -- irem / lrem --
                //
                // Both lowered to `a - (a / b) * b` via `SDIV` + `MSUB` with
                // NO divisor check at all. On AArch64 `SDIV` by zero does not
                // trap — it yields 0 — so the sequence quietly computed
                // `a - 0 * b == a`: `x % 0` returned `x` instead of throwing
                // `ArithmeticException`. A silent wrong answer is worse than
                // a bail, and there is no exception path on this backend to
                // route a correct throw through, so refuse the method.
                // (`Integer.MIN_VALUE % -1` is separately wrong for the same
                // 64-bit-lowering reason described in the module header.)
                0x70 | 0x71 => {
                    success = false;
                    break;
                }

                // -- dup_x1 (0x5a) --
                //
                // JVMS: both operands category-1. A `float`/`double` in either
                // position is on the other stack; refuse.
                0x5a => {
                    match self.int_stack_shuffle_entries(start_pc, 2) {
                        Some(c) if !c[0] && !c[1] => {}
                        _ => {
                            success = false;
                            break;
                        }
                    }
                    let val1 = self.pop_operand();
                    let val2 = self.pop_operand();
                    let dup = self.alloc_scratch();
                    self.buffer
                        .emit(Arm64Instruction::Mov { rd: dup, rm: val1 });
                    self.push_operand(dup);
                    self.push_operand(val2);
                    self.push_operand(val1);
                }

                // -- dup_x2 (0x5b) --
                //
                // FORM 1 is three category-1 entries; FORM 2 is a category-1
                // top over ONE category-2, i.e. two entries. The old
                // unconditional three-pop was FORM 1 only.
                0x5b => {
                    let Some(cats) = self.int_stack_shuffle_entries(start_pc, 2) else {
                        success = false;
                        break;
                    };
                    if cats[0] {
                        success = false; // no dup_x2 form has a category-2 top
                        break;
                    }
                    let below_entries = if cats[1] {
                        1 // FORM 2 — one category-2 entry under the top
                    } else {
                        match self.int_stack_shuffle_entries(start_pc, 3) {
                            Some(c) if !c[2] => 2, // FORM 1
                            _ => {
                                success = false;
                                break;
                            }
                        }
                    };
                    let val1 = self.pop_operand();
                    let mut below = Vec::with_capacity(below_entries);
                    for _ in 0..below_entries {
                        below.push(self.pop_operand());
                    }
                    let dup = self.alloc_scratch();
                    self.buffer
                        .emit(Arm64Instruction::Mov { rd: dup, rm: val1 });
                    self.push_operand(dup);
                    for reg in below.into_iter().rev() {
                        self.push_operand(reg);
                    }
                    self.push_operand(val1);
                }

                // -- dup2 (0x5c) --
                //
                // FORM 2 is a single category-2 entry, duplicated like `dup`.
                // The old unconditional two-pop duplicated an unrelated value
                // sitting under the long — the exact miscompile the x64
                // backend was fixed for (`dup2_category_safe`'s doc comment).
                0x5c => {
                    let Some(cats) = self.int_stack_shuffle_entries(start_pc, 1) else {
                        success = false;
                        break;
                    };
                    if cats[0] {
                        let val = self.pop_operand();
                        let dup = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::Mov { rd: dup, rm: val });
                        self.push_operand(val);
                        self.push_operand(dup);
                    } else {
                        match self.int_stack_shuffle_entries(start_pc, 2) {
                            Some(c) if !c[1] => {}
                            _ => {
                                success = false;
                                break;
                            }
                        }
                        let val1 = self.pop_operand();
                        let val2 = self.pop_operand();
                        let dup1 = self.alloc_scratch();
                        let dup2 = self.alloc_scratch();
                        self.buffer
                            .emit(Arm64Instruction::Mov { rd: dup1, rm: val1 });
                        self.buffer
                            .emit(Arm64Instruction::Mov { rd: dup2, rm: val2 });
                        self.push_operand(val2);
                        self.push_operand(val1);
                        self.push_operand(dup2);
                        self.push_operand(dup1);
                    }
                }

                // -- dup2_x1 (0x5d) --
                //
                // FORM 1 duplicates two category-1 entries over one; FORM 2
                // duplicates ONE category-2 entry over one. The old
                // unconditional three-pop was FORM 1 only.
                0x5d => {
                    let Some(cats) = self.int_stack_shuffle_entries(start_pc, 2) else {
                        success = false;
                        break;
                    };
                    // JVMS requires the entry under the duplicated group to be
                    // category-1 in both forms.
                    let dup_entries = if cats[0] {
                        if cats[1] {
                            success = false;
                            break;
                        }
                        1
                    } else {
                        match self.int_stack_shuffle_entries(start_pc, 3) {
                            Some(c) if !c[1] && !c[2] => 2,
                            _ => {
                                success = false;
                                break;
                            }
                        }
                    };
                    self.emit_dup_group_over(dup_entries, 1);
                }

                // -- dup2_x2 (0x5e) --
                //
                // The four JVMS forms, in this backend's one-entry-per-value
                // model. The old arm popped four unconditionally, which is
                // FORM 1 alone; on the other three it took entries belonging to
                // the caller's stack — the question the x64 page
                // (`dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend`)
                // raised and did not answer.
                //
                //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
                //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
                //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
                //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
                0x5e => {
                    let Some(cats) = self.int_stack_shuffle_entries(start_pc, 2) else {
                        success = false;
                        break;
                    };
                    let shape = if cats[0] {
                        if cats[1] {
                            Some((1usize, 1usize)) // FORM 4
                        } else {
                            match self.int_stack_shuffle_entries(start_pc, 3) {
                                Some(c) if !c[2] => Some((1, 2)), // FORM 2
                                _ => None,
                            }
                        }
                    } else if cats[1] {
                        None // no form has a category-2 under a category-1 top
                    } else {
                        match self.int_stack_shuffle_entries(start_pc, 3) {
                            Some(c) if c[2] => Some((2, 1)), // FORM 3
                            Some(_) => match self.int_stack_shuffle_entries(start_pc, 4) {
                                Some(c) if !c[3] => Some((2, 2)), // FORM 1
                                _ => None,
                            },
                            None => None,
                        }
                    };
                    let Some((dup_entries, under_entries)) = shape else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(dup_entries, under_entries);
                }

                // -- tableswitch (0xaa) --
                //
                // HIGH security fix: same `checked_tableswitch_count` audit
                // as the x64 path — reject adversarial overflow / oversize
                // tables. Bail out by setting `success = false` so the
                // ARM64 backend falls back to the interpreter.
                0xaa => {
                    let index = self.pop_operand();
                    // Align to 4-byte boundary
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    if pc + 12 > bytecode.len() {
                        success = false;
                        break;
                    }
                    let default_off = i32::from_be_bytes([
                        bytecode[pc],
                        bytecode[pc + 1],
                        bytecode[pc + 2],
                        bytecode[pc + 3],
                    ]);
                    let low = i32::from_be_bytes([
                        bytecode[pc + 4],
                        bytecode[pc + 5],
                        bytecode[pc + 6],
                        bytecode[pc + 7],
                    ]);
                    let high = i32::from_be_bytes([
                        bytecode[pc + 8],
                        bytecode[pc + 9],
                        bytecode[pc + 10],
                        bytecode[pc + 11],
                    ]);
                    pc += 12;
                    let count = match super::x64::checked_tableswitch_count(low, high) {
                        Some(n) => n,
                        None => {
                            success = false;
                            break;
                        }
                    };

                    // Range check: if index < low || index > high → default
                    let default_target = (start_pc as i32 + default_off) as usize;
                    let default_label = self.label_for_pc(default_target);

                    if low != 0 {
                        let adj = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::SubImm {
                            rd: adj,
                            rn: index,
                            imm: low,
                        });
                        // adj holds the 0-based index
                        let bound = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm {
                            rd: bound,
                            imm: count as i64,
                        });
                        self.buffer
                            .emit(Arm64Instruction::Cmp { rn: adj, rm: bound });
                        self.buffer.emit(Arm64Instruction::BCond {
                            cond: Arm64Condition::Cs,
                            label: default_label,
                        });
                    } else {
                        let bound = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm {
                            rd: bound,
                            imm: count as i64,
                        });
                        self.buffer.emit(Arm64Instruction::Cmp {
                            rn: index,
                            rm: bound,
                        });
                        self.buffer.emit(Arm64Instruction::BCond {
                            cond: Arm64Condition::Cs,
                            label: default_label,
                        });
                    }

                    // Emit linear chain of compares+branches for each case
                    for i in 0..count {
                        if pc + 4 > bytecode.len() {
                            success = false;
                            break;
                        }
                        let off = i32::from_be_bytes([
                            bytecode[pc],
                            bytecode[pc + 1],
                            bytecode[pc + 2],
                            bytecode[pc + 3],
                        ]);
                        pc += 4;
                        let target = (start_pc as i32 + off) as usize;
                        let case_label = self.label_for_pc(target);
                        let case_val = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm {
                            rd: case_val,
                            imm: (low + i as i32) as i64,
                        });
                        self.buffer.emit(Arm64Instruction::Cmp {
                            rn: index,
                            rm: case_val,
                        });
                        self.buffer.emit(Arm64Instruction::BCond {
                            cond: Arm64Condition::Eq,
                            label: case_label,
                        });
                    }
                    // Fall through to default
                    self.buffer.emit(Arm64Instruction::B {
                        label: default_label,
                    });
                }

                // -- lookupswitch (0xab) --
                //
                // HIGH security fix: validate `npairs` via
                // `checked_lookupswitch_npairs` (reject negative / oversize).
                0xab => {
                    let key = self.pop_operand();
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    if pc + 8 > bytecode.len() {
                        success = false;
                        break;
                    }
                    let default_off = i32::from_be_bytes([
                        bytecode[pc],
                        bytecode[pc + 1],
                        bytecode[pc + 2],
                        bytecode[pc + 3],
                    ]);
                    let npairs_raw = i32::from_be_bytes([
                        bytecode[pc + 4],
                        bytecode[pc + 5],
                        bytecode[pc + 6],
                        bytecode[pc + 7],
                    ]);
                    let npairs = match super::x64::checked_lookupswitch_npairs(npairs_raw) {
                        Some(n) => n,
                        None => {
                            success = false;
                            break;
                        }
                    };
                    pc += 8;

                    let default_target = (start_pc as i32 + default_off) as usize;
                    let default_label = self.label_for_pc(default_target);

                    for _ in 0..npairs {
                        if pc + 8 > bytecode.len() {
                            success = false;
                            break;
                        }
                        let match_val = i32::from_be_bytes([
                            bytecode[pc],
                            bytecode[pc + 1],
                            bytecode[pc + 2],
                            bytecode[pc + 3],
                        ]);
                        let off = i32::from_be_bytes([
                            bytecode[pc + 4],
                            bytecode[pc + 5],
                            bytecode[pc + 6],
                            bytecode[pc + 7],
                        ]);
                        pc += 8;
                        let target = (start_pc as i32 + off) as usize;
                        let case_label = self.label_for_pc(target);
                        let cmp_val = self.alloc_scratch();
                        self.buffer.emit(Arm64Instruction::MovImm {
                            rd: cmp_val,
                            imm: match_val as i64,
                        });
                        self.buffer.emit(Arm64Instruction::Cmp {
                            rn: key,
                            rm: cmp_val,
                        });
                        self.buffer.emit(Arm64Instruction::BCond {
                            cond: Arm64Condition::Eq,
                            label: case_label,
                        });
                    }
                    self.buffer.emit(Arm64Instruction::B {
                        label: default_label,
                    });
                }

                // -- if_acmpeq (0xa5), if_acmpne (0xa6) --
                0xa5 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
                    let offset = i16::from_be_bytes([bytecode[pc], bytecode[pc + 1]]) as isize;
                    pc += 2;
                    let target = (start_pc as isize + offset) as usize;
                    self.emit_if_icmp(Arm64Condition::Eq, target);
                }
                0xa6 => {
                    if pc + 1 >= bytecode.len() {
                        success = false;
                        break;
                    }
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
                    self.buffer.emit(Arm64Instruction::Comment(format!(
                        "unsupported opcode 0x{:02x} at pc={}",
                        opcode, start_pc
                    )));
                    self.buffer.emit(Arm64Instruction::Brk { imm: 0 });
                    success = false;
                }
            }
        }

        // Emit epilogue.
        self.emit_epilogue();

        let frame = match self.frame.take() {
            Some(f) => f,
            None => {
                return Arm64CompileResult {
                    instructions: Vec::new(),
                    frame: Arm64FrameLayout {
                        frame_size: 0,
                        callee_save_offset: 0,
                        spill_offset: 0,
                        num_spills: 0,
                        saved_regs: Vec::new(),
                        num_reg_locals: 0,
                    },
                    labels: HashMap::new(),
                    success: false,
                    pending_oop_maps: Vec::new(),
                }
            }
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
            pending_oop_maps: std::mem::take(&mut self.pending_oop_maps),
        }
    }

    // -- Arithmetic helpers -------------------------------------------------

    pub fn emit_int_add(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Add {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    pub fn emit_int_sub(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Sub {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    pub fn emit_int_mul(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Mul {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    /// NOT WIRED — no opcode arm calls this. `idiv`/`ldiv` (`0x6c`/`0x6d`)
    /// refuse the method instead, because the divide-by-zero path below is
    /// `BRK #1` (SIGTRAP → process death), not an `ArithmeticException`
    /// throw. Retained as the shape a correct lowering should take once this
    /// backend has an exception path: replace the `Brk` with a branch to a
    /// stub that calls the VM's throw helper. Exercised by
    /// `backend_int_div_emits_sdiv` only.
    pub fn emit_int_div(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        // Guard: if divisor is zero, trap — NOT an ArithmeticException; see
        // the doc comment above for why this helper is currently unwired.
        // CBZ rhs, trap_label; SDIV; B continue; trap: BRK
        let trap_label = self.buffer.new_label();
        let continue_label = self.buffer.new_label();
        self.buffer.emit(Arm64Instruction::Cbz {
            rt: rhs,
            label: trap_label,
        });
        self.buffer.emit(Arm64Instruction::SDiv {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.buffer.emit(Arm64Instruction::B {
            label: continue_label,
        });
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
        self.buffer.emit(Arm64Instruction::And {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    fn emit_int_or(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Orr {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    fn emit_int_xor(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Eor {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    fn emit_int_shl(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Lsl {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
        self.push_operand(dst);
    }

    fn emit_int_shr(&mut self) {
        let rhs = self.pop_operand();
        let lhs = self.pop_operand();
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Asr {
            rd: dst,
            rn: lhs,
            rm: rhs,
        });
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
                i32::try_from(spill_index)
                    .unwrap_or(i32::MAX)
                    .saturating_mul(8),
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
            self.buffer
                .emit(Arm64Instruction::Mov { rd: *reg, rm: src });
        } else {
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index)
                    .unwrap_or(i32::MAX)
                    .saturating_mul(8),
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
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: i64::from(value),
        });
        self.push_operand(dst);
    }

    pub fn emit_lconst(&mut self, value: i64) {
        let dst = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: value,
        });
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
        self.buffer.emit(Arm64Instruction::LdrLiteral {
            rt: rd,
            label: pool_label,
        });
        self.buffer.emit(Arm64Instruction::B { label: skip_label });
        self.buffer.emit(Arm64Instruction::ConstantPoolEntry {
            label: pool_label,
            value,
        });
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

        // Bug-fix (ARM64 BUG #3, infinite self-call): the previous code created
        // a fresh `call_label`, emitted `Bl { label: call_label }`, and never
        // bound it. The branch-patch loop in `emit_machine_code` leaves an
        // unbound label as a "branch to self" (`BL .`), which at runtime is an
        // infinite self-call / stack overflow. There is no resolved target
        // address available at emit time here, so the only safe action is to
        // bail to the interpreter for this method (AArch64 is the secondary
        // backend; correctness and a safe fallback take priority over feature
        // completeness). We set `self.failed`, which forces
        // `Arm64CompileResult.success = false`, and emit a `Brk` trap so that
        // any code path which mistakenly ignores `failed` still traps loudly
        // rather than executing a broken self-call.
        //
        // (If a concrete call target ever becomes available at emit time, the
        // correct lowering is `mov_imm64 IP0, <target>; BLR IP0` — the
        // range-unlimited indirect form — followed by capturing the X0 result.
        // Until then we must NOT emit any call.)
        self.failed = true;
        self.buffer.emit(Arm64Instruction::Comment(format!(
            "ARM64 BUG #3: unresolved invoke ({} args) — bailing to interpreter",
            num_args
        )));
        self.buffer.emit(Arm64Instruction::Brk { imm: 0 });

        // Keep the operand stack shape consistent for the remainder of the
        // (now-doomed) compilation pass: a call leaves one result value on the
        // stack. We push a scratch placeholder so later pops do not underflow
        // and mask the real failure cause. The emitted code is discarded
        // because `success` is false.
        let dst = self.alloc_scratch();
        self.push_operand(dst);
    }

    // -- Float/Double helpers ------------------------------------------------

    /// Load a float/double constant onto the float operand stack.
    ///
    /// Bug-fix (AArch64 MEDIUM, emit_fconst miscompile): the previous body
    /// materialized `value as i64` into a GP register and ran `ScvtfDouble`
    /// (signed-integer → double CONVERSION). That is only correct for INTEGRAL
    /// constants — e.g. `fconst_2` (2.0). For any non-integral value the
    /// truncating `as i64` followed by integer→float conversion produced the
    /// wrong number (2.5 → 2.0, 0.1 → 0.0, π → 3.0, etc.). This backend treats
    /// every FP register as a 64-bit double (`Dn`), so the correct lowering is
    /// a BIT-EXACT move: materialize the IEEE-754 double bit pattern into a GPR
    /// with `mov_imm64`, then `FMOV Dd, Xn` (`FmovToFp`) which copies the raw
    /// 64 bits with no numeric conversion. This reproduces every double exactly,
    /// integral or not.
    pub fn emit_fconst(&mut self, value: f64) {
        let dst = self.alloc_float_scratch();
        let tmp = self.alloc_scratch();
        // Reinterpret the f64 as its raw 64-bit pattern. `to_bits() as i64`
        // round-trips losslessly through `MovImm`'s `imm as u64` lowering, so
        // the GPR ends up holding the exact IEEE-754 encoding.
        let bits = value.to_bits() as i64;
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: tmp, imm: bits });
        // FMOV Dd, Xn — bit-exact GPR → FP move (NOT a numeric conversion).
        self.buffer
            .emit(Arm64Instruction::FmovToFp { vd: dst, rn: tmp });
        self.push_float_operand(dst);
    }

    /// Load a float/double local variable onto the float operand stack.
    /// Locals are stored in GP registers; we convert to FP via ScvtfDouble
    /// or load from spill slot.
    pub fn emit_fload(&mut self, index: usize) {
        let dst = self.alloc_float_scratch();
        // Check if this float local has a dedicated FP register (from graph-coloring).
        if let Some(Some(fp_reg)) = self.float_local_regs.get(index).copied() {
            self.buffer.emit(Arm64Instruction::FmovFp {
                vd: dst,
                vn: fp_reg,
            });
        } else if let Some(Some(gp_reg)) = self.local_regs.get(index).copied() {
            // Float local stored in a GP register: bit-pattern transfer to FP.
            self.buffer.emit(Arm64Instruction::FmovToFp {
                vd: dst,
                rn: gp_reg,
            });
        } else {
            // Spilled local: load from stack directly into FP register.
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index)
                    .unwrap_or(i32::MAX)
                    .saturating_mul(8),
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
            self.buffer.emit(Arm64Instruction::FmovFp {
                vd: fp_reg,
                vn: src,
            });
        } else if let Some(Some(gp_reg)) = self.local_regs.get(index) {
            // Float local stored in a GP register: bit-pattern transfer from FP.
            self.buffer.emit(Arm64Instruction::FmovFromFp {
                rd: *gp_reg,
                vn: src,
            });
        } else {
            // Spilled: store FP register directly to stack.
            let frame = self.frame.as_ref().unwrap();
            let spill_index = self.spill_index_for(index);
            let offset = frame.spill_offset.saturating_add(
                i32::try_from(spill_index)
                    .unwrap_or(i32::MAX)
                    .saturating_mul(8),
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
    /// How many spill slots the FRAME-HOMED LOCALS occupy -- equivalently, the
    /// first spill index the operand stack may use.
    ///
    /// `spill_index_for` numbers those locals `0..local_spill_count()`, and
    /// `num_spills` is computed as `gpr_spills + max_stack`, so the operand
    /// area is exactly `[local_spill_count(), num_spills)`. Both spillers must
    /// take their base from HERE or the two areas overlap -- see
    /// `operand_spill_slots_do_not_alias_frame_homed_locals`.
    fn local_spill_count(&self) -> usize {
        self.spill_index_for(self.local_regs.len())
    }

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
        self.buffer.emit(Arm64Instruction::FaddDouble {
            vd: dst,
            vn: lhs,
            vm: rhs,
        });
        self.push_float_operand(dst);
    }

    /// Float/double sub: pop two, emit FsubDouble, push result.
    pub fn emit_float_sub(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble {
            vd: dst,
            vn: lhs,
            vm: rhs,
        });
        self.push_float_operand(dst);
    }

    /// Float/double mul: pop two, emit FmulDouble, push result.
    pub fn emit_float_mul(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FmulDouble {
            vd: dst,
            vn: lhs,
            vm: rhs,
        });
        self.push_float_operand(dst);
    }

    /// Float/double div: pop two, emit FdivDouble, push result.
    pub fn emit_float_div(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FdivDouble {
            vd: dst,
            vn: lhs,
            vm: rhs,
        });
        self.push_float_operand(dst);
    }

    /// Float/double remainder: a - trunc(a/b)*b via FDIV + FCVTZS/SCVTF + FMUL + FSUB.
    ///
    /// UNWIRED / reference only. This is INEXACT: the FCVTZS round-trip used to
    /// truncate the quotient saturates to i64::MIN/MAX once |a/b| >= 2^63, so
    /// the remainder is wrong for large operands. The `frem`/`drem` dispatch
    /// now bails to the interpreter instead of calling this (see the 0x72/0x73
    /// arm). Kept as a starting point should an exact ARM64 reduction loop be
    /// implemented later; `#[allow(dead_code)]` because nothing wires it now.
    #[allow(dead_code)]
    pub fn emit_float_rem(&mut self) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        let quotient = self.alloc_float_scratch();
        // quotient = lhs / rhs
        self.buffer.emit(Arm64Instruction::FdivDouble {
            vd: quotient,
            vn: lhs,
            vm: rhs,
        });
        // Convert to integer and back to truncate: fcvtzs + scvtf
        let tmp_int = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::FcvtzsInt {
            rd: tmp_int,
            vn: quotient,
        });
        let trunc = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::ScvtfDouble {
            vd: trunc,
            rn: tmp_int,
        });
        // product = trunc * rhs
        let product = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FmulDouble {
            vd: product,
            vn: trunc,
            vm: rhs,
        });
        // result = lhs - product
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble {
            vd: dst,
            vn: lhs,
            vm: product,
        });
        self.push_float_operand(dst);
    }

    /// Float/double negate: FSUB from zero.
    pub fn emit_float_neg(&mut self) {
        let src = self.pop_float_operand();
        // Load zero into a V register
        let tmp = self.alloc_scratch();
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: tmp, imm: 0 });
        let zero = self.alloc_float_scratch();
        self.buffer
            .emit(Arm64Instruction::ScvtfDouble { vd: zero, rn: tmp });
        let dst = self.alloc_float_scratch();
        self.buffer.emit(Arm64Instruction::FsubDouble {
            vd: dst,
            vn: zero,
            vm: src,
        });
        self.push_float_operand(dst);
    }

    /// i2f / i2d: pop int from GP stack, convert to float, push onto float stack.
    pub fn emit_i2f(&mut self) {
        let src = self.pop_operand();
        let dst = self.alloc_float_scratch();
        self.buffer
            .emit(Arm64Instruction::ScvtfDouble { vd: dst, rn: src });
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
        self.buffer
            .emit(Arm64Instruction::FcvtzsInt { rd: dst, vn: src });
        self.push_operand(dst);
    }

    /// l2i: truncate long to int (mask lower 32 bits).
    pub fn emit_l2i(&mut self) {
        let src = self.pop_operand();
        let dst = self.alloc_scratch();
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: mask,
            imm: 0xFFFF_FFFFi64,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: dst,
            rn: src,
            rm: mask,
        });
        self.push_operand(dst);
    }

    /// Float compare: pop two floats, emit FcmpDouble + conditional set.
    /// `nan_minus`: if true, NaN produces -1 (fcmpl/dcmpl); if false, NaN produces 1 (fcmpg/dcmpg).
    pub fn emit_fcmp(&mut self, nan_minus: bool) {
        let rhs = self.pop_float_operand();
        let lhs = self.pop_float_operand();
        self.buffer
            .emit(Arm64Instruction::FcmpDouble { vn: lhs, vm: rhs });
        // After FCMP, use conditional logic to produce -1, 0, or 1 on the int stack.
        // GT → 1, EQ → 0, LT → -1, unordered (NaN) → nan_minus ? -1 : 1
        let dst = self.alloc_scratch();
        // Start with 0
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
        // If GT, set to 1
        let gt_label = self.buffer.new_label();
        let end_label = self.buffer.new_label();
        let lt_label = self.buffer.new_label();
        let unord_label = self.buffer.new_label();

        // B.VS unordered (overflow flag set when NaN)
        self.buffer
            .emit(Arm64Instruction::Comment("fcmp result dispatch".into()));
        // For simplicity, use a series of conditional branches:
        // After FCMP: EQ means equal, GT means greater, LT(MI) means less, VS means unordered
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Eq,
            label: end_label,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Gt,
            label: gt_label,
        });
        // If we get here, it's LT or unordered
        if nan_minus {
            // Both LT and NaN produce -1
            self.buffer
                .emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
        } else {
            // LT produces -1, NaN produces 1 — check VS for NaN
            self.buffer.emit(Arm64Instruction::BCond {
                cond: Arm64Condition::Lt,
                label: lt_label,
            });
            // Unordered: NaN → 1
            self.buffer
                .emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
            self.buffer.bind_label(lt_label);
            self.buffer
                .emit(Arm64Instruction::MovImm { rd: dst, imm: -1 });
            self.buffer.emit(Arm64Instruction::B { label: end_label });
        }
        self.buffer.bind_label(gt_label);
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: dst, imm: 1 });
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
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
    }

    pub fn emit_return_void(&mut self) {
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
    }

    // -----------------------------------------------------------------------
    // NEON vectorization helpers
    // -----------------------------------------------------------------------

    /// Emit a NEON-vectorized int-array sum loop.
    ///
    /// TEST-ONLY / NOT WIRED: this helper is exercised by unit tests only and
    /// is NOT reachable from the bytecode-compile dispatch in `compile_method`
    /// (no opcode handler calls it; there is no NEON auto-vectorization pass).
    /// It is retained as a worked reference for a future vectorizer. Before
    /// wiring it into codegen, derive the array base/length operands from a real
    /// induction-variable analysis (mirroring the x64 BCE path) rather than
    /// passing pre-chosen registers, and re-validate the horizontal-reduce
    /// stack spill (now red-zone-safe — see the SubImm/AddImm SP framing below).
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
    pub fn emit_neon_array_sum(
        &mut self,
        arr_reg: Arm64Register,
        len_reg: Arm64Register,
        result_reg: Arm64Register,
    ) {
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
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: result_reg,
            imm: 0,
        });
        // Zero the vector accumulator (EOR Vd, Vd, Vd)
        self.buffer.emit(Arm64Instruction::Eor {
            rd: Arm64Register(vacc.0 - 32),
            rn: Arm64Register(vacc.0 - 32),
            rm: Arm64Register(vacc.0 - 32),
        });

        // vec_len = len & ~3 (round down to multiple of 4)
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: mask,
            imm: !3i64,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: vec_len,
            rn: len_reg,
            rm: mask,
        });

        // ptr = arr_reg (starting pointer)
        self.buffer.emit(Arm64Instruction::Mov {
            rd: ptr,
            rm: arr_reg,
        });
        // idx = 0
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: idx, imm: 0 });

        // Skip vector loop if less than 4 elements
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: vec_len,
            rm: Arm64Register::XZR,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Eq,
            label: vec_done,
        });

        // Vector loop
        self.buffer.bind_label(vec_loop);
        // Load 4 ints (16 bytes) from [ptr] into vdata
        self.buffer
            .emit(Arm64Instruction::NeonLd1_4s { vt: vdata, rn: ptr });
        // vacc += vdata
        self.buffer.emit(Arm64Instruction::NeonAdd4s {
            vd: vacc,
            vn: vacc,
            vm: vdata,
        });
        // ptr += 16
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr,
            rn: ptr,
            imm: 16,
        });
        // idx += 4
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: idx,
            rn: idx,
            imm: 4,
        });
        // if idx < vec_len → loop
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: vec_len,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Lt,
            label: vec_loop,
        });

        self.buffer.bind_label(vec_done);

        // Horizontal reduce: sum the 4 lanes of vacc into result_reg.
        // We use ADDV to sum all lanes, but since we don't have ADDV encoded,
        // extract each lane via GP and add them.
        // We store vacc to a stack scratch area, then load the 4 ints.
        //
        // Bug-fix (AArch64 NEON SP-store): AArch64 has NO red zone — storing
        // below SP without first lowering SP can be clobbered by an interrupt
        // or signal handler that reuses the stack. Reserve 16 bytes (one Q-reg)
        // by lowering SP, spill the vector, read it back, then restore SP. SP
        // must stay 16-byte aligned per AAPCS64; 16 is already aligned.
        self.buffer.emit(Arm64Instruction::SubImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 16,
        });
        // STR the vector to the reserved [SP] slot.
        self.buffer.emit(Arm64Instruction::NeonSt1_4s {
            vt: vacc,
            rn: Arm64Register::SP,
        });
        // Load the 4 elements (as two 64-bit halves) and add them.
        let t0 = self.alloc_scratch();
        let t1 = self.alloc_scratch();
        // Load first 2 as a pair, then next 2
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: t0,
            rn: Arm64Register::SP,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: t1,
            rn: Arm64Register::SP,
            offset: 8,
        });
        // Release the reserved stack scratch now that the vector is back in GPRs.
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 16,
        });
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
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: mask32,
            imm: 0xFFFF_FFFF,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: lo0,
            rn: t0,
            rm: mask32,
        });
        // Extract high 32 bits
        let shift32 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: shift32,
            imm: 32,
        });
        self.buffer.emit(Arm64Instruction::Lsr {
            rd: hi0,
            rn: t0,
            rm: shift32,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: lo1,
            rn: t1,
            rm: mask32,
        });
        let hi1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Lsr {
            rd: hi1,
            rn: t1,
            rm: shift32,
        });
        // Sum all 4 lanes
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: lo0,
            rm: hi0,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: lo1,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: hi1,
        });

        // Scalar tail: process remaining elements (idx..len)
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: len_reg,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ge,
            label: scalar_done,
        });

        self.buffer.bind_label(scalar_loop);
        // Load arr[idx] (4-byte int at ptr)
        let elem = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: elem,
            rn: ptr,
            offset: 0,
        });
        // Mask to 32 bits (array element is i32 but LDR loads 64 bits)
        self.buffer.emit(Arm64Instruction::And {
            rd: elem,
            rn: elem,
            rm: mask32,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: elem,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr,
            rn: ptr,
            imm: 4,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: idx,
            rn: idx,
            imm: 1,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: len_reg,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Lt,
            label: scalar_loop,
        });

        self.buffer.bind_label(scalar_done);
    }

    /// Emit a NEON-vectorized dot product of two int arrays.
    ///
    /// Computes: sum(a[i] * b[i]) for i in 0..len
    ///
    /// TEST-ONLY / NOT WIRED: exercised by unit tests only; not reachable from
    /// the bytecode-compile dispatch (see the note on `emit_neon_array_sum`).
    /// Its horizontal-reduce stack spill is now red-zone-safe (SubImm/AddImm SP
    /// framing). Re-validate operand provenance before wiring into codegen.
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
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: result_reg,
            imm: 0,
        });
        self.buffer.emit(Arm64Instruction::Eor {
            rd: Arm64Register(vacc.0 - 32),
            rn: Arm64Register(vacc.0 - 32),
            rm: Arm64Register(vacc.0 - 32),
        });

        // vec_len = len & ~3
        let mask = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: mask,
            imm: !3i64,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: vec_len,
            rn: len_reg,
            rm: mask,
        });

        self.buffer.emit(Arm64Instruction::Mov {
            rd: ptr_a,
            rm: arr_a_reg,
        });
        self.buffer.emit(Arm64Instruction::Mov {
            rd: ptr_b,
            rm: arr_b_reg,
        });
        self.buffer
            .emit(Arm64Instruction::MovImm { rd: idx, imm: 0 });

        self.buffer.emit(Arm64Instruction::Cmp {
            rn: vec_len,
            rm: Arm64Register::XZR,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Eq,
            label: vec_done,
        });

        self.buffer.bind_label(vec_loop);
        self.buffer
            .emit(Arm64Instruction::NeonLd1_4s { vt: va, rn: ptr_a });
        self.buffer
            .emit(Arm64Instruction::NeonLd1_4s { vt: vb, rn: ptr_b });
        // vtmp = va * vb (element-wise)
        self.buffer.emit(Arm64Instruction::NeonMul4s {
            vd: vtmp,
            vn: va,
            vm: vb,
        });
        // vacc += vtmp
        self.buffer.emit(Arm64Instruction::NeonAdd4s {
            vd: vacc,
            vn: vacc,
            vm: vtmp,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr_a,
            rn: ptr_a,
            imm: 16,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr_b,
            rn: ptr_b,
            imm: 16,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: idx,
            rn: idx,
            imm: 4,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: vec_len,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Lt,
            label: vec_loop,
        });

        self.buffer.bind_label(vec_done);

        // Horizontal reduce vacc.
        //
        // Bug-fix (AArch64 NEON SP-store): reserve 16 bytes by lowering SP
        // before spilling the accumulator vector (AArch64 has no red zone — a
        // store below SP can be clobbered by an interrupt/signal). Restore SP
        // after reading the value back. 16 is 16-byte aligned per AAPCS64.
        self.buffer.emit(Arm64Instruction::SubImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 16,
        });
        self.buffer.emit(Arm64Instruction::NeonSt1_4s {
            vt: vacc,
            rn: Arm64Register::SP,
        });
        let t0 = self.alloc_scratch();
        let t1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: t0,
            rn: Arm64Register::SP,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: t1,
            rn: Arm64Register::SP,
            offset: 8,
        });
        // Release the reserved stack scratch.
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::SP,
            imm: 16,
        });
        let mask32 = self.alloc_scratch();
        let shift32 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: mask32,
            imm: 0xFFFF_FFFF,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: shift32,
            imm: 32,
        });
        let lo0 = self.alloc_scratch();
        let hi0 = self.alloc_scratch();
        let lo1 = self.alloc_scratch();
        let hi1 = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::And {
            rd: lo0,
            rn: t0,
            rm: mask32,
        });
        self.buffer.emit(Arm64Instruction::Lsr {
            rd: hi0,
            rn: t0,
            rm: shift32,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: lo1,
            rn: t1,
            rm: mask32,
        });
        self.buffer.emit(Arm64Instruction::Lsr {
            rd: hi1,
            rn: t1,
            rm: shift32,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: lo0,
            rm: hi0,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: lo1,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: hi1,
        });

        // Scalar tail
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: len_reg,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Ge,
            label: scalar_done,
        });

        self.buffer.bind_label(scalar_loop);
        let ea = self.alloc_scratch();
        let eb = self.alloc_scratch();
        let prod = self.alloc_scratch();
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: ea,
            rn: ptr_a,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::Ldr {
            rt: eb,
            rn: ptr_b,
            offset: 0,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: ea,
            rn: ea,
            rm: mask32,
        });
        self.buffer.emit(Arm64Instruction::And {
            rd: eb,
            rn: eb,
            rm: mask32,
        });
        self.buffer.emit(Arm64Instruction::Mul {
            rd: prod,
            rn: ea,
            rm: eb,
        });
        self.buffer.emit(Arm64Instruction::Add {
            rd: result_reg,
            rn: result_reg,
            rm: prod,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr_a,
            rn: ptr_a,
            imm: 4,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: ptr_b,
            rn: ptr_b,
            imm: 4,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: idx,
            rn: idx,
            imm: 1,
        });
        self.buffer.emit(Arm64Instruction::Cmp {
            rn: idx,
            rm: len_reg,
        });
        self.buffer.emit(Arm64Instruction::BCond {
            cond: Arm64Condition::Lt,
            label: scalar_loop,
        });

        self.buffer.bind_label(scalar_done);
    }
}

// NOTE: A `detect_neon_patterns` / `NeonVectorizablePattern` bytecode scanner
// previously lived here. It was dead code — only ever called from its own unit
// tests, never wired into any aarch64 codegen path — and, worse, it emitted
// patterns with hardcoded placeholder local indices (`array_local: 0` etc.) with
// the operand analysis left unfinished, so it would have vectorized against the
// wrong locals (a miscompile) if ever consumed. aarch64 is not the production
// backend (x64.rs is). Removed in the 2026-06-10 JIT cleanup pass rather than
// gated, since nothing non-test referenced it. If NEON auto-vectorization is
// pursued, reuse the real operand resolution from the x64 BCE analysis
// (`analyze_array_access_operands` / `find_induction_variable`) instead of
// placeholders, and wire the result into codegen before adding it back.

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

/// Materialize the effective address `base + offset` into IP0 (X16).
///
/// Bug-fix (ARM64 BUG #1): used by the `Ldr`/`Str` lowering when `offset`
/// falls outside the 9-bit signed range that LDUR/STUR can encode. We load the
/// (possibly large, possibly negative) byte offset into IP0 with `mov_imm64`
/// (sign-extended via the MOVN path for negatives) and add the base, so the
/// subsequent zero-offset access never touches the base register's value. IP0
/// (X16) is the AAPCS64 intra-procedure-call scratch register and is never a
/// regalloc output, so clobbering it here is safe.
fn emit_addr_into_ip0(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    base: crate::aarch64::Reg,
    offset: i32,
) {
    // IP0 = (i64)offset  (mov_imm64 picks MOVN for negative values, giving a
    // correct two's-complement 64-bit value).
    emitter.mov_imm64(crate::aarch64::Reg::X16, offset as i64 as u64);
    // IP0 = base + IP0
    emitter.add(crate::aarch64::Reg::X16, base, crate::aarch64::Reg::X16);
}

/// Lower an ADD/SUB-immediate (`rd = rn ± imm`) safely.
///
/// Bug-fix (AArch64 LOW, immediate truncation): the ADD/SUB-immediate
/// encoding (`addsub_imm`) only carries a 12-bit unsigned field and silently
/// masks anything wider (`imm12 as u32 & 0xFFF`). The previous lowering passed
/// `imm as u16` straight through, so any immediate > 0xFFF (e.g. a large
/// `iinc` constant or a frame offset folded into an `AddImm`) was silently
/// truncated to a WRONG value with no diagnostic.
///
/// This helper picks the correct, lossless encoding:
///   * magnitude fits 12 bits (≤ 0xFFF)                  → ADD/SUB #imm12
///   * magnitude fits the LSL-#12 shifted 12-bit form
///     (low 12 bits zero, high 12 bits ≤ 0xFFF)          → ADD/SUB #imm12, LSL #12
///   * otherwise                                         → materialize the
///     immediate into IP0 (X16) with `mov_imm64` and use the register form
///     ADD/SUB `rd, rn, X16`.
///
/// The sign is normalized first: a negative immediate flips ADD↔SUB so the
/// magnitude handed to the encoding is always non-negative. IP0 (X16) is the
/// AAPCS64 intra-procedure-call scratch and is never a regalloc output, so the
/// register-form fallback never clobbers a live value.
///
/// Returns `false` when no sound encoding exists (the SP case below), in which
/// case the caller must abandon the method. Previously this path emitted
/// `BRK #0` and reported success: the compile "succeeded", the body was
/// published, and the SP adjustment it was supposed to perform simply never
/// happened — the first thread to reach it died with SIGTRAP, which nothing in
/// the VM converts into anything. A trap is not a lowering; refuse instead.
#[must_use]
fn emit_addsub_imm_safe(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    imm: i32,
    is_sub: bool,
) -> bool {
    // Normalize: fold the sign into the operation so `mag` is non-negative.
    // (i32::MIN's magnitude does not fit i32, so widen to i64 first.)
    let signed = if is_sub { -(imm as i64) } else { imm as i64 };
    let effective_sub = signed < 0;
    let mag = signed.unsigned_abs();

    if mag <= 0xFFF {
        // Fits the plain 12-bit immediate form.
        if effective_sub {
            emitter.sub_imm(rd, rn, mag as u16, false);
        } else {
            emitter.add_imm(rd, rn, mag as u16, false);
        }
        true
    } else if mag & 0xFFF == 0 && (mag >> 12) <= 0xFFF {
        // Fits the LSL #12 shifted 12-bit immediate form.
        let hi = (mag >> 12) as u16;
        if effective_sub {
            emitter.sub_imm(rd, rn, hi, true);
        } else {
            emitter.add_imm(rd, rn, hi, true);
        }
        true
    } else if rd.enc() == 31 || rn.enc() == 31 {
        // SP edge case. Encoding 31 means SP in the ADD/SUB *immediate* form but
        // XZR in the shifted-*register* form, so we cannot fall back to the
        // register path with SP as an operand without changing the semantics
        // (and no extended-register `ADD/SUB (extended)` encoder exists here).
        // This only arises for an SP adjustment whose magnitude exceeds 0xFFF
        // and is not 4 KiB-aligned — i.e. a frame larger than 4095 bytes that
        // isn't page-step-aligned. Refuse the method instead of emitting a
        // silently-wrong SP update (or, as before, a BRK that reports success
        // and then kills the process on first execution). If this ever needs to
        // succeed, plumb an extended-register ADD/SUB into aarch64.rs.
        false
    } else {
        // Out of immediate range: materialize into IP0 (X16) and use the
        // register form. Never silently truncate. (rd/rn are guaranteed not to
        // be SP here, so the shifted-register encoding is correct.)
        emitter.mov_imm64(crate::aarch64::Reg::X16, mag);
        if effective_sub {
            emitter.sub(rd, rn, crate::aarch64::Reg::X16);
        } else {
            emitter.add(rd, rn, crate::aarch64::Reg::X16);
        }
        true
    }
}

/// Emit machine code bytes from the ARM64 pseudo-instruction sequence.
/// Uses `Aarch64Emitter` from `aarch64.rs` to encode each instruction.
///
/// Returns `None` — bail this method to the interpreter — when the pseudo-op
/// sequence cannot be encoded soundly. Four independent reasons:
///
/// 0. an `AddImm`/`SubImm` with no sound encoding — see
///    [`emit_addsub_imm_safe`], which returns `false` for the SP case that
///    used to emit `BRK #0` while still reporting a successful compile;
/// 1. `result.success == false` (an opcode arm refused the method);
/// 2. a branch or literal reference whose label is never bound (see the patch
///    loops at the end — an unbound label used to be left as "branch to
///    self", i.e. an infinite loop in *successfully* compiled code);
/// 3. `Aarch64Emitter::overflowed()` — a branch displacement that did not fit
///    its encoding field. `aarch64.rs` sets that sticky flag precisely so a
///    release build (where the parallel `debug_assert!` is compiled out) can
///    discard the buffer instead of executing a truncated branch. Until this
///    check was added the flag had **no production reader** anywhere in the
///    crate, so on a release `aarch64` build an out-of-range branch was
///    silently truncated and emitted as executable code.
/// Encode a compiled method, and report where every pseudo-op landed.
///
/// The second element is `pseudo_offsets`: `pseudo_offsets[i]` is the byte
/// offset at which `result.instructions[i]` was encoded, and the vector has one
/// extra trailing entry equal to the total code length, so a safepoint whose
/// following pseudo-op is one past the end still resolves. This is the mapping
/// an aarch64 oop map has to be keyed through -- see [`Arm64PendingOopMap`] for
/// why `instruction_count * 4` is not it.
fn emit_machine_code_inner(result: &Arm64CompileResult) -> Option<(Vec<u8>, Vec<usize>)> {
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

    // One entry per pseudo-op, recorded BEFORE it is encoded, so
    // `pseudo_offsets[i]` is where instruction `i` begins.
    let mut pseudo_offsets: Vec<usize> = Vec::with_capacity(result.instructions.len() + 1);
    for inst in &result.instructions {
        pseudo_offsets.push(emitter.offset());
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
                // Range-checked lowering: fits 12-bit / shifted-12-bit form, or
                // materializes into IP0 and uses the register form. Never
                // silently truncates a wide immediate (see emit_addsub_imm_safe).
                // `false` = no sound encoding exists → bail the method.
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::Sub { rd, rn, rm } => {
                emitter.sub(r(*rd), r(*rn), r(*rm));
            }
            Arm64Instruction::SubImm { rd, rn, imm } => {
                if !emit_addsub_imm_safe(&mut emitter, r(*rd), r(*rn), *imm, true) {
                    return None;
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
                // Range-checked lowering. CMP #imm12 (= SUBS XZR, rn, #imm12)
                // only carries an unsigned 12-bit field. A wider or negative
                // immediate cannot be encoded directly, so we materialize it
                // into IP0 (X16) and use the register form `CMP rn, X16`
                // (= SUBS XZR, rn, X16). Never silently truncate (the old
                // `*imm as u16` masked anything > 0xFFFF and the encoder then
                // masked again to 12 bits). The shifted LSL #12 form is left to
                // the register-materialization path to avoid threading a new
                // shifted-immediate encoder through aarch64.rs; CMP immediates
                // in this backend are small (almost always 0) so this costs at
                // most one extra MOV in a cold path.
                let imm = *imm;
                if imm >= 0 && imm <= 0xFFF {
                    emitter.cmp_imm(r(*rn), imm as u16);
                } else {
                    // Negative or out-of-range: materialize and compare by reg.
                    emitter.mov_imm64(crate::aarch64::Reg::X16, imm as i64 as u64);
                    emitter.cmp(r(*rn), crate::aarch64::Reg::X16);
                }
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
            //
            // Bug-fix (ARM64 BUG #1, frame-slot corruption): plain
            // `[base + #offset]` access must NEVER use the pre-index
            // writeback form (`ldr_pre`/`str_pre`), which mutates the base
            // register `base = base + offset` as a side effect. Every frame
            // slot is addressed at a NEGATIVE offset from FP, so the old code
            // corrupted FP on every spill/reload. Routing:
            //   * offset >= 0 and 8-aligned and in scaled range → scaled
            //     unsigned-offset LDR/STR (`ldr_imm`/`str_imm`);
            //   * offset in the signed imm9 range (−256..=255) → unscaled
            //     non-writeback LDUR/STUR (`ldur`/`stur`);
            //   * otherwise → materialize the effective address into a scratch
            //     (IP0/X16) and use a zero-offset access.
            Arm64Instruction::Ldr { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    emitter.ldr_imm(r(*rt), r(*rn), *offset as u16);
                } else if (-256..=255).contains(offset) {
                    emitter.ldur(r(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then LDR [IP0, #0].
                    emit_addr_into_ip0(&mut emitter, r(*rn), *offset);
                    emitter.ldur(r(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldrb { rt, rn, offset } => {
                // Byte loads use an UNSCALED imm12 (units of 1), and only the
                // non-negative unsigned-offset form is encodable here. The one
                // caller is the safepoint poll, which uses offset 0.
                if *offset < 0 || *offset > 0xFFF {
                    return None;
                }
                // Cast: bounds-checked immediately above.
                emitter.ldrb_imm(r(*rt), r(*rn), *offset as u16);
            }
            Arm64Instruction::Str { rt, rn, offset } => {
                if *offset >= 0 && *offset % 8 == 0 && *offset <= 32760 {
                    emitter.str_imm(r(*rt), r(*rn), *offset as u16);
                } else if (-256..=255).contains(offset) {
                    emitter.stur(r(*rt), r(*rn), *offset as i16);
                } else {
                    // Out of imm9 range: IP0 = base + offset, then STR [IP0, #0].
                    emit_addr_into_ip0(&mut emitter, r(*rn), *offset);
                    emitter.stur(r(*rt), crate::aarch64::Reg::X16, 0);
                }
            }
            Arm64Instruction::Ldp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                emitter.ldp(r(*rt1), r(*rt2), r(*rn), *offset as i16);
            }
            Arm64Instruction::Stp {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                emitter.stp(r(*rt1), r(*rt2), r(*rn), *offset as i16);
            }
            // Bug-fix (ARM64 BUG #2): writeback prologue/epilogue pair ops.
            Arm64Instruction::StpPre {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                emitter.stp_pre(r(*rt1), r(*rt2), r(*rn), *offset as i16);
            }
            Arm64Instruction::LdpPost {
                rt1,
                rt2,
                rn,
                offset,
            } => {
                emitter.ldp_post(r(*rt1), r(*rt2), r(*rn), *offset as i16);
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
            //
            // Bug-fix (aarch64 parity audit 2026-08-01, FP twin of ARM64 BUG #1):
            // the previous lowering was `*offset as u16` straight into the
            // SCALED UNSIGNED-offset form. Every FP spill slot is at a NEGATIVE
            // offset from FP (`Arm64FrameLayout::spill_offset` is always < 0),
            // and `-24i32 as u16` is 65512, which the encoder then scales by 8
            // — a load/store roughly 64 KiB ABOVE FP, i.e. into the caller's
            // frame. Silent memory corruption on every `fstore`/`fload` of a
            // spilled float or double.
            //
            // Routing now mirrors the GPR `Ldr`/`Str` arms exactly:
            //   * offset >= 0, correctly scaled, in range → scaled unsigned form
            //   * offset in the signed imm9 range (−256..=255) → unscaled,
            //     non-writeback LDUR/STUR (FP variants)
            //   * otherwise → materialize the address into IP0 (X16) and use a
            //     zero-offset access.
            Arm64Instruction::FpLdr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                // Scaled unsigned form: imm12 scaled by the access size, so the
                // reachable byte range is 8*4095 for D and 4*4095 for S.
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    if *is_double {
                        emitter.ldr_fp_d(fp(*vt), r(*rn), *offset as u16);
                    } else {
                        emitter.ldr_fp_s(fp(*vt), r(*rn), *offset as u16);
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    emit_addr_into_ip0(&mut emitter, r(*rn), *offset);
                    if *is_double {
                        emitter.ldur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.ldur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
                }
            }
            Arm64Instruction::FpStr {
                vt,
                rn,
                offset,
                is_double,
            } => {
                let (scale, max_scaled) = if *is_double {
                    (8i32, 32760i32)
                } else {
                    (4, 16380)
                };
                if *offset >= 0 && *offset % scale == 0 && *offset <= max_scaled {
                    if *is_double {
                        emitter.str_fp_d(fp(*vt), r(*rn), *offset as u16);
                    } else {
                        emitter.str_fp_s(fp(*vt), r(*rn), *offset as u16);
                    }
                } else if (-256..=255).contains(offset) {
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), r(*rn), *offset as i16);
                    } else {
                        emitter.stur_fp_s(fp(*vt), r(*rn), *offset as i16);
                    }
                } else {
                    emit_addr_into_ip0(&mut emitter, r(*rn), *offset);
                    if *is_double {
                        emitter.stur_fp_d(fp(*vt), crate::aarch64::Reg::X16, 0);
                    } else {
                        emitter.stur_fp_s(fp(*vt), crate::aarch64::Reg::X16, 0);
                    }
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

    // Patch branches.
    //
    // An unbound label is NOT recoverable here. The emitted placeholder is a
    // branch with displacement 0 — i.e. a branch to itself, an infinite loop —
    // and nothing downstream patches it: `jit::try_compile_inner`'s aarch64
    // arm copies these bytes straight into an `ExecutableBuffer` and hands the
    // entry point to the VM. The previous code deliberately left the
    // placeholder "for unresolved calls"; since `emit_invoke` always fails the
    // method and `compile_method_with_info`'s discovery pass now binds every
    // back-edge target, an unbound label can only mean a branch target that is
    // not a valid instruction boundary (malformed or truncated bytecode), so
    // the only safe action is to discard the method and interpret it.
    for &(offset, label, is_cond) in &branch_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        if is_cond {
            emitter.patch_bcond(offset, target);
        } else {
            emitter.patch_branch(offset, target);
        }
    }

    // Patch LDR literal instructions to point to their target labels.
    // The label should reference a position containing a 64-bit constant
    // (e.g. a literal pool entry appended after all code). Same reasoning as
    // above: an unpatched LDR-literal reads whatever happens to sit at
    // `pc + 0`, which is the instruction itself — garbage, not a constant.
    for &(offset, label) in &literal_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        emitter.patch_ldr_literal(offset, target);
    }

    // Sticky encoding-overflow check — see this function's doc comment. Must
    // come AFTER the patch loops: `patch_branch`/`patch_bcond`/
    // `patch_ldr_literal` are themselves able to trip the flag, and in a
    // release build they are the *only* thing that reports an out-of-range
    // patch (the `debug_assert!` inside `mark_branch_overflow` is compiled
    // out).
    if emitter.overflowed() {
        return None;
    }

    // The sentinel: a safepoint recorded at the very end of the stream has a
    // following pseudo-op index of `instructions.len()`, whose byte offset is
    // the end of the code.
    pseudo_offsets.push(emitter.offset());
    Some((emitter.code().to_vec(), pseudo_offsets))
}

/// Encode a compiled method.
pub fn emit_machine_code(result: &Arm64CompileResult) -> Option<Vec<u8>> {
    emit_machine_code_inner(result).map(|(code, _)| code)
}

/// Build the publishable artifact for an aarch64 compilation: encode it, and
/// attach the oop maps the GC will read.
///
/// # Why this lives here and not at the call site
///
/// Its caller in `try_compile_inner` sits behind
/// `#[cfg(target_arch = "aarch64")]`, so on an x86-64 developer host or CI
/// runner that block is not compiled AT ALL -- it is never type-checked, never
/// linted and never tested. That is how the publication path came to drop
/// `oop_maps` on the floor without anything noticing: it built its
/// `CompiledMethod` with `CompiledMethod::new(buf)` and never transferred the
/// backend's maps, so even a correct map writer would have produced nothing
/// observable.
///
/// Keeping the logic in a function with no `cfg` on it means every `cargo test`
/// run on any host compiles and exercises it (see
/// `tests::a_published_artifact_carries_its_resolved_oop_maps`), and the gated
/// caller shrinks to one line that cannot silently rot.
pub fn publish_compiled_method(result: &Arm64CompileResult) -> Option<crate::CompiledMethod> {
    let (machine_code, oop_maps) = emit_machine_code_with_oop_maps(result)?;
    let mut buf = crate::ExecutableBuffer::new(machine_code.len().max(4096))?;
    buf.set_tag("aarch64-backend");
    buf.emit(&machine_code);
    let mut cm = crate::CompiledMethod::new(buf);
    // The transfer that was missing. `has_precise_oop_maps()` becomes true when
    // this is non-empty, which makes the walker enumerate these slots IN
    // ADDITION to its conservative sweep -- strictly additive, because
    // suppressing the sweep is gated on `fully_oop_covered`, which this backend
    // never sets (no safepoint-id slot, no shadow stack, no relocation
    // support).
    cm.oop_maps = oop_maps;
    Some(cm)
}

/// Encode a compiled method AND resolve its oop maps to real byte offsets.
///
/// This is the only way an `OopMapEntry` is ever produced on this backend. The
/// compiler records [`Arm64PendingOopMap`]s keyed by pseudo-op index because a
/// byte offset is not knowable until the encoder has run: the pseudo-op stream
/// is not fixed-width (`Label`/`Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, `MovImm`/`AddImm`/`CmpImm` and out-of-range `Ldr`/`Str`
/// expand to 1-4 words). Keying a map by `instruction_count * 4` -- what this
/// backend used to do before the writer was made fail-closed in the 2026-08-01
/// parity audit -- lands the GC on the WRONG FRAME SLOTS at a real safepoint.
///
/// Fails closed: a pending map whose `pseudo_index` is not a valid index
/// discards the whole method rather than publish a map with a guessed PC.
pub fn emit_machine_code_with_oop_maps(
    result: &Arm64CompileResult,
) -> Option<(Vec<u8>, Vec<crate::OopMapEntry>)> {
    let (code, pseudo_offsets) = emit_machine_code_inner(result)?;
    let mut maps: Vec<crate::OopMapEntry> = Vec::with_capacity(result.pending_oop_maps.len());
    for pending in &result.pending_oop_maps {
        let idx = pending.pseudo_index as usize;
        // `pseudo_offsets` carries the trailing sentinel, so a safepoint at the
        // very end of the stream is in range; anything past that is a bug in
        // the writer, not a method we may publish a map for.
        let Some(&byte_off) = pseudo_offsets.get(idx) else {
            return None;
        };
        let Ok(native_pc_offset) = u32::try_from(byte_off) else {
            return None;
        };
        maps.push(crate::OopMapEntry {
            native_pc_offset,
            // Stage 3 precise relocation is x86-64 only: this backend records
            // no safepoint-id slot, so there is no bytecode PC to key on and
            // `find_oop_map_for_safepoint_id` can never select one of these.
            // `find_oop_map_for_pc` is the reader that applies here.
            bytecode_pc: 0,
            frame_slot_offsets: pending.frame_slot_offsets.clone(),
            // No shadow stack and no relocation support on this backend, and
            // register-resident oops are covered only by the CONSERVATIVE walk
            // (see `emit_oop_map_for_safepoint`) -- which marks but cannot
            // rewrite. Claiming moving-young coverage here would be the exact
            // false claim `relocation_coverage_complete` exists to prevent.
            moving_young_coverage_complete: false,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
        });
    }
    Some((code, maps))
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
                    Arm64Instruction::BCond {
                        cond: Arm64Condition::Eq,
                        label,
                    },
                ) => Some(Arm64Instruction::Cbz {
                    rt: *rn,
                    label: *label,
                }),
                (
                    Arm64Instruction::CmpImm { rn, imm: 0 },
                    Arm64Instruction::BCond {
                        cond: Arm64Condition::Ne,
                        label,
                    },
                ) => Some(Arm64Instruction::Cbnz {
                    rt: *rn,
                    label: *label,
                }),
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
                *inst = Arm64Instruction::Mov {
                    rd: *rd,
                    rm: Arm64Register::XZR,
                };
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
                    Arm64Instruction::Ldr {
                        rt: rt1,
                        rn: rn1,
                        offset: off1,
                    },
                    Arm64Instruction::Ldr {
                        rt: rt2,
                        rn: rn2,
                        offset: off2,
                    },
                ) if rn1 == rn2 && *off2 == *off1 + 8 && rt1 != rt2 => {
                    Some(Arm64Instruction::Ldp {
                        rt1: *rt1,
                        rt2: *rt2,
                        rn: *rn1,
                        offset: *off1,
                    })
                }
                (
                    Arm64Instruction::Str {
                        rt: rt1,
                        rn: rn1,
                        offset: off1,
                    },
                    Arm64Instruction::Str {
                        rt: rt2,
                        rn: rn2,
                        offset: off2,
                    },
                ) if rn1 == rn2 && *off2 == *off1 + 8 => Some(Arm64Instruction::Stp {
                    rt1: *rt1,
                    rt2: *rt2,
                    rn: *rn1,
                    offset: *off1,
                }),
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
                    Arm64Instruction::Mul {
                        rd: mul_rd,
                        rn: mul_rn,
                        rm: mul_rm,
                    },
                    Arm64Instruction::Add {
                        rd: add_rd,
                        rn: add_rn,
                        rm: add_rm,
                    },
                ) if mul_rd == add_rn => Some(Arm64Instruction::Madd {
                    rd: *add_rd,
                    rn: *mul_rn,
                    rm: *mul_rm,
                    ra: *add_rm,
                }),
                (
                    Arm64Instruction::Mul {
                        rd: mul_rd,
                        rn: mul_rn,
                        rm: mul_rm,
                    },
                    Arm64Instruction::Add {
                        rd: add_rd,
                        rn: add_rn,
                        rm: add_rm,
                    },
                ) if mul_rd == add_rm => Some(Arm64Instruction::Madd {
                    rd: *add_rd,
                    rn: *mul_rn,
                    rm: *mul_rm,
                    ra: *add_rn,
                }),
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
            assert!(
                Arm64Register(i).is_callee_saved(),
                "X{} should be callee-saved",
                i
            );
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
            Arm64Condition::Eq,
            Arm64Condition::Ne,
            Arm64Condition::Lt,
            Arm64Condition::Le,
            Arm64Condition::Gt,
            Arm64Condition::Ge,
            Arm64Condition::Hi,
            Arm64Condition::Ls,
            Arm64Condition::Cs,
            Arm64Condition::Cc,
            Arm64Condition::Al,
        ];
        assert_eq!(conds.len(), 11);
        // Check they're not all equal.
        assert_ne!(conds[0], conds[1]);
    }

    // -- CallingConvention tests --------------------------------------------

    #[test]
    fn calling_convention_int_arg_regs() {
        assert_eq!(Arm64CallingConvention::INT_ARG_REGS.len(), 8);
        assert_eq!(
            Arm64CallingConvention::int_arg_reg(0),
            Some(Arm64Register::X0)
        );
        assert_eq!(
            Arm64CallingConvention::int_arg_reg(7),
            Some(Arm64Register::X7)
        );
        assert_eq!(Arm64CallingConvention::int_arg_reg(8), None);
    }

    #[test]
    fn calling_convention_callee_saved_count() {
        assert_eq!(Arm64CallingConvention::CALLEE_SAVED.len(), 10);
    }

    #[test]
    fn calling_convention_local_reg_mapping() {
        assert_eq!(
            Arm64CallingConvention::local_reg(0),
            Some(Arm64Register::X19)
        );
        assert_eq!(
            Arm64CallingConvention::local_reg(9),
            Some(Arm64Register::X28)
        );
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
        let saved: Vec<_> = (0..5)
            .map(|i| Arm64CallingConvention::CALLEE_SAVED[i])
            .collect();
        let frame = Arm64FrameLayout::compute(5, 2, &saved);
        assert_eq!(frame.num_reg_locals, 5);
        assert_eq!(frame.num_spills, 2);
        assert_eq!(frame.saved_regs.len(), 5);
    }

    #[test]
    fn frame_layout_16_byte_alignment() {
        for n in 0..20 {
            let num_saved = n.min(Arm64CallingConvention::CALLEE_SAVED.len());
            let saved: Vec<_> = (0..num_saved)
                .map(|i| Arm64CallingConvention::CALLEE_SAVED[i])
                .collect();
            let frame = Arm64FrameLayout::compute(n, n, &saved);
            assert_eq!(
                frame.frame_size % 16,
                0,
                "frame_size {} not aligned for n={}",
                frame.frame_size,
                n
            );
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

    fn make_backend_with_method(
        num_locals: usize,
        num_params: usize,
        bytecode: &[u8],
    ) -> Arm64CompileResult {
        let mut backend = Arm64Backend::new();
        backend.compile_method(num_locals, num_params, 4, bytecode)
    }

    #[test]
    fn backend_prologue_emits_stp_fp_lr() {
        let result = make_backend_with_method(0, 0, &[0xb1]); // return void
                                                              // Bug-fix (ARM64 BUG #2): the prologue now uses the writeback
                                                              // `STP FP, LR, [SP, #-16]!` form (StpPre) so SP is decremented as part
                                                              // of the save, instead of the old non-writeback `Stp` with an unsound
                                                              // "16 already consumed" SUB fudge.
        let has_stp_fp_lr = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::StpPre { rt1, rt2, offset: -16, .. }
                    if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
            )
        });
        assert!(
            has_stp_fp_lr,
            "prologue must save FP/LR with writeback STP (StpPre, #-16)"
        );
    }

    #[test]
    fn backend_epilogue_emits_ldp_fp_lr_and_ret() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        // Bug-fix (ARM64 BUG #2): the epilogue now restores FP/LR with the
        // matching post-index `LDP FP, LR, [SP], #16` (LdpPost).
        let has_ldp = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::LdpPost { rt1, rt2, offset: 16, .. }
                    if *rt1 == Arm64Register::FP && *rt2 == Arm64Register::LR
            )
        });
        let has_ret = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Ret));
        assert!(
            has_ldp,
            "epilogue must restore FP/LR with post-index LDP (LdpPost, #16)"
        );
        assert!(has_ret, "epilogue must emit RET");
    }

    // -- Backend: instruction emission tests --------------------------------

    #[test]
    fn backend_iconst_emits_movimm() {
        // iconst_5 (opcode 0x08) then ireturn (0xac)
        let result = make_backend_with_method(0, 0, &[0x08, 0xac]);
        let _has_mov = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 2, .. }));
        // iconst_5 = opcode 0x08 - 3 = 5
        let has_mov5 = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 5, .. }));
        assert!(has_mov5, "iconst_5 should emit MovImm with imm=5");
    }

    #[test]
    fn backend_int_add_emits_add() {
        // iconst_1, iconst_2, iadd, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x60, 0xac]);
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Add { .. }));
        assert!(has_add, "iadd should emit Add instruction");
    }

    #[test]
    fn backend_int_sub_emits_sub() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x64, 0xac]);
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Sub { .. }));
        assert!(has_sub, "isub should emit Sub instruction");
    }

    #[test]
    fn backend_int_mul_emits_mul() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x68, 0xac]);
        let has_mul = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Mul { .. }));
        assert!(has_mul, "imul should emit Mul instruction");
    }

    /// `idiv` is REFUSED (see the module header and
    /// `backend_idiv_bails_to_interpreter`), and the refusal must survive
    /// compile-time-constant operands too: a constant-folding or peephole path
    /// that resolved `iconst_1 / iconst_2` before the opcode arm ran would
    /// reintroduce the `BRK #1`/SIGTRAP lowering for the shape that looks
    /// safest. Written as "no `SDiv` is emitted" rather than only
    /// "`!success`", so it still fails if the arm is re-wired.
    #[test]
    fn backend_int_div_bails_with_constant_operands() {
        // iconst_1, iconst_2, idiv, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x6c, 0xac]);
        assert!(
            !result.success,
            "idiv must bail, constant operands included"
        );
        let has_div = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SDiv { .. }));
        assert!(!has_div, "no SDiv may be emitted for a refused idiv");
    }

    #[test]
    fn backend_int_neg_emits_neg() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x74, 0xac]);
        let has_neg = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Neg { .. }));
        assert!(has_neg, "ineg should emit Neg instruction");
    }

    #[test]
    fn backend_iload_from_register() {
        // iload_0 (0x1a) then ireturn (0xac), with 1 local
        let result = make_backend_with_method(1, 0, &[0x1a, 0xac]);
        // iload from a register local emits Mov from a callee-saved register.
        let has_mov_from_callee_saved = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rm, .. } if rm.is_callee_saved()
            )
        });
        assert!(
            has_mov_from_callee_saved,
            "iload_0 with register local should emit Mov from callee-saved reg"
        );
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
        let ldr_from_fp = result
            .instructions
            .iter()
            .filter(|inst| {
                matches!(
                    inst,
                    Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
                )
            })
            .count();
        assert!(
            ldr_from_fp >= 2,
            "with 12 simultaneous locals, at least 2 should spill, got {ldr_from_fp} LDRs from FP"
        );
    }

    #[test]
    fn backend_istore_to_register() {
        // iconst_1, istore_0, return
        let result = make_backend_with_method(1, 0, &[0x04, 0x3b, 0xb1]);
        let has_mov_to_callee_saved = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rd, .. } if rd.is_callee_saved()
            )
        });
        assert!(
            has_mov_to_callee_saved,
            "istore_0 with register local should emit Mov to callee-saved reg"
        );
    }

    #[test]
    fn backend_return_int_uses_x0() {
        // iconst_3, ireturn
        let result = make_backend_with_method(0, 0, &[0x06, 0xac]);
        let has_mov_x0 = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Mov { rd, .. } if *rd == Arm64Register::X0
            )
        });
        assert!(has_mov_x0, "ireturn should move result to X0");
    }

    #[test]
    fn backend_return_void_emits_branch_to_epilogue() {
        let result = make_backend_with_method(0, 0, &[0xb1]);
        let has_b = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::B { .. }));
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
            Arm64Instruction::CmpImm {
                rn: Arm64Register::X9,
                imm: 0,
            },
            Arm64Instruction::BCond {
                cond: Arm64Condition::Eq,
                label: 42,
            },
        ];
        let count = Arm64PeepholeOptimizer::optimize_cbz(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(
            matches!(instrs[0], Arm64Instruction::Cbz { rt, label: 42 } if rt == Arm64Register::X9)
        );
    }

    #[test]
    fn peephole_cbnz_optimization() {
        let mut instrs = vec![
            Arm64Instruction::CmpImm {
                rn: Arm64Register::X10,
                imm: 0,
            },
            Arm64Instruction::BCond {
                cond: Arm64Condition::Ne,
                label: 7,
            },
        ];
        let count = Arm64PeepholeOptimizer::optimize_cbz(&mut instrs);
        assert_eq!(count, 1);
        assert!(
            matches!(instrs[0], Arm64Instruction::Cbnz { rt, label: 7 } if rt == Arm64Register::X10)
        );
    }

    #[test]
    fn peephole_remove_identity_add_zero() {
        let mut instrs = vec![
            Arm64Instruction::AddImm {
                rd: Arm64Register::X9,
                rn: Arm64Register::X9,
                imm: 0,
            },
            Arm64Instruction::Nop,
        ];
        let count = Arm64PeepholeOptimizer::remove_identity_ops(&mut instrs);
        assert_eq!(count, 1);
        assert_eq!(instrs.len(), 1);
        assert!(matches!(instrs[0], Arm64Instruction::Nop));
    }

    #[test]
    fn peephole_keep_non_identity_add() {
        let mut instrs = vec![Arm64Instruction::AddImm {
            rd: Arm64Register::X9,
            rn: Arm64Register::X9,
            imm: 4,
        }];
        let count = Arm64PeepholeOptimizer::remove_identity_ops(&mut instrs);
        assert_eq!(count, 0);
        assert_eq!(instrs.len(), 1);
    }

    #[test]
    fn peephole_merge_ldr_pair() {
        let mut instrs = vec![
            Arm64Instruction::Ldr {
                rt: Arm64Register::X9,
                rn: Arm64Register::FP,
                offset: -16,
            },
            Arm64Instruction::Ldr {
                rt: Arm64Register::X10,
                rn: Arm64Register::FP,
                offset: -8,
            },
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
            Arm64Instruction::Str {
                rt: Arm64Register::X19,
                rn: Arm64Register::FP,
                offset: -32,
            },
            Arm64Instruction::Str {
                rt: Arm64Register::X20,
                rn: Arm64Register::FP,
                offset: -24,
            },
        ];
        let count = Arm64PeepholeOptimizer::merge_load_store_pairs(&mut instrs);
        assert_eq!(count, 1);
        assert!(matches!(instrs[0], Arm64Instruction::Stp { .. }));
    }

    #[test]
    fn peephole_fuse_mul_add_to_madd() {
        let mut instrs = vec![
            Arm64Instruction::Mul {
                rd: Arm64Register::X9,
                rn: Arm64Register::X10,
                rm: Arm64Register::X11,
            },
            Arm64Instruction::Add {
                rd: Arm64Register::X12,
                rn: Arm64Register::X9,
                rm: Arm64Register::X13,
            },
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
            Arm64Instruction::AddImm {
                rd: Arm64Register::X9,
                rn: Arm64Register::X9,
                imm: 0,
            },
            Arm64Instruction::MovImm {
                rd: Arm64Register::X10,
                imm: 0,
            },
            Arm64Instruction::Nop,
        ];
        let total = Arm64PeepholeOptimizer::optimize(&mut instrs);
        // identity add removed (1) + zero mov -> xzr (1) = 2
        assert!(
            total >= 2,
            "expected at least 2 optimizations, got {}",
            total
        );
    }

    #[test]
    fn peephole_zero_reg_optimization() {
        let mut instrs = vec![Arm64Instruction::MovImm {
            rd: Arm64Register::X9,
            imm: 0,
        }];
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
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Add { .. }));
        assert!(has_add);
    }

    #[test]
    fn compile_bipush() {
        // bipush 42, ireturn
        let result = make_backend_with_method(0, 0, &[0x10, 42, 0xac]);
        assert!(result.success);
        let has_42 = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MovImm { imm: 42, .. }));
        assert!(has_42);
    }

    // -- Float/double operation tests ------------------------------------------

    #[test]
    fn backend_float_add_emits_fadd() {
        // fconst_1 (0x0c), fconst_1 (0x0c), fadd (0x62), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
        assert!(result.success, "float add should succeed");
        let has_fadd = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FaddDouble { .. }));
        assert!(has_fadd, "fadd should emit FaddDouble");
    }

    #[test]
    fn backend_float_sub_emits_fsub() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x66, 0xb1]);
        assert!(result.success);
        let has_fsub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FsubDouble { .. }));
        assert!(has_fsub);
    }

    #[test]
    fn backend_float_mul_emits_fmul() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x6a, 0xb1]);
        assert!(result.success);
        let has_fmul = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FmulDouble { .. }));
        assert!(has_fmul);
    }

    #[test]
    fn backend_float_div_emits_fdiv() {
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x6e, 0xb1]);
        assert!(result.success);
        let has_fdiv = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FdivDouble { .. }));
        assert!(has_fdiv);
    }

    #[test]
    fn backend_type_conversion_i2f() {
        // iconst_1 (0x04), i2f (0x86), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x04, 0x86, 0xb1]);
        assert!(result.success, "i2f conversion should succeed");
        let has_scvtf = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::ScvtfDouble { .. }));
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
        assert!(
            bytes.len() >= 16,
            "add method should produce at least a few instructions"
        );
    }

    #[test]
    fn backend_float_neg_emits_fsub() {
        // fconst_1 (0x0c), fneg (0x76), return void (0xb1)
        let result = make_backend_with_method(0, 0, &[0x0c, 0x76, 0xb1]);
        assert!(result.success, "float neg should succeed");
        // fneg uses FsubDouble (zero - value)
        let has_fsub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FsubDouble { .. }));
        assert!(has_fsub, "fneg should emit FsubDouble");
    }

    #[test]
    fn backend_float_rem_bails_to_interpreter() {
        // fconst_1, fconst_1, frem (0x72), return void.
        //
        // frem/drem have no EXACT ARM64 lowering in this backend (the
        // truncating FCVTZS round-trip saturates for large operands), so the
        // dispatch bails to the interpreter rather than emit a silently-wrong
        // result. The compile must report failure and emit_machine_code must
        // return None.
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x72, 0xb1]);
        assert!(
            !result.success,
            "frem should bail (success=false), not emit an inexact remainder"
        );
        assert!(
            emit_machine_code(&result).is_none(),
            "a failed compile must not produce machine code"
        );
    }

    #[test]
    fn backend_fconst_bit_exact_for_non_integral() {
        // emit_fconst must move the IEEE-754 bit pattern (FMOV Dd, Xn), NOT
        // perform an integer→float conversion (ScvtfDouble), which would round
        // 2.5 down to 2.0. Drive emit_fconst directly with a non-integral value.
        let mut backend = Arm64Backend::new();
        backend.emit_fconst(2.5);
        let expected_bits = 2.5f64.to_bits() as i64;
        let has_movimm_bits = backend.buffer.instructions().iter().any(
            |inst| matches!(inst, Arm64Instruction::MovImm { imm, .. } if *imm == expected_bits),
        );
        assert!(
            has_movimm_bits,
            "emit_fconst should materialize the exact f64 bit pattern {:#018x}",
            expected_bits as u64
        );
        let has_fmov = backend
            .buffer
            .instructions()
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FmovToFp { .. }));
        assert!(
            has_fmov,
            "emit_fconst should bit-move the pattern into FP via FmovToFp"
        );
        let has_scvtf = backend
            .buffer
            .instructions()
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::ScvtfDouble { .. }));
        assert!(
            !has_scvtf,
            "emit_fconst must NOT use ScvtfDouble (integer→float conversion)"
        );
    }

    #[test]
    fn backend_ldc_bails_to_interpreter() {
        // ldc (0x12) #1, return void. The backend has no constant pool, so it
        // must bail (success=false) rather than guess at the constant.
        let result = make_backend_with_method(0, 0, &[0x12, 0x01, 0xb1]);
        assert!(
            !result.success,
            "ldc should bail to the interpreter (no constant pool available)"
        );
        // ldc2_w (0x14) #1, return void — same reasoning (long/double constant).
        let result2 = make_backend_with_method(0, 0, &[0x14, 0x00, 0x01, 0xb1]);
        assert!(!result2.success, "ldc2_w should bail to the interpreter");
    }

    #[test]
    fn addsub_imm_safe_no_truncation_for_wide_immediate() {
        use crate::aarch64::{Aarch64Emitter, Reg};

        // Small immediate (fits 12 bits): one ADD-immediate instruction.
        let mut e_small = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e_small,
            Reg::X9,
            Reg::X9,
            5,
            false
        ));
        assert_eq!(
            e_small.code().len(),
            4,
            "a 12-bit immediate should lower to a single ADD-imm"
        );

        // Wide immediate (> 0xFFF, not 4 KiB-aligned): must NOT be truncated to
        // a single ADD-imm. It is materialized into X16 (one or more MOV-wide)
        // then added by register, so the sequence is longer than one
        // instruction and never encodes the (wrong) masked immediate.
        let wide = 5000i32; // 0x1388 — low 12 bits 0x388 != 0, > 0xFFF
        let mut e_wide = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(
            &mut e_wide,
            Reg::X9,
            Reg::X9,
            wide,
            false
        ));
        assert!(
            e_wide.code().len() > 4,
            "a >12-bit immediate must not collapse into one (truncated) ADD-imm"
        );

        // Compare against what a (buggy) single truncated ADD-imm would encode,
        // and assert the safe lowering's first 4 bytes are NOT that instruction.
        let mut e_trunc = Aarch64Emitter::new();
        // The old buggy path: add_imm with the value masked to 12 bits.
        e_trunc.add_imm(Reg::X9, Reg::X9, (wide as u16) & 0xFFF, false);
        let trunc_first = &e_trunc.code()[0..4];
        assert_ne!(
            &e_wide.code()[0..4],
            trunc_first,
            "safe lowering must not begin with the truncated ADD-imm encoding"
        );
    }

    #[test]
    fn backend_fcmp_emits_fcmpdouble() {
        // fconst_1, fconst_1, fcmpl (0x95), return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x95, 0xb1]);
        assert!(result.success, "fcmpl should succeed");
        let has_fcmp = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::FcmpDouble { .. }));
        assert!(has_fcmp, "fcmpl should emit FcmpDouble");
    }

    #[test]
    fn backend_emit_machine_code_with_float_ops() {
        // fconst_1, fconst_1, fadd, return void
        let result = make_backend_with_method(0, 0, &[0x0c, 0x0c, 0x62, 0xb1]);
        assert!(result.success);
        let code = emit_machine_code(&result);
        assert!(
            code.is_some(),
            "float operations should produce machine code"
        );
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

    // `backend_idiv_opcode` (iconst_2, iconst_1, idiv, ireturn — asserted
    // `success`) was removed with the 82a9d08fc div/rem refusal: inverted it
    // would assert strictly less than
    // `backend_int_div_bails_with_constant_operands` above, on the same shape.

    #[test]
    fn backend_ifeq_uses_label_not_raw_target() {
        // iconst_0, ifeq +5 (to second return), return, return
        // 0x03=iconst_0, 0x99=ifeq, 0x00 0x05=branch offset
        let result = make_backend_with_method(0, 0, &[0x03, 0x99, 0x00, 0x05, 0xb1, 0xb1]);
        assert!(
            result.success,
            "ifeq with label should compile successfully"
        );
    }

    #[test]
    fn backend_bounds_check_on_truncated_bytecode() {
        // Truncated ifeq: only 2 bytes instead of 3 (opcode + 2 offset bytes)
        let result = make_backend_with_method(0, 0, &[0x99, 0x00]);
        // Should fail gracefully, not panic
        assert!(
            !result.success || result.instructions.is_empty(),
            "truncated branch should not produce valid code"
        );
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
        assert!(
            saved_count >= 2 && saved_count <= 3,
            "graph coloring should identify 2-3 used callee-saved regs, got {}",
            saved_count
        );
        for reg in &result.frame.saved_regs {
            assert!(
                reg.is_callee_saved(),
                "saved reg X{} should be callee-saved",
                reg.0
            );
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
        assert!(
            saved_count <= 3 && saved_count >= 2,
            "expected 2-3 callee-saved regs, got {saved_count}"
        );
    }

    #[test]
    fn p95_backend_spill_slot_for_11th_local() {
        // 11 locals, all loaded → 10 get regs, 1 spills.
        // Load local 10 (spilled) and return it.
        let result = make_backend_with_method(11, 0, &[0x15, 10, 0xac]);
        assert!(result.success);
        // Local 10 should be loaded from a frame spill slot (Ldr from FP).
        let has_spill_load = result.instructions.iter().any(|inst| {
            matches!(
                inst,
                Arm64Instruction::Ldr { rn, .. } if *rn == Arm64Register::FP
            )
        });
        assert!(has_spill_load, "11th local should spill to stack");
    }

    /// Float locals must NOT be homed in `D8`–`D15`.
    ///
    /// Replaces `p95_backend_float_local_uses_fp_reg`, which asserted the
    /// opposite. `regalloc::ARM64_LOCAL_FPS` is `D8..D15`, which AAPCS64 makes
    /// **callee-saved**, and this backend's prologue/epilogue save only GPRs —
    /// so homing a float local there destroyed the caller's copy. See the
    /// comment on the `float_local_regs` loop in `compile_pass`.
    ///
    /// The observable consequence is that a spilled float local goes through a
    /// frame slot (`FpStr`/`FpLdr`) or a GPR bit-move, never `FmovFp` from a
    /// callee-saved D register.
    #[test]
    fn float_locals_never_use_callee_saved_fp_regs() {
        // fconst_1 (0x0c), fstore_0 (0x43), fload_0 (0x22), return void (0xb1)
        let result = make_backend_with_method(1, 0, &[0x0c, 0x43, 0x22, 0xb1]);
        assert!(result.success);

        // No instruction may name an FP register outside the caller-saved
        // scratch set V0-V7 (encoded 32..=39). D8-D15 would appear as 40..=47.
        let mut offenders: Vec<u8> = Vec::new();
        fn note(reg: Arm64Register, offenders: &mut Vec<u8>) {
            if reg.0 >= 40 {
                offenders.push(reg.0);
            }
        }
        for inst in &result.instructions {
            match inst {
                Arm64Instruction::FmovFp { vd, vn } => {
                    note(*vd, &mut offenders);
                    note(*vn, &mut offenders);
                }
                Arm64Instruction::FmovToFp { vd, .. } => note(*vd, &mut offenders),
                Arm64Instruction::FmovFromFp { vn, .. } => note(*vn, &mut offenders),
                Arm64Instruction::FpLdr { vt, .. } | Arm64Instruction::FpStr { vt, .. } => {
                    note(*vt, &mut offenders)
                }
                _ => {}
            }
        }
        assert!(
            offenders.is_empty(),
            "float locals must not be homed in callee-saved D8-D15 \
             (this backend never saves them); saw register encodings {offenders:?}"
        );

        // And the value really does round-trip through a frame slot, i.e. the
        // spill path — not silently dropped.
        let has_fp_spill = result
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FpStr { .. }));
        let has_gpr_home = result
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFromFp { .. }));
        assert!(
            has_fp_spill || has_gpr_home,
            "fstore_0 must store the float somewhere (frame slot or GPR)"
        );
    }

    /// The FP frame-slot lowering must never use the scaled unsigned-offset
    /// form for a negative displacement.
    ///
    /// `Arm64FrameLayout::spill_offset` is always negative, and the previous
    /// lowering did `offset as u16` — turning −24 into 65512, which the encoder
    /// then scales by 8. That is a load/store ~64 KiB *above* FP, inside the
    /// caller's frame. This checks the encoded bytes directly: an unscaled
    /// LDUR/STUR (bit 24 clear) rather than the scaled form (bit 24 set).
    #[test]
    fn fp_frame_slot_access_uses_unscaled_form_for_negative_offsets() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpStr {
                vt: Arm64Register::V0,
                rn: Arm64Register::FP,
                offset: -24,
                is_double: true,
            },
            Arm64Instruction::FpLdr {
                vt: Arm64Register::V1,
                rn: Arm64Register::FP,
                offset: -24,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("FP frame access must encode");
        assert_eq!(bytes.len(), 3 * 4);

        // STUR D0, [X29, #-24] = 0xFC1E83A0; LDUR D1, [X29, #-24] = 0xFC5E83A1.
        // (Derived from the GPR STUR/LDUR words with the V bit — 1<<26 — set;
        // imm9 = -24 & 0x1FF = 0x1E8.)
        let w0 = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        let w1 = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(w0, 0xFC1E_83A0, "STUR D0, [X29, #-24]");
        assert_eq!(w1, 0xFC5E_83A1, "LDUR D1, [X29, #-24]");

        for (name, w) in [("store", w0), ("load", w1)] {
            assert_eq!(
                (w >> 24) & 1,
                0,
                "{name} must NOT be the scaled unsigned-offset form (that form \
                 cannot encode a negative displacement — it reads -24 as 65512)"
            );
            assert_eq!(
                (w >> 10) & 0x3,
                0b00,
                "{name} must not write back to the base register (FP)"
            );
        }
    }

    /// A positive, correctly-scaled FP offset must still take the compact
    /// scaled form, so the fix above is a routing change and not a blanket
    /// switch to the (shorter-range) unscaled encoding.
    #[test]
    fn fp_positive_aligned_offset_still_uses_scaled_form() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpLdr {
                vt: Arm64Register::V0,
                rn: Arm64Register::X1,
                offset: 16,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("must encode");
        let w = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        // LDR D0, [X1, #16] — scaled unsigned form has bit 24 set, imm12 = 2.
        assert_eq!(
            (w >> 24) & 1,
            1,
            "positive aligned offset uses the scaled form"
        );
        assert_eq!((w >> 10) & 0xFFF, 2, "imm12 must be 16/8 == 2");
    }

    /// An FP offset outside the imm9 range must materialize the address rather
    /// than truncate. 4 words: MOVN/MOVZ(+MOVK) into IP0, ADD, then the access.
    #[test]
    fn fp_far_offset_materializes_address() {
        let result = result_from_instructions(vec![
            Arm64Instruction::FpStr {
                vt: Arm64Register::V0,
                rn: Arm64Register::FP,
                offset: -100_000,
                is_double: true,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("must encode");
        assert!(
            bytes.len() > 2 * 4,
            "a far FP offset must expand into an address materialization, \
             not a single truncated access"
        );
        // Last instruction before RET is the zero-offset access off IP0 (X16).
        let n = bytes.len();
        let access = u32::from_le_bytes([bytes[n - 8], bytes[n - 7], bytes[n - 6], bytes[n - 5]]);
        assert_eq!((access >> 5) & 0x1F, 16, "access base must be IP0 (X16)");
        assert_eq!(
            (access >> 12) & 0x1FF,
            0,
            "materialized access uses offset 0"
        );
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
        let has_add = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::AddImm { imm: 5, .. }));
        assert!(has_add, "iinc +5 should emit AddImm with imm=5");
    }

    #[test]
    fn p95_iinc_negative_compiles() {
        // iinc 0 -1
        let result = make_backend_with_method(1, 1, &[0x84, 0x00, 0xFF, 0x1a, 0xac]);
        assert!(result.success, "iinc -1 should compile");
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SubImm { imm: 1, .. }));
        assert!(has_sub, "iinc -1 should emit SubImm with imm=1");
    }

    #[test]
    fn p95_iushr_compiles() {
        // iconst_1, iconst_1, iushr, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x04, 0x7c, 0xac]);
        assert!(result.success, "iushr should compile");
        let has_lsr = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Lsr { .. }));
        assert!(has_lsr, "iushr should emit Lsr");
    }

    /// Constant-operand counterpart of
    /// `backend_irem_and_lrem_bail_to_interpreter`. The old `a - (a/b)*b`
    /// lowering is what made `x % 0` return `x` (AArch64 `SDIV` by zero yields
    /// 0 instead of trapping), so the assertion is that no `Msub` is emitted,
    /// not merely that the method bailed.
    #[test]
    fn p95_irem_bails_with_constant_operands() {
        // iconst_5, iconst_2, irem, ireturn
        let result = make_backend_with_method(0, 0, &[0x08, 0x05, 0x70, 0xac]);
        assert!(
            !result.success,
            "irem must bail, constant operands included"
        );
        let has_msub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::Msub { .. }));
        assert!(
            !has_msub,
            "no Msub may be emitted: `a - (a/b)*b` is exactly the lowering that \
             returned `a` for `a % 0`"
        );
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
        let result =
            make_backend_with_method(0, 0, &[0x04, 0x05, 0x5c, 0x57, 0x57, 0x57, 0x57, 0xb1]);
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
            0x04, // 0: iconst_1
            0xaa, // 1: tableswitch
            0x00, 0x00, // 2-3: padding
            0x00, 0x00, 0x00, 0x1b, // 4: default → +27 → PC 28
            0x00, 0x00, 0x00, 0x00, // 8: low = 0
            0x00, 0x00, 0x00, 0x02, // 12: high = 2
            0x00, 0x00, 0x00, 0x1b, // 16: case 0 → +27 → PC 28
            0x00, 0x00, 0x00, 0x1b, // 20: case 1 → +27
            0x00, 0x00, 0x00, 0x1b, // 24: case 2 → +27
            0xb1, // 28: return
        ];
        let result = make_backend_with_method(0, 0, bytecode);
        assert!(result.success, "tableswitch should compile");
    }

    #[test]
    fn p95_lookupswitch_compiles() {
        // iconst_1, lookupswitch { npairs=2, default→done, 1→done, 42→done }, return
        let _bytecode = &[
            0x04, // 0: iconst_1
            0xab, // 1: lookupswitch
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
            0x04, // 0: iconst_1
            0xab, // 1: lookupswitch
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

    /// A loop must be REFUSED, because this backend emits no safepoint poll.
    ///
    /// Formerly `p95_backend_fibonacci_compiles`, which asserted the opposite.
    /// See [`Arm64Backend::label_for_pc`]: x86-64 polls
    /// `helpers.safepoint_flag_addr` at every back-edge
    /// (`x64::Backend::emit_safepoint_poll`); this backend has no poll and no
    /// way to emit one, so a compiled loop is a region a thread can sit in
    /// forever without ever reaching a stop-the-world request — the GC then
    /// hangs the VM. Refusing the method and interpreting it is the only sound
    /// option, and interpretation restores the interpreter's own polls.
    #[test]
    fn loop_method_bails_no_safepoint_poll() {
        // Fibonacci-like: int fib(int n) with loop
        // local 0 = n (param), local 1 = a = 0, local 2 = b = 1, local 3 = tmp
        // istore_1(a=0), iconst_1, istore_2(b=1), iload_0, ifle done,
        // loop: iload_2, iload_1, iadd, istore_3, iload_2, istore_1, iload_3, istore_2,
        //       iinc 0 -1, iload_0, ifgt loop, done: iload_1, ireturn
        let bytecode: &[u8] = &[
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1  (a = 0)
            0x04, // 2: iconst_1
            0x3d, // 3: istore_2  (b = 1)
            0x1a, // 4: iload_0   (n)
            0x9e, 0x00, 0x14, // 5: ifle +20 → 25
            // loop body at PC 8:
            0x1c, // 8: iload_2   (b)
            0x1b, // 9: iload_1   (a)
            0x60, // 10: iadd     (a+b)
            0x3e, // 11: istore_3 (tmp = a+b)
            0x1c, // 12: iload_2  (b)
            0x3c, // 13: istore_1 (a = b)
            0x1d, // 14: iload_3  (tmp)
            0x3d, // 15: istore_2 (b = tmp)
            0x84, 0x00, 0xff, // 16: iinc 0, -1  (n--)
            0x1a, // 19: iload_0  (n)
            0x9d, 0xff, 0xf3, // 20: ifgt -13 → 8
            // done at PC 23:
            0x1b, // 23: iload_1  (a)
            0xac, // 24: ireturn
        ];
        let result = make_backend_with_method(4, 1, bytecode);
        assert!(
            !result.success,
            "a method with a loop back-edge must bail: there is no safepoint \
             poll to place on the back-edge, so a thread in this loop would \
             never reach a stop-the-world request"
        );
        assert!(
            emit_machine_code(&result).is_none(),
            "the encoder must also refuse a bailed result"
        );

        // Negative control: the same shape of body with only a FORWARD branch
        // still compiles, so the bail is specific to the back-edge and is not a
        // blanket refusal of branching methods.
        let straight_line: &[u8] = &[
            0x03, // 0: iconst_0
            0x3c, // 1: istore_1
            0x04, // 2: iconst_1
            0x3d, // 3: istore_2
            0x1a, // 4: iload_0
            0x9e, 0x00, 0x0a, // 5: ifle +10 → pc 15 (forward)
            0x1c, // 8: iload_2
            0x1b, // 9: iload_1
            0x60, // 10: iadd
            0x3e, // 11: istore_3
            0x1b, // 12: iload_1
            0xac, // 13: ireturn
            0x00, // 14: nop
            0x1b, // 15: iload_1
            0xac, // 16: ireturn
        ];
        let ok = make_backend_with_method(4, 1, straight_line);
        assert!(
            ok.success,
            "the same body with only a forward branch must still compile"
        );
    }

    /// A single backward `goto` — the minimal back-edge — must bail, including
    /// the degenerate `goto 0` self-loop.
    #[test]
    fn backward_goto_and_self_loop_both_bail() {
        // nop; goto -1 from pc 1 → target pc 0, the tightest possible loop.
        let self_loop = make_backend_with_method(0, 0, &[0x00, 0xa7, 0xff, 0xff]);
        assert!(!self_loop.success, "a one-instruction loop must bail");

        // nop; nop; goto -2 (back to pc 0)
        let back = make_backend_with_method(0, 0, &[0x00, 0x00, 0xa7, 0xff, 0xfe]);
        assert!(!back.success, "a backward goto must bail");

        // Forward goto over a return — still fine.
        let fwd = make_backend_with_method(0, 0, &[0xa7, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(fwd.success, "a forward goto must still compile");
    }

    /// A backward `tableswitch` case target is a back-edge too, and switch
    /// targets take a different code path (`label_for_pc` from the switch arm
    /// rather than from an `if*` arm), so it gets its own guard.
    #[test]
    fn backward_switch_case_target_bails() {
        // pc 0: nop
        // pc 1: nop
        // pc 2: nop
        // pc 3: iconst_1
        // pc 4: tableswitch, padding to pc 8, default +12 (→ pc 16), low=0,
        //       high=0, case 0 offset = -4 (→ pc 0, a back-edge)
        let bytecode: &[u8] = &[
            0x00, 0x00, 0x00, // 0-2: nop nop nop
            0x04, // 3: iconst_1
            0xaa, // 4: tableswitch
            0x00, 0x00, 0x00, // 5-7: padding to 4-byte boundary
            0x00, 0x00, 0x00, 0x14, // 8: default → +20 → pc 24 (forward)
            0x00, 0x00, 0x00, 0x00, // 12: low = 0
            0x00, 0x00, 0x00, 0x00, // 16: high = 0
            0xff, 0xff, 0xff, 0xfc, // 20: case 0 → -4 → pc 0 (BACK-EDGE)
            0xb1, // 24: return
        ];
        let result = make_backend_with_method(0, 0, bytecode);
        assert!(
            !result.success,
            "a backward switch case target is a back-edge and must bail too"
        );
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
            Arm64Register::X0, // arr_reg
            Arm64Register::X1, // len_reg
            Arm64Register::X2, // result_reg
        );

        let instrs = backend.buffer.instructions();

        // Should contain NeonLd1_4s (vector load)
        let has_ld1 = instrs
            .iter()
            .any(|i| matches!(i, Arm64Instruction::NeonLd1_4s { .. }));
        assert!(has_ld1, "NEON array sum should emit NeonLd1_4s");

        // Should contain NeonAdd4s (vector add)
        let has_add = instrs
            .iter()
            .any(|i| matches!(i, Arm64Instruction::NeonAdd4s { .. }));
        assert!(has_add, "NEON array sum should emit NeonAdd4s");

        // Should contain NeonSt1_4s (for horizontal reduce via store)
        let has_st1 = instrs
            .iter()
            .any(|i| matches!(i, Arm64Instruction::NeonSt1_4s { .. }));
        assert!(
            has_st1,
            "NEON array sum should emit NeonSt1_4s for horizontal reduce"
        );

        // Should contain scalar tail loop
        let branch_count = instrs
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::BCond { .. }))
            .count();
        assert!(
            branch_count >= 3,
            "should have vector loop + scalar tail branches, got {branch_count}"
        );
    }

    #[test]
    fn p95_neon_dot_product_emits_mul_and_add() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        backend.emit_neon_dot_product(
            Arm64Register::X0, // arr_a
            Arm64Register::X1, // arr_b
            Arm64Register::X2, // len
            Arm64Register::X3, // result
        );

        let instrs = backend.buffer.instructions();

        // Should contain NeonMul4s (vector multiply)
        let has_mul = instrs
            .iter()
            .any(|i| matches!(i, Arm64Instruction::NeonMul4s { .. }));
        assert!(has_mul, "NEON dot product should emit NeonMul4s");

        // Should contain NeonAdd4s (vector accumulate)
        let has_add = instrs
            .iter()
            .any(|i| matches!(i, Arm64Instruction::NeonAdd4s { .. }));
        assert!(has_add, "NEON dot product should emit NeonAdd4s");

        // Should contain two NeonLd1_4s (one for each array)
        let ld1_count = instrs
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::NeonLd1_4s { .. }))
            .count();
        assert_eq!(ld1_count, 2, "NEON dot product should load from two arrays");
    }

    // NOTE: the `p95_neon_pattern_detection_*` tests were removed alongside the
    // dead `detect_neon_patterns` / `NeonVectorizablePattern` scanner (2026-06-10
    // JIT cleanup). That scanner was never wired into codegen and emitted
    // placeholder local indices; see the removal note near the top of this file.

    #[test]
    fn p95_neon_machine_code_emission() {
        let mut backend = Arm64Backend::new();
        backend.buffer = Arm64CodeBuffer::new();
        backend.frame = Some(Arm64FrameLayout::compute(0, 16, &[]));
        backend.local_regs = Vec::new();
        backend.float_local_regs = Vec::new();

        // Emit a small NEON sequence
        backend.buffer.emit(Arm64Instruction::NeonLd1_4s {
            vt: Arm64Register::V0,
            rn: Arm64Register::X0,
        });
        backend.buffer.emit(Arm64Instruction::NeonAdd4s {
            vd: Arm64Register::V0,
            vn: Arm64Register::V0,
            vm: Arm64Register::V1,
        });
        backend.buffer.emit(Arm64Instruction::NeonMul4s {
            vd: Arm64Register::V2,
            vn: Arm64Register::V0,
            vm: Arm64Register::V1,
        });
        backend.buffer.emit(Arm64Instruction::NeonSt1_4s {
            vt: Arm64Register::V2,
            rn: Arm64Register::X1,
        });
        backend.buffer.emit(Arm64Instruction::Ret);

        let result = Arm64CompileResult {
            instructions: backend.buffer.instructions().to_vec(),
            frame: Arm64FrameLayout::compute(0, 0, &[]),
            labels: backend.buffer.labels.clone(),
            success: true,
            pending_oop_maps: Vec::new(),
        };

        let code = emit_machine_code(&result);
        assert!(
            code.is_some(),
            "NEON instructions should produce machine code"
        );
        let bytes = code.unwrap();
        assert_eq!(
            bytes.len(),
            5 * 4,
            "5 instructions × 4 bytes each = 20 bytes"
        );
        // All bytes should be non-zero (valid ARM64 encodings)
        assert!(
            bytes.iter().any(|&b| b != 0),
            "encoded bytes should be non-trivial"
        );
    }

    // -----------------------------------------------------------------------
    // aarch64-coverage audit (2026-07-26): `emit_machine_code` soundness gates
    // -----------------------------------------------------------------------

    /// Build a minimal `Arm64CompileResult` around a caller-supplied
    /// instruction sequence, with `success = true` so the only thing under
    /// test is `emit_machine_code`'s own validation.
    fn result_from_instructions(instructions: Vec<Arm64Instruction>) -> Arm64CompileResult {
        Arm64CompileResult {
            instructions,
            frame: Arm64FrameLayout::compute(0, 0, &[]),
            labels: HashMap::new(),
            success: true,
            pending_oop_maps: Vec::new(),
        }
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_branch_label() {
        // `B <label 7>` where label 7 is never bound. The old patch loop left
        // the displacement-0 placeholder in place — a branch to itself, i.e.
        // an infinite loop inside successfully-"compiled" code.
        let result = result_from_instructions(vec![
            Arm64Instruction::B { label: 7 },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound branch label must bail the method, not emit `B .`"
        );
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_conditional_branch_label() {
        let result = result_from_instructions(vec![
            Arm64Instruction::Cbz {
                rt: Arm64Register::X0,
                label: 3,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound CBZ label must bail the method"
        );
    }

    #[test]
    fn emit_machine_code_bails_on_unbound_ldr_literal_label() {
        // An unpatched LDR (literal) reads at `pc + 0` — the instruction
        // itself — rather than a constant-pool entry.
        let result = result_from_instructions(vec![
            Arm64Instruction::LdrLiteral {
                rt: Arm64Register::X0,
                label: 11,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unbound LDR-literal label must bail the method"
        );
    }

    #[test]
    fn emit_machine_code_accepts_bound_labels() {
        // Companion to the three bail tests: a correctly bound forward branch
        // must still encode, so the new checks cannot be satisfied by a
        // blanket refusal.
        let result = result_from_instructions(vec![
            Arm64Instruction::B { label: 1 },
            Arm64Instruction::Nop,
            Arm64Instruction::Label(1),
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("bound labels must encode");
        // 3 real instructions (Label emits nothing) × 4 bytes.
        assert_eq!(bytes.len(), 3 * 4);
        // The `B` at offset 0 targets offset 8 → imm26 == 2.
        let b = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(b >> 26, 0b000101, "opcode must be unconditional B");
        assert_eq!(b & 0x03ff_ffff, 2, "B displacement must be +2 instructions");
    }

    /// `B.cond`/`CBZ` carry a signed imm19 → ±1 MiB. A conditional branch
    /// over more than 2^18 instructions cannot be encoded.
    ///
    /// `Aarch64Emitter::mark_branch_overflow` does two things: trips a
    /// `debug_assert!` (active in debug/test builds) AND sets the sticky
    /// `overflowed` flag (always). `debug_assert!` is compiled out under
    /// `--release`, so the assertion this test can make differs per profile —
    /// same split as `aarch64::tests::assert_branch_overflow_detected`.
    #[test]
    fn emit_machine_code_bails_on_out_of_range_conditional_branch() {
        const SPAN: usize = (1 << 18) + 4; // > 2^18 instructions ⇒ > ±1 MiB

        let build = || {
            let mut insts = Vec::with_capacity(SPAN + 3);
            insts.push(Arm64Instruction::Cbz {
                rt: Arm64Register::X0,
                label: 1,
            });
            insts.resize(SPAN + 1, Arm64Instruction::Nop);
            insts.push(Arm64Instruction::Label(1));
            insts.push(Arm64Instruction::Ret);
            result_from_instructions(insts)
        };

        if cfg!(debug_assertions) {
            let prev = std::panic::take_hook();
            std::panic::set_hook(Box::new(|_| {}));
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                emit_machine_code(&build())
            }));
            std::panic::set_hook(prev);
            assert!(
                outcome.is_err(),
                "an out-of-range CBZ patch must trip the debug_assert! in debug builds"
            );
        } else {
            assert!(
                emit_machine_code(&build()).is_none(),
                "an out-of-range CBZ patch must set the sticky overflow flag and bail"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Division / remainder are refused — see the module header.
    // -----------------------------------------------------------------------

    #[test]
    fn backend_idiv_bails_to_interpreter() {
        // iload_0; iload_1; idiv; ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x6c, 0xac]);
        assert!(
            !result.success,
            "idiv must bail: its zero guard is BRK #1 (SIGTRAP), not ArithmeticException"
        );
        assert!(emit_machine_code(&result).is_none());
    }

    #[test]
    fn backend_ldiv_bails_to_interpreter() {
        // lload_0; lload_1; ldiv; lreturn
        let result = make_backend_with_method(2, 2, &[0x1e, 0x1f, 0x6d, 0xad]);
        assert!(
            !result.success,
            "ldiv must bail for the same reason as idiv"
        );
    }

    #[test]
    fn backend_irem_and_lrem_bail_to_interpreter() {
        // AArch64 SDIV by zero yields 0 (it does not trap), so the old
        // `a - (a/b)*b` lowering returned `a` for `a % 0` instead of throwing.
        for &(op, name) in &[(0x70u8, "irem"), (0x71u8, "lrem")] {
            let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
            assert!(
                !result.success,
                "{name} must bail: SDIV-by-zero silently yields 0 on AArch64"
            );
        }
    }

    /// The refusal must be specific to div/rem — the surrounding integer
    /// arithmetic still compiles.
    #[test]
    fn backend_imul_still_compiles_after_div_refusal() {
        // iload_0; iload_1; imul; ireturn
        let result = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x68, 0xac]);
        assert!(
            result.success,
            "imul must be unaffected by the div/rem bail"
        );
        assert!(emit_machine_code(&result).is_some());
    }

    /// Coverage guard for the module header's "39 unsupported opcodes" claim:
    /// every opcode of the object model must refuse the method. If someone
    /// implements one, this test tells them to update the header table.
    #[test]
    fn object_model_opcodes_are_all_unsupported() {
        // One representative per documented unsupported area, plus every
        // field/dispatch/allocation opcode (the ones whose absence defines
        // this backend's scope).
        let unsupported: &[u8] = &[
            0x2e, 0x32, 0x35, // iaload / aaload / saload
            0x4f, 0x53, 0x56, // iastore / aastore / sastore
            0xb2, 0xb3, 0xb4, 0xb5, // getstatic / putstatic / getfield / putfield
            0xb6, 0xb7, 0xb9, 0xba, // invokevirtual/special/interface/dynamic
            0xbb, 0xbc, 0xbd, 0xbe, // new / newarray / anewarray / arraylength
            0xbf, // athrow
            0xc0, 0xc1, // checkcast / instanceof
            0xc2, 0xc3, // monitorenter / monitorexit
            0xc4, 0xc5, 0xc8, // wide / multianewarray / goto_w
        ];
        for &op in unsupported {
            // Two operand bytes cover the widest of these; trailing bytes are
            // irrelevant because the arm bails before consuming them.
            let result = make_backend_with_method(2, 2, &[op, 0x00, 0x01, 0xb1]);
            assert!(
                !result.success,
                "opcode 0x{op:02x} unexpectedly compiled — update the coverage \
                 table in this module's header before landing that"
            );
        }
    }

    /// `invokestatic` has a match arm but `emit_invoke` always fails, so no
    /// method containing a call of any kind compiles on this backend.
    #[test]
    fn invokestatic_arm_exists_but_always_bails() {
        let result = make_backend_with_method(1, 1, &[0xb8, 0x00, 0x01, 0xb1]);
        assert!(
            !result.success,
            "there is no call-target resolution on this backend; invokestatic must bail"
        );
    }

    /// A loop back-edge must resolve to a real target, not the `B .`
    /// self-branch the lazy label discovery used to leave behind.
    ///
    /// ```text
    /// 0: iconst_0        0x03        // sum = 0
    /// 1: istore_1        0x3c
    /// 2: iload_1         0x1b   <-- back-edge target
    /// 3: iconst_1        0x04
    /// 4: iadd            0x60
    /// 5: istore_1        0x3c
    /// 6: goto -4         0xa7 ff fc  // back to pc 2
    /// ```
    ///
    /// (The `goto` is unconditional, so the loop never exits.)
    ///
    /// **This method no longer compiles at all** — see
    /// `loop_method_bails_no_safepoint_poll` and [`Arm64Backend::label_for_pc`]:
    /// a back-edge is refused because there is no safepoint poll to place on it.
    /// The original assertion ("a pure-arithmetic loop must compile") is
    /// therefore inverted here.
    ///
    /// The *encoder-level* guard the original test existed for — that a
    /// backward branch resolves to a real target rather than being left as the
    /// displacement-0 `B .` placeholder — is preserved below by driving
    /// `emit_machine_code` with a hand-built pseudo-op sequence containing a
    /// bound backward branch. That is a strictly stronger check on the piece
    /// that can still regress (the patch loop), and it survives the front-end
    /// policy change.
    #[test]
    fn backward_branch_resolves_to_its_target_not_itself() {
        // Front end: the loop is now refused outright.
        let code = [0x03, 0x3c, 0x1b, 0x04, 0x60, 0x3c, 0xa7, 0xff, 0xfc];
        let result = make_backend_with_method(2, 1, &code);
        assert!(
            !result.success,
            "a loop must bail — no safepoint poll exists for its back-edge"
        );

        // Encoder: a bound backward branch must still patch to a real negative
        // displacement, never to `B .`.
        let looped = result_from_instructions(vec![
            Arm64Instruction::Label(1),
            Arm64Instruction::Nop,
            Arm64Instruction::Nop,
            Arm64Instruction::B { label: 1 },
        ]);
        let bytes = emit_machine_code(&looped).expect("bound back-edge must encode");
        assert_eq!(bytes.len(), 3 * 4, "Label emits nothing; 3 real words");

        let mut last_b: Option<(usize, u32)> = None;
        for (i, w) in bytes.chunks_exact(4).enumerate() {
            let word = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            if word >> 26 == 0b000101 {
                last_b = Some((i, word));
            }
        }
        let (b_index, b_word) = last_b.expect("an unconditional B must be present");
        let imm26 = b_word & 0x03ff_ffff;
        assert_ne!(
            imm26, 0,
            "displacement 0 is `B .` — an infinite self-branch, the bug this guards"
        );
        // Sign-extend imm26 and confirm it points backwards, at a real
        // instruction inside the buffer.
        let signed = ((imm26 << 6) as i32) >> 6;
        assert_eq!(
            signed, -2,
            "B at word 2 targeting word 0 is a -2 displacement"
        );
        let target = (b_index as i64 + signed as i64) * 4;
        assert_eq!(target, 0, "back-edge target must be the bound label");
        assert!(
            (target as usize) < bytes.len(),
            "back-edge target must land inside the emitted buffer"
        );
    }

    /// A compiled method still carries NO oop maps -- and the reason is no
    /// longer the writer.
    ///
    /// `emit_oop_map_for_safepoint` is correct now (see
    /// `the_oop_map_pc_is_the_encoders_byte_offset`), but it still has no
    /// production call site, because this backend lowers no allocation, no call
    /// and no monitor and refuses back edges -- so a compiled method contains no
    /// GC-capable point to record a map AT. If this fires, a safepoint was
    /// added: that is the good outcome, and the module header's
    /// "Safety-critical gaps" section needs updating with it.
    #[test]
    fn compiled_methods_carry_no_oop_maps() {
        // aload_0; areturn — the reference path that *does* call
        // `mark_top_operand_as_oop`.
        let result = make_backend_with_method(1, 1, &[0x2a, 0xb0]);
        assert!(result.success);
        assert!(
            result.pending_oop_maps.is_empty(),
            "no safepoint is emitted on this backend, so nothing calls the map              writer; if this fires, a safepoint landed -- update the header"
        );
        // ...and the resolved side agrees, which is the half a GC would read.
        let (_code, maps) =
            emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert!(maps.is_empty());
    }

    // -----------------------------------------------------------------------
    // aarch64 parity audit (2026-08-01) — fail-closed gates
    // -----------------------------------------------------------------------

    // -----------------------------------------------------------------------
    // Safepoint polls (opt-in, `CRATONVM_JIT_ARM64_SAFEPOINTS`)
    // -----------------------------------------------------------------------

    /// Build a backend with polls on and plausible helper addresses.
    fn poll_backend() -> Arm64Backend {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(true);
        // SAFETY: plain struct of `usize` addresses.
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.safepoint_flag_addr = 0x1234_5678_9AB0;
        h.safepoint_slow_path = 0x7FFF_0000_1000;
        b.set_helpers(h);
        b
    }

    /// THE WIDTH OF THE FLAG READ, asserted as an exact instruction word.
    ///
    /// The safepoint flag is a Rust `AtomicBool` -- ONE byte -- and the
    /// `GcBarrier` fields that follow it (`gc_generation: AtomicU64`, ...) are
    /// not zero. Reading it with the 64-bit `ldr_imm` would fold those bytes
    /// into the `CBZ` and make the poll fire on every iteration once the
    /// generation counter is nonzero. A host that cannot execute aarch64 has to
    /// catch that by ENCODING, so this pins the literal word rather than
    /// merely asserting "an Ldrb was emitted".
    #[test]
    fn the_poll_reads_one_byte_and_the_encoding_says_so() {
        use crate::aarch64::{Aarch64Emitter, Reg};
        let mut e = Aarch64Emitter::new();
        e.ldrb_imm(Reg::X17, Reg::X16, 0);
        let word = u32::from_le_bytes(e.code()[0..4].try_into().unwrap());
        // LDRB Wt, [Xn, #0] = 0x39400000 | (Rn << 5) | Rt
        assert_eq!(
            word, 0x3940_0211,
            "LDRB W17, [X16] must encode as 0x39400211; got {word:#010x}"
        );
        // The 64-bit form is a DIFFERENT instruction -- the control that makes
        // the assertion above mean something.
        let mut e64 = Aarch64Emitter::new();
        e64.ldr_imm(Reg::X17, Reg::X16, 0);
        let w64 = u32::from_le_bytes(e64.code()[0..4].try_into().unwrap());
        assert_ne!(word, w64, "byte and doubleword loads must differ");
    }

    /// The entry poll's shape, read off the pseudo-op stream.
    #[test]
    fn the_entry_poll_has_the_expected_shape() {
        let mut b = poll_backend();
        // `return void` -- no operand stack, so the poll spills nothing.
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success, "the method must still compile");
        let ops = &result.instructions;
        let ldrb = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .expect("the poll must read the flag with a BYTE load");
        assert!(
            matches!(ops[ldrb - 1], Arm64Instruction::MovImm { .. }),
            "the flag address must be materialized right before the load"
        );
        assert!(
            matches!(ops[ldrb + 1], Arm64Instruction::Cbz { .. }),
            "a clear flag must branch PAST the call, not into it"
        );
        assert!(
            ops[ldrb..]
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Blr { .. })),
            "the poll must call the slow path"
        );
        // The CBZ target must be bound AFTER the BLR, or the poll skips
        // nothing -- or worse, branches backwards.
        let Arm64Instruction::Cbz { label, .. } = ops[ldrb + 1] else {
            unreachable!()
        };
        let blr = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .unwrap();
        let bound = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Label(l) if *l == label))
            .expect("the skip label must be bound");
        assert!(
            bound > blr,
            "the skip target must be past the call ({bound} vs {blr})"
        );
    }

    /// OFF is byte-identical to before, and still refuses loops.
    ///
    /// The negative control for every assertion above: without it they would
    /// pass just as well if the poll were emitted unconditionally.
    #[test]
    fn safepoints_off_emits_no_poll_and_still_refuses_loops() {
        let mut off = Arm64Backend::new();
        off.set_safepoints_enabled(false);
        let a = off.compile_method(0, 0, 4, &[0xb1]);
        assert!(a.success);
        assert!(
            !a.instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
            "no poll may be emitted with the switch off"
        );
        // The SAME loop `a_loop_compiles_with_a_poll_at_its_header` compiles
        // with the switch on, so this is a true A/B on one input: with polls
        // off the pre-existing safety gate still refuses it, because a compiled
        // loop containing no poll is a region a stop-the-world request can
        // never interrupt.
        //
        // (An earlier draft of this used a bare `goto -3` at pc 0. That target
        // is NEGATIVE, wraps when cast to `usize`, and so never looked like a
        // back edge at all -- the test passed for the wrong reason until the
        // loop above was written to compare against.)
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let mut off2 = Arm64Backend::new();
        off2.set_safepoints_enabled(false);
        let b = off2.compile_method(1, 0, 4, &code);
        assert!(
            !b.success,
            "with polls off, a backward branch must still refuse the method"
        );
    }

    /// A helper table with no flag address emits nothing, switch or no switch.
    ///
    /// The same optional-helper contract x64 has: an unwired build must not
    /// call through a null pointer, and must be byte-identical to before.
    #[test]
    fn an_unwired_helper_table_emits_no_poll() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(true); // switch ON, helpers absent
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert!(
            !result
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::Ldrb { .. })),
            "no flag address means no poll code at all"
        );
    }

    /// With polls on, a LOOP compiles -- and gets a poll at its header.
    ///
    /// This is the capability the refusal was trading away: `label_for_pc`
    /// refused every backward branch precisely because a compiled loop with no
    /// poll in it is a region a stop-the-world request can never interrupt.
    #[test]
    fn a_loop_compiles_with_a_poll_at_its_header() {
        // 0: iconst_0   1: istore_0   2: iinc 0,1   5: goto 2
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let mut b = poll_backend();
        let result = b.compile_method(1, 0, 4, &code);
        assert!(
            result.success,
            "a loop must compile once its header can carry a poll"
        );
        let polls = result
            .instructions
            .iter()
            .filter(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .count();
        assert_eq!(polls, 2, "expected an entry poll and a loop-header poll");
        // And it encodes: the backward branch patches to a NEGATIVE
        // displacement, which nothing on this backend had exercised before.
        assert!(
            emit_machine_code(&result).is_some(),
            "the loop must survive encoding, back edge and all"
        );
    }

    /// The poll leaves the compile-time spill model exactly as it found it.
    ///
    /// `spill_map` decides whether `pop_operand` emits a reload. The poll's
    /// spill sits INSIDE the `CBZ`-skipped block, so on the not-taken path
    /// (flag clear -- the common case) those stores never execute, and an entry
    /// left behind would make a later `pop_operand` reload a frame slot nothing
    /// wrote. Asserting the model is unchanged is how a non-executing host
    /// checks that.
    #[test]
    fn the_poll_restores_the_spill_model() {
        let mut b = poll_backend();
        b.frame = Some(Arm64FrameLayout::compute(0, 8, &[]));
        b.operand_stack = vec![Arm64Register::X9, Arm64Register::X10];
        b.operand_stack_oop_marks = vec![true, false];
        assert!(b.spill_map.is_empty());

        b.emit_safepoint_poll(false);

        assert!(!b.failed, "the poll must not refuse this frame");
        assert!(
            b.spill_map.is_empty(),
            "the poll must remove every spill entry it added; left: {:?}",
            b.spill_map
        );
        // The oop map DID record the reference operand's slot: the spill is
        // what makes it nameable, and the map is taken at the call.
        assert_eq!(
            b.pending_oop_maps.len(),
            1,
            "the safepoint must record a map naming the live reference"
        );
        assert_eq!(
            b.pending_oop_maps[0].frame_slot_offsets.len(),
            1,
            "only the marked (reference) operand belongs in the map"
        );
    }

    /// A poll whose spill has no reserved slot refuses the method.
    ///
    /// The alternative is a live reference sitting in a caller-saved register
    /// across a CALL, which is the exact hazard the spill exists for.
    #[test]
    fn a_poll_that_cannot_place_its_spill_refuses() {
        let mut b = poll_backend();
        b.frame = Some(Arm64FrameLayout::compute(0, 0, &[]));
        b.operand_stack = vec![Arm64Register::X9];
        b.operand_stack_oop_marks = vec![true];
        b.emit_safepoint_poll(false);
        assert!(
            b.failed,
            "a spill with nowhere to go must fail the method closed"
        );
    }


    /// The operand spill area and the frame-homed locals must NOT overlap.
    ///
    /// FIXED 2026-09-03; this failed as written.
    ///
    /// `spill_index_for` numbers a frame-homed local by how many locals BEFORE
    /// it also lack a register, so those locals occupy spill indices
    /// `0..gpr_spills`. `alloc_scratch` numbers an operand slot
    /// `frame.num_reg_locals + depth` -- and `num_reg_locals` is how many
    /// locals got a REGISTER, which is the complement of that count, not the
    /// end of it.
    ///
    /// `num_spills = gpr_spills + max_stack` is the tell: the frame is sized as
    /// if operands began at `gpr_spills`, which is exactly what makes the last
    /// operand slot the last reserved word. Basing them at `num_reg_locals`
    /// instead either ALIASES a local (fewer locals got registers) or runs PAST
    /// the reserved area into the callee-save slots (more did).
    #[test]
    fn operand_spill_slots_do_not_alias_frame_homed_locals() {
        let mut b = Arm64Backend::new();
        // Four locals: local 0 register-homed, locals 1..3 frame-homed.
        b.local_regs = vec![Some(Arm64Register::X19), None, None, None];
        b.float_local_regs = vec![None, None, None, None];
        let gpr_spills = 3usize;
        let max_stack = 4usize;
        b.frame = Some(Arm64FrameLayout::compute(
            4,
            gpr_spills + max_stack,
            &[Arm64Register::X19],
        ));

        let local_slots: Vec<usize> = (1..4).map(|i| b.spill_index_for(i)).collect();
        assert_eq!(
            local_slots,
            vec![0, 1, 2],
            "frame-homed locals occupy 0..gpr_spills"
        );

        assert_eq!(b.frame.as_ref().unwrap().num_reg_locals, 1);
        // The base both spillers now share.
        let operand_slot_0 = b.local_spill_count();
        assert_eq!(
            operand_slot_0, gpr_spills,
            "the operand area must begin where the locals end"
        );

        assert!(
            !local_slots.contains(&operand_slot_0),
            "operand depth 0 landed on spill slot {operand_slot_0}, which is \
             also a frame-homed local's slot -- the operand stack and the \
             locals share frame words"
        );
        // The OTHER direction: with more locals register-homed than not, the
        // old base ran past the reserved area into the callee-save slots,
        // overwriting a saved register the epilogue restores to the caller.
        let mut b2 = Arm64Backend::new();
        b2.local_regs = vec![
            Some(Arm64Register::X19),
            Some(Arm64Register::X20),
            Some(Arm64Register::X21),
            None,
        ];
        b2.float_local_regs = vec![None; 4];
        let saved = [Arm64Register::X19, Arm64Register::X20, Arm64Register::X21];
        let num_spills = 1 + max_stack; // gpr_spills(1) + max_stack
        b2.frame = Some(Arm64FrameLayout::compute(4, num_spills, &saved));
        let last = b2.local_spill_count() + (max_stack - 1);
        let old_last = b2.frame.as_ref().unwrap().num_reg_locals + (max_stack - 1);
        assert!(
            last < num_spills,
            "the deepest operand slot ({last}) must stay inside the reserved              area ({num_spills}); the old base put it at {old_last}"
        );
        assert!(old_last >= num_spills, "the old base really did overrun");
    }

    /// A frame for `n` locals of which `reg_homed` have registers.
    fn locals_frame(b: &mut Arm64Backend, reg: &[Option<Arm64Register>], max_stack: usize) {
        b.local_regs = reg.to_vec();
        b.float_local_regs = vec![None; reg.len()];
        let saved: Vec<Arm64Register> = reg.iter().flatten().copied().collect();
        let gpr_spills = reg.iter().filter(|r| r.is_none()).count();
        b.max_stack = max_stack;
        b.frame = Some(Arm64FrameLayout::compute(
            reg.len(),
            gpr_spills + max_stack + saved.len(),
            &saved,
        ));
    }

    /// THE POINT OF THIS WHOLE CHANGE: a REGISTER-HOMED reference local is
    /// stored to a frame home, named in the map, and reloaded after the call.
    ///
    /// X19-X28 are callee-saved, so the value survives the call on its own --
    /// but it survives inside the CALLEE's saved-register area, where only the
    /// conservative walk can see it, and a conservative walk marks without
    /// being able to REWRITE. A relocating collector therefore could not move
    /// an object whose only root was a register local. The store makes it a
    /// nameable, rewritable root; the reload is what carries a moved object's
    /// new address back into the register.
    #[test]
    fn a_register_homed_reference_local_is_spilled_named_and_reloaded() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        // The dataflow says local 0 holds a reference at this pc.
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![true];

        let home = b
            .safepoint_home_for_reg_local(0)
            .expect("a home must be reserved for a register-homed local");
        b.emit_safepoint_poll(false);
        assert!(!b.failed);

        let stored = b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Str { rt, rn, offset }
                     if *rt == Arm64Register::X19
                     && *rn == Arm64Register::FP
                     && *offset == home)
        });
        assert!(stored, "the register local must be stored to its home");

        assert_eq!(b.pending_oop_maps.len(), 1);
        let named: Vec<i16> = b.pending_oop_maps[0].frame_slot_offsets.clone();
        assert!(
            named.contains(&(home as i16)),
            "the map must name the home ({home}); named {named:?}"
        );

        let reloaded = b.buffer.instructions().iter().any(|i| {
            matches!(i, Arm64Instruction::Ldr { rt, rn, offset }
                     if *rt == Arm64Register::X19
                     && *rn == Arm64Register::FP
                     && *offset == home)
        });
        assert!(
            reloaded,
            "the register must be reloaded, or a relocated object's new address \
             never reaches the running code"
        );
    }

    /// A FRAME-HOMED reference local is named where it already lives -- no
    /// store, because there is nothing to move.
    #[test]
    fn a_frame_homed_reference_local_is_named_without_a_spill() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[None], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![true];

        let frame_off = b.frame.as_ref().unwrap().spill_offset; // slot 0
        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0]
                .frame_slot_offsets
                .contains(&(frame_off as i16)),
            "a frame-homed local must be named at its own slot"
        );
        assert!(
            !b.buffer.instructions().iter().any(|i| {
                matches!(i, Arm64Instruction::Str { offset, .. } if *offset == frame_off)
            }),
            "a frame-homed local is already where the GC reads it; storing it \
             again would be pure cost"
        );
    }

    /// A local the dataflow does NOT call a reference is not named.
    ///
    /// The precision control. Naming a primitive would hand a relocating
    /// collector a word to rewrite that is not a pointer -- and an `int` can
    /// coincidentally hold a value `is_object_address` accepts, which is
    /// exactly why this uses the flow-sensitive "must be oop" dataflow rather
    /// than the whole-method `find_reference_locals` approximation.
    #[test]
    fn a_non_reference_local_is_not_named() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0]; // reached, and NOT an oop
        b.local_oop_reached = vec![true];
        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert!(
            b.pending_oop_maps.is_empty(),
            "a safepoint with no live reference records no map"
        );
        assert!(
            !b.buffer.instructions().iter().any(|i| {
                matches!(i, Arm64Instruction::Str { rt, .. } if *rt == Arm64Register::X19)
            }),
            "a primitive local must not be spilled either"
        );
    }

    /// An UNREACHED pc makes no claim, rather than claiming "no oops".
    ///
    /// The dataflow does not reach pcs only an exception edge arrives at, and
    /// it is empty above 64 locals. Reading either as "nothing live" is how a
    /// collector loses a root, so both fall back to naming nothing and leaving
    /// the frame to the conservative scan.
    #[test]
    fn an_unreached_pc_names_nothing_rather_than_claiming_emptiness() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.cur_bytecode_pc = 0;
        b.local_oop_masks = vec![0b1];
        b.local_oop_reached = vec![false]; // never reached
        assert!(b.oop_locals_at_current_pc(false).is_none());
        b.emit_safepoint_poll(false);
        assert!(!b.failed, "no claim is not a refusal of the method");
        assert!(b.pending_oop_maps.is_empty());
    }

    /// The safepoint homes sit past both the locals and the operand area.
    ///
    /// Three regions share one spill area, and an overlap would have the GC
    /// read a word two of them write. The frame is extended by exactly the
    /// number of register-homed locals when polls are on, so a home is always
    /// inside the reservation and never on a local's or an operand's slot.
    #[test]
    fn safepoint_homes_do_not_collide_with_locals_or_operands() {
        let mut b = poll_backend();
        let regs = [Some(Arm64Register::X19), None, Some(Arm64Register::X20), None];
        let max_stack = 3usize;
        locals_frame(&mut b, &regs, max_stack);
        let frame_words = b.frame.as_ref().unwrap().num_spills;

        let local_slots: Vec<i32> = (0..regs.len())
            .filter(|&i| regs[i].is_none())
            .map(|i| b.spill_index_for(i) as i32)
            .collect();
        let operand_slots: Vec<i32> = (0..max_stack)
            .map(|d| (b.local_spill_count() + d) as i32)
            .collect();
        let home_slots: Vec<i32> = (0..regs.len())
            .filter(|&i| regs[i].is_some())
            .map(|i| {
                let off = b.safepoint_home_for_reg_local(i).unwrap();
                (off - b.frame.as_ref().unwrap().spill_offset) / 8
            })
            .collect();

        for h in &home_slots {
            assert!(
                !local_slots.contains(h) && !operand_slots.contains(h),
                "home slot {h} collides (locals {local_slots:?}, operands \
                 {operand_slots:?})"
            );
            assert!(
                (*h as usize) < frame_words,
                "home slot {h} is outside the reserved area ({frame_words})"
            );
        }
        assert_eq!(home_slots.len(), 2, "one home per register-homed local");
    }

    /// The ENTRY poll names the reference PARAMETERS.
    ///
    /// The prologue runs before the walk, so there is no bci to look up; the
    /// live oops there are exactly the reference parameters. Without the
    /// descriptor seed this mask is 0 and a reference parameter that is never
    /// `astore`d is never named -- covered by the conservative scan, but not
    /// precisely, which is the difference a relocating collector cares about.
    #[test]
    fn the_entry_poll_names_the_reference_parameters() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19), None], 2);
        // static (Ljava/lang/Object;I)V -> local 0 is a reference parameter.
        b.set_method_descriptor("(Ljava/lang/Object;I)V", true);
        assert_eq!(b.param_oop_mask & 1, 1, "param 0 is a reference");

        let home = b.safepoint_home_for_reg_local(0).unwrap();
        b.emit_safepoint_poll(true); // entry
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0]
                .frame_slot_offsets
                .contains(&(home as i16)),
            "the entry poll must name the reference parameter"
        );

        // The control: with no descriptor seeded, it names nothing.
        let mut b2 = poll_backend();
        locals_frame(&mut b2, &[Some(Arm64Register::X19), None], 2);
        b2.emit_safepoint_poll(true);
        assert!(b2.pending_oop_maps.is_empty());
    }
    /// A published artifact carries its resolved oop maps.
    ///
    /// The publication path used to build its `CompiledMethod` with
    /// `CompiledMethod::new(buf)` and never transfer the backend's maps -- so
    /// even a correct writer would have produced nothing a GC could read. That
    /// path sits behind `#[cfg(target_arch = "aarch64")]` and is therefore not
    /// compiled on an x86-64 host at all, which is how it stayed that way; the
    /// logic now lives in `publish_compiled_method`, which this exercises here.
    ///
    /// The buffer is allocated and finalized but never CALLED: these are aarch64
    /// bytes and the test host is x86-64. This asserts the metadata plumbing,
    /// which is the half that was broken.
    #[test]
    fn a_published_artifact_carries_its_resolved_oop_maps() {
        let mut result = result_from_instructions(vec![
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            },
            Arm64Instruction::Ret,
        ]);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![32],
        }];

        let expected_pc = emit_machine_code(&result_from_instructions(vec![
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            },
        ]))
        .expect("the prefix encodes")
        .len() as u32;

        let mut cm = publish_compiled_method(&result).expect("the artifact publishes");
        assert!(
            cm.has_precise_oop_maps(),
            "the published artifact must carry the maps -- this is the transfer              that was missing"
        );
        assert_eq!(cm.oop_maps.len(), 1);
        assert_eq!(cm.oop_maps[0].native_pc_offset, expected_pc);
        assert_eq!(cm.oop_maps[0].frame_slot_offsets, vec![32]);
        // The claim that would be false: no shadow stack, no relocation
        // support, and register oops covered only conservatively.
        assert!(!cm.oop_maps[0].moving_young_coverage_complete);
        assert!(
            !cm.fully_oop_covered,
            "this backend has no safepoint-id slot, so it may never claim full              precise coverage"
        );
        // And `find_oop_map_for_pc` -- the reader that applies here, since there
        // is no safepoint-id to select by -- finds it at that PC.
        assert!(cm.find_oop_map_for_pc(expected_pc).is_some());
    }

    /// THE BUG THIS FIXES, pinned as a difference.
    ///
    /// `emit_oop_map_for_safepoint` used to key its map as
    /// `instruction_count * 4`, and the 2026-08-01 parity audit made it fail
    /// the method closed rather than let a caller inherit that, noting the fix
    /// was "to key oop maps off the *encoder's* byte offset ... then delete
    /// this guard".
    ///
    /// The stream below is built from exactly the pseudo-ops that break the old
    /// arithmetic: a `Comment` and a `Label` that emit NOTHING, a wide `MovImm`
    /// that expands to several words, and a `ConstantPoolEntry` that emits 8
    /// bytes. The map's PC must be where the encoder actually put the following
    /// instruction -- and must NOT be `index * 4`.
    #[test]
    fn the_oop_map_pc_is_the_encoders_byte_offset() {
        let prefix = vec![
            Arm64Instruction::Comment("emits nothing".to_string()),
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            },
            Arm64Instruction::Label(1),
            Arm64Instruction::ConstantPoolEntry {
                label: 2,
                value: 0xDEAD_BEEF,
            },
        ];
        // The safepoint sits before the instruction at index 4.
        let sp_index = prefix.len() as u32;
        let mut instructions = prefix.clone();
        instructions.push(Arm64Instruction::Ret);

        // The expected byte offset, computed by ENCODING THE PREFIX rather than
        // by hardcoding any instruction's width -- so this test cannot drift
        // with the encoder.
        let expected = emit_machine_code(&result_from_instructions(prefix))
            .expect("the prefix encodes")
            .len() as u32;

        let mut result = result_from_instructions(instructions);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: sp_index,
            frame_slot_offsets: vec![16, 24],
        }];

        let (_code, maps) =
            emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert_eq!(maps.len(), 1);
        assert_eq!(
            maps[0].native_pc_offset, expected,
            "the map must be keyed by the encoder's byte offset"
        );
        assert_eq!(maps[0].frame_slot_offsets, vec![16, 24]);

        // And the old arithmetic really would have been wrong here -- without
        // this the test would pass on a stream where the two happen to agree.
        assert_ne!(
            expected,
            sp_index * 4,
            "this stream must actually distinguish the encoder offset from the              pseudo-op count; pick different pseudo-ops if it stops doing so"
        );
    }

    /// The writer records a PENDING map, and refuses rather than truncate.
    #[test]
    fn the_oop_map_writer_records_a_pending_map() {
        let mut backend = Arm64Backend::new();
        assert!(!backend.failed);
        // Nothing marked as an oop yet: an empty map is not recorded at all.
        backend.emit_oop_map_for_safepoint();
        assert!(!backend.failed, "the writer no longer fails the method closed");
        assert!(
            backend.pending_oop_maps.is_empty(),
            "a safepoint with no live reference records nothing"
        );
    }

    /// Fail closed on a pending map the encoder cannot place.
    ///
    /// A `pseudo_index` past the end of the stream cannot be resolved to a byte
    /// offset, and a GUESSED PC is the failure mode this whole two-phase
    /// arrangement exists to prevent -- so the method is discarded.
    #[test]
    fn an_unplaceable_oop_map_discards_the_method() {
        let mut result = result_from_instructions(vec![Arm64Instruction::Ret]);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 99,
            frame_slot_offsets: vec![8],
        }];
        assert!(
            emit_machine_code_with_oop_maps(&result).is_none(),
            "an oop map that cannot be placed must discard the method"
        );
        // The plain encoder is unaffected: it publishes no map, so an
        // unresolvable one cannot mislead anything through that path.
        assert!(emit_machine_code(&result).is_some());
    }

    /// A safepoint at the very END of the stream still resolves, via the
    /// trailing sentinel in `pseudo_offsets`.
    #[test]
    fn a_safepoint_at_the_end_of_the_stream_resolves_to_the_code_length() {
        let instructions = vec![Arm64Instruction::Ret];
        let mut result = result_from_instructions(instructions);
        result.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 1,
            frame_slot_offsets: vec![8],
        }];
        let (code, maps) =
            emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].native_pc_offset as usize, code.len());
    }

    /// A frame big enough to step over the stack guard page must bail, because
    /// this backend emits no stack bang (x86-64 does — `emit_stack_bang_before_frame_alloc`).
    #[test]
    fn oversized_frame_bails_no_stack_bang() {
        // max_stack = 600 → 600 spill slots → ~4.8 KiB frame, past one guard page.
        let mut big = Arm64Backend::new();
        let result = big.compile_method(0, 0, 600, &[0xb1]);
        assert!(
            result.frame.frame_size >= 4096,
            "test precondition: frame must exceed a guard page (got {})",
            result.frame.frame_size
        );
        assert!(
            !result.success,
            "a frame larger than the guard page must bail: `SUB SP, SP, #frame` \
             with no bang can jump clean past the guard"
        );

        // Negative control: an ordinary small frame still compiles, so the gate
        // is a size threshold and not a blanket refusal.
        let mut small = Arm64Backend::new();
        let ok = small.compile_method(0, 0, 4, &[0xb1]);
        assert!(ok.frame.frame_size < 4096);
        assert!(ok.success, "a normal frame must still compile");
    }

    /// The one ADD/SUB-immediate shape with no sound encoding (SP operand,
    /// magnitude > 0xFFF, not 4 KiB-aligned) must report failure, not emit a
    /// `BRK` and claim success. A BRK raises SIGTRAP, which nothing in the VM
    /// converts into anything — the process just dies on first execution.
    #[test]
    fn addsub_imm_safe_refuses_unencodable_sp_adjustment() {
        use crate::aarch64::{Aarch64Emitter, Reg};

        let mut e = Aarch64Emitter::new();
        assert!(
            !emit_addsub_imm_safe(&mut e, Reg::SP, Reg::SP, 5000, true),
            "an unencodable SP adjustment must be refused"
        );

        // A 4 KiB-aligned SP adjustment of the same magnitude class IS
        // encodable (shifted-by-12 immediate form) and must still succeed.
        let mut e2 = Aarch64Emitter::new();
        assert!(
            emit_addsub_imm_safe(&mut e2, Reg::SP, Reg::SP, 8192, true),
            "a shifted-12 encodable SP adjustment must still lower"
        );
        assert_eq!(e2.code().len(), 4, "and it lowers to a single instruction");
    }

    /// `emit_machine_code` must propagate that refusal instead of publishing a
    /// body whose SP adjustment silently did not happen.
    #[test]
    fn emit_machine_code_bails_on_unencodable_sp_immediate() {
        let result = result_from_instructions(vec![
            Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: 5000,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(
            emit_machine_code(&result).is_none(),
            "an unencodable SP adjustment must bail the method, not emit a BRK"
        );

        // Negative control: the encodable form still produces code.
        let ok = result_from_instructions(vec![
            Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: 8192,
            },
            Arm64Instruction::Ret,
        ]);
        assert!(emit_machine_code(&ok).is_some());
    }
    // =====================================================================
    // The category-dependent stack shuffles.
    //
    // This backend keeps TWO simulated operand stacks — `operand_stack` for
    // int/long/reference and `float_operand_stack` for float/double — and its
    // shuffle arms popped a FIXED number of entries from the first one. Both
    // assumptions are wrong in general, and the x64 `dup2_x2` page
    // (`dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend`) raised
    // exactly this as a question it did not answer: whether that backend's
    // unconditional four-pop was live here too.
    //
    // It was, and so were four more arms. These tests read the simulated
    // stack directly after the walk, because entry COUNT is what the defect
    // was about: a category-2 value is one entry here and two JVM slots, so a
    // fixed pop either leaves the shuffle short or reaches past it into
    // entries the shuffle must not touch.
    // =====================================================================

    /// `[int, int, long, long] dup2_x2` is JVMS FORM 4 — two entries, not
    /// four. The old unconditional four-pop swallowed the two `int`s beneath
    /// and pushed a six-entry stack in the wrong order; the `int`s below the
    /// shuffle must come through untouched.
    #[test]
    fn dup2_x2_form4_leaves_the_entries_beneath_it_alone() {
        // iconst_0, iconst_1, lconst_0, lconst_1
        let prefix = [0x03u8, 0x04, 0x09, 0x0a];
        let mut control = Arm64Backend::new();
        assert!(control.compile_method(4, 0, 8, &prefix).success);
        let before = control.operand_stack.clone();
        assert_eq!(before.len(), 4, "control: four values, four entries");

        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e); // dup2_x2
        assert!(
            backend.compile_method(4, 0, 8, &code).success,
            "FORM 4 dup2_x2 must lower"
        );
        let after = &backend.operand_stack;
        assert_eq!(after.len(), 5, "[v2, v1] -> [v1, v2, v1] over two ints");
        assert_eq!(
            &after[..2],
            &before[..2],
            "the two ints below the shuffle must not move"
        );
        assert_eq!(after[3], before[2], "v2 stays in place");
        assert_eq!(after[4], before[3], "v1 stays on top");
        assert!(
            after[2] != after[4],
            "the inserted copy is a fresh register, not the original"
        );
    }

    /// `[long, int, int] dup2_x2` is FORM 3 — the copy goes three entries
    /// down, not four. The old arm popped a fourth entry that did not exist.
    #[test]
    fn dup2_x2_form3_duplicates_two_entries_over_one() {
        let prefix = [0x09u8, 0x03, 0x04]; // lconst_0, iconst_0, iconst_1
        let mut control = Arm64Backend::new();
        assert!(control.compile_method(4, 0, 8, &prefix).success);
        let before = control.operand_stack.clone();

        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e);
        assert!(
            backend.compile_method(4, 0, 8, &code).success,
            "FORM 3 dup2_x2 must lower"
        );
        let after = &backend.operand_stack;
        assert_eq!(after.len(), 5, "[v3, v2, v1] -> [v2, v1, v3, v2, v1]");
        assert_eq!(after[2], before[0], "the long stays where it was");
        assert_eq!(after[3], before[1]);
        assert_eq!(after[4], before[2]);
    }

    /// FORM 1, the only form the old arm handled: four category-1 entries.
    #[test]
    fn dup2_x2_form1_still_lowers() {
        let prefix = [0x03u8, 0x04, 0x05, 0x06]; // iconst_0..3
        let mut backend = Arm64Backend::new();
        let mut code = prefix.to_vec();
        code.push(0x5e);
        assert!(backend.compile_method(4, 0, 8, &code).success);
        assert_eq!(backend.operand_stack.len(), 6);
    }

    /// A `double` operand lives on `float_operand_stack`, so a shuffle that
    /// pops `operand_stack` for it takes an unrelated value. Refuse.
    #[test]
    fn a_floating_point_operand_refuses_the_shuffle_rather_than_moving_the_wrong_stack() {
        for (name, code) in [
            // The int stack has enough entries here that the old arms did NOT
            // underflow — they duplicated `iconst_1` and left the float where
            // it was, silently. Without these two cases the rest of this test
            // passes against the pre-fix backend for the wrong reason (an
            // accidental underflow), which is no test at all.
            (
                "dup of a float over two ints",
                vec![0x03u8, 0x04, 0x0b, 0x59],
            ),
            ("pop of a float over two ints", vec![0x03, 0x04, 0x0b, 0x57]),
            ("dup of a double", vec![0x0e, 0x59]),
            ("dup2 of a double", vec![0x0e, 0x5c]),
            ("dup2_x2 of two doubles", vec![0x0e, 0x0f, 0x5e]),
            ("dup_x1 with a float below", vec![0x0b, 0x03, 0x5a]),
            ("swap with a float below", vec![0x0b, 0x03, 0x5f]),
            ("pop of a float", vec![0x0b, 0x57]),
        ] {
            let mut backend = Arm64Backend::new();
            assert!(
                !backend.compile_method(4, 0, 8, &code).success,
                "{name} must refuse the method, not shuffle the integer stack"
            );
        }
    }

    /// `pop2` over a single category-2 value pops ONE entry here. The old arm
    /// popped two, discarding whatever the `long` was sitting on.
    #[test]
    fn pop2_over_a_long_discards_one_entry_not_two() {
        // iconst_0, lconst_0, pop2 -> the int must survive.
        let mut backend = Arm64Backend::new();
        assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x58]).success);
        assert_eq!(
            backend.operand_stack.len(),
            1,
            "pop2 of a category-2 value leaves the int beneath it"
        );
    }

    /// `dup2` over a single category-2 value duplicates ONE entry. The old arm
    /// duplicated the unrelated value beneath the `long` as well — the same
    /// miscompile x64 was fixed for.
    #[test]
    fn dup2_over_a_long_duplicates_one_entry_not_two() {
        let mut backend = Arm64Backend::new();
        assert!(backend.compile_method(4, 0, 8, &[0x03, 0x09, 0x5c]).success);
        assert_eq!(
            backend.operand_stack.len(),
            3,
            "[int, long] -> [int, long, long]"
        );
    }

    /// When the width analysis cannot type the operands, the method is
    /// refused rather than shuffled on a guess. `dup2_x2` at pc 0 has no
    /// operands at all.
    #[test]
    fn an_untypeable_shuffle_refuses_the_method() {
        let mut backend = Arm64Backend::new();
        assert!(!backend.compile_method(4, 0, 8, &[0x5e]).success);
    }
}
