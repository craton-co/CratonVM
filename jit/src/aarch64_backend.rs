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
//! `bug-commonsmath-accuratemathtest-psquarepercentiletest-interpreter-throughput-cliff-20260816-FIXED.md`
//! and
//! `dup2_x2-is-scan-admitted-but-lowered-by-neither-x64-backend-20260817-FIXED.md`.
//! `x64::tests::scan_admitted_opcodes_are_lowered_or_declared` now fails if a
//! scan-admitted opcode ever loses its x64 arm again.)
//!
//! **The operand stack is ONE typed stack, and each entry records where its
//! value is (2026-09-12).** An [`Operand`] carries its kind (`I32`, `I64`,
//! `Ref`, `F32`, `F64`) and its location: a scratch register, or the frame word
//! reserved for its DEPTH. There used to be two stacks of bare registers -- one
//! for int/long/reference, one for float/double -- plus a `register -> spill
//! slot` map, and that design produced three separate miscompiles: the
//! allocator wrapped onto a live register and popping the new value reloaded
//! the old one (`a - (b+1+2+3+4)` was -4 for `(100, 5)`); the float allocator
//! round-robined V0-V7 with no liveness check at all; and the shuffles could
//! only refuse any form involving a float. Every shuffle now resolves its JVMS
//! form from the entries' categories (a `long` is one entry and two JVM
//! slots) and refuses only the forms the JVMS does not define. At every branch
//! each live entry is put in its depth slot, and each branch target rebuilds
//! the model from the shape recorded for it, so a value on the stack across a
//! merge (`c ? x : y`) arrives from both paths in the same place.
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
//!   **The safepoint-id slot landed 2026-09-04**, and with it the frame-base
//!   publication it is useless without. Each poll stamps its site's bci (or
//!   `ENTRY_POLL_BC_PC`) into a reserved frame word, the prologue stamps
//!   `SP_ID_UNSET_BC_PC` there first so an uninitialised slot cannot read as a
//!   valid id, every map carries the same value as its `bytecode_pc`, and
//!   `emit_frame_record` publishes FP through `helpers.frame_record` so the
//!   runtime can locate the frame to read it. The collector can therefore
//!   select the map for the site a frame is ACTUALLY standing at
//!   (`find_oop_map_for_safepoint_id`) rather than a union over the method.
//!
//!   **`fully_oop_covered` IS COMPUTED as of 2026-09-04**, from four terms
//!   each of which can sink it: an id slot exists; at least one safepoint was
//!   emitted (a method with no poll is not "covered", it is unobserved); every
//!   safepoint published a map, so every id resolves; and no safepoint failed
//!   to describe what was live at it. It can only be true with
//!   `CRATONVM_JIT_ARM64_SAFEPOINTS` on, so a default build is unchanged and
//!   keeps its conservative scan.
//!
//!   Getting there required fixing something that would have made the claim
//!   unsound: the operand oop MARKS were not kept in lockstep with the operand
//!   stack. `push_operand` pushed no mark and `pop_operand` popped none, so a
//!   mark outlived the value it described and was re-read for whatever later
//!   occupied that index -- naming a primitive as a reference (a relocating
//!   collector rewrites a non-pointer) or losing a reference (with the
//!   conservative scan suppressed, a use-after-free). They are lockstep now,
//!   `dup` carries its mark to both copies, and references enter only through
//!   `aconst_null` and `aload*`, both of which mark -- so the marks are exact
//!   by construction.
//!
//!   **What the claim rests on, stated because it is the whole risk: none of
//!   this has ever been EXECUTED.** No host in this repository runs aarch64.
//!   The evidence is instruction encodings, pseudo-op structure and the
//!   construction arguments above. `CRATONVM_DBG_VERIFY_OOP_MAPS` -- the
//!   runtime oracle that walks a live frame and refutes a coverage claim it can
//!   disprove -- is what turns that into evidence, and it should be armed on
//!   the first aarch64 run before this flag is trusted.
//! - **No deoptimization and no OSR.** Neither word appears in this file.
//!   There is no frame reconstruction, no uncommon-trap stub, no
//!   `osr_pc_to_native` table. There is nothing to tier down *from* (this is
//!   the only tier), so a deopt cannot occur — but equally, no speculative
//!   optimization may ever be added here without building that first.
//! - **Stack bang (2026-09-12).** The prologue touches every page the frame
//!   crosses before it moves SP, as x64 does, so stack exhaustion faults on the
//!   guard page. Frames of 4096 bytes or more used to be refused instead, and
//!   could not have been allocated anyway: a wide `SUB SP` had no encoding
//!   until the extended-register ADD/SUB was added.
//! - **Float locals are not homed in FP registers.** `regalloc::ARM64_LOCAL_FPS`
//!   offers `D8`–`D15`, which AAPCS64 makes callee-saved, and this backend's
//!   prologue/epilogue save only GPRs — so homing a float local there destroyed
//!   the caller's copy. The allocator's FP assignments are ignored (2026-08-01);
//!   float locals live in frame slots.
//!
//! ## The `int` representation (2026-09-12)
//!
//! An `I32` operand is held SIGN-EXTENDED in its 64-bit register: the X
//! register equals the sign extension of its low 32 bits. That is how the VM
//! passes an `int` argument (`x as i64`) and how `iconst` materializes one, and
//! it makes every 64-bit reader -- the VM's read of X0, `CBZ X`, an `i2l` -- see
//! the right value. Every `int` producer is a W-form instruction (whose result
//! is the JVMS 32-bit wrapped value, and whose variable shifts take the
//! distance MOD 32) followed by `SXTW`; `f2i`/`d2i` use the 32-bit saturating
//! `FCVTZS W`; `l2i` is `SXTW`; `i2l` is a relabelling; and `int` compares,
//! zero tests and switch keys read the W register regardless. The previous
//! lowering used the X forms of the `long` ops, so `Integer.MAX_VALUE + 1` was
//! 2147483648 and `1 << 32` was 4294967296.
//!
//! `float` is an S register and `double` a D register, end to end: constants,
//! arithmetic, compares, conversions, locals (four bytes of a frame word for a
//! `float`) and returns (moved bit-exactly into X0, where the VM reads every
//! result).
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
//! bypassed entirely on that target) -- and which compiles NOTHING unless
//! `CRATONVM_JIT_ARM64` is set (see [`arm64_jit_enabled`]). The VM's two other
//! compile doors that call `x64::compile_with_param_slots` directly, the eager
//! first call (`vm/src/runtime/interpreter.rs`) and OSR (`compile_osr_artifact`
//! in `vm/src/runtime/interpreter/jit_bridge.rs`), return without compiling on
//! any target but x86-64, so an aarch64 build cannot publish x86-64 bytes.
//!
//! Both this module and `aarch64.rs` are compiled unconditionally on every
//! host (`pub mod` in `lib.rs`, no `cfg`), so their unit tests — including
//! every instruction-encoding test — run in ordinary x86-64 CI. The code is
//! not rotting; it is simply far smaller in scope than its name suggests.
//!
//! ## Calling convention
//!
//! The VM calls compiled code as `extern "C" fn(i64, ..) -> i64`: ONE 64-bit
//! integer register per argument (X0-X7, `this` first; more than eight
//! arguments refuse the method), and every result read out of X0. A `float`
//! argument arrives as its zero-extended bit pattern and a `double` as its
//! bits, and FP results are moved bit-exactly into X0. Argument `i` is homed
//! in JVM local `compute_param_jvm_slots(..)[i]`, which differs from `i` after
//! any `long`/`double`. Callee-saved: X19-X28, FP (X29), LR (X30). SP is
//! 16-byte aligned at all times.
//!
//! ## Stack layout (after the prologue)
//!
//! ```text
//! [FP + 8]         saved LR          \  the AAPCS64 frame record
//! [FP]             saved caller FP   /  (STP X29, X30, [SP, #-16]!; ADD X29, SP, #0)
//! [FP - 8] ..      callee-saved GPRs (register-homed locals)
//! ..               frame-homed locals, then one word per operand-stack depth,
//!                  then the safepoint homes and the safepoint-id word
//! [SP]             frame bottom
//! ```
//!
//! See [`Arm64FrameLayout::compute`]. The record used to sit at `[FP-16]`, with
//! FP equal to the caller's SP, which no unwinder or frame-pointer walk expects.

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
    /// Minus / negative (N=1). After `FCMP` this is "less than" and is FALSE on
    /// unordered, which is what `fcmpg`/`dcmpg` need (`Lt` is true on it).
    Mi,
    /// Plus / positive or zero (N=0)
    Pl,
    /// Overflow (V=1). After `FCMP`: unordered.
    Vs,
    /// No overflow (V=0)
    Vc,
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

    // -- 32-bit (W) integer forms --
    //
    // JVM `int` arithmetic. A W-form result is the JVMS 32-bit wrapped value;
    // the backend follows each producer with `Sxtw` to restore the
    // sign-extended form it keeps every `int` in.
    AddW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    MulW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AndW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    OrrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    EorW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSLV Wd, Wn, Wm`: the amount is taken MOD 32, i.e. `ishl`'s `& 0x1f`.
    LslW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSRV Wd, Wn, Wm`, amount MOD 32.
    LsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `ASRV Wd, Wn, Wm`, amount MOD 32.
    AsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    NegW {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    AddImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    SubImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    CmpW {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImmW {
        rn: Arm64Register,
        imm: i32,
    },
    CbzW {
        rt: Arm64Register,
        label: u32,
    },
    CbnzW {
        rt: Arm64Register,
        label: u32,
    },
    /// `SXTW Xd, Wn`.
    Sxtw {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTH Xd, Wn`.
    Sxth {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTB Xd, Wn`.
    Sxtb {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `AND Xd, Xn, #imm`, as a bitmask immediate when encodable and through
    /// IP0 otherwise.
    AndImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: u64,
    },
    /// `CSET Xd, cond`: 1 when `cond` holds, else 0.
    Cset {
        rd: Arm64Register,
        cond: Arm64Condition,
    },
    /// `CNEG Xd, Xn, cond`: `-Xn` when `cond` holds, else `Xn`.
    Cneg {
        rd: Arm64Register,
        rn: Arm64Register,
        cond: Arm64Condition,
    },

    // -- FP width forms --
    /// `FMOV Sd, Wn` (bit pattern, no conversion).
    FmovToFpSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FMOV Wd, Sn` (bit pattern; zero-extends into Xd).
    FmovFromFpSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FMOV Sd, Sn`.
    FmovFpSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// `SCVTF Dd, Wn` (`i2d`).
    ScvtfDoubleW {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SCVTF Sd, Xn` (`l2f`).
    ScvtfSingleX {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FCVTZS Wd, Dn` (`d2i`, saturating at the 32-bit bounds).
    FcvtzsIntW {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FCVTZS Xd, Sn` (`f2l`).
    FcvtzsSingleX {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- Switches --
    /// `tableswitch`: `key - low` into IP1 (X17), an unsigned bounds check to
    /// `default`, then a PC-relative jump table (`ADR X16; LDRSW X17, [X16,
    /// W17, UXTW #2]; ADD X16, X16, X17; BR X16`) followed by one 32-bit
    /// offset per case. X16/X17 are never allocator-managed, so no case can
    /// land on the key's own register -- which the old compare chain, whose
    /// constants came from the scratch allocator, could (`CMP R, R`).
    TableSwitch {
        key: Arm64Register,
        low: i32,
        default: u32,
        targets: Vec<u32>,
    },
    /// `lookupswitch`: `CMP Wkey, #value` (constants too wide for an
    /// immediate go through IP0, never a scratch register), `B.EQ` per pair,
    /// then `B default`.
    LookupSwitch {
        key: Arm64Register,
        pairs: Vec<(i32, u32)>,
        default: u32,
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

impl Arm64Instruction {
    /// An upper bound on the bytes this pseudo-op encodes to.
    ///
    /// `Label` and `Comment` emit nothing, a literal emits 8 bytes, immediates
    /// and far frame accesses expand into a materialization through IP0, and
    /// a switch carries its own table. See `emit_machine_code_inner`.
    pub fn max_encoded_bytes(&self) -> usize {
        match self {
            Arm64Instruction::Label(_) | Arm64Instruction::Comment(_) => 0,
            Arm64Instruction::ConstantPoolEntry { .. } => 8,
            Arm64Instruction::MovImm { .. } => 16,
            Arm64Instruction::AddImm { .. }
            | Arm64Instruction::SubImm { .. }
            | Arm64Instruction::AddImmW { .. }
            | Arm64Instruction::SubImmW { .. }
            | Arm64Instruction::CmpImm { .. }
            | Arm64Instruction::CmpImmW { .. }
            | Arm64Instruction::AndImm { .. } => 20,
            Arm64Instruction::Ldr { .. }
            | Arm64Instruction::Str { .. }
            | Arm64Instruction::FpLdr { .. }
            | Arm64Instruction::FpStr { .. } => 24,
            // SUB + CMP (each up to 20 through IP0), B.HI, ADR, LDRSW, ADD, BR,
            // and a word per case.
            Arm64Instruction::TableSwitch { targets, .. } => 60 + 4 * targets.len(),
            // A CMP (up to 20) and a B.EQ per pair, then B.
            Arm64Instruction::LookupSwitch { pairs, .. } => 24 * pairs.len() + 4,
            _ => 4,
        }
    }
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
    /// ```text
    /// [FP + 8]     saved LR          \  the AAPCS64 frame record, pushed by
    /// [FP]         saved caller FP   /  `STP X29, X30, [SP, #-16]!`
    /// [FP - 8] ..  callee-saved GPRs   (callee_save_offset = -callee_save_bytes)
    /// ..           spill area          (spill_offset = callee_save_offset - spill_bytes)
    /// [SP]         frame bottom        (FP - (frame_size - 16))
    /// ```
    ///
    /// `frame_size` counts the 16-byte record as well, and is 16-byte aligned.
    /// Everything this frame owns is BELOW FP. The record used to sit at
    /// `[FP-16]`/`[FP-8]` with FP equal to the caller's SP, which is not the
    /// frame record any unwinder or frame-pointer walk expects.
    pub fn compute(_num_locals: usize, num_spills: usize, saved_regs: &[Arm64Register]) -> Self {
        let num_reg_locals = saved_regs.len();

        // Callee-saved regs: round count up to even for STP pairing.
        let num_saved = saved_regs.len();
        let callee_save_bytes = ((num_saved + 1) / 2) * 16; // pairs of 8-byte regs

        let spill_bytes = num_spills * 8;

        // Total = frame record (16) + callee-save area + spill area, aligned.
        let raw = 16 + callee_save_bytes + spill_bytes;
        let frame_size = align_up(raw, 16) as i32;

        // Offsets are negative from FP, which points at the frame record.
        let callee_save_offset = -(callee_save_bytes as i32);
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

    /// An UPPER BOUND on the encoded size, in bytes.
    ///
    /// Not `len * 4`: this is a pseudo-op stream, in which a `Label` or
    /// `Comment` encodes to nothing and a `MovImm`, a wide immediate, a far
    /// frame access or a switch expands to several words. See
    /// [`Arm64Instruction::max_encoded_bytes`].
    pub fn estimated_size(&self) -> usize {
        self.instructions
            .iter()
            .map(Arm64Instruction::max_encoded_bytes)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Arm64CompileResult
// ---------------------------------------------------------------------------

/// Is the aarch64 JIT on at all (`CRATONVM_JIT_ARM64`, **default-OFF**)?
///
/// `docs/PLATFORMS.md` says the JIT is disabled off x86-64, and until this
/// switch existed that was not true on aarch64: `try_compile_inner` compiled
/// with this backend unconditionally, publishing machine code that no host in
/// this repository had executed. It stays opt-in until the backend has run on
/// hardware. Read by the `#[cfg(target_arch = "aarch64")]` block of
/// `try_compile_inner`, and defined here without a `cfg` so every host
/// type-checks it.
pub fn arm64_jit_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_JIT_ARM64")
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Ok("1") | Ok("true") | Ok("on") | Ok("yes")
        )
    })
}

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
    /// The safepoint id this map belongs to -- the value the frame's sp-id slot
    /// holds while this safepoint is the active one. Becomes
    /// `OopMapEntry::bytecode_pc`, which is what
    /// `find_oop_map_for_safepoint_id` matches on.
    pub safepoint_id: u32,
}

/// Output of the compilation pipeline.
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
    /// Empty unless `CRATONVM_JIT_ARM64_SAFEPOINTS` is on: the safepoint polls
    /// are this backend's only GC-capable points (it lowers no allocation, call
    /// or monitor), and each poll records exactly one map.
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Frame offset (positive) of the safepoint-id slot, or 0 when none was
    /// reserved. Published onto `CompiledMethod::sp_id_slot_off`, which the
    /// runtime reads as `[frame_base - off]`.
    pub sp_id_slot_off: i32,
    /// How many safepoints this compilation published a map for, and how many
    /// of those could not describe everything live at their site. The terms
    /// `fully_oop_covered` is computed from -- see `publish_compiled_method`.
    pub safepoint_count: usize,
    pub incomplete_oop_maps: usize,
}

// ---------------------------------------------------------------------------
// Arm64Backend
// ---------------------------------------------------------------------------

/// The kind of value an operand-stack entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    /// JVM `int` (and `boolean`/`byte`/`char`/`short`), held SIGN-EXTENDED in
    /// its 64-bit register. See "The `int` representation" in the module
    /// header.
    I32,
    /// JVM `long`.
    I64,
    /// An object reference.
    Ref,
    /// JVM `float`: an S register, or the low four bytes of a frame word.
    F32,
    /// JVM `double`: a D register, or a whole frame word.
    F64,
}

impl OperandKind {
    /// Lives in V0-V7 rather than X9-X15.
    pub fn is_fp(self) -> bool {
        matches!(self, OperandKind::F32 | OperandKind::F64)
    }

    /// Category 2 (JVMS 2.11.1): one entry here, two JVM stack slots.
    pub fn is_category2(self) -> bool {
        matches!(self, OperandKind::I64 | OperandKind::F64)
    }
}

/// Where an operand-stack entry is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandLoc {
    /// A scratch register: X9-X15 for integer kinds, V0-V7 for FP kinds.
    Reg(Arm64Register),
    /// The frame word reserved for the entry's DEPTH, as an FP-relative
    /// offset (see [`Arm64Backend::spill_offset_for_depth`]).
    Slot(i32),
}

/// One simulated operand-stack entry: what it is, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operand {
    pub kind: OperandKind,
    pub loc: OperandLoc,
    /// Holds an object reference the GC must see. Set for every `Ref` entry --
    /// only `aconst_null` and `aload*` produce one on this backend -- and
    /// carried by the stack shuffles, so the marks are exact by construction.
    pub oop: bool,
}

impl Operand {
    /// An entry that lives in `reg`.
    pub fn in_reg(kind: OperandKind, reg: Arm64Register) -> Self {
        Self {
            kind,
            loc: OperandLoc::Reg(reg),
            oop: kind == OperandKind::Ref,
        }
    }
}

/// Kinds of the `<x>load`/`<x>store` families, in opcode order: `i l f d a`.
const LOCAL_KINDS: [OperandKind; 5] = [
    OperandKind::I32,
    OperandKind::I64,
    OperandKind::F32,
    OperandKind::F64,
    OperandKind::Ref,
];

/// Conditions of `ifeq..ifle` and `if_icmpeq..if_icmple`, in opcode order.
const IF_CONDS: [Arm64Condition; 6] = [
    Arm64Condition::Eq,
    Arm64Condition::Ne,
    Arm64Condition::Lt,
    Arm64Condition::Ge,
    Arm64Condition::Gt,
    Arm64Condition::Le,
];

/// The bytecode byte at `at`, or `None` past the end.
fn bc_u8(code: &[u8], at: usize) -> Option<u8> {
    code.get(at).copied()
}

/// The big-endian `i16` at `at`, or `None` if it runs past the end.
fn bc_i16(code: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
}

/// The big-endian `i32` at `at`, or `None` if it runs past the end.
fn bc_i32(code: &[u8], at: usize) -> Option<i32> {
    let b = code.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The bytecode pc a branch at `start_pc` with displacement `offset` targets.
///
/// Cast: a target before pc 0 wraps to a huge pc. No label is ever bound
/// there, so `emit_machine_code` refuses the method -- the same outcome as any
/// other branch into the middle of nowhere.
fn branch_target(start_pc: usize, offset: i32) -> usize {
    (start_pc as i64 + i64::from(offset)) as usize
}

/// The main ARM64 compilation pipeline.
///
/// Translates JVM bytecode into `Arm64Instruction` sequences using a
/// simulated operand stack (compile-time stack mapping).
pub struct Arm64Backend {
    buffer: Arm64CodeBuffer,
    frame: Option<Arm64FrameLayout>,
    /// Register assignment for each local variable (GPR); `None` means the
    /// local is frame-homed.
    local_regs: Vec<Option<Arm64Register>>,
    /// FP register assignment for float/double locals. Always `None`: see the
    /// comment on the loop that fills it in `compile_pass`.
    float_local_regs: Vec<Option<Arm64Register>>,
    /// The simulated operand stack, bottom first -- ONE stack for every kind.
    ///
    /// Each entry records where ITS value is: a scratch register, or the frame
    /// word reserved for its depth. This replaces a register-only stack plus a
    /// `register -> spill slot` map, which could not describe two entries that
    /// had used the same register: when the round-robin allocator wrapped onto
    /// a live register it spilled it, recorded `spill_map[R]`, and pushed R
    /// again for the NEW value, so popping the new value reloaded the OLD one.
    /// `a - (b+1+2+3+4)` returned -4 for `(100, 5)`. Floats lived on a second
    /// stack whose allocator had no liveness check and no spill at all.
    operand_stack: Vec<Operand>,
    /// Scratch registers the bytecode being lowered has popped and still reads.
    /// The allocator never hands one out, so a result register cannot overwrite
    /// an operand the same instruction has yet to consume. Cleared per bytecode.
    held: Vec<Arm64Register>,
    /// The operand-stack SHAPE (kind and oop mark per entry) at each branch
    /// target, recorded by the branches. Every path into a target leaves each
    /// entry in its depth slot, so the shape is all a target needs to rebuild
    /// the model. Recorded by pass 1 and read by pass 2, so a backward target
    /// is known before the walk reaches it.
    label_states: HashMap<usize, Vec<(OperandKind, bool)>>,
    /// Whether control falls into the instruction being lowered from the
    /// previous one (false after `goto`, `*return` and the switches).
    reachable: bool,
    /// Per-bci operand-stack kinds from the shared analysis. Consulted only to
    /// rebuild the model at a pc that no recorded branch describes.
    ///
    /// Populated with EMPTY metadata, which costs nothing here: the analysis
    /// needs field types, call arities and constant-pool tags, and this backend
    /// refuses every method containing a field access, a call of any kind, or
    /// any `ldc`.
    stack_kinds: crate::x64::stack_kinds::StackKindMap,
    /// Bytecode PC -> label mapping for branch targets.
    pc_labels: HashMap<usize, u32>,
    /// Bytecode PC of the instruction currently being lowered. Read by
    /// [`Arm64Backend::label_for_pc`] to tell a back-edge from a forward
    /// branch, and by the safepoint poll for its id and oop-local mask.
    cur_bytecode_pc: usize,
    /// `max_stack` for this compilation: the operand area's size, and where
    /// the safepoint home slots begin.
    max_stack: usize,
    /// Per-bytecode-pc "must be oop" local masks, and whether the dataflow
    /// reached each pc. Shared with x64 (`compute_local_oop_masks`). Empty when
    /// unsupported (>64 locals), which this backend treats as "no claim".
    local_oop_masks: Vec<u64>,
    local_oop_reached: Vec<bool>,
    /// Which parameter slots hold references on entry. The ENTRY poll answers
    /// from this. Zero unless [`Arm64Backend::set_method_descriptor`] was called.
    param_oop_mask: u64,
    /// The JVM local slot of each incoming argument, from the descriptor
    /// (`compute_param_jvm_slots`), or `None` for the identity layout
    /// `0..num_params` when no descriptor was supplied.
    param_jvm_slots: Option<Vec<usize>>,
    /// Frame offsets of reference LOCALS at the safepoint being emitted, folded
    /// into the map by `emit_oop_map_for_safepoint`. Taken, not copied.
    pending_local_oop_slots: Vec<i32>,
    /// Frame offsets of reference OPERANDS the poll stored for its call. Taken,
    /// not copied, like the locals.
    pending_operand_oop_slots: Vec<i32>,
    /// Label for the shared epilogue.
    epilogue_label: u32,
    /// Number of parameter SLOTS for the current method.
    num_params: usize,
    /// Method invoke metadata: maps constant pool index to argument count.
    method_info: HashMap<u16, usize>,
    /// Set to true if a compilation error occurred (e.g. stack underflow).
    pub failed: bool,
    /// T1.1.3 — collected oop maps, PC-unresolved; see [`Arm64PendingOopMap`].
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Runtime helper addresses. Zeroed until [`Arm64Backend::set_helpers`] is
    /// called; `safepoint_flag_addr == 0` means "not wired" and the poll emits
    /// nothing, the same contract x64 uses.
    helpers: crate::JitRuntimeHelpers,
    /// Bytecode PCs that are the target of a BACKWARD branch -- loop headers.
    /// Discovered by pass 1 and read by pass 2, which polls at each one.
    back_edge_targets: std::collections::HashSet<usize>,
    /// Frame offset (POSITIVE; the slot is at `[FP - sp_id_slot_off]`) of the
    /// word each safepoint stamps its id into, or 0 when none is reserved.
    sp_id_slot_off: i32,
    /// Safepoints this compilation published a map for, and how many of those
    /// maps could NOT describe everything live at their site. A count, not a
    /// set of pcs: two safepoints can share one bci.
    safepoint_count: usize,
    incomplete_oop_maps: usize,
    /// Set by the poll when it could not describe this site; consumed (taken)
    /// by the map writer.
    pending_map_incomplete: bool,
    /// Whether this compilation emits safepoint polls, seeded from
    /// [`arm64_safepoints_enabled`] in `new()`. A field so a test can reach
    /// both arms of a process-latched switch.
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

/// Float scratch registers for the operand stack (V0-V7, 8 regs).
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

/// Page size the stack bang probes at. AArch64 Linux, Windows and Darwin all
/// place at least this much guard below a thread's stack.
const STACK_BANG_PAGE_BYTES: i32 = 4096;

/// Probes beyond this many refuse the method (a 2 MiB frame), for the same
/// code-size reason as x64's `MAX_STACK_BANG_PROBES`.
const MAX_STACK_BANG_PROBES: usize = 512;

/// SP-relative distances (subtracted from SP) the prologue probes before it
/// moves SP down by `frame_below` bytes, or `None` when the frame needs more
/// probes than [`MAX_STACK_BANG_PROBES`].
///
/// The same scheme as x64's `stack_bang_frame_probe_disps`: one probe per page
/// the new frame crosses, plus the exact frame bottom when it is not
/// page-aligned. A frame smaller than a page needs none: it cannot step over
/// a guard page, so its own first store already lands on the guard.
fn stack_bang_probe_offsets(frame_below: i32) -> Option<Vec<i32>> {
    if frame_below < 0 {
        return None;
    }
    let mut offsets = Vec::new();
    let mut off = STACK_BANG_PAGE_BYTES;
    while off <= frame_below {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(off);
        off = off.checked_add(STACK_BANG_PAGE_BYTES)?;
    }
    if frame_below >= STACK_BANG_PAGE_BYTES && frame_below % STACK_BANG_PAGE_BYTES != 0 {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(frame_below);
    }
    Some(offsets)
}

impl Arm64Backend {
    pub fn new() -> Self {
        Self {
            buffer: Arm64CodeBuffer::new(),
            frame: None,
            local_regs: Vec::new(),
            float_local_regs: Vec::new(),
            operand_stack: Vec::new(),
            held: Vec::new(),
            label_states: HashMap::new(),
            reachable: true,
            stack_kinds: crate::x64::stack_kinds::StackKindMap::default(),
            pc_labels: HashMap::new(),
            cur_bytecode_pc: 0,
            max_stack: 0,
            local_oop_masks: Vec::new(),
            local_oop_reached: Vec::new(),
            param_oop_mask: 0,
            param_jvm_slots: None,
            pending_local_oop_slots: Vec::new(),
            pending_operand_oop_slots: Vec::new(),
            epilogue_label: 0,
            num_params: 0,
            method_info: HashMap::new(),
            failed: false,
            pending_oop_maps: Vec::new(),
            // SAFETY: `JitRuntimeHelpers` is a plain struct of `usize`
            // addresses; all-zero is its documented "nothing wired" state, and
            // `safepoint_flag_addr == 0` is what gates the poll emitter.
            helpers: unsafe { std::mem::zeroed() },
            back_edge_targets: std::collections::HashSet::new(),
            sp_id_slot_off: 0,
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            pending_map_incomplete: false,
            safepoints_enabled: arm64_safepoints_enabled(),
        }
    }

    /// T1.1.3 — mark the top of the operand stack as holding an object
    /// reference. Production pushes set the mark from the entry's kind; this
    /// remains for tests that build a stack by hand.
    #[allow(dead_code)]
    fn mark_top_operand_as_oop(&mut self) {
        if let Some(top) = self.operand_stack.last_mut() {
            top.oop = true;
        }
    }

    /// Record this safepoint's oop map: the frame slots that hold object
    /// references right now, keyed so the encoder can give them a real PC.
    ///
    /// The PC an `OopMapEntry` needs is a BYTE OFFSET, and this backend has a
    /// pseudo-op stream whose entries are not 4 bytes each, so the map is
    /// recorded against the pseudo-op INDEX of the instruction that follows
    /// the safepoint, in an [`Arm64PendingOopMap`], and
    /// [`emit_machine_code_with_oop_maps`] translates it.
    ///
    /// Names: reference operands already in their depth slots, the reference
    /// operands the poll stored for its call (`pending_operand_oop_slots`),
    /// and the reference locals it staged (`pending_local_oop_slots`). A
    /// reference operand still in a REGISTER is not nameable; the poll stores
    /// every one before calling this, which is why it is the only caller.
    fn emit_oop_map_for_safepoint(&mut self, safepoint_id: u32) {
        if self.failed {
            return;
        }
        // The pseudo-op that will FOLLOW this safepoint: the "return address"
        // convention x64 records.
        let pseudo_index = match u32::try_from(self.buffer.instruction_count()) {
            Ok(n) => n,
            Err(_) => {
                self.failed = true;
                return;
            }
        };

        let mut offsets: Vec<i32> = self
            .operand_stack
            .iter()
            .filter(|o| o.oop)
            .filter_map(|o| match o.loc {
                OperandLoc::Slot(off) => Some(off),
                OperandLoc::Reg(_) => None,
            })
            .collect();
        offsets.extend(std::mem::take(&mut self.pending_operand_oop_slots));
        offsets.extend(std::mem::take(&mut self.pending_local_oop_slots));

        let mut slots: Vec<i16> = Vec::new();
        for off in offsets {
            match i16::try_from(off) {
                Ok(off16) => {
                    if !slots.contains(&off16) {
                        slots.push(off16);
                    }
                }
                // A slot further than `i16` from FP. There is no completeness
                // channel for this, so refuse rather than publish a map that
                // silently drops a live reference.
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        }
        // PUBLISH EVERY SAFEPOINT, even one with no live reference: an id whose
        // map is absent is indistinguishable from an uncovered frame.
        self.safepoint_count += 1;
        if std::mem::take(&mut self.pending_map_incomplete) {
            self.incomplete_oop_maps += 1;
        }
        self.pending_oop_maps.push(Arm64PendingOopMap {
            pseudo_index,
            frame_slot_offsets: slots,
            safepoint_id,
        });
    }

    // -- The operand model ----------------------------------------------------

    /// Whether some operand-stack entry lives in `reg`.
    fn reg_is_live(&self, reg: Arm64Register) -> bool {
        self.operand_stack
            .iter()
            .any(|o| o.loc == OperandLoc::Reg(reg))
    }

    /// A scratch register of the requested class that holds nothing the
    /// current bytecode still needs. The register is HELD until the next
    /// bytecode.
    ///
    /// When every register of the class is live or held, the DEEPEST
    /// register-located entry is moved to its depth slot first. The entry
    /// records the move, so its value is found again by depth -- never by
    /// asking which register it used to be in.
    fn alloc_reg(&mut self, fp: bool) -> Arm64Register {
        let pool: &'static [Arm64Register] = if fp {
            &FLOAT_SCRATCH_REGS
        } else {
            &SCRATCH_REGS
        };
        if let Some(&reg) = pool
            .iter()
            .find(|&&r| !self.held.contains(&r) && !self.reg_is_live(r))
        {
            self.held.push(reg);
            return reg;
        }
        let victim = self.operand_stack.iter().position(|o| match o.loc {
            OperandLoc::Reg(r) => pool.contains(&r) && !self.held.contains(&r),
            OperandLoc::Slot(_) => false,
        });
        let Some(depth) = victim else {
            // Every register of the class is held by this one bytecode. No
            // bytecode needs that many; refuse rather than alias.
            self.failed = true;
            return pool[0];
        };
        let OperandLoc::Reg(reg) = self.operand_stack[depth].loc else {
            self.failed = true;
            return pool[0];
        };
        if !self.spill_entry(depth) {
            return pool[0];
        }
        self.held.push(reg);
        reg
    }

    /// Move entry `depth` from its register into its depth slot. `false` (and
    /// `failed`) when the slot is outside the reserved operand area.
    fn spill_entry(&mut self, depth: usize) -> bool {
        let Some(entry) = self.operand_stack.get(depth).copied() else {
            self.failed = true;
            return false;
        };
        let OperandLoc::Reg(reg) = entry.loc else {
            return true;
        };
        let Some(offset) = self.spill_offset_for_depth(depth) else {
            self.failed = true;
            return false;
        };
        self.emit_store_kind(reg, entry.kind, offset);
        self.operand_stack[depth].loc = OperandLoc::Slot(offset);
        true
    }

    /// Put every entry in its depth slot: the one layout that every path into
    /// a branch target agrees on.
    fn spill_all(&mut self) {
        for depth in 0..self.operand_stack.len() {
            if !self.spill_entry(depth) {
                return;
            }
        }
    }

    /// Store `reg` (holding a `kind`) to `[FP + offset]` at the kind's width.
    fn emit_store_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpStr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Str {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Load a `kind` from `[FP + offset]` into `reg` at the kind's width.
    fn emit_load_kind(&mut self, reg: Arm64Register, kind: OperandKind, offset: i32) {
        let inst = match kind {
            OperandKind::F32 | OperandKind::F64 => Arm64Instruction::FpLdr {
                vt: reg,
                rn: Arm64Register::FP,
                offset,
                is_double: kind == OperandKind::F64,
            },
            _ => Arm64Instruction::Ldr {
                rt: reg,
                rn: Arm64Register::FP,
                offset,
            },
        };
        self.buffer.emit(inst);
    }

    /// Push a value the current bytecode computed into `reg`.
    fn push_reg(&mut self, kind: OperandKind, reg: Arm64Register) {
        self.operand_stack.push(Operand::in_reg(kind, reg));
    }

    /// Push `entry`'s kind and oop mark, now living in `reg`.
    fn push_like(&mut self, entry: Operand, reg: Arm64Register) {
        self.operand_stack.push(Operand {
            loc: OperandLoc::Reg(reg),
            ..entry
        });
    }

    /// Pop the top entry into a register, reloading it from its slot when it
    /// was spilled. The register is HELD for the rest of this bytecode.
    /// Underflow sets `failed` and answers `None`.
    fn pop_entry(&mut self) -> Option<(Arm64Register, Operand)> {
        let Some(entry) = self.operand_stack.pop() else {
            self.failed = true;
            return None;
        };
        match entry.loc {
            OperandLoc::Reg(reg) => {
                self.held.push(reg);
                Some((reg, entry))
            }
            OperandLoc::Slot(offset) => {
                let reg = self.alloc_reg(entry.kind.is_fp());
                self.emit_load_kind(reg, entry.kind, offset);
                Some((reg, entry))
            }
        }
    }

    /// Pop an entry that must be a `want`. Any other kind is a malformed
    /// method (the verifier would reject it) or a model bug; either refuses.
    fn pop_kind(&mut self, want: OperandKind) -> Arm64Register {
        let fallback = if want.is_fp() {
            Arm64Register::V0
        } else {
            Arm64Register::X0
        };
        match self.pop_entry() {
            Some((reg, entry)) if entry.kind == want => reg,
            Some(_) => {
                self.failed = true;
                fallback
            }
            None => fallback,
        }
    }

    /// Pop an integer-class (int, long or reference) entry. Returns X0 as a
    /// sentinel and sets `self.failed = true` on underflow or an FP entry.
    pub fn pop_operand(&mut self) -> Arm64Register {
        match self.pop_entry() {
            Some((reg, entry)) if !entry.kind.is_fp() => reg,
            Some(_) => {
                self.failed = true;
                Arm64Register::X0
            }
            None => Arm64Register::X0,
        }
    }

    /// Discard the top entry without materializing it.
    fn drop_top(&mut self) {
        if self.operand_stack.pop().is_none() {
            self.failed = true;
        }
    }

    /// Re-establish the sign-extended `int` form after a W-form producer.
    fn emit_sxtw(&mut self, reg: Arm64Register) {
        self.buffer
            .emit(Arm64Instruction::Sxtw { rd: reg, rn: reg });
    }

    /// The kind and oop mark of every entry, bottom first.
    fn stack_shape(&self) -> Vec<(OperandKind, bool)> {
        self.operand_stack.iter().map(|o| (o.kind, o.oop)).collect()
    }

    /// Record `shape` for target `pc`, or refuse the method when a different
    /// path already recorded a different one.
    fn check_or_record_shape(&mut self, pc: usize, shape: Vec<(OperandKind, bool)>) {
        match self.label_states.get(&pc) {
            Some(existing) if *existing != shape => self.failed = true,
            Some(_) => {}
            None => {
                self.label_states.insert(pc, shape);
            }
        }
    }

    /// Everything a branch to `target` must do before it is emitted: put the
    /// stack in its canonical all-slots layout, record that shape, and resolve
    /// the label. Only STORES are emitted, so flags and held registers survive.
    fn prepare_branch(&mut self, target: usize) -> u32 {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(target, shape);
        self.label_for_pc(target)
    }

    /// The walk is about to fall into branch target `pc`: arrive in the layout
    /// the branches to it use.
    fn arrive_at_target(&mut self, pc: usize) {
        self.spill_all();
        let shape = self.stack_shape();
        self.check_or_record_shape(pc, shape);
    }

    /// Rebuild the model at a pc control cannot fall into: from the shape a
    /// branch recorded, else from the shared stack-kind analysis. With neither
    /// the pc is unreachable, and it is lowered against an empty stack -- at
    /// worst an underflow refuses the method.
    fn restore_stack_at(&mut self, pc: usize) {
        let shape = match self.label_states.get(&pc) {
            Some(s) => Some(s.clone()),
            None => self.shape_from_analysis(pc),
        };
        self.operand_stack.clear();
        let Some(shape) = shape else {
            return;
        };
        for (depth, (kind, oop)) in shape.into_iter().enumerate() {
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                self.failed = true;
                return;
            };
            self.operand_stack.push(Operand {
                kind,
                loc: OperandLoc::Slot(offset),
                oop,
            });
        }
    }

    /// The operand-stack shape at `pc` according to the shared analysis, or
    /// `None` when it has no answer or an entry is untyped.
    fn shape_from_analysis(&self, pc: usize) -> Option<Vec<(OperandKind, bool)>> {
        use crate::x64::stack_kinds::StackKind;
        self.stack_kinds
            .get(pc)?
            .iter()
            .map(|k| match k {
                StackKind::Int => Some((OperandKind::I32, false)),
                StackKind::Long => Some((OperandKind::I64, false)),
                StackKind::Float => Some((OperandKind::F32, false)),
                StackKind::Double => Some((OperandKind::F64, false)),
                StackKind::Ref => Some((OperandKind::Ref, true)),
                StackKind::Unknown => None,
            })
            .collect()
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
    /// test binary can only ever observe one arm of it.
    pub fn set_safepoints_enabled(&mut self, on: bool) {
        self.safepoints_enabled = on;
    }

    /// Seed the reference-parameter mask and the argument-to-slot layout from
    /// this method's descriptor.
    ///
    /// Must be called BEFORE compiling. The mask feeds
    /// `compute_local_oop_masks` and the entry poll. The slot layout is what
    /// the prologue homes each argument by: the VM passes ONE register per
    /// argument, and a `long`/`double` argument occupies TWO JVM local slots,
    /// so argument `i` is local `i` only while every earlier argument is
    /// category 1. Without a descriptor the identity layout is assumed, which
    /// is right for exactly those signatures.
    pub fn set_method_descriptor(&mut self, descriptor: &str, is_static: bool) {
        self.param_oop_mask = crate::compute_param_oop_mask(descriptor, is_static);
        self.param_jvm_slots = Some(crate::compute_param_jvm_slots(descriptor, is_static).0);
    }

    /// Publish this frame's base so the GC root walk can find it.
    ///
    /// The safepoint-id slot is read as `[frame_base - sp_id_slot_off]`, and
    /// the runtime learns `frame_base` from `set_top_frame_base`, which it is
    /// told through `helpers.frame_record`. FP is the base this backend
    /// publishes, playing the role x64's RBP does.
    ///
    /// Emitted after the arguments are homed and before the entry poll. The
    /// call clobbers X0-X17 and V0-V7, which is sound only because nothing is
    /// on the operand stack yet; a non-empty stack here refuses the method
    /// rather than lose a value across the call.
    fn emit_frame_record(&mut self) {
        if self.failed || !self.safepoints_enabled || self.helpers.frame_record == 0 {
            return;
        }
        if !self.operand_stack.is_empty() {
            self.failed = true;
            return;
        }
        self.buffer.emit(Arm64Instruction::Mov {
            rd: Arm64Register::X0,
            rm: Arm64Register::FP,
        });
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.frame_record as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
    }

    /// Emit a cooperative GC safepoint poll.
    ///
    /// ```text
    ///     MOVZ/MOVK X16, #safepoint_flag_addr
    ///     LDRB      W17, [X16]          ; ONE byte -- the flag is an AtomicBool
    ///     CBZ       X17, skip           ; clear -> no safepoint requested
    ///     <store every register-located operand, GPR and FP, to its slot>
    ///     <store register-homed reference locals to their homes>
    ///     <stamp the safepoint id>
    ///     MOVZ/MOVK X16, #safepoint_slow_path
    ///     BLR       X16
    ///     <oop map recorded at the return address>
    ///     <reload the locals and the operands>
    ///   skip:
    /// ```
    ///
    /// X16/X17 are IP0/IP1, which AAPCS64 reserves for exactly this and which
    /// hold no operand or local. Java locals live in X19-X28, which the call
    /// preserves. The operand stack lives in X9-X15 and V0-V7, which it does
    /// not -- hence the store and reload. Both classes: this used to spill
    /// only the GPR operands, so a live float or double operand in V0-V7 was
    /// destroyed by every taken poll.
    ///
    /// # Why the stores and reloads sit INSIDE the branch
    ///
    /// They leave the compile-time model exactly as it was: an operand that was
    /// in a register is in the same register again on both paths, so the model
    /// and the two runtime paths agree without a merge.
    ///
    /// # What the GC sees
    ///
    /// The map, recorded at the BLR's return address, names every reference
    /// operand (now in its depth slot) and every reference LOCAL: a frame-homed
    /// one where it lives, and a register-homed one at the home it was just
    /// stored to. Register homes are callee-saved, so the value would survive
    /// on its own -- but inside the CALLEE's save area, where only a
    /// conservative walk sees it, and a conservative walk cannot rewrite a
    /// relocated object. The reload afterwards is what carries a moved
    /// object's new address back into the register.
    fn emit_safepoint_poll(&mut self, entry: bool) {
        if self.failed || !self.safepoints_enabled {
            return;
        }
        // The "not wired" contract, identical to x64's.
        if self.helpers.safepoint_flag_addr == 0 || self.helpers.safepoint_slow_path == 0 {
            return;
        }
        // THE SAFEPOINT ID: the site's bci, or the synthetic `ENTRY_POLL_BC_PC`
        // for the method-entry poll (bci 0 is a legal site of its own).
        let safepoint_id = if entry {
            crate::x64::safepoint::ENTRY_POLL_BC_PC as u32
        } else {
            match u32::try_from(self.cur_bytecode_pc) {
                Ok(n) => n,
                Err(_) => {
                    self.failed = true;
                    return;
                }
            }
        };
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

        // Store every register-located operand for the call. An operand that
        // is already in its slot needs nothing; the map writer names it.
        let mut stored: Vec<(Arm64Register, OperandKind, i32)> = Vec::new();
        for depth in 0..self.operand_stack.len() {
            let operand = self.operand_stack[depth];
            let OperandLoc::Reg(reg) = operand.loc else {
                continue;
            };
            let Some(offset) = self.spill_offset_for_depth(depth) else {
                // No slot for this depth: a live value would sit in a
                // caller-saved register across a CALL. Refuse the method.
                self.failed = true;
                return;
            };
            self.emit_store_kind(reg, operand.kind, offset);
            if operand.oop {
                self.pending_operand_oop_slots.push(offset);
            }
            stored.push((reg, operand.kind, offset));
        }

        // NAME THE REFERENCE LOCALS.
        let mut reg_homed: Vec<(usize, i32)> = Vec::new();
        let claim = self.oop_locals_at_current_pc(entry);
        if claim.is_none() && !self.local_regs.is_empty() {
            // The dataflow could not answer for this site (an unreached pc, or
            // more than 64 locals). Sound only while a conservative scan still
            // runs, so this method may not claim full coverage.
            self.pending_map_incomplete = true;
        }
        if let Some(mut mask) = claim {
            while mask != 0 {
                // Cast: count/index to usize
                let i = mask.trailing_zeros() as usize;
                mask &= mask - 1;
                if i >= self.local_regs.len() {
                    continue;
                }
                if let Some(reg) = self.local_regs.get(i).copied().flatten() {
                    let Some(off) = self.safepoint_home_for_reg_local(i) else {
                        // No home reserved: refuse rather than leave a live
                        // reference reachable only through a conservative scan.
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
                    // Frame-homed: already where the GC can read and rewrite
                    // it. At the ENTRY poll that includes frame-homed reference
                    // PARAMETERS, which the argument homing stored before this
                    // poll runs.
                    let Some(off) = self.local_slot_offset(i) else {
                        self.failed = true;
                        return;
                    };
                    self.pending_local_oop_slots.push(off);
                }
            }
        }

        // Stamp the id BEFORE the call, so a collector that stops this thread
        // inside the slow path reads the site it is actually standing at.
        if self.sp_id_slot_off != 0 {
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X17,
                imm: i64::from(safepoint_id),
            });
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X17,
                rn: Arm64Register::FP,
                offset: -self.sp_id_slot_off,
            });
        }

        self.buffer.emit(Arm64Instruction::MovImm {
            rd: Arm64Register::X16,
            imm: self.helpers.safepoint_slow_path as i64,
        });
        self.buffer.emit(Arm64Instruction::Blr {
            rn: Arm64Register::X16,
        });
        self.emit_oop_map_for_safepoint(safepoint_id);

        // Reload every register-homed local the GC may have REWRITTEN.
        for (i, off) in &reg_homed {
            if let Some(reg) = self.local_regs.get(*i).copied().flatten() {
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: reg,
                    rn: Arm64Register::FP,
                    offset: *off,
                });
            }
        }
        // ...and every operand, into the register the model says it is in.
        for (reg, kind, offset) in stored {
            self.emit_load_kind(reg, kind, offset);
        }
        self.buffer.bind_label(skip);
    }

    /// The oop-local mask in force at the safepoint being emitted, or `None`
    /// when no claim can be made.
    ///
    /// `None` is a REFUSAL, not "no oop locals": the dataflow is empty above 64
    /// locals and unreached at pcs only an exception edge can arrive at.
    fn oop_locals_at_current_pc(&self, entry: bool) -> Option<u64> {
        if entry {
            // The entry poll runs before the walk; the live oops there are
            // exactly the reference parameters.
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
    /// `index`, or `None` if it has no register or no home was reserved.
    ///
    /// Homes sit after the operand area: `local_spill_count() + max_stack + k`,
    /// where `k` numbers the register-homed locals in order.
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

    /// Frame offset of the slot reserved for operand-stack depth `depth`, or
    /// `None` when `depth` is outside the operand area.
    ///
    /// The operand area is `[local_spill_count(), local_spill_count() +
    /// max_stack)`. `max_stack` counts JVM slots and every entry takes at least
    /// one, so every legal depth has a slot. A depth at or past `max_stack`
    /// would land on a safepoint home or the id word, so it refuses instead.
    fn spill_offset_for_depth(&self, depth: usize) -> Option<i32> {
        if depth >= self.max_stack {
            return None;
        }
        let frame = self.frame.as_ref()?;
        let spill_slot = self.local_spill_count().checked_add(depth)?;
        if spill_slot >= frame.num_spills {
            return None;
        }
        let scaled = i32::try_from(spill_slot).ok()?.checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// Run the shared operand-stack kind analysis over `bytecode`.
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

    /// Whether each of the top `want` entries is category 2, top first, or
    /// `None` when the stack holds fewer entries.
    ///
    /// The stack shuffles are specified in JVM SLOTS and categories, and this
    /// model has one entry per VALUE, typed. So each shuffle arm resolves its
    /// JVMS form from these categories, and a form the JVMS does not define
    /// (a `dup` of a `long`, say) refuses the method.
    fn top_categories(&self, want: usize) -> Option<Vec<bool>> {
        let n = self.operand_stack.len();
        if n < want {
            return None;
        }
        Some(
            self.operand_stack[n - want..]
                .iter()
                .rev()
                .map(|o| o.kind.is_category2())
                .collect(),
        )
    }

    /// A fresh register holding a copy of the `kind` value in `reg`.
    fn copy_value(&mut self, reg: Arm64Register, kind: OperandKind) -> Arm64Register {
        let dst = self.alloc_reg(kind.is_fp());
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFpSingle { vd: dst, vn: reg },
            OperandKind::F64 => Arm64Instruction::FmovFp { vd: dst, vn: reg },
            _ => Arm64Instruction::Mov { rd: dst, rm: reg },
        };
        self.buffer.emit(inst);
        dst
    }

    /// The shared duplicate shuffle, `[under.., group..] -> [group'.., under..,
    /// group..]`, counted in ENTRIES. Every `dup*` form is one of these once
    /// its categories are resolved: `dup` is (1, 0), `dup_x1` (1, 1), `dup2` of
    /// two category-1 values (2, 0), and so on.
    fn emit_dup_group_over(&mut self, group_entries: usize, under_entries: usize) {
        let mut group = Vec::with_capacity(group_entries);
        for _ in 0..group_entries {
            match self.pop_entry() {
                Some(e) => group.push(e),
                None => return,
            }
        }
        let mut under = Vec::with_capacity(under_entries);
        for _ in 0..under_entries {
            match self.pop_entry() {
                Some(e) => under.push(e),
                None => return,
            }
        }
        let copies: Vec<Arm64Register> = group
            .iter()
            .map(|&(reg, e)| self.copy_value(reg, e.kind))
            .collect();
        for (&copy, &(_, e)) in copies.iter().zip(group.iter()).rev() {
            self.push_like(e, copy);
        }
        for &(reg, e) in under.iter().rev() {
            self.push_like(e, reg);
        }
        for &(reg, e) in group.iter().rev() {
            self.push_like(e, reg);
        }
    }

    /// Get or create a label for a bytecode PC.
    ///
    /// Also the single chokepoint where a **loop back-edge** is detected: every
    /// branch target on this backend -- `goto`, `if*`, and every switch case and
    /// default -- is resolved through here, so a target at or before the
    /// instruction being lowered is exactly the set of back-edges.
    ///
    /// A compiled loop needs a safepoint poll: without one a thread inside it
    /// never observes a stop-the-world request and any GC that needs to stop
    /// it hangs the VM. With polls on (`CRATONVM_JIT_ARM64_SAFEPOINTS`) the
    /// target is RECORDED and pass 2 emits a poll at the loop header. With
    /// polls off there is nothing to put there, so the method is refused and
    /// interpreted -- deliberately for provably terminating loops too, because
    /// the property that matters is bounded time to the next safepoint.
    fn label_for_pc(&mut self, pc: usize) -> u32 {
        if pc <= self.cur_bytecode_pc {
            if self.safepoints_enabled {
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
    /// the discovery pass already identified as a branch target. That step is
    /// not itself a branch (and at `pc == 0` the check would misfire).
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

    /// Emit the AAPCS64 prologue.
    ///
    /// ```text
    ///     STP  X29, X30, [SP, #-16]!   ; the frame record
    ///     ADD  X29, SP, #0             ; FP -> the record: [FP] = caller FP, [FP+8] = LR
    ///     <stack bang: SUB X16, SP, #off; STR XZR, [X16] per page crossed>
    ///     SUB  SP, SP, #(frame_size - 16)
    ///     STP/STR callee-saved GPRs at [FP - 8 ...]
    ///     <stamp the safepoint-id slot unset>
    /// ```
    ///
    /// # The frame record is the standard one now
    ///
    /// The previous prologue set FP to the caller's SP, leaving the saved pair
    /// at `[FP-16]`/`[FP-8]`. AAPCS64 (and Darwin, and every unwinder, and the
    /// frame-pointer walk in `vm/src/jit/helpers.rs`) expects FP to point AT the
    /// record: `[FP]` = caller's FP, `[FP+8]` = LR. A walk through one of these
    /// frames read the caller's LR as its FP. Every FP-relative offset is
    /// rebased accordingly in `Arm64FrameLayout::compute`.
    ///
    /// `ADD X29, SP, #0` and not `MOV X29, SP`: `Mov` lowers to `ORR`, where
    /// register 31 is XZR, so it would set FP to zero.
    ///
    /// # The stack bang
    ///
    /// Before SP moves, every page the frame will cross is touched, so stack
    /// exhaustion faults ON the guard page (recoverable) instead of a large
    /// `SUB SP` stepping clean past it into unrelated memory. x64 does the same
    /// (`emit_stack_bang_before_frame_alloc`). This replaces the old refusal
    /// of every frame of 4096 bytes or more.
    fn emit_prologue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let frame_size = frame.frame_size;
        let callee_save_offset = frame.callee_save_offset;
        let saved_len = frame.saved_regs.len();

        self.buffer.emit(Arm64Instruction::StpPre {
            rt1: Arm64Register::FP,
            rt2: Arm64Register::LR,
            rn: Arm64Register::SP,
            offset: -16,
        });
        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::FP,
            rn: Arm64Register::SP,
            imm: 0,
        });

        let below = frame_size - 16;
        let Some(probes) = stack_bang_probe_offsets(below) else {
            self.failed = true;
            return;
        };
        for off in probes {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::X16,
                rn: Arm64Register::SP,
                imm: off,
            });
            // Register 31 as a store's Rt is XZR: the probe writes zero.
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::XZR,
                rn: Arm64Register::X16,
                offset: 0,
            });
        }
        if below > 0 {
            self.buffer.emit(Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: below,
            });
        }

        // Save callee-saved registers used for locals (in pairs), below FP.
        let mut i = 0;
        while i + 1 < saved_len {
            // Cast: i < 10 (CALLEE_SAVED.len()).
            let offset = callee_save_offset + (i as i32) * 8;
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
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            let rt = self.frame.as_ref().expect("frame present").saved_regs[i];
            self.buffer.emit(Arm64Instruction::Str {
                rt,
                rn: Arm64Register::FP,
                offset,
            });
        }

        // STAMP THE SAFEPOINT-ID SLOT with "this frame has not reached a
        // safepoint yet" (`SP_ID_UNSET_BC_PC`, which matches no map). Left
        // uninitialised it could read as a valid id. X17 is not an argument
        // register, so this is safe before the arguments are homed.
        if self.sp_id_slot_off != 0 {
            self.buffer.emit(Arm64Instruction::MovImm {
                rd: Arm64Register::X17,
                imm: crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64,
            });
            self.buffer.emit(Arm64Instruction::Str {
                rt: Arm64Register::X17,
                rn: Arm64Register::FP,
                offset: -self.sp_id_slot_off,
            });
        }
        // The method-entry poll is NOT emitted here: X0-X7 still hold the
        // arguments until `emit_argument_homing`, and the poll's call may
        // destroy them. See `the_entry_poll_runs_after_the_argument_copy`.
    }

    /// Deposit each incoming argument in its JVM local's home.
    ///
    /// The VM passes one 64-bit register per ARGUMENT (`this` first), and an
    /// `int` arrives sign-extended, a `float` as its zero-extended bit pattern
    /// and a `double` as its bits. Argument `i` goes to JVM local
    /// `arg_slots[i]`, which differs from `i` after any `long`/`double`
    /// argument. A register home gets a `MOV`; a frame home gets a `STR` of
    /// the whole word, which the local's later loads read at their own width.
    ///
    /// The previous prologue moved X_i into local i's register and did nothing
    /// for a frame-homed local -- which every `float`/`double` local is, since
    /// the allocator never gives one a GPR -- so every FP parameter, and every
    /// parameter after a `long`/`double`, read garbage.
    fn emit_argument_homing(&mut self, arg_slots: &[usize]) {
        if arg_slots.len() > Arm64CallingConvention::INT_ARG_REGS.len() {
            // Arguments past the eighth arrive on the stack, which this
            // prologue does not read.
            self.failed = true;
            return;
        }
        for (i, &slot) in arg_slots.iter().enumerate() {
            let arg = Arm64CallingConvention::INT_ARG_REGS[i];
            match self.local_regs.get(slot).copied() {
                Some(Some(home)) => {
                    if home != arg {
                        self.buffer
                            .emit(Arm64Instruction::Mov { rd: home, rm: arg });
                    }
                }
                Some(None) => {
                    let Some(offset) = self.local_slot_offset(slot) else {
                        self.failed = true;
                        return;
                    };
                    self.buffer.emit(Arm64Instruction::Str {
                        rt: arg,
                        rn: Arm64Register::FP,
                        offset,
                    });
                }
                // A parameter slot past `max_locals`: a malformed method.
                None => {
                    self.failed = true;
                    return;
                }
            }
        }
    }

    /// Emit the epilogue: restore the callee-saved GPRs, `ADD SP, X29, #0`
    /// (again not `MOV`, for the same register-31 reason), `LDP X29, X30,
    /// [SP], #16`, `RET`.
    fn emit_epilogue(&mut self) {
        let Some(frame) = self.frame.as_ref() else {
            self.failed = true;
            return;
        };
        let callee_save_offset = frame.callee_save_offset;
        let saved = frame.saved_regs.clone();

        self.buffer.bind_label(self.epilogue_label);

        let mut i = 0;
        while i + 1 < saved.len() {
            // Cast: i < 10.
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
            // Cast: i < 10.
            let offset = callee_save_offset + (i as i32) * 8;
            self.buffer.emit(Arm64Instruction::Ldr {
                rt: saved[i],
                rn: Arm64Register::FP,
                offset,
            });
        }

        self.buffer.emit(Arm64Instruction::AddImm {
            rd: Arm64Register::SP,
            rn: Arm64Register::FP,
            imm: 0,
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

    /// Compile a JVM bytecode method to ARM64 instructions.
    ///
    /// `method_info` maps constant pool indices (from invokestatic operands) to
    /// the number of arguments the target method expects.
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
    /// Runs [`Arm64Backend::compile_pass`] **twice**. Branch targets are
    /// discovered as each branch is decoded, which is too late for a BACKWARD
    /// branch: its target was walked past before the label existed. Pass 1
    /// discovers every target (and every target's operand-stack shape, and
    /// every loop header); pass 2 re-walks with them in hand.
    pub fn compile_method_with_info(
        &mut self,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        bytecode: &[u8],
        method_info: HashMap<u16, usize>,
    ) -> Arm64CompileResult {
        // Discovered by pass 1 and read by pass 2, so `compile_pass` must not
        // clear them; cleared here, per compile.
        self.back_edge_targets.clear();
        self.label_states.clear();

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
        // `failed` is sticky and intentionally NOT cleared between the passes.

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
    /// runs twice and what `branch_targets` carries between the passes.
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
        self.held.clear();
        self.reachable = true;
        self.pc_labels.clear();
        self.cur_bytecode_pc = 0;
        self.local_regs.clear();
        self.float_local_regs.clear();
        self.num_params = num_params;
        self.method_info = method_info;
        self.stack_kinds = Self::analyze_stack_kinds(bytecode);
        self.max_stack = max_stack;
        self.pending_local_oop_slots.clear();
        self.pending_operand_oop_slots.clear();
        self.safepoint_count = 0;
        self.incomplete_oop_maps = 0;
        self.pending_map_incomplete = false;
        // The same "must be oop" local dataflow x64 uses, seeded with this
        // method's reference parameters.
        let (lo_masks, lo_reached) = crate::x64::compute_local_oop_masks(
            bytecode,
            bytecode.len(),
            num_locals,
            self.param_oop_mask,
        );
        self.local_oop_masks = lo_masks;
        self.local_oop_reached = lo_reached;

        // The JVM local slot of each incoming argument.
        let arg_slots: Vec<usize> = match &self.param_jvm_slots {
            Some(slots) => slots.clone(),
            None => (0..num_params).collect(),
        };

        // Graph-coloring register allocation, told where the parameters really
        // are: its liveness seeds them live-on-entry, and seeding the identity
        // layout left a parameter after a `long`/`double` dead on entry, free to
        // share a register with a live one.
        let alloc = super::regalloc::allocate_registers_arm64_with_param_slots(
            bytecode,
            bytecode.len(),
            num_locals,
            num_params,
            &arg_slots,
            &[],
        );

        for &a in &alloc.assignments {
            self.local_regs.push(a.map(Arm64Register));
        }
        while self.local_regs.len() < num_locals {
            self.local_regs.push(None);
        }

        // Float/double locals get NO dedicated FP register on this backend.
        // `regalloc::ARM64_LOCAL_FPS` is D8-D15, which AAPCS64 makes callee-saved,
        // and this prologue saves only GPRs -- homing a float local there
        // destroyed the caller's copy (aarch64 parity audit, 2026-08-01). They
        // live in frame slots. If FP homing is wanted back, the prerequisite is
        // an FP save area in `Arm64FrameLayout::compute` driven by
        // `alloc.used_xmm_regs`. Asserted by
        // `float_locals_never_use_callee_saved_fp_regs`.
        for _ in 0..num_locals {
            self.float_local_regs.push(None);
        }

        let saved_regs: Vec<Arm64Register> = alloc
            .used_callee_saved
            .iter()
            .map(|&n| Arm64Register(n))
            .collect();

        // Frame words: the frame-homed locals, the operand area, one safepoint
        // home per register-homed local and the safepoint-id word (the last two
        // only when this compilation polls). See `safepoint_home_for_reg_local`.
        let gpr_spills = self.local_spill_count();
        let safepoint_homes = if self.safepoints_enabled {
            saved_regs.len()
        } else {
            0
        };
        let sp_id_words = usize::from(self.safepoints_enabled);
        let num_spills = gpr_spills + max_stack + safepoint_homes + sp_id_words;
        let layout = Arm64FrameLayout::compute(num_locals, num_spills, &saved_regs);
        self.frame = Some(layout);
        // The sp-id word sits past the locals, the operand area and the homes.
        self.sp_id_slot_off = if self.safepoints_enabled {
            let f = self.frame.as_ref().expect("just set");
            let idx = f.num_spills.saturating_sub(1);
            let off = f.spill_offset + (idx as i32) * 8; // Cast: bounded by num_spills
                                                         // The runtime reads `[frame_base - off]`, so publish the magnitude.
            -off
        } else {
            0
        };

        self.epilogue_label = self.buffer.new_label();

        self.emit_prologue();
        self.emit_argument_homing(&arg_slots);
        // Publish the frame base BEFORE the first poll stamps an id into it.
        self.emit_frame_record();
        // METHOD-ENTRY SAFEPOINT POLL, after the arguments are homed: its call
        // may destroy X0-X7.
        self.emit_safepoint_poll(true);

        let mut pc = 0;
        let mut success = true;
        while pc < bytecode.len() {
            // Pre-seed this PC's label if the discovery pass saw a branch to it.
            if branch_targets.binary_search(&pc).is_ok() {
                let _ = self.label_for_pc_unchecked(pc);
            }
            self.held.clear();

            if let Some(&label) = self.pc_labels.get(&pc) {
                if self.reachable {
                    self.arrive_at_target(pc);
                } else {
                    self.restore_stack_at(pc);
                }
                if !self.buffer.labels.contains_key(&label) {
                    self.buffer.bind_label(label);
                }
            } else if !self.reachable {
                self.restore_stack_at(pc);
            }
            self.reachable = true;

            let opcode = bytecode[pc];
            let start_pc = pc;
            // Published before the loop-header poll (whose id and oop-local mask
            // are this pc's) and before lowering (so `label_for_pc` can tell a
            // back-edge from a forward branch).
            self.cur_bytecode_pc = start_pc;

            // LOOP-HEADER SAFEPOINT POLL, after the label so a back edge lands on
            // it -- one poll per header regardless of how many branches target it.
            if self.back_edge_targets.contains(&pc) {
                self.emit_safepoint_poll(false);
            }
            pc += 1;

            match opcode {
                // nop
                0x00 => self.buffer.emit(Arm64Instruction::Nop),
                // aconst_null
                0x01 => {
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::MovImm { rd: dst, imm: 0 });
                    self.push_reg(OperandKind::Ref, dst);
                }
                // iconst_m1 .. iconst_5
                0x02..=0x08 => self.emit_iconst(i32::from(opcode) - 3),
                // lconst_0, lconst_1
                0x09 | 0x0a => self.emit_lconst(i64::from(opcode - 0x09)),
                // fconst_0 .. fconst_2
                0x0b..=0x0d => self.emit_fconst(f32::from(opcode - 0x0b)),
                // dconst_0, dconst_1
                0x0e | 0x0f => self.emit_dconst(f64::from(opcode - 0x0e)),
                // bipush
                0x10 => {
                    let Some(v) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    // Cast: bipush's operand is a signed byte.
                    self.emit_iconst(i32::from(v as i8));
                }
                // sipush
                0x11 => {
                    let Some(v) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_iconst(i32::from(v));
                }
                // ldc / ldc_w / ldc2_w: this backend is never handed the
                // constant pool, so it cannot recover the constant. Refuse.
                0x12..=0x14 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "ldc family: constant pool not available — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // iload lload fload dload aload
                0x15..=0x19 => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_load_local(usize::from(idx), LOCAL_KINDS[usize::from(opcode - 0x15)]);
                }
                // iload_0 .. aload_3
                0x1a..=0x2d => {
                    let n = opcode - 0x1a;
                    self.emit_load_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // istore lstore fstore dstore astore
                0x36..=0x3a => {
                    let Some(idx) = bc_u8(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 1;
                    self.emit_store_local(
                        usize::from(idx),
                        LOCAL_KINDS[usize::from(opcode - 0x36)],
                    );
                }
                // istore_0 .. astore_3
                0x3b..=0x4e => {
                    let n = opcode - 0x3b;
                    self.emit_store_local(usize::from(n % 4), LOCAL_KINDS[usize::from(n / 4)]);
                }
                // pop: one category-1 value.
                0x57 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.drop_top(),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // pop2: one category-2 value, or two category-1 values.
                0x58 => match self.top_categories(1) {
                    Some(c) if c[0] => self.drop_top(),
                    Some(_) => match self.top_categories(2) {
                        Some(c) if !c[1] => {
                            self.drop_top();
                            self.drop_top();
                        }
                        _ => {
                            success = false;
                            break;
                        }
                    },
                    None => {
                        success = false;
                        break;
                    }
                },
                // dup: category-1 top.
                0x59 => match self.top_categories(1) {
                    Some(c) if !c[0] => self.emit_dup_group_over(1, 0),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x1: both category-1.
                0x5a => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => self.emit_dup_group_over(1, 1),
                    _ => {
                        success = false;
                        break;
                    }
                },
                // dup_x2: FORM 1 is three category-1 values; FORM 2 a category-1
                // top over one category-2 value.
                0x5b => {
                    let under = match self.top_categories(2) {
                        Some(c) if !c[0] && c[1] => Some(1),
                        Some(c) if !c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(under) = under else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(1, under);
                }
                // dup2: FORM 1 two category-1 values; FORM 2 one category-2.
                0x5c => {
                    let group = match self.top_categories(1) {
                        Some(c) if c[0] => Some(1),
                        Some(_) => match self.top_categories(2) {
                            Some(c) if !c[1] => Some(2),
                            _ => None,
                        },
                        None => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 0);
                }
                // dup2_x1: FORM 1 two category-1 over one; FORM 2 one category-2
                // over one. The entry underneath is category-1 in both.
                0x5d => {
                    let group = match self.top_categories(2) {
                        Some(c) if c[0] && !c[1] => Some(1),
                        Some(c) if !c[0] && !c[1] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some(2),
                            _ => None,
                        },
                        _ => None,
                    };
                    let Some(group) = group else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, 1);
                }
                // dup2_x2, in this backend's one-entry-per-value model:
                //   FORM 4  v1,v2 cat-2   [v2, v1]         -> [v1, v2, v1]
                //   FORM 2  v1 cat-2      [v3, v2, v1]     -> [v1, v3, v2, v1]
                //   FORM 3  v3 cat-2      [v3, v2, v1]     -> [v2, v1, v3, v2, v1]
                //   FORM 1  all cat-1     [v4, v3, v2, v1] -> [v2, v1, v4, v3, v2, v1]
                0x5e => {
                    let shape = match self.top_categories(2) {
                        Some(c) if c[0] && c[1] => Some((1usize, 1usize)),
                        Some(c) if c[0] => match self.top_categories(3) {
                            Some(c3) if !c3[2] => Some((1, 2)),
                            _ => None,
                        },
                        Some(c) if !c[1] => match self.top_categories(3) {
                            Some(c3) if c3[2] => Some((2, 1)),
                            Some(_) => match self.top_categories(4) {
                                Some(c4) if !c4[3] => Some((2, 2)),
                                _ => None,
                            },
                            None => None,
                        },
                        _ => None,
                    };
                    let Some((group, under)) = shape else {
                        success = false;
                        break;
                    };
                    self.emit_dup_group_over(group, under);
                }
                // swap: both category-1.
                0x5f => match self.top_categories(2) {
                    Some(c) if !c[0] && !c[1] => {
                        let (Some(v1), Some(v2)) = (self.pop_entry(), self.pop_entry()) else {
                            success = false;
                            break;
                        };
                        self.push_like(v1.1, v1.0);
                        self.push_like(v2.1, v2.0);
                    }
                    _ => {
                        success = false;
                        break;
                    }
                },
                // iadd ladd fadd dadd
                0x60 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AddW { rd, rn, rm }
                }),
                0x61 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Add { rd, rn, rm }
                }),
                0x62 | 0x63 => self.emit_binary_fp(
                    if opcode == 0x62 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FaddSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FaddDouble { vd, vn, vm },
                ),
                // isub lsub fsub dsub
                0x64 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::SubW { rd, rn, rm }
                }),
                0x65 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Sub { rd, rn, rm }
                }),
                0x66 | 0x67 => self.emit_binary_fp(
                    if opcode == 0x66 {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FsubSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FsubDouble { vd, vn, vm },
                ),
                // imul lmul fmul dmul
                0x68 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::MulW { rd, rn, rm }
                }),
                0x69 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Mul { rd, rn, rm }
                }),
                0x6a | 0x6b => self.emit_binary_fp(
                    if opcode == 0x6a {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FmulSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FmulDouble { vd, vn, vm },
                ),
                // idiv / ldiv / irem / lrem -- refused. There is no exception
                // path on this backend: the old divide-by-zero guard was `BRK #1`
                // (SIGTRAP, which nothing converts), and AArch64 `SDIV` by zero
                // yields 0, so `x % 0` silently returned `x`.
                0x6c | 0x6d | 0x70 | 0x71 => {
                    success = false;
                    break;
                }
                // fdiv ddiv
                0x6e | 0x6f => self.emit_binary_fp(
                    if opcode == 0x6e {
                        OperandKind::F32
                    } else {
                        OperandKind::F64
                    },
                    |vd, vn, vm| Arm64Instruction::FdivSingle { vd, vn, vm },
                    |vd, vn, vm| Arm64Instruction::FdivDouble { vd, vn, vm },
                ),
                // frem / drem -- refused: there is no exact lowering (the
                // truncating FCVTZS round-trip saturates for |a/b| >= 2^63, and
                // Java's remainder is exact across the whole range).
                0x72 | 0x73 => {
                    self.buffer.emit(Arm64Instruction::Comment(
                        "frem/drem: no exact lowering — bailing to interpreter".into(),
                    ));
                    success = false;
                    break;
                }
                // ineg lneg fneg dneg
                0x74 => {
                    let src = self.pop_kind(OperandKind::I32);
                    let dst = self.alloc_reg(false);
                    self.buffer
                        .emit(Arm64Instruction::NegW { rd: dst, rn: src });
                    self.emit_sxtw(dst);
                    self.push_reg(OperandKind::I32, dst);
                }
                0x75 => {
                    let src = self.pop_kind(OperandKind::I64);
                    let dst = self.alloc_reg(false);
                    self.buffer.emit(Arm64Instruction::Neg { rd: dst, rn: src });
                    self.push_reg(OperandKind::I64, dst);
                }
                0x76 => self.emit_float_neg(OperandKind::F32),
                0x77 => self.emit_float_neg(OperandKind::F64),
                // ishl lshl ishr lshr iushr lushr. The W-form variable shifts take
                // the amount MOD 32 and the X forms MOD 64, which is exactly the
                // JVMS `& 0x1f` / `& 0x3f`.
                0x78 => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LslW {
                    rd,
                    rn,
                    rm,
                }),
                0x79 => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsl {
                    rd,
                    rn,
                    rm,
                }),
                0x7a => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::AsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7b => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Asr {
                    rd,
                    rn,
                    rm,
                }),
                0x7c => self.emit_shift(OperandKind::I32, |rd, rn, rm| Arm64Instruction::LsrW {
                    rd,
                    rn,
                    rm,
                }),
                0x7d => self.emit_shift(OperandKind::I64, |rd, rn, rm| Arm64Instruction::Lsr {
                    rd,
                    rn,
                    rm,
                }),
                // iand land ior lor ixor lxor
                0x7e => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::AndW { rd, rn, rm }
                }),
                0x7f => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::And { rd, rn, rm }
                }),
                0x80 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::OrrW { rd, rn, rm }
                }),
                0x81 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Orr { rd, rn, rm }
                }),
                0x82 => self.emit_binary_int(OperandKind::I32, |rd, rn, rm| {
                    Arm64Instruction::EorW { rd, rn, rm }
                }),
                0x83 => self.emit_binary_int(OperandKind::I64, |rd, rn, rm| {
                    Arm64Instruction::Eor { rd, rn, rm }
                }),
                // iinc
                0x84 => {
                    let (Some(idx), Some(delta)) = (bc_u8(bytecode, pc), bc_u8(bytecode, pc + 1))
                    else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: iinc's constant is a signed byte.
                    self.emit_iinc(usize::from(idx), i32::from(delta as i8));
                }
                // i2l l2i i2f i2d l2f l2d f2i f2l f2d d2i d2l d2f i2b i2c i2s
                0x85..=0x93 => self.emit_conversion(opcode),
                // lcmp
                0x94 => self.emit_lcmp(),
                // fcmpl fcmpg dcmpl dcmpg
                0x95 => self.emit_fcmp(OperandKind::F32, false),
                0x96 => self.emit_fcmp(OperandKind::F32, true),
                0x97 => self.emit_fcmp(OperandKind::F64, false),
                0x98 => self.emit_fcmp(OperandKind::F64, true),
                // ifeq ifne iflt ifge ifgt ifle
                0x99..=0x9e => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x99)];
                    self.emit_if_zero(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_icmpeq .. if_icmple
                0x9f..=0xa4 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = IF_CONDS[usize::from(opcode - 0x9f)];
                    self.emit_if_icmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // if_acmpeq if_acmpne
                0xa5 | 0xa6 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let cond = if opcode == 0xa5 {
                        Arm64Condition::Eq
                    } else {
                        Arm64Condition::Ne
                    };
                    self.emit_if_acmp(cond, branch_target(start_pc, i32::from(off)));
                }
                // goto
                0xa7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    let label = self.prepare_branch(branch_target(start_pc, i32::from(off)));
                    self.buffer.emit(Arm64Instruction::B { label });
                    self.reachable = false;
                }
                // tableswitch. The `checked_tableswitch_count` audit rejects
                // adversarial overflow / oversize tables, as on x64.
                0xaa => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(low), Some(high)) = (
                        bc_i32(bytecode, pc),
                        bc_i32(bytecode, pc + 4),
                        bc_i32(bytecode, pc + 8),
                    ) else {
                        success = false;
                        break;
                    };
                    pc += 12;
                    let Some(count) = super::x64::checked_tableswitch_count(low, high) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut targets = Vec::with_capacity(count);
                    for _ in 0..count {
                        let Some(off) = bc_i32(bytecode, pc) else {
                            break;
                        };
                        pc += 4;
                        targets.push(self.prepare_branch(branch_target(start_pc, off)));
                    }
                    if targets.len() != count {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::TableSwitch {
                        key,
                        low,
                        default,
                        targets,
                    });
                    self.reachable = false;
                }
                // lookupswitch, with `checked_lookupswitch_npairs` validation.
                0xab => {
                    let key = self.pop_kind(OperandKind::I32);
                    while pc % 4 != 0 {
                        pc += 1;
                    }
                    let (Some(default_off), Some(npairs_raw)) =
                        (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                    else {
                        success = false;
                        break;
                    };
                    pc += 8;
                    let Some(npairs) = super::x64::checked_lookupswitch_npairs(npairs_raw) else {
                        success = false;
                        break;
                    };
                    let default = self.prepare_branch(branch_target(start_pc, default_off));
                    let mut pairs = Vec::with_capacity(npairs);
                    for _ in 0..npairs {
                        let (Some(value), Some(off)) =
                            (bc_i32(bytecode, pc), bc_i32(bytecode, pc + 4))
                        else {
                            break;
                        };
                        pc += 8;
                        pairs.push((value, self.prepare_branch(branch_target(start_pc, off))));
                    }
                    if pairs.len() != npairs {
                        success = false;
                        break;
                    }
                    self.buffer.emit(Arm64Instruction::LookupSwitch {
                        key,
                        pairs,
                        default,
                    });
                    self.reachable = false;
                }
                // ireturn lreturn freturn dreturn areturn
                0xac..=0xb0 => {
                    const RETURN_KINDS: [OperandKind; 5] = LOCAL_KINDS;
                    self.emit_return_value(RETURN_KINDS[usize::from(opcode - 0xac)]);
                }
                // return
                0xb1 => {
                    self.buffer.emit(Arm64Instruction::B {
                        label: self.epilogue_label,
                    });
                    self.reachable = false;
                }
                // invokestatic -- always refused; see `emit_invoke`.
                0xb8 => {
                    let Some(cp_idx) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    // Cast: the constant-pool index is an unsigned u2.
                    let num_args = self
                        .method_info
                        .get(&(cp_idx as u16))
                        .copied()
                        .unwrap_or(self.num_params);
                    self.emit_invoke(num_args);
                }
                // ifnull ifnonnull
                0xc6 | 0xc7 => {
                    let Some(off) = bc_i16(bytecode, pc) else {
                        success = false;
                        break;
                    };
                    pc += 2;
                    self.emit_if_null(opcode == 0xc7, branch_target(start_pc, i32::from(off)));
                }
                _ => {
                    self.buffer.emit(Arm64Instruction::Comment(format!(
                        "unsupported opcode 0x{:02x} at pc={}",
                        opcode, start_pc
                    )));
                    success = false;
                    break;
                }
            }
        }

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
                    sp_id_slot_off: 0,
                    safepoint_count: 0,
                    incomplete_oop_maps: 0,
                }
            }
        };
        Arm64CompileResult {
            instructions: self.buffer.instructions.clone(),
            frame,
            labels: self.buffer.labels.clone(),
            success: success && !self.failed,
            sp_id_slot_off: self.sp_id_slot_off,
            safepoint_count: self.safepoint_count,
            incomplete_oop_maps: self.incomplete_oop_maps,
            pending_oop_maps: std::mem::take(&mut self.pending_oop_maps),
        }
    }

    // -- Arithmetic ----------------------------------------------------------

    /// A two-operand integer operation on `kind` operands. For `I32` the
    /// instruction is a W form, whose result is the JVMS 32-bit wrapped value,
    /// and the result is sign-extended back to the canonical form.
    fn emit_binary_int(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, lhs, rhs));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A shift of a `kind` value by an `int` amount.
    fn emit_shift(
        &mut self,
        kind: OperandKind,
        make: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let amount = self.pop_kind(OperandKind::I32);
        let value = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        self.buffer.emit(make(dst, value, amount));
        if kind == OperandKind::I32 {
            self.emit_sxtw(dst);
        }
        self.push_reg(kind, dst);
    }

    /// A two-operand FP operation, in the S form for `float` and the D form
    /// for `double`. `float` used to be computed as `double` end to end, which
    /// gets rounding, overflow and the bits handed back to the VM all wrong.
    fn emit_binary_fp(
        &mut self,
        kind: OperandKind,
        single: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
        double: fn(Arm64Register, Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            single(dst, lhs, rhs)
        } else {
            double(dst, lhs, rhs)
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `fneg` / `dneg`: FNEG, which flips the sign bit. The old `0.0 - x`
    /// turned `-(+0.0)` into `+0.0` rather than `-0.0`.
    fn emit_float_neg(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let dst = self.alloc_reg(true);
        let inst = if kind == OperandKind::F32 {
            Arm64Instruction::FnegSingle { vd: dst, vn: src }
        } else {
            Arm64Instruction::FnegDouble { vd: dst, vn: src }
        };
        self.buffer.emit(inst);
        self.push_reg(kind, dst);
    }

    /// `iinc`: a W-form add, re-sign-extended, on the local's home.
    fn emit_iinc(&mut self, index: usize, delta: i32) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let add = |rd: Arm64Register| {
            if delta >= 0 {
                Arm64Instruction::AddImmW {
                    rd,
                    rn: rd,
                    imm: delta,
                }
            } else {
                Arm64Instruction::SubImmW {
                    rd,
                    rn: rd,
                    imm: -delta,
                }
            }
        };
        match home {
            Some(reg) => {
                self.buffer.emit(add(reg));
                self.emit_sxtw(reg);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                let tmp = self.alloc_reg(false);
                self.buffer.emit(Arm64Instruction::Ldr {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
                self.buffer.emit(add(tmp));
                self.emit_sxtw(tmp);
                self.buffer.emit(Arm64Instruction::Str {
                    rt: tmp,
                    rn: Arm64Register::FP,
                    offset,
                });
            }
        }
    }

    /// The numeric conversions `i2l` (0x85) through `i2s` (0x93).
    ///
    /// Width discipline, given that an `int` is held sign-extended:
    /// * `i2l` is a relabelling -- the register already holds the `long`.
    /// * `l2i` is `SXTW`, keeping the low 32 bits as a signed value. It used to
    ///   `AND` with `0xFFFFFFFF`, i.e. zero-extend, so `(int) -1L` read back as
    ///   4294967295 anywhere the upper half was observed.
    /// * `f2i`/`d2i` are `FCVTZS W` then `SXTW`: the W form saturates at the
    ///   32-bit bounds, as the JVMS requires. The X form they used saturated at
    ///   the 64-bit bounds, so `(int) 1e10` was not `Integer.MAX_VALUE`.
    /// * `i2f`/`i2d` read the W register; `l2f`/`l2d` read the X register.
    fn emit_conversion(&mut self, opcode: u8) {
        use OperandKind::{F32, F64, I32, I64};
        match opcode {
            // i2l
            0x85 => {
                let Some(top) = self.operand_stack.last_mut() else {
                    self.failed = true;
                    return;
                };
                if top.kind != I32 {
                    self.failed = true;
                    return;
                }
                top.kind = I64;
            }
            0x86 => self.emit_convert(I32, F32, false, |d, s| Arm64Instruction::ScvtfSingle {
                vd: d,
                rn: s,
            }),
            0x87 => self.emit_convert(I32, F64, false, |d, s| Arm64Instruction::ScvtfDoubleW {
                vd: d,
                rn: s,
            }),
            0x88 => self.emit_convert(I64, I32, false, |d, s| Arm64Instruction::Sxtw {
                rd: d,
                rn: s,
            }),
            0x89 => self.emit_convert(I64, F32, false, |d, s| Arm64Instruction::ScvtfSingleX {
                vd: d,
                rn: s,
            }),
            0x8a => self.emit_convert(I64, F64, false, |d, s| Arm64Instruction::ScvtfDouble {
                vd: d,
                rn: s,
            }),
            0x8b => self.emit_convert(F32, I32, true, |d, s| Arm64Instruction::FcvtzsSingle {
                rd: d,
                vn: s,
            }),
            0x8c => self.emit_convert(F32, I64, false, |d, s| Arm64Instruction::FcvtzsSingleX {
                rd: d,
                vn: s,
            }),
            0x8d => self.emit_convert(F32, F64, false, |d, s| {
                Arm64Instruction::FcvtSingleToDouble { vd: d, vn: s }
            }),
            0x8e => self.emit_convert(F64, I32, true, |d, s| Arm64Instruction::FcvtzsIntW {
                rd: d,
                vn: s,
            }),
            0x8f => self.emit_convert(F64, I64, false, |d, s| Arm64Instruction::FcvtzsInt {
                rd: d,
                vn: s,
            }),
            0x90 => self.emit_convert(F64, F32, false, |d, s| {
                Arm64Instruction::FcvtDoubleToSingle { vd: d, vn: s }
            }),
            // i2b, i2c (zero-extend 16 bits), i2s
            0x91 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxtb {
                rd: d,
                rn: s,
            }),
            0x92 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::AndImm {
                rd: d,
                rn: s,
                imm: 0xFFFF,
            }),
            0x93 => self.emit_convert(I32, I32, false, |d, s| Arm64Instruction::Sxth {
                rd: d,
                rn: s,
            }),
            _ => self.failed = true,
        }
    }

    /// Pop a `from`, emit `make(dst, src)` into a fresh register of the `to`
    /// class, sign-extend when `sxtw`, and push a `to`.
    fn emit_convert(
        &mut self,
        from: OperandKind,
        to: OperandKind,
        sxtw: bool,
        make: fn(Arm64Register, Arm64Register) -> Arm64Instruction,
    ) {
        let src = self.pop_kind(from);
        let dst = self.alloc_reg(to.is_fp());
        self.buffer.emit(make(dst, src));
        if sxtw {
            self.emit_sxtw(dst);
        }
        self.push_reg(to, dst);
    }

    // -- Compare / Branch ---------------------------------------------------

    /// `lcmp`: `CMP; CSET ne; CNEG lt` -- 1, 0 or -1 with no branch.
    fn emit_lcmp(&mut self) {
        let rhs = self.pop_kind(OperandKind::I64);
        let lhs = self.pop_kind(OperandKind::I64);
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: Arm64Condition::Lt,
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `fcmpl`/`fcmpg`/`dcmpl`/`dcmpg`: `FCMP; CSET ne; CNEG <c>`.
    ///
    /// After `FCMP`: equal is `Z=1 C=1`, less `N=1`, greater `C=1`, and
    /// unordered `C=1 V=1`. `CSET ne` gives 1 for everything but equal; the
    /// negation then decides where NaN lands:
    ///
    /// * `*cmpg` (NaN -> +1) negates on `MI` (`N=1`), which only "less" sets.
    /// * `*cmpl` (NaN -> -1) negates on `LT` (`N!=V`), which "less" and
    ///   "unordered" both satisfy.
    ///
    /// The branchy version it replaces took `B.LT` for `*cmpg`, which is TRUE
    /// on unordered, so NaN produced -1 where the JVMS requires +1.
    fn emit_fcmp(&mut self, kind: OperandKind, nan_is_greater: bool) {
        let rhs = self.pop_kind(kind);
        let lhs = self.pop_kind(kind);
        let dst = self.alloc_reg(false);
        let cmp = if kind == OperandKind::F32 {
            Arm64Instruction::FcmpSingle { vn: lhs, vm: rhs }
        } else {
            Arm64Instruction::FcmpDouble { vn: lhs, vm: rhs }
        };
        self.buffer.emit(cmp);
        self.buffer.emit(Arm64Instruction::Cset {
            rd: dst,
            cond: Arm64Condition::Ne,
        });
        self.buffer.emit(Arm64Instruction::Cneg {
            rd: dst,
            rn: dst,
            cond: if nan_is_greater {
                Arm64Condition::Mi
            } else {
                Arm64Condition::Lt
            },
        });
        self.push_reg(OperandKind::I32, dst);
    }

    /// `if_icmp<cond>`: a W-form compare.
    pub fn emit_if_icmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::I32);
        let lhs = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        self.buffer
            .emit(Arm64Instruction::CmpW { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if_acmp<cond>`: references are full 64-bit words.
    fn emit_if_acmp(&mut self, cond: Arm64Condition, target_pc: usize) {
        let rhs = self.pop_kind(OperandKind::Ref);
        let lhs = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        self.buffer.emit(Arm64Instruction::Cmp { rn: lhs, rm: rhs });
        self.buffer.emit(Arm64Instruction::BCond { cond, label });
    }

    /// `if<cond>` against zero, on the W register.
    fn emit_if_zero(&mut self, cond: Arm64Condition, target_pc: usize) {
        let val = self.pop_kind(OperandKind::I32);
        let label = self.prepare_branch(target_pc);
        match cond {
            Arm64Condition::Eq => self.buffer.emit(Arm64Instruction::CbzW { rt: val, label }),
            Arm64Condition::Ne => self.buffer.emit(Arm64Instruction::CbnzW { rt: val, label }),
            _ => {
                self.buffer
                    .emit(Arm64Instruction::CmpImmW { rn: val, imm: 0 });
                self.buffer.emit(Arm64Instruction::BCond { cond, label });
            }
        }
    }

    /// `ifnull` / `ifnonnull`.
    fn emit_if_null(&mut self, nonnull: bool, target_pc: usize) {
        let val = self.pop_kind(OperandKind::Ref);
        let label = self.prepare_branch(target_pc);
        if nonnull {
            self.buffer.emit(Arm64Instruction::Cbnz { rt: val, label });
        } else {
            self.buffer.emit(Arm64Instruction::Cbz { rt: val, label });
        }
    }

    // -- Load / Store locals ------------------------------------------------

    /// FP-relative offset of frame-homed local `index`'s word.
    fn local_slot_offset(&self, index: usize) -> Option<i32> {
        let frame = self.frame.as_ref()?;
        let scaled = i32::try_from(self.spill_index_for(index))
            .ok()?
            .checked_mul(8)?;
        frame.spill_offset.checked_add(scaled)
    }

    /// Push a copy of local `index`, read as a `kind`.
    fn emit_load_local(&mut self, index: usize, kind: OperandKind) {
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        let dst = self.alloc_reg(kind.is_fp());
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovToFpSingle { vd: dst, rn: reg },
                    OperandKind::F64 => Arm64Instruction::FmovToFp { vd: dst, rn: reg },
                    _ => Arm64Instruction::Mov { rd: dst, rm: reg },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_load_kind(dst, kind, offset);
            }
        }
        self.push_reg(kind, dst);
    }

    /// Pop a `kind` and store it to local `index`.
    fn emit_store_local(&mut self, index: usize, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let Some(home) = self.local_regs.get(index).copied() else {
            self.failed = true;
            return;
        };
        match home {
            Some(reg) => {
                let inst = match kind {
                    OperandKind::F32 => Arm64Instruction::FmovFromFpSingle { rd: reg, vn: src },
                    OperandKind::F64 => Arm64Instruction::FmovFromFp { rd: reg, vn: src },
                    _ => Arm64Instruction::Mov { rd: reg, rm: src },
                };
                self.buffer.emit(inst);
            }
            None => {
                let Some(offset) = self.local_slot_offset(index) else {
                    self.failed = true;
                    return;
                };
                self.emit_store_kind(src, kind, offset);
            }
        }
    }

    /// How many spill slots the FRAME-HOMED LOCALS occupy -- equivalently, the
    /// first spill index the operand area uses. Both the operand area and the
    /// safepoint homes take their base from here, or they overlap the locals
    /// (see `operand_spill_slots_do_not_alias_frame_homed_locals`).
    fn local_spill_count(&self) -> usize {
        self.spill_index_for(self.local_regs.len())
    }

    /// The spill index of a local that has no register: how many locals before
    /// it also have none.
    fn spill_index_for(&self, local_index: usize) -> usize {
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

    // -- Constants ----------------------------------------------------------

    /// An `int` constant, sign-extended -- already the canonical form.
    pub fn emit_iconst(&mut self, value: i32) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: i64::from(value),
        });
        self.push_reg(OperandKind::I32, dst);
    }

    pub fn emit_lconst(&mut self, value: i64) {
        let dst = self.alloc_reg(false);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: dst,
            imm: value,
        });
        self.push_reg(OperandKind::I64, dst);
    }

    /// A `float` constant, bit-exact: its IEEE-754 single pattern into a GPR,
    /// then `FMOV Sd, Wn` (a bit move, not a conversion). It used to load the
    /// DOUBLE pattern, so every `float` was a `double` in disguise.
    pub fn emit_fconst(&mut self, value: f32) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: i64::from(value.to_bits()),
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFpSingle { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F32, dst);
    }

    /// A `double` constant, bit-exact via `FMOV Dd, Xn`.
    pub fn emit_dconst(&mut self, value: f64) {
        let tmp = self.alloc_reg(false);
        let dst = self.alloc_reg(true);
        // Cast: reinterpret the f64 as its raw 64-bit pattern.
        self.buffer.emit(Arm64Instruction::MovImm {
            rd: tmp,
            imm: value.to_bits() as i64,
        });
        self.buffer
            .emit(Arm64Instruction::FmovToFp { vd: dst, rn: tmp });
        self.push_reg(OperandKind::F64, dst);
    }

    // -- Invoke -------------------------------------------------------------

    /// Refuse the method: there is no call-target resolution on this backend.
    ///
    /// Emitting `BL` to an unbound label was an infinite self-call (ARM64 BUG
    /// #3). If a concrete target ever becomes available, the lowering is
    /// `mov_imm64 IP0, <target>; BLR IP0` with the arguments marshalled and
    /// the operand stack spilled around the call, as the safepoint poll does.
    pub fn emit_invoke(&mut self, num_args: usize) {
        self.failed = true;
        self.buffer.emit(Arm64Instruction::Comment(format!(
            "unresolved invoke ({} args) — bailing to interpreter",
            num_args
        )));
    }

    // -- Return -------------------------------------------------------------

    /// `ireturn`/`lreturn`/`areturn`/`freturn`/`dreturn`.
    ///
    /// The VM calls compiled code as `extern "C" fn(i64, ..) -> i64` and reads
    /// EVERY result out of X0, decoding a `float` as `f32::from_bits(x0 as
    /// u32)` and a `double` as `f64::from_bits(x0)`. So an FP result is moved
    /// into X0 with a bit-exact `FMOV` (`FMOV W0, Sn` for a `float`), not left
    /// in V0. `freturn`/`dreturn` used to pop and DISCARD the value and emit a
    /// bare `RET`, skipping the epilogue: the caller's callee-saved registers
    /// were never restored, SP and FP were left pointing into this frame, and
    /// the "result" was whatever X0 held.
    fn emit_return_value(&mut self, kind: OperandKind) {
        let src = self.pop_kind(kind);
        let inst = match kind {
            OperandKind::F32 => Arm64Instruction::FmovFromFpSingle {
                rd: Arm64Register::X0,
                vn: src,
            },
            OperandKind::F64 => Arm64Instruction::FmovFromFp {
                rd: Arm64Register::X0,
                vn: src,
            },
            _ => Arm64Instruction::Mov {
                rd: Arm64Register::X0,
                rm: src,
            },
        };
        self.buffer.emit(inst);
        self.buffer.emit(Arm64Instruction::B {
            label: self.epilogue_label,
        });
        self.reachable = false;
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
        Arm64Condition::Mi => crate::aarch64::Cond::MI,
        Arm64Condition::Pl => crate::aarch64::Cond::PL,
        Arm64Condition::Vs => crate::aarch64::Cond::VS,
        Arm64Condition::Vc => crate::aarch64::Cond::VC,
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
    // IP0 = base + IP0, in the EXTENDED-register form. The shifted-register
    // ADD reads register 31 as XZR, so with an SP base it computed `0 + offset`
    // and the access went to an absolute address. The extended form reads 31
    // as SP and is otherwise the same instruction.
    emitter.add_ext(
        crate::aarch64::Reg::X16,
        base,
        crate::aarch64::Reg::X16,
        crate::aarch64::Extend::UXTX,
        0,
    );
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
/// The register fallback uses the EXTENDED-register form, so it is sound with
/// SP as `rd` or `rn` too. It used to be the shifted-register form, where 31
/// is XZR, so an SP adjustment wider than 12 bits had no lowering at all -- it
/// first emitted `BRK #0` while reporting success, later refused the method,
/// and either way a frame of 4096 bytes or more could not be allocated.
///
/// Returns `false` only when the fallback would clobber its own operand (`rn`
/// is IP0), in which case the caller must abandon the method.
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
    } else if rn.enc() == 16 {
        // The fallback materializes into IP0, which would destroy an operand
        // that is itself IP0 before it is read. No lowering emits that shape.
        false
    } else {
        // Out of immediate range: materialize into IP0 (X16) and use the
        // extended-register form, which reads 31 as SP on both sides. Never
        // silently truncate.
        emitter.mov_imm64(crate::aarch64::Reg::X16, mag);
        if effective_sub {
            emitter.sub_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        } else {
            emitter.add_ext(
                rd,
                rn,
                crate::aarch64::Reg::X16,
                crate::aarch64::Extend::UXTX,
                0,
            );
        }
        true
    }
}

/// Lower `rd = rn +/- imm` on W registers (`AddImmW`/`SubImmW`).
///
/// A 32-bit result depends only on the addend mod 2^32, so a wide immediate is
/// materialized as its low 32 bits. `false` when that would clobber `rn`.
#[must_use]
fn emit_addsub_imm_w(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    imm: i32,
    is_sub: bool,
) -> bool {
    let signed = if is_sub {
        -i64::from(imm)
    } else {
        i64::from(imm)
    };
    let mag = signed.unsigned_abs();
    if mag <= 0xFFF {
        // Cast: bounds-checked immediately above.
        if signed < 0 {
            emitter.sub_imm_w(rd, rn, mag as u16, false);
        } else {
            emitter.add_imm_w(rd, rn, mag as u16, false);
        }
        true
    } else if rn.enc() == 16 {
        false
    } else {
        // Cast: the low 32 bits of the two's-complement addend.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(signed as u32));
        emitter.add_w(rd, rn, crate::aarch64::Reg::X16);
        true
    }
}

/// `rd = rn - value` on W registers, through IP0 when `value` is wide.
fn emit_w_sub_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rd: crate::aarch64::Reg,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.sub_imm_w(rd, rn, v as u16, false);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.add_imm_w(rd, rn, (-v) as u16, false);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.sub_w(rd, rn, crate::aarch64::Reg::X16);
    }
}

/// Set the flags for `Wrn - value`: `CMP #imm`, `CMN #-imm`, or a compare
/// against IP0. Never a scratch register, so it cannot collide with `rn`.
fn emit_w_cmp_const(
    emitter: &mut crate::aarch64::Aarch64Emitter,
    rn: crate::aarch64::Reg,
    value: i32,
) {
    let v = i64::from(value);
    if (0..=0xFFF).contains(&v) {
        // Cast: range-checked.
        emitter.cmp_imm_w(rn, v as u16);
    } else if (-0xFFF..0).contains(&v) {
        // Cast: range-checked.
        emitter.cmn_imm_w(rn, (-v) as u16);
    } else {
        // Cast: the constant's 32-bit pattern.
        emitter.mov_imm64(crate::aarch64::Reg::X16, u64::from(value as u32));
        emitter.cmp_w(rn, crate::aarch64::Reg::X16);
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
    // (entry_offset, table_base, label_id) for jump-table words
    let mut table_patches: Vec<(usize, usize, u32)> = Vec::new();

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

            // -- 32-bit (W) integer forms --
            Arm64Instruction::AddW { rd, rn, rm } => emitter.add_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::SubW { rd, rn, rm } => emitter.sub_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::MulW { rd, rn, rm } => emitter.mul_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::AndW { rd, rn, rm } => emitter.and_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::OrrW { rd, rn, rm } => emitter.orr_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::EorW { rd, rn, rm } => emitter.eor_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::LslW { rd, rn, rm } => emitter.lsl_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::LsrW { rd, rn, rm } => emitter.lsr_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::AsrW { rd, rn, rm } => emitter.asr_w(r(*rd), r(*rn), r(*rm)),
            Arm64Instruction::NegW { rd, rn } => emitter.neg_w(r(*rd), r(*rn)),
            Arm64Instruction::AddImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r(*rd), r(*rn), *imm, false) {
                    return None;
                }
            }
            Arm64Instruction::SubImmW { rd, rn, imm } => {
                if !emit_addsub_imm_w(&mut emitter, r(*rd), r(*rn), *imm, true) {
                    return None;
                }
            }
            Arm64Instruction::CmpW { rn, rm } => emitter.cmp_w(r(*rn), r(*rm)),
            Arm64Instruction::CmpImmW { rn, imm } => emit_w_cmp_const(&mut emitter, r(*rn), *imm),
            Arm64Instruction::CbzW { rt, label } => {
                let pos = emitter.cbz_w(r(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::CbnzW { rt, label } => {
                let pos = emitter.cbnz_w(r(*rt), 0);
                branch_patches.push((pos, *label, true));
            }
            Arm64Instruction::Sxtw { rd, rn } => emitter.sxtw(r(*rd), r(*rn)),
            Arm64Instruction::Sxth { rd, rn } => emitter.sxth(r(*rd), r(*rn)),
            Arm64Instruction::Sxtb { rd, rn } => emitter.sxtb(r(*rd), r(*rn)),
            Arm64Instruction::AndImm { rd, rn, imm } => {
                if !emitter.and_imm(r(*rd), r(*rn), *imm) {
                    if rn.0 == 16 {
                        return None;
                    }
                    emitter.mov_imm64(crate::aarch64::Reg::X16, *imm);
                    emitter.and(r(*rd), r(*rn), crate::aarch64::Reg::X16);
                }
            }
            Arm64Instruction::Cset { rd, cond } => emitter.cset(r(*rd), to_cond(cond)),
            Arm64Instruction::Cneg { rd, rn, cond } => {
                emitter.cneg(r(*rd), r(*rn), to_cond(cond));
            }

            // -- FP width forms --
            Arm64Instruction::FmovToFpSingle { vd, rn } => emitter.fmov_s_from_w(fp(*vd), r(*rn)),
            Arm64Instruction::FmovFromFpSingle { rd, vn } => emitter.fmov_w_from_s(r(*rd), fp(*vn)),
            Arm64Instruction::FmovFpSingle { vd, vn } => emitter.fmov_s(fp(*vd), fp(*vn)),
            Arm64Instruction::ScvtfDoubleW { vd, rn } => emitter.scvtf_d_w(fp(*vd), r(*rn)),
            Arm64Instruction::ScvtfSingleX { vd, rn } => emitter.scvtf_s_x(fp(*vd), r(*rn)),
            Arm64Instruction::FcvtzsIntW { rd, vn } => emitter.fcvtzs_w_d(r(*rd), fp(*vn)),
            Arm64Instruction::FcvtzsSingleX { rd, vn } => emitter.fcvtzs_x_s(r(*rd), fp(*vn)),

            // -- Switches --
            Arm64Instruction::TableSwitch {
                key,
                low,
                default,
                targets,
            } => {
                use crate::aarch64::{Cond, Reg};
                let Some(max_index) = targets
                    .len()
                    .checked_sub(1)
                    .and_then(|m| u32::try_from(m).ok())
                else {
                    return None;
                };
                // X17 = key - low, wrapping at 32 bits, so a key below `low`
                // becomes a huge index and fails the unsigned check below.
                emit_w_sub_const(&mut emitter, Reg::X17, r(*key), *low);
                if max_index <= 0xFFF {
                    // Cast: range-checked.
                    emitter.cmp_imm_w(Reg::X17, max_index as u16);
                } else {
                    emitter.mov_imm64(Reg::X16, u64::from(max_index));
                    emitter.cmp_w(Reg::X17, Reg::X16);
                }
                let to_default = emitter.b_cond(Cond::HI, 0);
                branch_patches.push((to_default, *default, true));
                let adr = emitter.adr(Reg::X16, 0);
                emitter.ldrsw_reg_uxtw_scaled(Reg::X17, Reg::X16, Reg::X17);
                emitter.add(Reg::X16, Reg::X16, Reg::X17);
                emitter.br(Reg::X16);
                let table = emitter.offset();
                emitter.patch_adr(adr, table);
                for &label in targets {
                    let entry = emitter.emit_u32_data(0);
                    table_patches.push((entry, table, label));
                }
            }
            Arm64Instruction::LookupSwitch {
                key,
                pairs,
                default,
            } => {
                for &(value, label) in pairs {
                    emit_w_cmp_const(&mut emitter, r(*key), value);
                    let pos = emitter.b_cond(crate::aarch64::Cond::EQ, 0);
                    branch_patches.push((pos, label, true));
                }
                let pos = emitter.b(0);
                branch_patches.push((pos, *default, false));
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

    // Patch jump-table words: each is the signed distance from the table base
    // to its case, which the dispatch adds to the base it loaded with `ADR`.
    for &(entry, base, label) in &table_patches {
        let target = match label_offsets.get(&label) {
            Some(&t) => t,
            None => return None,
        };
        let Ok(delta) = i32::try_from(target as i64 - base as i64) else {
            return None;
        };
        // Cast: the two's-complement word `LDRSW` sign-extends back.
        emitter.patch_u32(entry, delta as u32);
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

/// This frame's storage-class partition, for the GC's band verifier.
///
/// Without it `CompiledMethod::frame_layout` stays all-zero, and a zero layout
/// tells the verifier that NOTHING is a register image -- so the prologue's
/// saved FP/LR pair and the caller's saved X19-X28 all count as in-band words
/// of THIS frame. Those hold the CALLER's live references, which this frame's
/// maps have no business naming, so the oracle would report them `never_mapped`
/// and refute a coverage claim on pure noise. An oracle that cries wolf is
/// worse than one that is off.
///
/// The saved FP/LR pair is the frame record AT FP (`[FP]`, `[FP+8]`), outside
/// the `[FP - size, FP)` band altogether -- where x86-64 keeps its saved RBP and
/// return address. Below FP come the callee-saved GPRs, and the spill area
/// BELOW those, which is the mirror of x86-64's order. `callee_saved_shallow`
/// says so, so the verifier uses the range exclusion instead of x86-64's
/// "everything at or beyond `callee_saved_lo`" half-line, which here would
/// swallow the spill area -- the one region the oop maps describe.
///
/// Offsets are positive, meaning `[FP - off]`, matching the x64 convention the
/// consumer expects.
fn arm64_frame_layout(frame: &Arm64FrameLayout) -> crate::FrameLayout {
    let mut out = crate::FrameLayout::default();
    // Register images: the callee-saved GPRs, `[FP-8]` down to
    // `[FP-callee_save_bytes]`. With none saved there is no image in the band.
    if !frame.saved_regs.is_empty() {
        out.callee_saved_lo = 8;
        // `-callee_save_offset` is the DEEPEST saved-register offset; `+8`
        // makes the range half-open over it.
        out.callee_saved_hi = (-frame.callee_save_offset) + 8;
    }
    out.callee_saved_shallow = true;
    // The spill area: frame-homed locals, then operand slots, then the
    // safepoint homes and the sp-id word. Every one of those is described by
    // the dataflow or the operand marks, which is what lets the verifier treat
    // a word the active map does not name as DEAD rather than missed.
    if frame.num_spills > 0 {
        let deepest = -frame.spill_offset; // slot 0 is the deepest word
        let shallowest = -(frame.spill_offset + (frame.num_spills as i32 - 1) * 8);
        out.spill_lo = shallowest;
        out.spill_hi = deepest + 8;
    }
    // `java_locals_hi` is deliberately left 0: this backend has no
    // `[FP - (i+1)*8]` local convention -- a local is either in a callee-saved
    // register or in the spill area above.
    out
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
    // never sets -- not for want of the safepoint-id slot, which exists now,
    // but because suppressing the conservative scan is a claim no non-aarch64
    // host can earn.
    cm.oop_maps = oop_maps;
    // The slot the maps above are keyed through. Without it the runtime's
    // `active_safepoint_id` returns `None` and no map can be selected by id.
    cm.sp_id_slot_off = result.sp_id_slot_off;
    // The storage-class partition the band verifier needs to tell this frame's
    // words from the caller's saved registers. Publishing a zero layout would
    // make the oracle report the caller's live references as this frame's
    // missed roots.
    cm.frame_layout = arm64_frame_layout(&result.frame);
    // FULLY OOP COVERED -- the claim that lets the collector SUPPRESS its
    // conservative scan of these frames, so every term is a thing that had to
    // be built rather than assumed:
    //
    //   * an id slot, or `active_safepoint_id` returns `None` and no map can be
    //     selected at all;
    //   * at least one map, so the claim is not vacuously true for a method
    //     that emitted no safepoint (a method with no poll is not "covered",
    //     it is unobserved);
    //   * one map per safepoint, so every id resolves -- `find_oop_map_for_safepoint_id`
    //     finding nothing is indistinguishable from a frame that is not covered;
    //   * and NO safepoint that failed to describe what was live at it
    //     (`incomplete_oop_maps`), which is a count and not a set of bytecode
    //     pcs, because two safepoints can share one bci and a set lets a
    //     complete map mask an incomplete one beside it.
    //
    // It can only be true when `CRATONVM_JIT_ARM64_SAFEPOINTS` is on, since
    // nothing else reserves the slot -- so a default build is unchanged.
    //
    // WHAT IT STILL RESTS ON, stated because it is the whole risk: the
    // operand-oop marks are exact BY CONSTRUCTION (lockstep push/pop, `dup`
    // carrying its mark, and references entering only through `aconst_null` and
    // `aload*`), the local oop-ness comes from the flow-sensitive dataflow, and
    // none of it has ever been EXECUTED, because no host here runs aarch64. The
    // runtime oracle `CRATONVM_DBG_VERIFY_OOP_MAPS` is the check that turns this
    // from a construction into evidence, and it should be armed on the first
    // aarch64 run.
    cm.fully_oop_covered = result.sp_id_slot_off != 0
        && !cm.oop_maps.is_empty()
        && cm.oop_maps.len() == result.safepoint_count
        && result.incomplete_oop_maps == 0;
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
            // The safepoint id this map belongs to, and the reason the frame
            // stamps the same value into its sp-id slot: this is what
            // `find_oop_map_for_safepoint_id` matches on, so the collector
            // selects the map for the site the frame is ACTUALLY standing at
            // rather than a union over every safepoint in the method.
            bytecode_pc: pending.safepoint_id,
            frame_slot_offsets: pending.frame_slot_offsets.clone(),
            // No shadow stack and no relocation support on this backend, and
            // register-resident oops are covered only by the CONSERVATIVE walk
            // (see `emit_oop_map_for_safepoint`) -- which marks but cannot
            // rewrite. Claiming moving-young coverage here would be the exact
            // false claim `relocation_coverage_complete` exists to prevent.
            moving_young_coverage_complete: false,
            // AArch64 publishes no blind GPR spill image, so there is no
            // slot set for a mask to narrow. `None` is the honest answer.
            reg_oop_mask: None,
            live_frame_hi: 0,
            local_oop_mask: None,
            num_locals: 0,
            inline_local_scopes: Vec::new(),
            non_oop_stack_slots: Vec::new(),
            stack_marks_exact: false,
            shadow_pushed: 0,
        });
    }
    Some((code, maps))
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
            .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
        assert!(has_add, "iadd should emit the 32-bit AddW");
    }

    #[test]
    fn backend_int_sub_emits_sub() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x64, 0xac]);
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SubW { .. }));
        assert!(has_sub, "isub should emit the 32-bit SubW");
    }

    #[test]
    fn backend_int_mul_emits_mul() {
        let result = make_backend_with_method(0, 0, &[0x04, 0x05, 0x68, 0xac]);
        let has_mul = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::MulW { .. }));
        assert!(has_mul, "imul should emit the 32-bit MulW");
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
            .any(|inst| matches!(inst, Arm64Instruction::NegW { .. }));
        assert!(has_neg, "ineg should emit the 32-bit NegW");
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
            .any(|inst| matches!(inst, Arm64Instruction::AddW { .. }));
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

    /// `float` arithmetic lowers to the S forms and `double` to the D forms.
    ///
    /// Both used to lower to the D forms: `float` was modelled as `double`
    /// end to end, so rounding, overflow and the bits handed back to the VM
    /// were a double's.
    #[test]
    fn float_and_double_arithmetic_use_their_own_widths() {
        fn single(op: u8) -> Arm64CompileResult {
            make_backend_with_method(0, 0, &[0x0c, 0x0c, op, 0xb1])
        }
        fn double(op: u8) -> Arm64CompileResult {
            make_backend_with_method(0, 0, &[0x0f, 0x0f, op, 0xb1])
        }
        fn has(r: &Arm64CompileResult, f: fn(&Arm64Instruction) -> bool) -> bool {
            r.success && r.instructions.iter().any(|i| f(i))
        }
        assert!(has(&single(0x62), |i| matches!(
            i,
            Arm64Instruction::FaddSingle { .. }
        )));
        assert!(has(&single(0x66), |i| matches!(
            i,
            Arm64Instruction::FsubSingle { .. }
        )));
        assert!(has(&single(0x6a), |i| matches!(
            i,
            Arm64Instruction::FmulSingle { .. }
        )));
        assert!(has(&single(0x6e), |i| matches!(
            i,
            Arm64Instruction::FdivSingle { .. }
        )));
        assert!(has(&double(0x63), |i| matches!(
            i,
            Arm64Instruction::FaddDouble { .. }
        )));
        assert!(has(&double(0x67), |i| matches!(
            i,
            Arm64Instruction::FsubDouble { .. }
        )));
        assert!(has(&double(0x6b), |i| matches!(
            i,
            Arm64Instruction::FmulDouble { .. }
        )));
        assert!(has(&double(0x6f), |i| matches!(
            i,
            Arm64Instruction::FdivDouble { .. }
        )));
        assert!(
            !single(0x62)
                .instructions
                .iter()
                .any(|i| matches!(i, Arm64Instruction::FaddDouble { .. })),
            "fadd must not use the double form"
        );
        // A float operand is not a double: `dadd` of two `fconst`s is refused.
        assert!(!single(0x63).success, "dadd over floats is ill-typed");
    }

    /// `i2f` converts from the W register into an S register, `i2d` into a D
    /// register, and `l2f`/`l2d` from the X register.
    #[test]
    fn integer_to_fp_conversions_pick_source_and_destination_widths() {
        fn conv(code: &[u8]) -> Arm64CompileResult {
            make_backend_with_method(0, 0, code)
        }
        let i2f = conv(&[0x04, 0x86, 0xb1]);
        assert!(i2f.success);
        assert!(i2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfSingle { .. })));
        let i2d = conv(&[0x04, 0x87, 0xb1]);
        assert!(i2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfDoubleW { .. })));
        let l2f = conv(&[0x0a, 0x89, 0xb1]);
        assert!(l2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfSingleX { .. })));
        let l2d = conv(&[0x0a, 0x8a, 0xb1]);
        assert!(l2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::ScvtfDouble { .. })));
        // f2d / d2f are real conversions now, not no-ops.
        let f2d = conv(&[0x0c, 0x8d, 0xb1]);
        assert!(f2d
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FcvtSingleToDouble { .. })));
        let d2f = conv(&[0x0f, 0x90, 0xb1]);
        assert!(d2f
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FcvtDoubleToSingle { .. })));
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

    /// `fneg`/`dneg` flip the sign bit with FNEG.
    ///
    /// Formerly `backend_float_neg_emits_fsub`, which asserted the bug: the
    /// lowering computed `0.0 - x`, and `0.0 - 0.0` is `+0.0`, so `-(0.0f)`
    /// lost its sign.
    #[test]
    fn fneg_and_dneg_use_fneg_not_a_subtraction_from_zero() {
        for (code, single) in [
            (&[0x0c, 0x76, 0xb1][..], true),
            (&[0x0f, 0x77, 0xb1][..], false),
        ] {
            let result = make_backend_with_method(0, 0, code);
            assert!(result.success);
            let neg = result.instructions.iter().any(|i| {
                if single {
                    matches!(i, Arm64Instruction::FnegSingle { .. })
                } else {
                    matches!(i, Arm64Instruction::FnegDouble { .. })
                }
            });
            assert!(neg, "negation must be FNEG of the operand's own width");
            assert!(
                !result.instructions.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::FsubDouble { .. } | Arm64Instruction::FsubSingle { .. }
                )),
                "0.0 - x turns -0.0 into +0.0"
            );
        }
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
        // A constant must move the IEEE-754 bit pattern, NOT perform an
        // integer→float conversion, which would round 2.5 down to 2.0. And a
        // `float` constant must be the SINGLE pattern into an S register.
        let mut single = Arm64Backend::new();
        single.emit_fconst(2.5);
        assert!(single.buffer.instructions().iter().any(
            |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == i64::from(2.5f32.to_bits()))
        ));
        assert!(single
            .buffer
            .instructions()
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovToFpSingle { .. })));
        assert_eq!(single.operand_stack[0].kind, OperandKind::F32);

        let mut backend = Arm64Backend::new();
        backend.emit_dconst(2.5);
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
            .any(|inst| matches!(inst, Arm64Instruction::FcmpSingle { .. }));
        assert!(has_fcmp, "fcmpl of two floats should emit FcmpSingle");
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
            .any(|inst| matches!(inst, Arm64Instruction::AddImmW { imm: 5, .. }));
        assert!(has_add, "iinc +5 should emit the 32-bit AddImmW with imm=5");
    }

    #[test]
    fn p95_iinc_negative_compiles() {
        // iinc 0 -1
        let result = make_backend_with_method(1, 1, &[0x84, 0x00, 0xFF, 0x1a, 0xac]);
        assert!(result.success, "iinc -1 should compile");
        let has_sub = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::SubImmW { imm: 1, .. }));
        assert!(has_sub, "iinc -1 should emit the 32-bit SubImmW with imm=1");
    }

    #[test]
    fn p95_iushr_compiles() {
        // iconst_1, iconst_1, iushr, ireturn
        let result = make_backend_with_method(0, 0, &[0x04, 0x04, 0x7c, 0xac]);
        assert!(result.success, "iushr should compile");
        let has_lsr = result
            .instructions
            .iter()
            .any(|inst| matches!(inst, Arm64Instruction::LsrW { .. }));
        assert!(
            has_lsr,
            "iushr should emit the 32-bit LsrW, which shifts by the amount mod 32"
        );
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
        // lconst_1, iconst_1, lshl, iconst_1, lushr, lreturn. The shift amount
        // is an `int` (JVMS 6.5 `lshl`); an earlier version of this test shifted
        // by a `long`, which the typed operand stack now refuses.
        let result = make_backend_with_method(0, 0, &[0x0a, 0x04, 0x79, 0x04, 0x7d, 0xad]);
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
            safepoint_count: 0,
            incomplete_oop_maps: 0,
            sp_id_slot_off: 0,
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
            sp_id_slot_off: 0,
            safepoint_count: 0,
            incomplete_oop_maps: 0,
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
        let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
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

    /// The poll leaves the compile-time operand model exactly as it found it.
    ///
    /// Its stores and reloads sit INSIDE the `CBZ`-skipped block, so the model
    /// must say the same thing on both paths: every operand in the register it
    /// was in. Asserting the model is unchanged is how a non-executing host
    /// checks that.
    #[test]
    fn the_poll_restores_the_operand_model() {
        let mut b = poll_backend();
        b.frame = Some(Arm64FrameLayout::compute(0, 8, &[]));
        b.max_stack = 8;
        b.operand_stack = vec![
            Operand::in_reg(OperandKind::Ref, Arm64Register::X9),
            Operand::in_reg(OperandKind::I32, Arm64Register::X10),
            Operand::in_reg(OperandKind::F64, Arm64Register::V0),
        ];
        let before = b.operand_stack.clone();

        b.emit_safepoint_poll(false);

        assert!(!b.failed, "the poll must not refuse this frame");
        assert_eq!(
            b.operand_stack, before,
            "the poll must leave the model as it found it"
        );
        // The oop map DID record the reference operand's slot: the store is
        // what makes it nameable, and the map is taken at the call.
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].frame_slot_offsets.len(),
            1,
            "only the reference operand belongs in the map"
        );
    }

    /// FLOATING-POINT OPERANDS SURVIVE THE POLL'S CALL.
    ///
    /// V0-V7 are as caller-saved as X9-X15, and the poll used to store only the
    /// GPR operand stack: a float or double live across a taken poll was
    /// whatever the slow path left in its register.
    #[test]
    fn the_poll_stores_and_reloads_floating_point_operands() {
        let mut b = poll_backend();
        b.frame = Some(Arm64FrameLayout::compute(0, 8, &[]));
        b.max_stack = 8;
        b.operand_stack = vec![
            Operand::in_reg(OperandKind::F32, Arm64Register::V1),
            Operand::in_reg(OperandKind::F64, Arm64Register::V2),
        ];
        b.emit_safepoint_poll(false);
        assert!(!b.failed);

        let ops = b.buffer.instructions();
        let blr = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
            .expect("the poll calls the slow path");
        let stored = |vt: Arm64Register, double: bool| {
            ops[..blr].iter().any(|i| {
                matches!(i, Arm64Instruction::FpStr { vt: v, rn, is_double, .. }
                         if *v == vt && *rn == Arm64Register::FP && *is_double == double)
            })
        };
        let reloaded = |vt: Arm64Register, double: bool| {
            ops[blr..].iter().any(|i| {
                matches!(i, Arm64Instruction::FpLdr { vt: v, rn, is_double, .. }
                         if *v == vt && *rn == Arm64Register::FP && *is_double == double)
            })
        };
        assert!(
            stored(Arm64Register::V1, false),
            "the float is stored at its own width"
        );
        assert!(stored(Arm64Register::V2, true), "the double is stored");
        assert!(
            reloaded(Arm64Register::V1, false),
            "the float is reloaded after the call"
        );
        assert!(
            reloaded(Arm64Register::V2, true),
            "the double is reloaded after the call"
        );
    }

    /// A poll whose store has no reserved slot refuses the method.
    ///
    /// The alternative is a live value sitting in a caller-saved register
    /// across a CALL, which is the exact hazard the store exists for.
    #[test]
    fn a_poll_that_cannot_place_its_spill_refuses() {
        let mut b = poll_backend();
        b.frame = Some(Arm64FrameLayout::compute(0, 0, &[]));
        b.operand_stack = vec![Operand::in_reg(OperandKind::Ref, Arm64Register::X9)];
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
        // EVERY safepoint publishes an entry now, so its id resolves; a site
        // with nothing live publishes one that NAMES nothing.
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert!(
            b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "a primitive local must not be named"
        );
        assert_eq!(
            b.incomplete_oop_maps, 0,
            "a site with nothing live is COVERED, not incomplete"
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
        assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
        assert!(b.pending_oop_maps[0].frame_slot_offsets.is_empty());
        // ...but the METHOD may not claim coverage: "could not answer" is not
        // "nothing was live", and `fully_oop_covered` switches off the
        // conservative scan that is currently covering for it.
        assert_eq!(
            b.incomplete_oop_maps, 1,
            "an unanswerable site must sink the method's coverage claim"
        );
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
        let regs = [
            Some(Arm64Register::X19),
            None,
            Some(Arm64Register::X20),
            None,
        ];
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
        assert_eq!(b2.pending_oop_maps.len(), 1, "the id still resolves");
        assert!(
            b2.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "with no descriptor seeded the parameter is not named"
        );
    }

    /// THE ENTRY POLL MUST NOT RUN WHILE THE ARGUMENTS ARE STILL IN X0-X7.
    ///
    /// `compile_pass` copies the incoming arguments into their local registers
    /// AFTER `emit_prologue` returns. The entry poll was emitted from the END
    /// of the prologue, so its `BLR` sat between the arguments arriving and
    /// being consumed -- and X0-X7 are caller-saved, so the safepoint slow path
    /// is entitled to destroy every one of them. Every parameter of every
    /// compiled method would have been garbage on the taken path.
    ///
    /// Asserted as an ORDER over the emitted stream: no call may precede the
    /// argument copy.
    #[test]
    fn the_entry_poll_runs_after_the_argument_copy() {
        let mut b = poll_backend();
        // static (Ljava/lang/Object;)I { aload_0; areturn } -- one reference
        // parameter, so there is an argument copy to be clobbered.
        b.set_method_descriptor("(Ljava/lang/Object;)Ljava/lang/Object;", true);
        let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
        assert!(result.success, "the method must compile");

        let ops = result.instructions;
        let first_call = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }));
        let arg_copy = ops.iter().position(|i| {
            matches!(i, Arm64Instruction::Mov { rm, .. }
                     if *rm == Arm64CallingConvention::INT_ARG_REGS[0])
        });

        if let (Some(call), Some(copy)) = (first_call, arg_copy) {
            assert!(
                copy < call,
                "the argument copy (op {copy}) must precede the first call \
                 (op {call}); X0-X7 are caller-saved and the poll's slow path \
                 may destroy them"
            );
        } else {
            // If either is absent the test is vacuous -- say so rather than
            // pass silently.
            panic!(
                "expected both an argument copy and a poll call; got copy={arg_copy:?} \
                 call={first_call:?}"
            );
        }
    }

    /// The prologue STAMPS "not yet at a safepoint" into the id slot.
    ///
    /// The quiet hazard this removes: an uninitialised slot holds whatever the
    /// stack last left there, and that can READ as a valid id for the method
    /// standing at this frame base -- so a relocating collector would rewrite
    /// the frame against the wrong program point's map. `SP_ID_UNSET_BC_PC`
    /// (`u32::MAX - 1`) matches no map, so the proof fails CLOSED. It cannot be
    /// 0, because bci 0 is a legal and very common safepoint.
    #[test]
    fn the_prologue_stamps_the_id_slot_unset() {
        let mut b = poll_backend();
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_ne!(result.sp_id_slot_off, 0, "a slot must be reserved");
        let off = -result.sp_id_slot_off;

        let ops = &result.instructions;
        let stamp = ops.iter().position(|i| {
            matches!(i, Arm64Instruction::MovImm { imm, .. }
                     if *imm == crate::x64::safepoint::SP_ID_UNSET_BC_PC as i64)
        });
        let stamp = stamp.expect("the prologue must stamp the unset sentinel");
        assert!(
            matches!(ops[stamp + 1], Arm64Instruction::Str { rn, offset, .. }
                     if rn == Arm64Register::FP && offset == off),
            "the sentinel must be stored to the id slot"
        );
        // And it precedes every call, or a frame could be walked before it.
        if let Some(call) = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Blr { .. }))
        {
            assert!(stamp < call, "the stamp must precede any call");
        }
        assert_ne!(
            crate::x64::safepoint::SP_ID_UNSET_BC_PC,
            0,
            "0 is a legal bci and must never be the sentinel"
        );
    }

    /// Each safepoint stores ITS OWN id, and the map carries the same value.
    ///
    /// This is the pairing the runtime depends on: `active_safepoint_id` reads
    /// the slot, `find_oop_map_for_safepoint_id` matches it against
    /// `OopMapEntry::bytecode_pc`. If the two ever disagree the collector
    /// selects a map for a program point the frame is not standing at.
    #[test]
    fn the_stored_id_and_the_maps_id_are_the_same_value() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.sp_id_slot_off = 64;
        b.cur_bytecode_pc = 41;
        b.local_oop_masks = vec![0; 64];
        b.local_oop_reached = vec![true; 64];
        b.local_oop_masks[41] = 0b1;

        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].safepoint_id, 41,
            "the map must be keyed by the site's bci"
        );
        let stored = b
            .buffer
            .instructions()
            .iter()
            .any(|i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 41));
        assert!(stored, "the site must store its own bci into the slot");
    }

    /// The ENTRY poll uses the synthetic pc, not 0.
    #[test]
    fn the_entry_poll_uses_the_synthetic_id() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[Some(Arm64Register::X19)], 2);
        b.sp_id_slot_off = 64;
        b.set_method_descriptor("(Ljava/lang/Object;)V", true);
        b.emit_safepoint_poll(true);
        assert_eq!(b.pending_oop_maps.len(), 1);
        assert_eq!(
            b.pending_oop_maps[0].safepoint_id,
            crate::x64::safepoint::ENTRY_POLL_BC_PC as u32,
            "the entry poll must not reuse bci 0, which is a legal safepoint"
        );
    }

    /// The published artifact carries the slot, and the runtime's OWN reader
    /// selects the right map through it.
    ///
    /// The end-to-end check a non-executing host can still make: build a frame
    /// image by hand, put an id in the slot at the offset the artifact
    /// publishes, and ask `CompiledMethod::find_oop_map_for_safepoint_id` --
    /// the function the GC root walk calls -- which map that selects. Two maps
    /// with different ids make it a discrimination rather than a lookup.
    #[test]
    fn the_runtime_selects_a_map_through_the_published_slot() {
        let mut result = result_from_instructions(vec![
            Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234,
            },
            Arm64Instruction::Ret,
        ]);
        result.sp_id_slot_off = 24;
        result.pending_oop_maps = vec![
            Arm64PendingOopMap {
                pseudo_index: 1,
                frame_slot_offsets: vec![-16],
                safepoint_id: 41,
            },
            Arm64PendingOopMap {
                pseudo_index: 1,
                frame_slot_offsets: vec![-32],
                safepoint_id: 77,
            },
        ];

        let cm = publish_compiled_method(&result).expect("publishes");
        assert_eq!(
            cm.sp_id_slot_off, 24,
            "the artifact must carry the slot the maps are keyed through"
        );

        // A frame image: 8 words, with the id written where the artifact says.
        let mut frame = [0usize; 8];
        let base = frame.as_mut_ptr() as usize + frame.len() * 8;
        // SAFETY: writing inside our own array, at the published offset.
        unsafe {
            *((base - cm.sp_id_slot_off as usize) as *mut usize) = 77;
        }

        let selected: Vec<i16> = cm
            .oop_maps
            .iter()
            .filter(|m| m.bytecode_pc == 77)
            .flat_map(|m| m.frame_slot_offsets.clone())
            .collect();
        assert_eq!(
            selected,
            vec![-32],
            "the id in the slot must select the map for THAT site"
        );

        // The control: the other id selects the other map, so this is a
        // discrimination and not a single-map lookup that would pass anyway.
        let other: Vec<i16> = cm
            .oop_maps
            .iter()
            .filter(|m| m.bytecode_pc == 41)
            .flat_map(|m| m.frame_slot_offsets.clone())
            .collect();
        assert_eq!(other, vec![-16]);
        // And the unset sentinel selects NOTHING -- fail closed.
        assert!(cm
            .oop_maps
            .iter()
            .all(|m| m.bytecode_pc != crate::x64::safepoint::SP_ID_UNSET_BC_PC as u32));
    }

    /// The frame base is published, or the id is a number in a frame nothing
    /// can locate.
    #[test]
    fn the_frame_base_is_published_before_the_first_poll() {
        let mut b = poll_backend();
        // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
        let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
        h.safepoint_flag_addr = 0x1234_5678_9AB0;
        h.safepoint_slow_path = 0x7FFF_0000_1000;
        h.frame_record = 0x7FFF_0000_2000;
        b.set_helpers(h);

        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        let ops = &result.instructions;
        let record = ops
            .iter()
            .position(
                |i| matches!(i, Arm64Instruction::MovImm { imm, .. } if *imm == 0x7FFF_0000_2000),
            )
            .expect("the frame-record address must be materialized");
        let poll = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::Ldrb { .. }))
            .expect("the entry poll must be emitted");
        assert!(
            record < poll,
            "the frame base must be published before the first poll stamps an id"
        );
        // FP is what gets published -- the base the runtime subtracts from.
        assert!(
            ops[..record].iter().any(|i| {
                matches!(i, Arm64Instruction::Mov { rd, rm }
                         if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
            }),
            "arg0 must be FP"
        );
    }

    /// An unwired frame-record helper emits no call, like every other optional
    /// helper here.
    #[test]
    fn an_unwired_frame_record_emits_nothing() {
        let mut b = poll_backend(); // frame_record left 0
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert!(!result.instructions.iter().any(|i| {
            matches!(i, Arm64Instruction::Mov { rd, rm }
                     if *rd == Arm64Register::X0 && *rm == Arm64Register::FP)
        }));
    }

    /// An operand's oop mark belongs to the VALUE, not to its stack index.
    ///
    /// The marks once lived in a vector beside the stack that neither push nor
    /// pop maintained, so a mark outlived its value and was re-read for the
    /// next one: a primitive named as a reference (a relocating collector
    /// rewrites a non-pointer) or a reference lost. The mark is a field of the
    /// entry now, so it cannot drift; this pins that it does not.
    #[test]
    fn operand_oop_marks_track_the_value_not_the_index() {
        let mut b = poll_backend();
        locals_frame(&mut b, &[], 4);
        b.sp_id_slot_off = 64;

        // A reference at depth 0...
        b.push_reg(OperandKind::Ref, Arm64Register::X9);
        assert!(b.operand_stack[0].oop);

        // ...consumed...
        let _ = b.pop_operand();
        b.held.clear();
        // ...and an INT pushed into the same slot.
        b.push_reg(OperandKind::I32, Arm64Register::X10);

        b.emit_safepoint_poll(false);
        assert!(!b.failed);
        assert_eq!(b.pending_oop_maps.len(), 1, "the id must still resolve");
        assert!(
            b.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "an int at depth 0 was named as a reference: the mark from the \
             popped value survived and was re-read for the new one. map={:?}",
            b.pending_oop_maps[0]
        );
    }

    /// `fully_oop_covered` is COMPUTED, and every term can sink it.
    ///
    /// This is the claim that lets the collector suppress its conservative scan
    /// of these frames, so the test that matters is not "it can be true" but
    /// "each thing that should make it false does".
    #[test]
    fn fully_oop_covered_is_computed_from_terms_that_can_each_sink_it() {
        // A clean method: polls on, helpers wired, no locals, one entry poll.
        let mut b = poll_backend();
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_ne!(result.sp_id_slot_off, 0);
        assert_eq!(result.incomplete_oop_maps, 0);
        assert!(result.safepoint_count > 0, "the entry poll is a safepoint");
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            cm.fully_oop_covered,
            "a method whose every safepoint is described must be able to say so"
        );

        // (1) No id slot -> no map can be selected at all.
        let mut r1 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r1.sp_id_slot_off = 0;
        r1.safepoint_count = 1;
        r1.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }];
        assert!(!publish_compiled_method(&r1).unwrap().fully_oop_covered);

        // (2) A safepoint that published no map -- its id cannot resolve, and
        //     "no map for this id" is indistinguishable from "not covered".
        let mut r2 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r2.sp_id_slot_off = 24;
        r2.safepoint_count = 2; // two safepoints...
        r2.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }]; // ...one map
        assert!(!publish_compiled_method(&r2).unwrap().fully_oop_covered);

        // (3) A safepoint that could not describe what was live at it.
        let mut r3 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r3.sp_id_slot_off = 24;
        r3.safepoint_count = 1;
        r3.incomplete_oop_maps = 1;
        r3.pending_oop_maps = vec![Arm64PendingOopMap {
            pseudo_index: 0,
            frame_slot_offsets: vec![],
            safepoint_id: 5,
        }];
        assert!(!publish_compiled_method(&r3).unwrap().fully_oop_covered);

        // (4) No safepoint at all is NOT coverage -- it is a frame nothing ever
        //     observed. A vacuous true here would be the worst of the four.
        let mut r4 = result_from_instructions(vec![Arm64Instruction::Ret]);
        r4.sp_id_slot_off = 24;
        r4.safepoint_count = 0;
        assert!(!publish_compiled_method(&r4).unwrap().fully_oop_covered);
    }

    /// With polls OFF the claim is never made, so a default build is unchanged.
    #[test]
    fn safepoints_off_never_claims_coverage() {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        let result = b.compile_method(0, 0, 4, &[0xb1]);
        assert!(result.success);
        assert_eq!(result.sp_id_slot_off, 0, "no slot is reserved");
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            !cm.fully_oop_covered,
            "the default build must keep its conservative scan"
        );
        assert!(!cm.has_precise_oop_maps());
    }

    /// A method whose dataflow cannot answer keeps the conservative scan.
    ///
    /// The end-to-end version of the `incomplete_oop_maps` term: a real
    /// compile of a method with MORE THAN 64 LOCALS, where
    /// `compute_local_oop_masks` returns nothing at all.
    #[test]
    fn a_method_the_dataflow_cannot_describe_does_not_claim_coverage() {
        let mut b = poll_backend();
        // 70 locals -- past the 64-slot mask, so `compute_local_oop_masks`
        // returns nothing at all -- AND a loop, so there is a non-entry
        // safepoint that has to consult it. The entry poll alone would not do:
        // it answers from `param_oop_mask` without touching the dataflow, and
        // for a method with no reference parameters that answer is complete.
        let code = [0x03, 0x3b, 0x84, 0x00, 0x01, 0xa7, 0xFF, 0xFD];
        let result = b.compile_method(70, 0, 4, &code);
        assert!(result.success);
        assert!(
            result.incomplete_oop_maps > 0,
            "a site the dataflow cannot answer for must be counted incomplete"
        );
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            !cm.fully_oop_covered,
            "and the method must not claim coverage it cannot prove"
        );
    }

    /// The published frame layout separates the caller's saved registers from
    /// this frame's own words.
    ///
    /// A ZERO layout tells the band verifier that nothing is a register image,
    /// so the prologue's saved FP/LR pair and the caller's saved X19-X28 would
    /// count as in-band words of THIS frame. They hold the CALLER's live
    /// references, which this frame's maps have no business naming -- the
    /// oracle would report them `never_mapped` and refute the coverage claim on
    /// noise. An oracle that cries wolf is worse than one that is off.
    #[test]
    fn the_published_frame_layout_excludes_the_callers_saved_registers() {
        let mut b = poll_backend();
        let result = b.compile_method(3, 1, 4, &[0x2a, 0xb0]); // aload_0; areturn
        assert!(result.success);
        let layout = arm64_frame_layout(&result.frame);

        assert!(
            layout.callee_saved_shallow,
            "aarch64 puts the save area next to the frame pointer; saying so is \
             what stops the verifier's x86-64 half-line from swallowing the \
             whole spill area"
        );
        // The FP/LR pair is the frame record AT FP, not a word of this frame's
        // band: nothing at or above FP may be claimed.
        assert!(
            !layout.is_register_image(0),
            "[FP] is the caller's FP, outside the band"
        );
        assert!(
            !layout.is_register_image(-8),
            "[FP+8] is the return address"
        );
        // Every saved GPR is a register image.
        for (i, _) in result.frame.saved_regs.iter().enumerate() {
            let off = -(result.frame.callee_save_offset + (i as i32) * 8);
            assert!(
                layout.is_register_image(off),
                "saved register {i} at [FP-{off}] must be a register image"
            );
        }
        // The spill area is NOT a register image -- it is this frame's own
        // words, and it is where the oop maps point.
        if result.frame.num_spills > 0 {
            let deepest = -result.frame.spill_offset;
            assert!(
                !layout.is_register_image(deepest),
                "the spill area must stay visible to the verifier"
            );
            assert!(
                layout.spill_hi > layout.spill_lo,
                "the spill range must be published, or the verifier cannot tell \
                 a dead slot from a missed root"
            );
            assert!(
                deepest >= layout.spill_lo && deepest < layout.spill_hi,
                "slot 0 ({deepest}) must fall inside the published spill range \
                 [{}, {})",
                layout.spill_lo,
                layout.spill_hi
            );
        }
        // The two regions must not overlap, or a word belongs to both.
        assert!(
            layout.spill_lo >= layout.callee_saved_hi,
            "spill [{}, {}) overlaps the register images [{}, {})",
            layout.spill_lo,
            layout.spill_hi,
            layout.callee_saved_lo,
            layout.callee_saved_hi
        );
    }

    /// A published artifact carries that layout, not the all-zero default.
    #[test]
    fn a_published_artifact_carries_its_frame_layout() {
        let mut b = poll_backend();
        // aload_0; areturn -- local 0 is live, so it gets a callee-saved home.
        let result = b.compile_method(1, 1, 4, &[0x2a, 0xb0]);
        assert!(result.success);
        assert!(
            !result.frame.saved_regs.is_empty(),
            "test precondition: a saved GPR"
        );
        let cm = publish_compiled_method(&result).expect("publishes");
        assert!(
            cm.frame_layout.callee_saved_shallow,
            "the artifact must carry the aarch64 geometry"
        );
        let deepest_save = -result.frame.callee_save_offset;
        assert!(
            cm.frame_layout.is_register_image(deepest_save),
            "the caller's saved X19 at [FP-{deepest_save}] must be excluded from this frame's band"
        );
        assert_ne!(
            cm.frame_layout,
            cratonvm_jit_frame_layout_default(),
            "a zero layout would make the oracle report the caller's registers \
             as this frame's missed roots"
        );
    }

    fn cratonvm_jit_frame_layout_default() -> crate::FrameLayout {
        crate::FrameLayout::default()
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
            safepoint_id: 11,
        }];

        let expected_pc =
            emit_machine_code(&result_from_instructions(vec![Arm64Instruction::MovImm {
                rd: Arm64Register::X9,
                imm: 0x1234_5678_9ABC,
            }]))
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
            "`fully_oop_covered` licenses SUPPRESSING the conservative scan, and              nothing here can execute aarch64 to earn that -- the slot exists              now, the evidence does not"
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
            safepoint_id: 7,
        }];

        let (_code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
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
        backend.emit_oop_map_for_safepoint(0);
        assert!(
            !backend.failed,
            "the writer no longer fails the method closed"
        );
        assert_eq!(
            backend.pending_oop_maps.len(),
            1,
            "every safepoint publishes an entry, so its id resolves"
        );
        assert!(
            backend.pending_oop_maps[0].frame_slot_offsets.is_empty(),
            "...and a site with no live reference names nothing"
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
            safepoint_id: 3,
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
            safepoint_id: 3,
        }];
        let (code, maps) = emit_machine_code_with_oop_maps(&result).expect("the method encodes");
        assert_eq!(maps.len(), 1);
        assert_eq!(maps[0].native_pc_offset as usize, code.len());
    }

    /// A frame larger than a guard page compiles, and touches every page it
    /// crosses before SP moves.
    ///
    /// Formerly `oversized_frame_bails_no_stack_bang`: with no bang, `SUB SP,
    /// SP, #frame` could step clean past the guard page, so such frames were
    /// refused outright.
    #[test]
    fn a_large_frame_bangs_every_page_before_moving_sp() {
        // max_stack = 600 -> 600 operand words -> a 4816-byte frame.
        let mut big = Arm64Backend::new();
        big.set_safepoints_enabled(false);
        let result = big.compile_method(0, 0, 600, &[0xb1]);
        assert!(result.frame.frame_size >= 4096, "test precondition");
        assert!(
            result.success,
            "a large frame must compile once it is banged"
        );

        let below = result.frame.frame_size - 16;
        let ops = &result.instructions;
        let probes: Vec<i32> = ops
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::SubImm { rd, rn, imm }
                    if *rd == Arm64Register::X16 && *rn == Arm64Register::SP =>
                {
                    Some(*imm)
                }
                _ => None,
            })
            .collect();
        assert_eq!(Some(probes.clone()), stack_bang_probe_offsets(below));
        assert_eq!(
            probes.last().copied(),
            Some(below),
            "the exact frame bottom is probed"
        );

        let alloc = ops
            .iter()
            .position(|i| {
                matches!(i, Arm64Instruction::SubImm { rd, rn, .. }
                         if *rd == Arm64Register::SP && *rn == Arm64Register::SP)
            })
            .expect("the frame is allocated");
        let last_touch = ops
            .iter()
            .rposition(|i| {
                matches!(i, Arm64Instruction::Str { rt, rn, offset: 0 }
                         if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
            })
            .expect("each probe stores through X16");
        assert!(last_touch < alloc, "every page is touched BEFORE SP moves");
        assert!(
            emit_machine_code(&result).is_some(),
            "the 4800-byte SUB SP needs the extended-register form, which now exists"
        );

        // A frame under a page cannot skip the guard, and gets no probe.
        let mut small = Arm64Backend::new();
        small.set_safepoints_enabled(false);
        let ok = small.compile_method(0, 0, 4, &[0xb1]);
        assert!(ok.success);
        assert!(!ok.instructions.iter().any(|i| {
            matches!(i, Arm64Instruction::Str { rt, rn, .. }
                     if *rt == Arm64Register::XZR && *rn == Arm64Register::X16)
        }));
    }

    #[test]
    fn stack_bang_probe_offsets_cover_every_page_crossed() {
        assert_eq!(stack_bang_probe_offsets(0), Some(vec![]));
        assert_eq!(stack_bang_probe_offsets(4095), Some(vec![]));
        assert_eq!(stack_bang_probe_offsets(4096), Some(vec![4096]));
        assert_eq!(stack_bang_probe_offsets(8200), Some(vec![4096, 8192, 8200]));
        assert_eq!(stack_bang_probe_offsets(-1), None);
        assert_eq!(
            stack_bang_probe_offsets(4096 * (MAX_STACK_BANG_PROBES as i32 + 1)),
            None,
            "past the probe cap the method is refused"
        );
    }

    /// A wide SP adjustment lowers through the extended-register form.
    ///
    /// Formerly `addsub_imm_safe_refuses_unencodable_sp_adjustment`: the only
    /// register fallback was the shifted-register form, where 31 is XZR.
    #[test]
    fn addsub_imm_safe_lowers_a_wide_sp_adjustment() {
        use crate::aarch64::{Aarch64Emitter, Reg};

        let mut e = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(&mut e, Reg::SP, Reg::SP, 5000, true));
        let n = e.code().len();
        assert_eq!(n, 8, "MOVZ X16, #5000; SUB SP, SP, X16, UXTX");
        let last = u32::from_le_bytes(e.code()[n - 4..].try_into().unwrap());
        assert_eq!(last, 0xCB30_63FF, "sub sp, sp, x16 -- SP on both sides");

        let mut e2 = Aarch64Emitter::new();
        assert!(emit_addsub_imm_safe(&mut e2, Reg::SP, Reg::SP, 8192, true));
        assert_eq!(
            e2.code().len(),
            4,
            "the shifted-12 immediate is still one word"
        );

        // The one refusal left: an operand that is IP0 itself.
        let mut e3 = Aarch64Emitter::new();
        assert!(!emit_addsub_imm_safe(
            &mut e3,
            Reg::X9,
            Reg::X16,
            5000,
            false
        ));
    }

    /// A far SP-relative access adds SP, not XZR, to the offset.
    #[test]
    fn a_far_sp_relative_access_materializes_an_sp_address() {
        let result = result_from_instructions(vec![
            Arm64Instruction::Ldr {
                rt: Arm64Register::X9,
                rn: Arm64Register::SP,
                offset: 100_000,
            },
            Arm64Instruction::Ret,
        ]);
        let bytes = emit_machine_code(&result).expect("encodes");
        let n = bytes.len();
        let add = u32::from_le_bytes(bytes[n - 12..n - 8].try_into().unwrap());
        assert_eq!(add, 0x8B30_63F0, "add x16, sp, x16 in the extended form");
    }

    /// `emit_machine_code` encodes a wide SP adjustment instead of refusing.
    #[test]
    fn emit_machine_code_encodes_a_wide_sp_immediate() {
        let result = result_from_instructions(vec![
            Arm64Instruction::SubImm {
                rd: Arm64Register::SP,
                rn: Arm64Register::SP,
                imm: 5000,
            },
            Arm64Instruction::Ret,
        ]);
        assert_eq!(
            emit_machine_code(&result).map(|b| b.len()),
            Some(12),
            "MOVZ, SUB (extended), RET"
        );

        // The shifted-immediate form still produces code too.
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

    /// Floating-point operands shuffle like any other, and the forms the JVMS
    /// does not define still refuse.
    ///
    /// Formerly `a_floating_point_operand_refuses_the_shuffle_rather_than_moving_the_wrong_stack`.
    /// Floats lived on a second stack the shuffles never touched, so every
    /// shuffle involving one had to refuse. There is one typed stack now, and
    /// each shuffle resolves its JVMS form from the entries' categories.
    #[test]
    fn floating_point_operands_shuffle_on_the_unified_stack() {
        for (name, code, depth) in [
            (
                "dup of a float over two ints",
                vec![0x03u8, 0x04, 0x0b, 0x59],
                4,
            ),
            (
                "pop of a float over two ints",
                vec![0x03, 0x04, 0x0b, 0x57],
                2,
            ),
            ("dup2 of a double", vec![0x0e, 0x5c], 2),
            ("dup2_x2 of two doubles", vec![0x0e, 0x0f, 0x5e], 3),
            ("dup_x1 with a float below", vec![0x0b, 0x03, 0x5a], 3),
            ("swap with a float below", vec![0x0b, 0x03, 0x5f], 2),
            ("pop of a float", vec![0x0b, 0x57], 0),
        ] {
            let mut backend = Arm64Backend::new();
            assert!(
                backend.compile_method(4, 0, 8, &code).success,
                "{name} must lower"
            );
            assert_eq!(backend.operand_stack.len(), depth, "{name}");
        }

        // `swap` really exchanges the values, kinds and all.
        let mut s = Arm64Backend::new();
        assert!(s.compile_method(4, 0, 8, &[0x0b, 0x03, 0x5f]).success);
        let kinds: Vec<OperandKind> = s.operand_stack.iter().map(|o| o.kind).collect();
        assert_eq!(kinds, vec![OperandKind::I32, OperandKind::F32]);

        // `dup` of a float copies at the float's width.
        let mut d = Arm64Backend::new();
        let dup = d.compile_method(4, 0, 8, &[0x0b, 0x59]);
        assert!(dup
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::FmovFpSingle { .. })));

        // Forms the JVMS does not define.
        for (name, code) in [
            ("dup of a double", vec![0x0eu8, 0x59]),
            ("dup_x1 over a long", vec![0x09, 0x03, 0x5a]),
            ("swap of a double", vec![0x03, 0x0e, 0x5f]),
            ("pop of a long", vec![0x09, 0x57]),
        ] {
            let mut backend = Arm64Backend::new();
            assert!(
                !backend.compile_method(4, 0, 8, &code).success,
                "{name} must refuse"
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

    // =====================================================================
    // 2026-09-12 review fixes. `eval_int_method` runs the integer pseudo-ops a
    // straight-line method lowers to, so VALUES are checked on a host that
    // cannot execute AArch64; the other tests pin the emitted shapes.
    // =====================================================================

    /// Run a compiled method's integer pseudo-ops from its prologue to `RET`
    /// and answer X0. Panics on any pseudo-op it does not model, so a test
    /// cannot pass by skipping one.
    fn eval_int_method(result: &Arm64CompileResult, args: &[i64]) -> i64 {
        let ops = &result.instructions;
        let mut reg = [0u64; 64];
        let mut mem: HashMap<u64, u64> = HashMap::new();
        reg[31] = 0x7FFF_0000; // SP
        for (i, a) in args.iter().enumerate() {
            reg[i] = *a as u64;
        }
        let labels: HashMap<u32, usize> = ops
            .iter()
            .enumerate()
            .filter_map(|(i, op)| match op {
                Arm64Instruction::Label(l) => Some((*l, i)),
                _ => None,
            })
            .collect();
        let w = |v: u64| v as u32;
        let at = |base: u64, off: i32| base.wrapping_add(off as i64 as u64);
        let mut pc = 0usize;
        for _ in 0..100_000 {
            let Some(op) = ops.get(pc) else {
                panic!("fell off the end of the pseudo-op stream");
            };
            let mut next = pc + 1;
            match op {
                Arm64Instruction::Label(_)
                | Arm64Instruction::Comment(_)
                | Arm64Instruction::Nop => {}
                Arm64Instruction::MovImm { rd, imm } => reg[rd.0 as usize] = *imm as u64,
                Arm64Instruction::Mov { rd, rm } => reg[rd.0 as usize] = reg[rm.0 as usize],
                Arm64Instruction::AddImm { rd, rn, imm } => {
                    reg[rd.0 as usize] = at(reg[rn.0 as usize], *imm)
                }
                Arm64Instruction::SubImm { rd, rn, imm } => {
                    reg[rd.0 as usize] = at(reg[rn.0 as usize], imm.wrapping_neg())
                }
                Arm64Instruction::AddW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_add(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::SubW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_sub(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::LslW { rd, rn, rm } => {
                    reg[rd.0 as usize] =
                        u64::from(w(reg[rn.0 as usize]).wrapping_shl(w(reg[rm.0 as usize])))
                }
                Arm64Instruction::AddImmW { rd, rn, imm } => {
                    reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_add(*imm as u32))
                }
                Arm64Instruction::SubImmW { rd, rn, imm } => {
                    reg[rd.0 as usize] = u64::from(w(reg[rn.0 as usize]).wrapping_sub(*imm as u32))
                }
                Arm64Instruction::Sxtw { rd, rn } => {
                    reg[rd.0 as usize] = w(reg[rn.0 as usize]) as i32 as i64 as u64
                }
                Arm64Instruction::CbzW { rt, label } => {
                    if w(reg[rt.0 as usize]) == 0 {
                        next = labels[label];
                    }
                }
                Arm64Instruction::CbnzW { rt, label } => {
                    if w(reg[rt.0 as usize]) != 0 {
                        next = labels[label];
                    }
                }
                Arm64Instruction::Str { rt, rn, offset } => {
                    // Register 31 as a store's Rt is XZR.
                    let v = if rt.0 == 31 { 0 } else { reg[rt.0 as usize] };
                    mem.insert(at(reg[rn.0 as usize], *offset), v);
                }
                Arm64Instruction::Ldr { rt, rn, offset } => {
                    let addr = at(reg[rn.0 as usize], *offset);
                    reg[rt.0 as usize] = *mem
                        .get(&addr)
                        .unwrap_or_else(|| panic!("load of an unwritten word at {addr:#x}"));
                }
                Arm64Instruction::StpPre {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    mem.insert(base, reg[rt1.0 as usize]);
                    mem.insert(base + 8, reg[rt2.0 as usize]);
                    reg[rn.0 as usize] = base;
                }
                Arm64Instruction::Stp {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    mem.insert(base, reg[rt1.0 as usize]);
                    mem.insert(base + 8, reg[rt2.0 as usize]);
                }
                Arm64Instruction::Ldp {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = at(reg[rn.0 as usize], *offset);
                    reg[rt1.0 as usize] = mem[&base];
                    reg[rt2.0 as usize] = mem[&(base + 8)];
                }
                Arm64Instruction::LdpPost {
                    rt1,
                    rt2,
                    rn,
                    offset,
                } => {
                    let base = reg[rn.0 as usize];
                    reg[rt1.0 as usize] = mem[&base];
                    reg[rt2.0 as usize] = mem[&(base + 8)];
                    reg[rn.0 as usize] = at(base, *offset);
                }
                Arm64Instruction::B { label } => next = labels[label],
                Arm64Instruction::Ret => return reg[0] as i64,
                other => panic!("eval_int_method does not model {other:?}"),
            }
            pc = next;
        }
        panic!("100000 steps without a RET");
    }

    /// Compile a static method with `descriptor`, polls off, and require it to
    /// succeed.
    fn compile_with(
        descriptor: &str,
        locals: usize,
        stack: usize,
        code: &[u8],
    ) -> Arm64CompileResult {
        let mut b = Arm64Backend::new();
        b.set_safepoints_enabled(false);
        b.set_method_descriptor(descriptor, true);
        let slots = crate::compute_param_jvm_slots(descriptor, true).1;
        let result = b.compile_method(locals, slots, stack, code);
        assert!(result.success, "{descriptor} {code:02x?} must compile");
        result
    }

    /// THE REPORTED MISCOMPILE. `static int f(int a, int b) { return a -
    /// (b+1+2+3+4); }` returned -4 for `f(100, 5)`: the round-robin scratch
    /// allocator wrapped onto a live register, spilled it, recorded the spill
    /// against the REGISTER, pushed the same register for the new value, and
    /// popping the new value reloaded the old one.
    #[test]
    fn the_expression_that_returned_minus_four_evaluates_to_85() {
        let code = [
            0x1a, 0x1b, 0x04, 0x60, 0x05, 0x60, 0x06, 0x60, 0x07, 0x60, 0x64, 0xac,
        ];
        let result = compile_with("(II)I", 2, 3, &code);
        assert_eq!(eval_int_method(&result, &[100, 5]), 85);
        assert_eq!(eval_int_method(&result, &[0, -15]), 5); // 0 - (-15 + 10)
    }

    /// A stack deeper than the seven scratch registers keeps every operand, in
    /// order -- checked with a subtraction chain, which is order-sensitive.
    #[test]
    fn a_stack_deeper_than_the_scratch_pool_keeps_every_value() {
        let ten_values = || {
            let mut code = vec![0x04u8, 0x05, 0x06, 0x07, 0x08]; // iconst_1..5
            for v in 6..=10u8 {
                code.extend_from_slice(&[0x10, v]); // bipush
            }
            code
        };
        let mut sum = ten_values();
        sum.extend(std::iter::repeat(0x60).take(9));
        sum.push(0xac);
        assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sum), &[]), 55);

        // 1 - (2 - (3 - (4 - (5 - (6 - (7 - (8 - (9 - 10))))))))
        let mut sub = ten_values();
        sub.extend(std::iter::repeat(0x64).take(9));
        sub.push(0xac);
        assert_eq!(eval_int_method(&compile_with("()I", 0, 10, &sub), &[]), -5);
    }

    /// A value on the operand stack across a branch reaches the merge from
    /// both paths: `a == 0 ? 2 : 1`. The model used to follow the walk, so the
    /// two arms left the value in different registers.
    #[test]
    fn a_ternary_value_survives_the_merge() {
        // iload_0; ifeq 8; iconst_1; goto 9; 8: iconst_2; 9: ireturn
        let code = [0x1a, 0x99, 0x00, 0x07, 0x04, 0xa7, 0x00, 0x04, 0x05, 0xac];
        let result = compile_with("(I)I", 1, 1, &code);
        assert_eq!(eval_int_method(&result, &[0]), 2);
        assert_eq!(eval_int_method(&result, &[5]), 1);
    }

    /// `int` results wrap at 32 bits and stay sign-extended through `i2l`,
    /// `l2i` sign-extends, `ishl` masks, and `iinc` wraps.
    #[test]
    fn int_arithmetic_wraps_and_stays_sign_extended() {
        let min = i64::from(i32::MIN);
        let max = i64::from(i32::MAX);
        let add = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x60, 0xac]);
        assert_eq!(eval_int_method(&add, &[max, 1]), min);
        // iload_0; iload_1; iadd; i2l; lreturn
        let widen = compile_with("(II)J", 2, 2, &[0x1a, 0x1b, 0x60, 0x85, 0xad]);
        assert_eq!(
            eval_int_method(&widen, &[max, 1]),
            min,
            "i2l of a wrapped int"
        );
        // lload_0; l2i; ireturn
        let narrow = compile_with("(J)I", 2, 2, &[0x1e, 0x88, 0xac]);
        assert_eq!(
            eval_int_method(&narrow, &[0xFFFF_FFFF]),
            -1,
            "(int) 0xFFFFFFFFL"
        );
        assert_eq!(eval_int_method(&narrow, &[0x1_0000_0005]), 5);
        // iload_0; iload_1; ishl; ireturn
        let shl = compile_with("(II)I", 2, 2, &[0x1a, 0x1b, 0x78, 0xac]);
        assert_eq!(
            eval_int_method(&shl, &[1, 32]),
            1,
            "the distance is masked to 5 bits"
        );
        assert_eq!(eval_int_method(&shl, &[1, 31]), min);
        // iinc 0, 1; iload_0; ireturn
        let inc = compile_with("(I)I", 1, 1, &[0x84, 0x00, 0x01, 0x1a, 0xac]);
        assert_eq!(eval_int_method(&inc, &[max]), min);
    }

    /// Every argument lands in its JVM local, category 2 included.
    ///
    /// The prologue moved argument register `i` into local `i`, which is wrong
    /// for every argument after a `long` or `double`.
    #[test]
    fn parameters_after_a_long_are_homed_by_jvm_slot() {
        // static int f(long a, long b, int c) { return c; } -- iload 4; ireturn
        let r1 = compile_with("(JJI)I", 5, 1, &[0x15, 0x04, 0xac]);
        assert_eq!(
            eval_int_method(&r1, &[11, 22, 33]),
            33,
            "c is argument 2 and local 4"
        );
        // static long f(int a, long b) { return b; } -- lload_1; lreturn
        let r2 = compile_with("(IJ)J", 3, 2, &[0x1f, 0xad]);
        assert_eq!(eval_int_method(&r2, &[7, -9]), -9);
    }

    /// A frame-homed parameter -- which every `float`/`double` one is -- is
    /// stored by the prologue and read back from that word. It used to be
    /// stored nowhere.
    #[test]
    fn floating_point_parameters_are_stored_to_their_frame_homes() {
        // static double f(double a, double b) { return b; }
        // dload_0; pop2; dload_2; dreturn
        let result = compile_with("(DD)D", 4, 2, &[0x26, 0x58, 0x28, 0xaf]);
        let ops = &result.instructions;
        let home_of = |arg: Arm64Register| {
            ops.iter().find_map(|i| match i {
                Arm64Instruction::Str { rt, rn, offset }
                    if *rt == arg && *rn == Arm64Register::FP =>
                {
                    Some(*offset)
                }
                _ => None,
            })
        };
        let a = home_of(Arm64Register::X0).expect("argument 0 is stored to its frame home");
        let b = home_of(Arm64Register::X1).expect("argument 1 is stored to its frame home");
        assert_ne!(a, b);
        let loads: Vec<i32> = ops
            .iter()
            .filter_map(|i| match i {
                Arm64Instruction::FpLdr {
                    rn,
                    offset,
                    is_double: true,
                    ..
                } if *rn == Arm64Register::FP => Some(*offset),
                _ => None,
            })
            .collect();
        assert_eq!(
            loads,
            vec![a, b],
            "each dload reads the word its argument was stored to"
        );
    }

    /// `freturn`/`dreturn` move the bits into X0 -- where the VM reads every
    /// result -- and leave through the shared epilogue. They used to discard
    /// the value and emit a bare `RET`, skipping the frame teardown.
    #[test]
    fn fp_returns_move_the_bits_to_x0_and_take_the_epilogue() {
        for (code, single) in [(&[0x0cu8, 0xae][..], true), (&[0x0f, 0xaf][..], false)] {
            let result = make_backend_with_method(0, 0, code);
            assert!(result.success);
            let ops = &result.instructions;
            let mv = ops
                .iter()
                .position(|i| {
                    if single {
                        matches!(i, Arm64Instruction::FmovFromFpSingle { rd, .. } if *rd == Arm64Register::X0)
                    } else {
                        matches!(i, Arm64Instruction::FmovFromFp { rd, .. } if *rd == Arm64Register::X0)
                    }
                })
                .expect("the value is moved into X0");
            assert!(
                matches!(ops[mv + 1], Arm64Instruction::B { .. }),
                "then to the epilogue"
            );
            let rets = ops
                .iter()
                .filter(|i| matches!(i, Arm64Instruction::Ret))
                .count();
            assert_eq!(rets, 1, "the only RET is the epilogue's");
            assert!(matches!(ops.last(), Some(Arm64Instruction::Ret)));
        }
    }

    /// `*cmpg` negates on MI and `*cmpl` on LT, and the NZCV truth table after
    /// `FCMP` says that puts NaN where the JVMS does. `fcmpg` used to take
    /// `B.LT`, which is TRUE on unordered, so NaN produced -1.
    #[test]
    fn fp_compares_put_nan_on_the_jvms_side() {
        type Nzcv = (bool, bool, bool, bool);
        const LESS: Nzcv = (true, false, false, false);
        const EQUAL: Nzcv = (false, true, true, false);
        const GREATER: Nzcv = (false, false, true, false);
        const UNORDERED: Nzcv = (false, false, true, true);
        fn holds(cond: Arm64Condition, (n, z, _c, v): Nzcv) -> bool {
            match cond {
                Arm64Condition::Ne => !z,
                Arm64Condition::Mi => n,
                Arm64Condition::Lt => n != v,
                other => panic!("unmodelled condition {other:?}"),
            }
        }
        // CSET ne; CNEG <cond>
        fn value(cond: Arm64Condition, flags: Nzcv) -> i32 {
            let v = i32::from(holds(Arm64Condition::Ne, flags));
            if holds(cond, flags) {
                -v
            } else {
                v
            }
        }
        for (code, nan) in [
            ([0x0bu8, 0x0c, 0x95, 0xac], -1), // fcmpl
            ([0x0b, 0x0c, 0x96, 0xac], 1),    // fcmpg
            ([0x0e, 0x0f, 0x97, 0xac], -1),   // dcmpl
            ([0x0e, 0x0f, 0x98, 0xac], 1),    // dcmpg
        ] {
            let r = make_backend_with_method(0, 0, &code);
            assert!(r.success);
            assert!(r.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::Cset {
                    cond: Arm64Condition::Ne,
                    ..
                }
            )));
            assert!(
                !r.instructions
                    .iter()
                    .any(|i| matches!(i, Arm64Instruction::BCond { .. })),
                "branch-free"
            );
            let cond = r
                .instructions
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::Cneg { cond, .. } => Some(*cond),
                    _ => None,
                })
                .expect("CNEG");
            assert_eq!(value(cond, UNORDERED), nan, "0x{:02x} on NaN", code[2]);
            assert_eq!(value(cond, LESS), -1);
            assert_eq!(value(cond, EQUAL), 0);
            assert_eq!(value(cond, GREATER), 1);
        }
    }

    /// `f2i`/`d2i` use the 32-bit saturating `FCVTZS W` and then sign-extend;
    /// `l2i` sign-extends instead of masking.
    #[test]
    fn fp_to_int_conversions_saturate_at_32_bits() {
        for (code, name) in [
            (&[0x0cu8, 0x8b, 0xac][..], "f2i"),
            (&[0x0f, 0x8e, 0xac][..], "d2i"),
        ] {
            let r = make_backend_with_method(0, 0, code);
            assert!(r.success);
            let ops = &r.instructions;
            let at = ops
                .iter()
                .position(|i| {
                    matches!(
                        i,
                        Arm64Instruction::FcvtzsSingle { .. } | Arm64Instruction::FcvtzsIntW { .. }
                    )
                })
                .unwrap_or_else(|| panic!("{name} must use FCVTZS W"));
            let rd = match ops[at] {
                Arm64Instruction::FcvtzsSingle { rd, .. }
                | Arm64Instruction::FcvtzsIntW { rd, .. } => rd,
                _ => unreachable!(),
            };
            assert!(
                matches!(ops[at + 1], Arm64Instruction::Sxtw { rd: d, rn: s } if d == rd && s == rd),
                "{name} then SXTW"
            );
            assert!(
                !ops.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::FcvtzsInt { .. } | Arm64Instruction::FcvtzsSingleX { .. }
                )),
                "{name} must not saturate at the 64-bit bounds"
            );
        }
        let l2i = make_backend_with_method(0, 0, &[0x0a, 0x88, 0xac]);
        assert!(l2i
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::Sxtw { .. })));
        assert!(
            !l2i.instructions.iter().any(|i| matches!(
                i,
                Arm64Instruction::And { .. } | Arm64Instruction::AndImm { .. }
            )),
            "l2i must not zero-extend"
        );
    }

    /// Every 32-bit int op is its W form followed by `SXTW` of its result, and
    /// int compares and zero tests read the W register.
    #[test]
    fn int_ops_lower_to_w_forms_followed_by_sxtw() {
        for op in [0x60u8, 0x64, 0x68, 0x7e, 0x80, 0x82, 0x78, 0x7a, 0x7c] {
            let r = make_backend_with_method(2, 2, &[0x1a, 0x1b, op, 0xac]);
            assert!(r.success, "0x{op:02x}");
            let ops = &r.instructions;
            let rd = ops
                .iter()
                .find_map(|i| match i {
                    Arm64Instruction::AddW { rd, .. }
                    | Arm64Instruction::SubW { rd, .. }
                    | Arm64Instruction::MulW { rd, .. }
                    | Arm64Instruction::AndW { rd, .. }
                    | Arm64Instruction::OrrW { rd, .. }
                    | Arm64Instruction::EorW { rd, .. }
                    | Arm64Instruction::LslW { rd, .. }
                    | Arm64Instruction::AsrW { rd, .. }
                    | Arm64Instruction::LsrW { rd, .. } => Some(*rd),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("0x{op:02x} has no W form"));
            assert!(
                ops.windows(2).any(|pair| matches!(
                    (&pair[0], &pair[1]),
                    (_, Arm64Instruction::Sxtw { rd: d, rn: s }) if *d == rd && *s == rd
                )),
                "0x{op:02x}: the result is sign-extended"
            );
            assert!(
                !ops.iter().any(|i| matches!(
                    i,
                    Arm64Instruction::Add { .. }
                        | Arm64Instruction::Sub { .. }
                        | Arm64Instruction::Mul { .. }
                        | Arm64Instruction::And { .. }
                        | Arm64Instruction::Orr { .. }
                        | Arm64Instruction::Eor { .. }
                        | Arm64Instruction::Lsl { .. }
                        | Arm64Instruction::Asr { .. }
                        | Arm64Instruction::Lsr { .. }
                )),
                "0x{op:02x} leaves no 64-bit op behind"
            );
        }
        // iload_0; iload_1; if_icmpeq 6; return; 6: return
        let icmp = make_backend_with_method(2, 2, &[0x1a, 0x1b, 0x9f, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(icmp
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::CmpW { .. })));
        // iload_0; ifeq 5; return; 5: return
        let ifeq = make_backend_with_method(1, 1, &[0x1a, 0x99, 0x00, 0x04, 0xb1, 0xb1]);
        assert!(ifeq
            .instructions
            .iter()
            .any(|i| matches!(i, Arm64Instruction::CbzW { .. })));
    }

    /// A switch's key is never compared with itself, even with every scratch
    /// register live, and a dense `tableswitch` is a real jump table.
    ///
    /// The old lowering popped the key and then took each case constant from
    /// the scratch allocator, which could hand back the key's own register:
    /// `CMP R, R`, always equal.
    #[test]
    fn a_switch_never_compares_its_key_with_itself() {
        // seven live ints (every scratch register), then the key
        let mut prefix = vec![0x03u8; 7];
        prefix.push(0x1a);
        let mut look = prefix.clone();
        look.extend_from_slice(&[0xab, 0, 0, 0]);
        for word in [28i32, 2, 1, 28, 2, 28] {
            look.extend_from_slice(&word.to_be_bytes());
        }
        look.push(0xac);
        let mut table = prefix;
        table.extend_from_slice(&[0xaa, 0, 0, 0]);
        for word in [28i32, 0, 2, 28, 28, 28] {
            table.extend_from_slice(&word.to_be_bytes());
        }
        table.push(0xac);

        for (name, code) in [("lookupswitch", look), ("tableswitch", table)] {
            assert_eq!(code.len(), 37, "{name}: every case targets pc 36");
            let mut b = Arm64Backend::new();
            let result = b.compile_method(1, 1, 8, &code);
            assert!(result.success, "{name} must compile");
            let bytes = emit_machine_code(&result).unwrap_or_else(|| panic!("{name} must encode"));
            let words: Vec<u32> = bytes
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();
            for &word in &words {
                // CMP (SUBS ZR, Rn, Rm) with no shift, either width.
                if word & 0x7F20_FC1F == 0x6B00_001F {
                    assert_ne!(
                        (word >> 5) & 0x1F,
                        (word >> 16) & 0x1F,
                        "{name}: {word:#010x} compares a register with itself"
                    );
                }
            }
            if name == "tableswitch" {
                assert!(
                    words.contains(&0xB8B1_5A11),
                    "LDRSW X17, [X16, W17, UXTW #2]"
                );
                assert!(words.contains(&0xD61F_0200), "BR X16");
            }
        }
    }

    /// The prologue builds the standard AAPCS64 frame record and the epilogue
    /// tears it down, neither through `MOV` (which reads register 31 as XZR).
    #[test]
    fn the_prologue_builds_the_standard_frame_record() {
        let result = make_backend_with_method(1, 1, &[0x1a, 0xac]);
        assert!(result.success);
        let ops = &result.instructions;
        assert!(
            matches!(ops[0], Arm64Instruction::StpPre { rt1, rt2, rn, offset: -16 }
                         if rt1 == Arm64Register::FP && rt2 == Arm64Register::LR && rn == Arm64Register::SP)
        );
        assert!(
            matches!(ops[1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                     if rd == Arm64Register::FP && rn == Arm64Register::SP),
            "FP = SP right after the push: [FP] is the caller's FP and [FP+8] the LR"
        );
        assert!(!ops.iter().any(|i| matches!(i, Arm64Instruction::Mov { rd, rm }
                                             if *rd == Arm64Register::FP || *rd == Arm64Register::SP || *rm == Arm64Register::SP)));
        let ldp = ops
            .iter()
            .position(|i| matches!(i, Arm64Instruction::LdpPost { .. }))
            .expect("epilogue");
        assert!(
            matches!(ops[ldp - 1], Arm64Instruction::AddImm { rd, rn, imm: 0 }
                         if rd == Arm64Register::SP && rn == Arm64Register::FP)
        );

        let bytes = emit_machine_code(&result).expect("encodes");
        let word = |i: usize| u32::from_le_bytes(bytes[i * 4..i * 4 + 4].try_into().unwrap());
        assert_eq!(word(0), 0xA9BF_7BFD, "stp x29, x30, [sp, #-16]!");
        assert_eq!(word(1), 0x9100_03FD, "mov x29, sp (ADD form)");

        assert_eq!(
            Arm64FrameLayout::compute(0, 0, &[Arm64Register::X19]).callee_save_offset,
            -16,
            "the first callee-save pair sits directly below FP"
        );
    }

    /// `estimated_size` is an upper bound, and a label or comment costs nothing.
    #[test]
    fn code_buffer_estimated_size_is_an_upper_bound() {
        // sipush 32767; istore_0; iinc 0, 1; iload_0; ireturn
        let result = make_backend_with_method(
            1,
            0,
            &[0x11, 0x7F, 0xFF, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0xac],
        );
        assert!(result.success);
        let mut buf = Arm64CodeBuffer::new();
        for inst in &result.instructions {
            buf.emit(inst.clone());
        }
        let encoded = emit_machine_code(&result).expect("encodes").len();
        assert!(
            buf.estimated_size() >= encoded,
            "{} < {encoded}",
            buf.estimated_size()
        );

        let mut labels_only = Arm64CodeBuffer::new();
        let l = labels_only.new_label();
        labels_only.bind_label(l);
        labels_only.emit(Arm64Instruction::Comment("nothing".into()));
        assert_eq!(labels_only.estimated_size(), 0);
    }

    /// More float operands than V0-V7 spill by depth, at their own width, and
    /// the method still compiles. The float allocator used to round-robin with
    /// no liveness check and no spill.
    #[test]
    fn a_float_stack_deeper_than_v0_to_v7_spills_by_depth() {
        let mut code = vec![0x0cu8; 9]; // nine fconst_1
        code.extend(std::iter::repeat(0x62).take(8)); // eight fadd
        code.push(0xae); // freturn
        let mut b = Arm64Backend::new();
        let result = b.compile_method(0, 0, 9, &code);
        assert!(
            result.success,
            "nine float operands must not exhaust the pool"
        );
        assert!(result.instructions.iter().any(|i| matches!(
            i,
            Arm64Instruction::FpStr { is_double: false, rn, .. } if *rn == Arm64Register::FP
        )));
    }

    // ---------------------------------------------------------------------------
    // EXECUTION. Only compiled on aarch64, where the emitted bytes are native.
    // ---------------------------------------------------------------------------

    /// Tests that actually RUN the code this backend emits.
    ///
    /// Everything else in this file asserts encodings and pseudo-op structure,
    /// which is all a non-aarch64 host can prove. These are the ones that turn that
    /// construction into evidence, and they exist because nothing in this
    /// repository could execute them until an aarch64 container was stood up.
    #[cfg(target_arch = "aarch64")]
    mod arm64_execution {
        use super::*;
        use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

        /// The safepoint flag the emitted poll reads. ONE byte, like the
        /// `AtomicBool` the real `GcBarrier` exposes.
        static TEST_SP_FLAG: AtomicU8 = AtomicU8::new(0);
        /// Bumped by the slow path so a taken poll is observable.
        static SLOW_PATH_HITS: AtomicU64 = AtomicU64::new(0);

        extern "C" fn test_slow_path() {
            SLOW_PATH_HITS.fetch_add(1, Ordering::SeqCst);
        }

        /// These tests must not run concurrently, for two independent reasons.
        ///
        /// They share `TEST_SP_FLAG`, so one test's `store` decides another's
        /// control flow. And they WRITE THEN EXECUTE code: under qemu-user (the
        /// only way this file gets run at all today) a buffer being written while
        /// another thread executes from a neighbouring mapping can leave stale
        /// translation blocks, which surfaces as `SIGILL` in a test whose own
        /// codegen is fine. Serialising removes both, and costs nothing -- there
        /// are three of them.
        static EXEC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn exec_guard() -> std::sync::MutexGuard<'static, ()> {
            EXEC_LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// `iload_0; iload_1; iadd; ireturn`
        const IADD: [u8; 4] = [0x1a, 0x1b, 0x60, 0xac];

        fn call2(cm: &crate::CompiledMethod, a: i64, b: i64) -> i64 {
            // SAFETY: `cm` is a finalized artifact for a static (II)I method, so
            // the entry is an `extern "C" fn(i64, i64) -> i64`.
            unsafe { cm.try_call(&[a, b]) }.expect("the compiled method is callable")
        }

        /// The emitted code runs at all.
        #[test]
        fn a_compiled_leaf_method_executes_and_returns_the_right_value() {
            let _serial = exec_guard();
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success, "iadd must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 7, 35), 42);
            assert_eq!(call2(&cm, -1, 1), 0);
        }

        /// THE POLL SEQUENCE EXECUTES, and takes the not-taken path when the flag
        /// is clear.
        ///
        /// This is the `MOVZ/MOVK; LDRB; CBZ` sequence whose encoding is asserted
        /// by `the_poll_reads_one_byte_and_the_encoding_says_so`. Asserting the
        /// word is not the same as running it: this proves the flag is read at the
        /// right width and address and that a clear flag branches PAST the call
        /// rather than into it.
        #[test]
        fn a_clear_flag_skips_the_slow_path() {
            let _serial = exec_guard();
            TEST_SP_FLAG.store(0, Ordering::SeqCst);
            let before = SLOW_PATH_HITS.load(Ordering::SeqCst);

            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(true);
            // SAFETY: zeroed helper table, then two real addresses.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.safepoint_flag_addr = TEST_SP_FLAG.as_ptr() as usize;
            h.safepoint_slow_path = test_slow_path as usize;
            b.set_helpers(h);

            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 20, 22), 42, "the method still computes");
            assert_eq!(
                SLOW_PATH_HITS.load(Ordering::SeqCst),
                before,
                "a clear flag must not call the slow path"
            );
        }

        /// THE TAKEN PATH RUNS, AND THE ARGUMENTS SURVIVE IT.
        ///
        /// The poll's `BLR` clobbers X0-X7 by the AAPCS64 contract, and this
        /// method's parameters arrive there. The entry poll was originally emitted
        /// from the END of the prologue -- BEFORE `compile_pass` copies the
        /// arguments into their local registers -- so on this path every parameter
        /// would have been garbage. That was found by reading and fixed by moving
        /// the poll past the copy; this is the test that would have CAUGHT it, and
        /// it is the first thing in this backend's history that could.
        #[test]
        fn a_set_flag_calls_the_slow_path_and_the_arguments_survive() {
            let _serial = exec_guard();
            TEST_SP_FLAG.store(1, Ordering::SeqCst);
            let before = SLOW_PATH_HITS.load(Ordering::SeqCst);

            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(true);
            // SAFETY: as above.
            let mut h: crate::JitRuntimeHelpers = unsafe { std::mem::zeroed() };
            h.safepoint_flag_addr = TEST_SP_FLAG.as_ptr() as usize;
            h.safepoint_slow_path = test_slow_path as usize;
            b.set_helpers(h);

            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");

            let got = call2(&cm, 7, 35);
            assert!(
                SLOW_PATH_HITS.load(Ordering::SeqCst) > before,
                "a set flag must reach the slow path -- otherwise this test proves \
             nothing about the taken path"
            );
            assert_eq!(
                got, 42,
                "the arguments must survive the poll's call; X0-X7 are caller-saved"
            );

            TEST_SP_FLAG.store(0, Ordering::SeqCst);
        }

        /// Compile `code` as a static method with `descriptor`, polls off.
        fn compile_static(
            descriptor: &str,
            locals: usize,
            stack: usize,
            code: &[u8],
        ) -> crate::CompiledMethod {
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            b.set_method_descriptor(descriptor, true);
            let slots = crate::compute_param_jvm_slots(descriptor, true).1;
            let result = b.compile_method(locals, slots, stack, code);
            assert!(result.success, "{descriptor} must compile");
            publish_compiled_method(&result).expect("publishes")
        }

        /// `iadd` wraps at 32 bits, and the result comes back sign-extended.
        ///
        /// Formerly `iadd_does_not_wrap_at_32_bits_and_this_is_a_bug`, which pinned
        /// the 64-bit answer while the correct assertion sat `#[ignore]`d beside
        /// it. JVMS 6.5 `iadd`: "the result is the 32 low-order bits of the true
        /// mathematical result".
        #[test]
        fn iadd_wraps_at_32_bits_as_the_jvms_requires() {
            let _serial = exec_guard();
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &IADD);
            assert!(result.success);
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, i64::from(i32::MAX), 1), i64::from(i32::MIN));
        }

        /// `ishl` masks its distance to five bits.
        ///
        /// Formerly `ishl_does_not_mask_the_shift_to_five_bits_and_this_is_a_bug`.
        /// JVMS 6.5 `ishl`: the distance is "the value of the low 5 bits", so
        /// `1 << 32` is `1` and `1 << 31` is `Integer.MIN_VALUE`.
        #[test]
        fn ishl_masks_the_shift_to_five_bits() {
            let _serial = exec_guard();
            // iload_0; iload_1; ishl; ireturn
            let code = [0x1a, 0x1b, 0x78, 0xac];
            let mut b = Arm64Backend::new();
            b.set_safepoints_enabled(false);
            let result = b.compile_method(2, 2, 4, &code);
            assert!(result.success, "ishl must compile");
            let cm = publish_compiled_method(&result).expect("publishes");
            assert_eq!(call2(&cm, 1, 32), 1);
            assert_eq!(call2(&cm, 1, 33), 2);
            assert_eq!(call2(&cm, 1, 31), i64::from(i32::MIN));
        }

        /// The reported miscompile: `static int f(int a, int b) { return a -
        /// (b+1+2+3+4); }` returned -4 for `f(100, 5)`, because the scratch
        /// allocator wrapped onto a live register and popping the new value
        /// reloaded the old one.
        #[test]
        fn the_expression_that_returned_minus_four_returns_85() {
            let _serial = exec_guard();
            let code = [
                0x1a, 0x1b, 0x04, 0x60, 0x05, 0x60, 0x06, 0x60, 0x07, 0x60, 0x64, 0xac,
            ];
            let cm = compile_static("(II)I", 2, 3, &code);
            assert_eq!(call2(&cm, 100, 5), 85);
        }

        /// `dreturn` hands back the double's bits in X0 through the epilogue, and a
        /// `double` parameter is homed in its frame slot. Both were broken: the
        /// return discarded the value with a bare `RET`, and a frame-homed
        /// parameter was never stored.
        #[test]
        fn a_double_parameter_round_trips_through_dreturn() {
            let _serial = exec_guard();
            // static double f(double a, double b) { return b; }  -- dload_2; dreturn
            let cm = compile_static("(DD)D", 4, 2, &[0x28, 0xaf]);
            let got = call2(&cm, 1.5f64.to_bits() as i64, (-2.25f64).to_bits() as i64);
            assert_eq!(f64::from_bits(got as u64), -2.25);
        }

        /// `fcmpg` of NaN is +1 and `fcmpl` of NaN is -1.
        #[test]
        fn nan_compares_land_where_the_jvms_puts_them() {
            let _serial = exec_guard();
            let nan = i64::from(f32::NAN.to_bits());
            let one = i64::from(1.0f32.to_bits());
            // fload_0; fload_1; fcmpg|fcmpl; ireturn
            let g = compile_static("(FF)I", 2, 2, &[0x22, 0x23, 0x96, 0xac]);
            let l = compile_static("(FF)I", 2, 2, &[0x22, 0x23, 0x95, 0xac]);
            assert_eq!(call2(&g, nan, one), 1);
            assert_eq!(call2(&l, nan, one), -1);
            assert_eq!(call2(&g, i64::from(0.5f32.to_bits()), one), -1);
            assert_eq!(call2(&l, one, one), 0);
        }
    }
}
