// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Vector (SIMD) machine-code emission for loops the admission gate cleared.
//!
//! [`super::simd_analysis::vector_gate`] decides *whether* a counted loop may be
//! widened and emits nothing. This module is the other half: given an admitted
//! [`VecPlan`] plus a concrete description of where the loop's values live in
//! machine registers, it produces the bytes — or refuses.
//!
//! # The contract, and why it is shaped this way
//!
//! The gate's module doc states the rule this module obeys literally:
//!
//! > A consumer that cannot emit one of those guards has to treat the whole plan
//! > as refused — a partially-emitted guard set proves nothing.
//!
//! So [`emit_vector_loop`] requires **one binding per `plan.guards` entry, in
//! the same order**, and refuses the whole request when the counts differ or a
//! binding does not match its guard's shape. There is no path through this
//! module that emits a vector loop with a guard left over.
//!
//! Every other refusal follows the same discipline: it is always better to
//! decline a loop than to emit machine code that is silently wrong. Every
//! variant of [`VecEmitRefusal`] is a hard "no", never a warning.
//!
//! # What is emitted
//!
//! One shape, parameterised:
//!
//! ```text
//!   <guards>                       ; cmp + jcc → fallback (caller patches)
//!   <accumulator init>             ; reduction only: VPXOR acc, acc, acc
//! loop_head:
//!   lea   scratch, [iv + lanes]
//!   cmp   scratch, bound
//!   jg    epilogue                 ; iv + lanes > bound → no full pass left
//!   <body: vector loads, lane-wise arithmetic, vector stores, accumulates>
//!   add   iv, lanes
//!   jmp   loop_head
//! epilogue:
//!   <reduction horizontal fold, then fold into the scalar accumulator GPR>
//!   vzeroupper                     ; only when 256-bit registers were touched
//!   ; ← remainder entry: control falls out with `iv` live, into the caller's
//!   ;   untouched scalar loop
//! ```
//!
//! **Guards come first and branch out before any vector register is touched**,
//! which is what makes the fallback edge free of a `VZEROUPPER` obligation.
//!
//! # The remainder loop
//!
//! There is no separate remainder *loop*. [`TailStrategy::ScalarRemainder`] is
//! implemented by falling out of the vector loop with the induction variable
//! live in its GPR, straight into the caller's original scalar loop, which is
//! left completely untouched. The scalar loop's own header test then runs the
//! last `trip % lanes` iterations. This is the cheapest correct tail and the
//! only one available: masked tails need AVX-512 predicate registers, which
//! `super::cpu_features` does not detect and no [`VectorIsa`] models.
//!
//! [`TailStrategy::None`] emits exactly the same code. The head test simply
//! never fails early, and the fall-out lands on a scalar loop whose own test
//! immediately exits. Emitting one shape for both means the tail strategy
//! cannot be the thing that is wrong.
//!
//! # The reduction epilogue
//!
//! Only `int` reductions with `+`, `|` and `^` are emitted. Those three are
//! associative *and* commutative over the whole two's-complement domain and
//! have `0` as identity, so the vector accumulator can be zero-initialised with
//! `VPXOR` and folded into the incoming scalar accumulator at the end. The fold
//! is the standard log2(lanes) tree: `VEXTRACTI128` (256-bit only), then two
//! `VPSHUFD` steps, then `VMOVD` into the scratch GPR and one scalar
//! `add`/`or`/`xor` into the accumulator.
//!
//! Because the vector accumulator starts at the identity, the epilogue is also
//! correct when the head test fails on its very first evaluation: zero vector
//! passes fold an identity into the accumulator, which is a no-op.
//!
//! `&` reductions are refused (identity is all-ones, which needs a materialised
//! constant), `*` is refused (identity 1, same problem, and `VPMULLQ` is
//! AVX-512), `long` reductions are refused, and floating-point reductions are
//! refused **unconditionally here** even when the gate admitted one under
//! [`FpRelaxation::AllowReassociation`] — the emitter does not take the
//! caller's word for a changed FP result.
//!
//! # Object references
//!
//! A vector store of object references bypasses the GC write barrier. That is
//! the same barrier-elision family that produced a use-after-free on this
//! branch, and there is no lane count that makes it safe. The gate already
//! refuses it ([`VecRefusal::GcReferenceAccess`]); this module refuses it
//! **again**, independently, on `plan.elem == MemKind::Ref`, and the refusal
//! has its own test. Two gates, because one of them being edited away should
//! not be enough to ship a barrier-free oop store.
//!
//! Sub-word elements (`byte`, `char`, `short`) are refused for a different
//! reason that is just as sharp: JVM arithmetic on them is performed in `int`
//! and narrowed only at the store, so a lane-wise `PADDB` wraps at 8 bits where
//! the scalar loop wraps at 32.
//!
//! # Registers
//!
//! `jit/src/regalloc.rs` has no vector register class. Rather than reach into
//! it, this module carries a self-contained pool of **XMM0..XMM5** — the six
//! vector registers that are caller-saved under *both* the SysV and the Windows
//! x64 ABIs (Windows makes XMM6..XMM15 callee-saved, so using them would owe a
//! save/restore this module does not emit). Allocation is lowest-free-index
//! first, freeing happens at each value's last use, and exhaustion is a refusal
//! rather than a spill. Unifying this with `regalloc.rs` is deliberate future
//! work; see `docs/jit/vectorization-emitter.md`.
//!
//! # Off by default
//!
//! [`VecEmitPolicy::from_flags`] answers [`VecEmitPolicy::Disabled`] unless
//! `CRATONVM_JIT_VECTORIZE` is set to something other than a falsey word, and
//! [`emit_vector_loop`] refuses a `Disabled` request before looking at anything
//! else. The call site passes `VecEmitPolicy::from_flags()`; tests pass
//! `VecEmitPolicy::Enabled` explicitly, so no test mutates process-global
//! environment state.
//!
//! [`VectorIsa`]: super::simd_analysis::vector_gate::VectorIsa
//! [`FpRelaxation`]: super::simd_analysis::vector_gate::FpRelaxation
//! [`VecRefusal::GcReferenceAccess`]: super::simd_analysis::vector_gate::VecRefusal::GcReferenceAccess

#![allow(dead_code)]

use super::disp::{Disp, DispOutOfRange};
use super::simd_analysis::vector_gate::{
    elem_bytes, Alignment, AlignmentPolicy, TailStrategy, VecOp, VecPlan,
};
use crate::ir::MemKind;
use crate::scev::PreheaderGuard;
use cratonvm_types::HEADER_SIZE;

// ---------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------

/// Whether vectorized emission is switched on at all.
///
/// Obtained from [`VecEmitPolicy::from_flags`] in production. The default is
/// [`VecEmitPolicy::Disabled`]: the deep-research report is explicit that vector
/// work multiplies wrong-code risk, so nothing here runs unless it is asked for
/// by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecEmitPolicy {
    /// Emit.
    Enabled,
    /// Refuse every request with [`VecEmitRefusal::Disabled`].
    Disabled,
}

/// The environment variable that turns vectorized emission on.
pub(crate) const VECTORIZE_FLAG: &str = "CRATONVM_JIT_VECTORIZE";

impl VecEmitPolicy {
    /// Read the switch from the VM's flag layer.
    pub(crate) fn from_flags() -> VecEmitPolicy {
        let raw = cratonvm_types::flags::runtime_var_os(VECTORIZE_FLAG);
        VecEmitPolicy::from_flag_value(raw.as_ref().and_then(|v| v.to_str()))
    }

    /// The pure half of [`VecEmitPolicy::from_flags`], so the default can be
    /// tested without touching process-global environment state.
    ///
    /// `None` means unset *or* not valid UTF-8; both answer `Disabled`.
    fn from_flag_value(value: Option<&str>) -> VecEmitPolicy {
        match value {
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "0" | "false" | "off" | "no" => VecEmitPolicy::Disabled,
                _ => VecEmitPolicy::Enabled,
            },
            None => VecEmitPolicy::Disabled,
        }
    }
}

// ---------------------------------------------------------------------------
// Host features
// ---------------------------------------------------------------------------

/// What the *host CPU* actually supports, as opposed to what the plan assumed.
///
/// The plan's [`VectorIsa`] is a compile-time modelling decision; this is the
/// runtime fact. Emitting an AVX2 instruction on a machine without AVX2 is a
/// `SIGILL`, so the two are checked separately and both must agree.
///
/// Outside `cfg(test)` the only constructor is [`HostVectorSupport::detect`],
/// which reads `super::cpu_features`. Production code therefore cannot
/// fabricate a capability.
///
/// [`VectorIsa`]: super::simd_analysis::vector_gate::VectorIsa
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HostVectorSupport {
    avx2: bool,
}

impl HostVectorSupport {
    /// Query the host.
    pub(crate) fn detect() -> HostVectorSupport {
        HostVectorSupport {
            avx2: super::cpu_features::has_avx2(),
        }
    }

    /// Whether the host has AVX2.
    pub(crate) fn has_avx2(self) -> bool {
        self.avx2
    }

    /// A stated capability, for tests only, so the encoding tests run on hosts
    /// without AVX2 (the bytes are checked, never executed).
    #[cfg(test)]
    pub(crate) const fn for_test(avx2: bool) -> HostVectorSupport {
        HostVectorSupport { avx2 }
    }
}

// ---------------------------------------------------------------------------
// The loop shape the emitter is handed
// ---------------------------------------------------------------------------

/// A vector value inside the widened body. Values are single-assignment: each
/// id is defined by exactly one step and must be used at least once.
pub(crate) type VecValueId = usize;

/// The largest value id the emitter will track.
pub(crate) const MAX_VEC_VALUES: usize = 64;

/// One array element access, as a machine address.
///
/// The byte address is `base + HEADER_SIZE + (iv + index_offset) * elem_bytes`,
/// which is the layout `super::simd_analysis::vector_gate::analyze_alignment`
/// and `cratonvm_types::heap_types` both describe: array elements are natural
/// width and contiguous from `HEADER_SIZE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VecArrayOperand {
    /// 64-bit GPR holding the array reference.
    pub base: u8,
    /// The subscript's constant displacement, in elements.
    pub index_offset: i32,
}

/// One straight-line operation of the widened body, in program order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecStep {
    /// `v[dst] = from[iv + off .. +lanes]`
    Load {
        /// Value defined.
        dst: VecValueId,
        /// Where it is read from.
        from: VecArrayOperand,
    },
    /// `v[dst] = v[lhs] OP v[rhs]`, lane-wise.
    Binary {
        /// Value defined.
        dst: VecValueId,
        /// The lane-wise operation.
        op: VecOp,
        /// Left operand.
        lhs: VecValueId,
        /// Right operand.
        rhs: VecValueId,
    },
    /// `to[iv + off .. +lanes] = v[src]`
    Store {
        /// Where it is written.
        to: VecArrayOperand,
        /// Value stored.
        src: VecValueId,
    },
    /// `acc = acc OP v[src]`, lane-wise, into the reduction accumulator.
    Accumulate {
        /// Value folded in.
        src: VecValueId,
    },
}

/// The loop's reduction, when it has one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VecReduction {
    /// The reduction operator. Only `Add`, `Or` and `Xor` are emitted.
    pub op: VecOp,
    /// 64-bit GPR holding the scalar accumulator. Its low 32 bits are read and
    /// written by the epilogue.
    pub acc: u8,
}

/// Where the loop's values live, and what the widened body does.
///
/// Every register named here is a **64-bit** GPR. `iv` and `bound` must hold
/// sign-extended `int` values: the head test is computed in 64 bits precisely so
/// that `iv + lanes` cannot wrap the way the scalar `int` expression could.
#[derive(Debug, Clone)]
pub(crate) struct VecLoopShape {
    /// GPR holding the induction variable, in elements. Live on entry, advanced
    /// by `lanes` per pass, live on exit for the scalar remainder.
    pub iv: u8,
    /// GPR holding the loop's exclusive element bound.
    pub bound: u8,
    /// A GPR the emitter may clobber freely.
    pub scratch: u8,
    /// The widened body, in program order.
    pub body: Vec<VecStep>,
    /// The reduction, if any.
    pub reduction: Option<VecReduction>,
}

/// The runtime operands that discharge one [`PreheaderGuard`].
///
/// The *comparison* is derived from the guard variant, never supplied: a caller
/// can say where the value is, but not what the check means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecGuardValues {
    /// The guard's `term`, already evaluated into a 64-bit GPR.
    ///
    /// Discharges [`PreheaderGuard::NonNegative`], [`PreheaderGuard::AtMost`],
    /// [`PreheaderGuard::AtLeast`] and [`PreheaderGuard::TripCountAtLeast`].
    Term {
        /// GPR holding the term.
        term: u8,
    },
    /// The guarded array's length and the guard's `term`, both in 64-bit GPRs.
    ///
    /// Discharges [`PreheaderGuard::LengthAtLeast`].
    LengthAtLeast {
        /// GPR holding the array's length.
        length: u8,
        /// GPR holding the term.
        term: u8,
    },
}

/// Everything [`emit_vector_loop`] is asked to emit.
#[derive(Debug, Clone, Copy)]
pub(crate) struct VecEmitRequest<'a> {
    /// The admitted plan, straight from
    /// `super::simd_analysis::vector_gate::admit_vectorization`.
    pub plan: &'a VecPlan,
    /// Where the loop's values live and what the body does.
    pub shape: &'a VecLoopShape,
    /// Exactly one entry per `plan.guards`, in the same order.
    pub guards: &'a [VecGuardValues],
    /// What the host CPU has.
    pub host: HostVectorSupport,
    /// Whether vectorized emission is on.
    pub policy: VecEmitPolicy,
    /// The XMM registers the caller guarantees are **dead across this whole
    /// region**, in preference order.
    ///
    /// An argument rather than a constant, and that is the point. Since
    /// 2026-08-04 `regalloc::xmm_roles::VECTOR_REGION_MAX` is XMM8–XMM15 and is
    /// **disjoint** from `ir_lower`'s FP scratch pair and its linear-scan file,
    /// so the caller no longer owes a "these scalars are dead" argument for
    /// them. What it still owes is [`Self::frame_saved_xmms`]: on Windows every
    /// register in the pool is callee-saved.
    ///
    /// An empty slice is legal and refuses at the first allocation, which is
    /// the right answer for a caller that has not done the analysis. Any
    /// register `regalloc::xmm_roles::vector_pool_is_encodable` rejects refuses
    /// the whole region.
    pub vector_pool: &'a [u8],
    /// The XMM registers the calling frame's prologue saves and its exits
    /// restore — `ir_lower::IR_LOWER_SAVED_XMMS`, or whatever a future caller
    /// reserves.
    ///
    /// Only consulted on Windows, where every register in [`Self::vector_pool`]
    /// is non-volatile. It is a separate field rather than an implication of
    /// `vector_pool` because the two answer different questions — "is this
    /// register free *inside* this method" and "does this frame restore it for
    /// its *caller*" — and a caller that conflated them is exactly the bug this
    /// pair exists to make unwritable.
    pub frame_saved_xmms: &'a [u8],
}

// ---------------------------------------------------------------------------
// The result
// ---------------------------------------------------------------------------

/// The emitted vector region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VecLoopCode {
    /// The machine code, position-independent apart from `fallback_sites`.
    pub code: Vec<u8>,
    /// Byte offsets, within `code`, of `rel32` displacement fields that must be
    /// patched to point at the scalar loop's pre-header. One per guard. The
    /// caller **must** patch every one of them.
    pub fallback_sites: Vec<usize>,
    /// Offset one past the last emitted byte: where control resumes, with `iv`
    /// live, in the caller's untouched scalar loop.
    pub remainder_entry: usize,
    /// XMM registers written by this region.
    pub clobbered_vector_regs: Vec<u8>,
    /// GPRs written by this region, besides the induction variable.
    pub clobbered_gprs: Vec<u8>,
    /// Lanes per vector pass, echoed from the plan.
    pub lanes: usize,
    /// Bytes per vector pass, echoed from the plan.
    pub width_bytes: usize,
    /// Upper bound on the scalar iterations left to the remainder.
    pub max_remainder_iterations: usize,
}

/// Why a request was not emitted. Each variant is a hard refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VecEmitRefusal {
    /// Vectorized emission is switched off.
    Disabled,
    /// The host CPU has no AVX2, and every encoding here is VEX.
    HostLacksAvx2,
    /// The plan was decided against an ISA whose vector moves must be naturally
    /// aligned, which this emitter does not model.
    StrictAlignmentIsa,
    /// The plan's alignment verdict is unknown and its ISA needs it proven.
    UnprovableAlignment,
    /// `lanes * elem_bytes(elem)` is not the plan's `width_bytes`, or the width
    /// is not 16 or 32 bytes.
    UnsupportedWidth {
        /// The width asked for.
        width_bytes: usize,
        /// The lane count asked for.
        lanes: usize,
    },
    /// The element type is a **reference**. A vector store of oops bypasses the
    /// GC write barrier; there is no lane count that rescues it.
    ObjectReferenceElement,
    /// The element type is `byte`, `char` or `short`. JVM arithmetic on those is
    /// performed in `int` and narrowed at the store, so a lane-wise sub-word
    /// operation wraps at the wrong width.
    SubwordElement {
        /// The element type.
        elem: MemKind,
    },
    /// The lane-wise operation has no encoding here for this element type.
    UnsupportedOp {
        /// Element type.
        elem: MemKind,
        /// Operator.
        op: VecOp,
    },
    /// The operation needs an ISA feature the plan's target does not claim.
    MissingIsaFeature {
        /// Element type.
        elem: MemKind,
        /// Operator.
        op: VecOp,
    },
    /// The reduction operator or accumulator type is not one of the three the
    /// epilogue implements.
    UnsupportedReduction {
        /// Accumulator type.
        elem: MemKind,
        /// Operator.
        op: VecOp,
    },
    /// The body contains an `Accumulate` step but the shape declares no
    /// reduction, or declares one the body never accumulates into.
    ReductionMismatch,
    /// The number of guard bindings differs from the number of plan guards.
    GuardCountMismatch {
        /// Guards in the plan.
        wanted: usize,
        /// Bindings supplied.
        got: usize,
    },
    /// A binding does not have the shape its guard needs.
    GuardBindingMismatch {
        /// Index into `plan.guards`.
        index: usize,
    },
    /// A guard variant this emitter has no encoding for.
    ///
    /// Only [`PreheaderGuard::StrideInRange`] reaches this, and only in
    /// principle: `jit/src/scev.rs:1582` produces it exclusively for
    /// `Stride::Variable`, which the admission gate already refuses with
    /// `VecRefusal::VariableStride`. Refusing it here costs nothing and closes
    /// the case where that stops being true.
    UnsupportedGuard {
        /// Index into `plan.guards`.
        index: usize,
    },
    /// A guard's immediate operand does not fit a signed 32-bit field.
    GuardImmediateOutOfRange {
        /// Index into `plan.guards`.
        index: usize,
    },
    /// A register named by the shape is `RSP`, which cannot be a SIB index and
    /// must never be clobbered.
    ForbiddenRegister {
        /// The offending register number.
        reg: u8,
    },
    /// Two roles in the shape were given the same register.
    RegisterConflict {
        /// The doubly-assigned register.
        reg: u8,
    },
    /// A register number is not a valid x86-64 GPR encoding.
    InvalidRegister {
        /// The offending number.
        reg: u8,
    },
    /// The body is empty; there is nothing to widen.
    EmptyBody,
    /// A step uses a value that no earlier step defines.
    ValueNotDefined {
        /// The value id.
        value: VecValueId,
    },
    /// A value is defined twice, or its id is out of range.
    ValueRedefined {
        /// The value id.
        value: VecValueId,
    },
    /// A value is defined and never used. A dead vector load in a widened body
    /// is a producer bug, not something to silently emit.
    DeadVectorValue {
        /// The value id.
        value: VecValueId,
    },
    /// The XMM pool ran out. This emitter does not spill.
    OutOfVectorRegisters,
    /// The caller offered a register outside [`VEC_POOL`], or — on Windows,
    /// where the whole pool is callee-saved — one its own prologue does not
    /// save (`VecEmitRequest::frame_saved_xmms`). Refused for the whole region
    /// rather than skipped, because a pool the caller believes it handed over
    /// and this module quietly narrowed is a pool nobody is reasoning about
    /// correctly.
    UnusableVectorPool {
        /// The offending register number.
        reg: u8,
    },
    /// An element address needs a displacement x86-64 cannot encode.
    DisplacementOutOfRange {
        /// The displacement asked for.
        disp: i64,
    },
    /// A branch displacement does not fit a `rel32`.
    BranchOutOfRange,
}

impl From<DispOutOfRange> for VecEmitRefusal {
    fn from(e: DispOutOfRange) -> VecEmitRefusal {
        VecEmitRefusal::DisplacementOutOfRange { disp: e.value }
    }
}

// ---------------------------------------------------------------------------
// The vector register pool
// ---------------------------------------------------------------------------

/// The widest pool a caller may hand this emitter, and the set the tests drive
/// it with.
///
/// XMM8..XMM15, and every one of them is callee-saved on Windows and volatile
/// on System V. That asymmetry is the reason the second half of the
/// admissibility test is a *frame* property: see
/// [`VecEmitRequest::frame_saved_xmms`].
///
/// It used to be XMM0..XMM5 — caller-saved everywhere, so free of any prologue
/// obligation, but also `ir_lower`'s FP scratch pair *and* its entire
/// linear-scan file, so a region that helped itself to all six destroyed any
/// scalar `double` living there and the caller had to prove it did not. Since
/// `ir_lower::emit_prologue` grew a save area (2026-08-04) the pool sits above
/// both scalar authorities and that proof obligation is gone; what replaced it
/// is narrower and mechanical. `regalloc::xmm_roles::disjointness_violation`
/// is the check.
pub(crate) const VEC_POOL: [u8; VEC_POOL_LEN] = crate::regalloc::xmm_roles::VECTOR_REGION_MAX;

/// How many registers [`VEC_POOL`] holds, and the cap on any caller's pool.
pub(crate) const VEC_POOL_LEN: usize = 8;

/// Lowest-free-index allocation over a **caller-supplied** register set, with
/// no spilling.
#[derive(Debug, Clone)]
struct VecRegPool<'p> {
    pool: &'p [u8],
    in_use: [bool; VEC_POOL_LEN],
    ever_used: [bool; VEC_POOL_LEN],
}

impl<'p> VecRegPool<'p> {
    /// An empty pool is a legal argument and a useful one: it refuses at the
    /// first allocation, which is what a caller that has proved nothing about
    /// its scalar FP values should get.
    fn new(pool: &'p [u8]) -> VecRegPool<'p> {
        VecRegPool {
            pool,
            in_use: [false; VEC_POOL_LEN],
            ever_used: [false; VEC_POOL_LEN],
        }
    }

    /// Take the lowest free register, or refuse.
    fn alloc(&mut self) -> Result<u8, VecEmitRefusal> {
        for (slot, reg) in self.pool.iter().enumerate().take(VEC_POOL_LEN) {
            match self.in_use.get(slot) {
                Some(false) => {
                    if let Some(u) = self.in_use.get_mut(slot) {
                        *u = true;
                    }
                    if let Some(e) = self.ever_used.get_mut(slot) {
                        *e = true;
                    }
                    return Ok(*reg);
                }
                _ => continue,
            }
        }
        Err(VecEmitRefusal::OutOfVectorRegisters)
    }

    /// Return a register to the pool. Freeing a register that is not held is a
    /// no-op rather than a panic — this module never panics in production.
    fn free(&mut self, reg: u8) {
        if let Some(slot) = self.pool.iter().position(|r| *r == reg) {
            if let Some(u) = self.in_use.get_mut(slot) {
                *u = false;
            }
        }
    }

    /// Every register the pool has handed out at least once.
    fn clobbered(&self) -> Vec<u8> {
        self.pool
            .iter()
            .enumerate()
            .take(VEC_POOL_LEN)
            .filter(|(slot, _)| matches!(self.ever_used.get(*slot), Some(true)))
            .map(|(_, reg)| *reg)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Instruction encodings
// ---------------------------------------------------------------------------

/// `RSP`. Cannot be a SIB index, and clobbering it destroys the frame.
const RSP: u8 = 4;

/// `JL rel32` — jump if signed less.
const CC_LESS: u8 = 0x8C;
/// `JG rel32` — jump if signed greater.
const CC_GREATER: u8 = 0x8F;

/// A VEX-encoded packed instruction: escape map, mandatory prefix, `W`, opcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VexOpcode {
    /// VEX `mmmmm`: 1 = `0F`, 2 = `0F 38`, 3 = `0F 3A`.
    map: u8,
    /// VEX `pp`: 0 = none, 1 = `66`, 2 = `F3`, 3 = `F2`.
    pp: u8,
    /// VEX `W`.
    w: bool,
    /// The opcode byte.
    op: u8,
}

/// The unaligned packed move for `elem`: `(load, store)`.
///
/// Integer lanes move with `VMOVDQU`; `float`/`double` lanes move with
/// `VMOVUPS`/`VMOVUPD` so the data does not cross the integer/FP domain on
/// every iteration.
fn move_opcodes(elem: MemKind) -> Result<(VexOpcode, VexOpcode), VecEmitRefusal> {
    let (pp, load, store) = match elem {
        // VEX.F3.0F 6F /r, VEX.F3.0F 7F /r — VMOVDQU
        MemKind::Int | MemKind::Long => (2u8, 0x6Fu8, 0x7Fu8),
        // VEX.0F 10 /r, VEX.0F 11 /r — VMOVUPS
        MemKind::Float => (0, 0x10, 0x11),
        // VEX.66.0F 10 /r, VEX.66.0F 11 /r — VMOVUPD
        MemKind::Double => (1, 0x10, 0x11),
        MemKind::Ref => return Err(VecEmitRefusal::ObjectReferenceElement),
        other => return Err(VecEmitRefusal::SubwordElement { elem: other }),
    };
    Ok((
        VexOpcode {
            map: 1,
            pp,
            w: false,
            op: load,
        },
        VexOpcode {
            map: 1,
            pp,
            w: false,
            op: store,
        },
    ))
}

/// The lane-wise encoding of `op` on `elem`, or a refusal.
///
/// `int32_mul_minmax` is the plan's ISA claim: `PMULLD` is SSE4.1, so a plan
/// admitted against plain SSE2 must not reach a `Mul` here.
fn arith_opcode(
    elem: MemKind,
    op: VecOp,
    int32_mul_minmax: bool,
) -> Result<VexOpcode, VecEmitRefusal> {
    let unsupported = Err(VecEmitRefusal::UnsupportedOp { elem, op });
    let enc = match elem {
        MemKind::Int => match op {
            // VEX.66.0F FE /r  VPADDD
            VecOp::Add => (1u8, 1u8, 0xFEu8),
            // VEX.66.0F FA /r  VPSUBD
            VecOp::Sub => (1, 1, 0xFA),
            // VEX.66.0F DB /r  VPAND
            VecOp::And => (1, 1, 0xDB),
            // VEX.66.0F EB /r  VPOR
            VecOp::Or => (1, 1, 0xEB),
            // VEX.66.0F EF /r  VPXOR
            VecOp::Xor => (1, 1, 0xEF),
            // VEX.66.0F38 40 /r  VPMULLD — SSE4.1 and above only.
            VecOp::Mul => {
                if !int32_mul_minmax {
                    return Err(VecEmitRefusal::MissingIsaFeature { elem, op });
                }
                (2, 1, 0x40)
            }
            _ => return unsupported,
        },
        MemKind::Long => match op {
            // VEX.66.0F D4 /r  VPADDQ
            VecOp::Add => (1, 1, 0xD4),
            // VEX.66.0F FB /r  VPSUBQ
            VecOp::Sub => (1, 1, 0xFB),
            VecOp::And => (1, 1, 0xDB),
            VecOp::Or => (1, 1, 0xEB),
            VecOp::Xor => (1, 1, 0xEF),
            // No `VPMULLQ` outside AVX-512.
            _ => return unsupported,
        },
        MemKind::Float => match op {
            // VEX.0F 58/5C/59/5E /r  VADDPS / VSUBPS / VMULPS / VDIVPS
            VecOp::Add => (1, 0, 0x58),
            VecOp::Sub => (1, 0, 0x5C),
            VecOp::Mul => (1, 0, 0x59),
            VecOp::Div => (1, 0, 0x5E),
            _ => return unsupported,
        },
        MemKind::Double => match op {
            // VEX.66.0F 58/5C/59/5E /r  VADDPD / VSUBPD / VMULPD / VDIVPD
            VecOp::Add => (1, 1, 0x58),
            VecOp::Sub => (1, 1, 0x5C),
            VecOp::Mul => (1, 1, 0x59),
            VecOp::Div => (1, 1, 0x5E),
            _ => return unsupported,
        },
        MemKind::Ref => return Err(VecEmitRefusal::ObjectReferenceElement),
        other => return Err(VecEmitRefusal::SubwordElement { elem: other }),
    };
    Ok(VexOpcode {
        map: enc.0,
        pp: enc.1,
        w: false,
        op: enc.2,
    })
}

/// The scalar `op r/m32, r32` opcode that folds the reduction's final lane into
/// the accumulator GPR.
fn scalar_fold_opcode(op: VecOp) -> Option<u8> {
    match op {
        VecOp::Add => Some(0x01),
        VecOp::Or => Some(0x09),
        VecOp::Xor => Some(0x31),
        _ => None,
    }
}

/// A byte sink with the handful of x86-64 forms this module needs.
#[derive(Debug, Default)]
struct Asm {
    out: Vec<u8>,
}

impl Asm {
    fn new() -> Asm {
        Asm::default()
    }

    fn len(&self) -> usize {
        self.out.len()
    }

    fn byte(&mut self, b: u8) {
        self.out.push(b);
    }

    fn bytes(&mut self, bs: &[u8]) {
        self.out.extend_from_slice(bs);
    }

    /// Emit a VEX prefix.
    ///
    /// `rex_r`, `rex_x` and `rex_b` are the **true** extension bits (`true` =
    /// the register is r8..r15 / xmm8..xmm15). VEX stores them inverted; the
    /// inversion happens here exactly once, which is the whole reason this
    /// takes uninverted bits rather than the pre-inverted ones the older
    /// hand-written emitters in `x64.rs` pass around.
    ///
    /// The two-byte form is legal only when `X`, `B` and `W` are all clear and
    /// the escape map is `0F`; otherwise the three-byte form is emitted.
    #[allow(clippy::too_many_arguments)]
    fn vex_prefix(
        &mut self,
        rex_r: bool,
        rex_x: bool,
        rex_b: bool,
        map: u8,
        w: bool,
        vvvv: u8,
        l: bool,
        pp: u8,
    ) {
        let l_bit = if l { 0x04 } else { 0x00 };
        let vvvv_bits = ((!vvvv) & 0x0F) << 3;
        if !rex_x && !rex_b && !w && map == 1 {
            self.byte(0xC5);
            let r_bit = if rex_r { 0x00 } else { 0x80 };
            self.byte(r_bit | vvvv_bits | l_bit | (pp & 0x03));
            return;
        }
        self.byte(0xC4);
        let b1 = (if rex_r { 0x00 } else { 0x80 })
            | (if rex_x { 0x00 } else { 0x40 })
            | (if rex_b { 0x00 } else { 0x20 })
            | (map & 0x1F);
        self.byte(b1);
        let b2 = (if w { 0x80 } else { 0x00 }) | vvvv_bits | l_bit | (pp & 0x03);
        self.byte(b2);
    }

    /// `vop vreg_dst, vreg_src1, vreg_src2` — a three-operand register form.
    fn vec_rr(&mut self, enc: VexOpcode, l: bool, dst: u8, src1: u8, src2: u8) {
        self.vex_prefix(dst >= 8, false, src2 >= 8, enc.map, enc.w, src1, l, enc.pp);
        self.byte(enc.op);
        self.byte(0xC0 | ((dst & 7) << 3) | (src2 & 7));
    }

    /// `vop vreg, [base + index*scale + disp]`, or the store direction with the
    /// same operand layout (`vreg` is always the ModRM `reg` field).
    ///
    /// Always emits a SIB byte: the address always has an index.
    #[allow(clippy::too_many_arguments)]
    fn vec_mem(
        &mut self,
        enc: VexOpcode,
        l: bool,
        vreg: u8,
        base: u8,
        index: u8,
        scale_log2: u8,
        disp: Disp,
    ) {
        self.vex_prefix(
            vreg >= 8,
            index >= 8,
            base >= 8,
            enc.map,
            enc.w,
            0,
            l,
            enc.pp,
        );
        self.byte(enc.op);
        self.byte(disp.modrm(vreg, 0b100));
        self.byte(((scale_log2 & 3) << 6) | ((index & 7) << 3) | (base & 7));
        disp.emit_into(&mut self.out);
    }

    /// `VPSHUFD xmm_dst, xmm_src, imm8` — VEX.128.66.0F 70 /r ib.
    fn vpshufd(&mut self, dst: u8, src: u8, imm: u8) {
        self.vex_prefix(dst >= 8, false, src >= 8, 1, false, 0, false, 1);
        self.byte(0x70);
        self.byte(0xC0 | ((dst & 7) << 3) | (src & 7));
        self.byte(imm);
    }

    /// `VEXTRACTI128 xmm_dst, ymm_src, imm8` — VEX.256.66.0F3A.W0 39 /r ib.
    ///
    /// Note the operand direction: the *source* is the ModRM `reg` field and
    /// the destination is `r/m`.
    fn vextracti128(&mut self, dst: u8, src: u8, lane: u8) {
        self.vex_prefix(src >= 8, false, dst >= 8, 3, false, 0, true, 1);
        self.byte(0x39);
        self.byte(0xC0 | ((src & 7) << 3) | (dst & 7));
        self.byte(lane);
    }

    /// `VMOVD r32, xmm` — VEX.128.66.0F.W0 7E /r. The XMM register is the
    /// ModRM `reg` field.
    fn vmovd_to_gpr(&mut self, dst_gpr: u8, src_xmm: u8) {
        self.vex_prefix(src_xmm >= 8, false, dst_gpr >= 8, 1, false, 0, false, 1);
        self.byte(0x7E);
        self.byte(0xC0 | ((src_xmm & 7) << 3) | (dst_gpr & 7));
    }

    /// `VZEROUPPER` — VEX.128.0F.WIG 77.
    fn vzeroupper(&mut self) {
        self.bytes(&[0xC5, 0xF8, 0x77]);
    }

    /// `REX` byte, emitted unconditionally (every caller here sets `W`).
    fn rex_w(&mut self, r: bool, x: bool, b: bool) {
        self.byte(
            0x48 | (if r { 0x04 } else { 0 })
                | (if x { 0x02 } else { 0 })
                | (if b { 0x01 } else { 0 }),
        );
    }

    /// `lea dst64, [base + disp]`.
    fn lea(&mut self, dst: u8, base: u8, disp: i64) -> Result<(), VecEmitRefusal> {
        let d = Disp::encode_for_base(disp, base)?;
        self.rex_w(dst >= 8, false, base >= 8);
        self.byte(0x8D);
        if super::disp::base_requires_sib(base) {
            self.byte(d.modrm(dst, 0b100));
            self.byte(0x24);
        } else {
            self.byte(d.modrm(dst, base));
        }
        d.emit_into(&mut self.out);
        Ok(())
    }

    /// `cmp a64, b64` — `REX.W 3B /r`, `reg` = `a`.
    fn cmp_rr(&mut self, a: u8, b: u8) {
        self.rex_w(a >= 8, false, b >= 8);
        self.byte(0x3B);
        self.byte(0xC0 | ((a & 7) << 3) | (b & 7));
    }

    /// `cmp r64, imm` — `REX.W 83 /7 ib` or `REX.W 81 /7 id`.
    fn cmp_ri(&mut self, r: u8, imm: i64) -> Result<(), VecEmitRefusal> {
        self.alu_ri(r, 7, imm)
    }

    /// `add r64, imm` — `REX.W 83 /0 ib` or `REX.W 81 /0 id`.
    fn add_ri(&mut self, r: u8, imm: i64) -> Result<(), VecEmitRefusal> {
        self.alu_ri(r, 0, imm)
    }

    fn alu_ri(&mut self, r: u8, ext: u8, imm: i64) -> Result<(), VecEmitRefusal> {
        self.rex_w(false, false, r >= 8);
        if let Ok(i) = i8::try_from(imm) {
            self.byte(0x83);
            self.byte(0xC0 | ((ext & 7) << 3) | (r & 7));
            // Cast: `imm` is already range-checked as a signed `i8`; this is
            // the encoding of that value, not a narrowing.
            self.byte(i as u8);
            return Ok(());
        }
        match i32::try_from(imm) {
            Ok(i) => {
                self.byte(0x81);
                self.byte(0xC0 | ((ext & 7) << 3) | (r & 7));
                self.bytes(&i.to_le_bytes());
                Ok(())
            }
            Err(_) => Err(VecEmitRefusal::DisplacementOutOfRange { disp: imm }),
        }
    }

    /// `op r/m32, r32` — the scalar reduction fold. `reg` = source.
    fn alu_rr32(&mut self, opcode: u8, dst: u8, src: u8) {
        if dst >= 8 || src >= 8 {
            self.byte(0x40 | (if src >= 8 { 0x04 } else { 0 }) | (if dst >= 8 { 0x01 } else { 0 }));
        }
        self.byte(opcode);
        self.byte(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// `jcc rel32`. Returns the offset of the four displacement bytes.
    fn jcc_rel32(&mut self, cc: u8) -> usize {
        self.byte(0x0F);
        self.byte(cc);
        let site = self.len();
        self.bytes(&[0, 0, 0, 0]);
        site
    }

    /// `jmp rel32`. Returns the offset of the four displacement bytes.
    fn jmp_rel32(&mut self) -> usize {
        self.byte(0xE9);
        let site = self.len();
        self.bytes(&[0, 0, 0, 0]);
        site
    }

    /// Patch a `rel32` site so the branch lands on `target`.
    fn patch_rel32(&mut self, site: usize, target: usize) -> Result<(), VecEmitRefusal> {
        let next = site
            .checked_add(4)
            .ok_or(VecEmitRefusal::BranchOutOfRange)?;
        let delta = (target as i64) - (next as i64);
        let rel = i32::try_from(delta).map_err(|_| VecEmitRefusal::BranchOutOfRange)?;
        let bytes = rel.to_le_bytes();
        for (k, b) in bytes.iter().enumerate() {
            match self.out.get_mut(site + k) {
                Some(slot) => *slot = *b,
                None => return Err(VecEmitRefusal::BranchOutOfRange),
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `log2` of an element size, for the SIB `scale` field.
fn scale_log2(elem_size: usize) -> Option<u8> {
    match elem_size {
        1 => Some(0),
        2 => Some(1),
        4 => Some(2),
        8 => Some(3),
        _ => None,
    }
}

fn check_gpr(reg: u8) -> Result<(), VecEmitRefusal> {
    if reg > 15 {
        return Err(VecEmitRefusal::InvalidRegister { reg });
    }
    if reg == RSP {
        return Err(VecEmitRefusal::ForbiddenRegister { reg });
    }
    Ok(())
}

/// Per-value definition and last-use indices.
#[derive(Debug, Clone, Copy, Default)]
struct ValueLife {
    def: Option<usize>,
    last_use: Option<usize>,
}

/// Validate the body's single-assignment discipline and compute last uses.
fn analyse_values(body: &[VecStep]) -> Result<Vec<ValueLife>, VecEmitRefusal> {
    fn use_value(lives: &mut [ValueLife], v: VecValueId, at: usize) -> Result<(), VecEmitRefusal> {
        match lives.get_mut(v) {
            Some(life) if life.def.is_some() => {
                life.last_use = Some(at);
                Ok(())
            }
            _ => Err(VecEmitRefusal::ValueNotDefined { value: v }),
        }
    }

    fn def_value(lives: &mut [ValueLife], v: VecValueId, at: usize) -> Result<(), VecEmitRefusal> {
        match lives.get_mut(v) {
            Some(life) if life.def.is_none() => {
                life.def = Some(at);
                Ok(())
            }
            _ => Err(VecEmitRefusal::ValueRedefined { value: v }),
        }
    }

    let mut lives = vec![ValueLife::default(); MAX_VEC_VALUES];
    for (i, step) in body.iter().enumerate() {
        match step {
            VecStep::Load { dst, .. } => def_value(&mut lives, *dst, i)?,
            VecStep::Binary { dst, lhs, rhs, .. } => {
                use_value(&mut lives, *lhs, i)?;
                use_value(&mut lives, *rhs, i)?;
                def_value(&mut lives, *dst, i)?;
            }
            VecStep::Store { src, .. } => use_value(&mut lives, *src, i)?,
            VecStep::Accumulate { src } => use_value(&mut lives, *src, i)?,
        }
    }

    for (v, life) in lives.iter().enumerate() {
        if life.def.is_some() && life.last_use.is_none() {
            return Err(VecEmitRefusal::DeadVectorValue { value: v });
        }
    }
    Ok(lives)
}

/// Every GPR role the shape names, checked for validity and for collisions.
fn check_registers(shape: &VecLoopShape) -> Result<(), VecEmitRefusal> {
    let mut exclusive: Vec<u8> = vec![shape.iv, shape.bound, shape.scratch];
    if let Some(r) = shape.reduction {
        exclusive.push(r.acc);
    }
    for reg in &exclusive {
        check_gpr(*reg)?;
    }
    for (i, a) in exclusive.iter().enumerate() {
        for b in exclusive.iter().skip(i + 1) {
            if a == b {
                return Err(VecEmitRefusal::RegisterConflict { reg: *a });
            }
        }
    }
    for step in &shape.body {
        let base = match step {
            VecStep::Load { from, .. } => Some(from.base),
            VecStep::Store { to, .. } => Some(to.base),
            _ => None,
        };
        if let Some(base) = base {
            check_gpr(base)?;
            if exclusive.contains(&base) {
                return Err(VecEmitRefusal::RegisterConflict { reg: base });
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The entry point
// ---------------------------------------------------------------------------

/// Emit the vector region for an admitted loop, or refuse.
///
/// This is the module's only entry point. It is deliberately *not* wired into
/// `x64.rs`: the call site is added separately, and it must pass
/// [`VecEmitPolicy::from_flags`] and [`HostVectorSupport::detect`] rather than
/// asserting either.
///
/// On success the caller must:
///
/// 1. Place `code` immediately after the loop's pre-header, so that control
///    reaches offset `0` with `shape.iv`, `shape.bound` and every array base
///    already live in the registers the shape names.
/// 2. Patch **every** offset in `fallback_sites` to the scalar loop's
///    pre-header. They are `rel32` fields; a site at offset `s` needs
///    `target - (s + 4)`.
/// 3. Continue with the *unmodified* scalar loop at `remainder_entry`, which
///    runs the tail with `shape.iv` live.
///
/// Failing to do (2) for even one site emits a vector loop whose safety proof is
/// incomplete — which is why the count of sites always equals `plan.guards.len()`
/// and a request that cannot supply a binding for every guard is refused outright.
pub(crate) fn emit_vector_loop(req: &VecEmitRequest<'_>) -> Result<VecLoopCode, VecEmitRefusal> {
    if req.policy == VecEmitPolicy::Disabled {
        return Err(VecEmitRefusal::Disabled);
    }
    // Every encoding below is VEX. A host without AVX2 would take a SIGILL.
    if !req.host.has_avx2() {
        return Err(VecEmitRefusal::HostLacksAvx2);
    }
    // The caller's pool, checked before anything is emitted. Two ways to fail:
    // a register outside `VEC_POOL` is not this emitter's to give, and one the
    // caller's own prologue does not save would corrupt a caller's
    // floating-point state on Windows and not on Linux — the worst shape a bug
    // can have.
    for &reg in req.vector_pool {
        if !crate::regalloc::xmm_roles::vector_pool_is_encodable(reg, req.frame_saved_xmms) {
            return Err(VecEmitRefusal::UnusableVectorPool { reg });
        }
    }

    let plan = req.plan;
    let shape = req.shape;

    // ---- the plan's target -------------------------------------------------
    if plan.isa.alignment == AlignmentPolicy::NaturalRequired {
        // Every move emitted here is the unaligned form. A strict-alignment
        // target is a different emitter, not a different flag.
        if plan.alignment == Alignment::Unknown {
            return Err(VecEmitRefusal::UnprovableAlignment);
        }
        return Err(VecEmitRefusal::StrictAlignmentIsa);
    }

    // ---- the element type --------------------------------------------------
    if plan.elem == MemKind::Ref {
        // A vector store of oops bypasses the GC write barrier. Refused here
        // independently of the gate's own `GcReferenceAccess` refusal.
        return Err(VecEmitRefusal::ObjectReferenceElement);
    }
    if matches!(plan.elem, MemKind::Byte | MemKind::Char | MemKind::Short) {
        return Err(VecEmitRefusal::SubwordElement { elem: plan.elem });
    }
    let elem_size = elem_bytes(plan.elem);
    let scale = match scale_log2(elem_size) {
        Some(s) => s,
        None => return Err(VecEmitRefusal::SubwordElement { elem: plan.elem }),
    };

    // ---- the width ---------------------------------------------------------
    if plan.lanes < 2
        || elem_size == 0
        || plan.lanes.checked_mul(elem_size) != Some(plan.width_bytes)
        || !matches!(plan.width_bytes, 16 | 32)
    {
        return Err(VecEmitRefusal::UnsupportedWidth {
            width_bytes: plan.width_bytes,
            lanes: plan.lanes,
        });
    }
    let l = plan.width_bytes == 32;

    // ---- the body ----------------------------------------------------------
    if shape.body.is_empty() {
        return Err(VecEmitRefusal::EmptyBody);
    }
    check_registers(shape)?;
    let lives = analyse_values(&shape.body)?;

    let accumulates = shape
        .body
        .iter()
        .any(|s| matches!(s, VecStep::Accumulate { .. }));
    match (accumulates, shape.reduction) {
        (true, Some(_)) | (false, None) => {}
        _ => return Err(VecEmitRefusal::ReductionMismatch),
    }

    // The reduction epilogue exists for exactly three operators on `int`.
    // Floating-point reductions are refused here even when the gate admitted
    // one under `FpRelaxation::AllowReassociation`.
    let reduction_enc = match shape.reduction {
        None => None,
        Some(r) => {
            if plan.elem != MemKind::Int || scalar_fold_opcode(r.op).is_none() {
                return Err(VecEmitRefusal::UnsupportedReduction {
                    elem: plan.elem,
                    op: r.op,
                });
            }
            Some((r, arith_opcode(plan.elem, r.op, plan.isa.int32_mul_minmax)?))
        }
    };

    let (load_enc, store_enc) = move_opcodes(plan.elem)?;

    let mut asm = Asm::new();
    let mut pool = VecRegPool::new(req.vector_pool);
    let mut fallback_sites: Vec<usize> = Vec::new();

    // ---- guards, before any vector register is touched ---------------------
    if req.guards.len() != plan.guards.len() {
        return Err(VecEmitRefusal::GuardCountMismatch {
            wanted: plan.guards.len(),
            got: req.guards.len(),
        });
    }
    for (index, entry) in plan.guards.iter().enumerate() {
        let binding = match req.guards.get(index) {
            Some(b) => *b,
            None => {
                return Err(VecEmitRefusal::GuardCountMismatch {
                    wanted: plan.guards.len(),
                    got: req.guards.len(),
                })
            }
        };
        emit_guard(&mut asm, index, &entry.guard, binding, &mut fallback_sites)?;
    }

    // ---- the reduction accumulator ----------------------------------------
    let acc_reg = match reduction_enc {
        None => None,
        Some((_, _)) => {
            let reg = pool.alloc()?;
            // VPXOR acc, acc, acc — identity for `+`, `|` and `^`.
            let zero = VexOpcode {
                map: 1,
                pp: 1,
                w: false,
                op: 0xEF,
            };
            asm.vec_rr(zero, l, reg, reg, reg);
            Some(reg)
        }
    };

    // ---- the head test -----------------------------------------------------
    let loop_head = asm.len();
    asm.lea(shape.scratch, shape.iv, plan.lanes as i64)?;
    asm.cmp_rr(shape.scratch, shape.bound);
    let exit_site = asm.jcc_rel32(CC_GREATER);

    // ---- the widened body --------------------------------------------------
    let mut regs: Vec<Option<u8>> = vec![None; MAX_VEC_VALUES];
    for (i, step) in shape.body.iter().enumerate() {
        match step {
            VecStep::Load { dst, from } => {
                let disp = element_disp(from.index_offset, elem_size)?;
                let d = Disp::encode_for_base(disp, from.base)?;
                let reg = pool.alloc()?;
                asm.vec_mem(load_enc, l, reg, from.base, shape.iv, scale, d);
                if let Some(slot) = regs.get_mut(*dst) {
                    *slot = Some(reg);
                }
            }
            VecStep::Binary { dst, op, lhs, rhs } => {
                let lreg = value_reg(&regs, *lhs)?;
                let rreg = value_reg(&regs, *rhs)?;
                let enc = arith_opcode(plan.elem, *op, plan.isa.int32_mul_minmax)?;
                // Free dead sources *before* allocating the destination: the
                // VEX three-operand form reads both sources before writing, so
                // reusing a source's register for the result is safe and keeps
                // the pool small.
                release_if_dead(&mut pool, &mut regs, &lives, *lhs, i);
                release_if_dead(&mut pool, &mut regs, &lives, *rhs, i);
                let dreg = pool.alloc()?;
                asm.vec_rr(enc, l, dreg, lreg, rreg);
                if let Some(slot) = regs.get_mut(*dst) {
                    *slot = Some(dreg);
                }
            }
            VecStep::Store { to, src } => {
                let sreg = value_reg(&regs, *src)?;
                let disp = element_disp(to.index_offset, elem_size)?;
                let d = Disp::encode_for_base(disp, to.base)?;
                asm.vec_mem(store_enc, l, sreg, to.base, shape.iv, scale, d);
                release_if_dead(&mut pool, &mut regs, &lives, *src, i);
            }
            VecStep::Accumulate { src } => {
                let sreg = value_reg(&regs, *src)?;
                match (acc_reg, reduction_enc) {
                    (Some(acc), Some((_, enc))) => asm.vec_rr(enc, l, acc, acc, sreg),
                    _ => return Err(VecEmitRefusal::ReductionMismatch),
                }
                release_if_dead(&mut pool, &mut regs, &lives, *src, i);
            }
        }
    }

    // ---- advance and repeat ------------------------------------------------
    asm.add_ri(shape.iv, plan.lanes as i64)?;
    let back_site = asm.jmp_rel32();
    asm.patch_rel32(back_site, loop_head)?;

    // ---- the epilogue ------------------------------------------------------
    let epilogue = asm.len();
    asm.patch_rel32(exit_site, epilogue)?;

    if let (Some(acc), Some((red, enc))) = (acc_reg, reduction_enc) {
        let tmp = pool.alloc()?;
        // 256-bit: fold the high 128 bits into the low ones first.
        if l {
            asm.vextracti128(tmp, acc, 1);
            asm.vec_rr(enc, false, acc, acc, tmp);
        }
        // Two 128-bit shuffles reduce four 32-bit lanes to one.
        asm.vpshufd(tmp, acc, 0x4E);
        asm.vec_rr(enc, false, acc, acc, tmp);
        asm.vpshufd(tmp, acc, 0xB1);
        asm.vec_rr(enc, false, acc, acc, tmp);
        asm.vmovd_to_gpr(shape.scratch, acc);
        match scalar_fold_opcode(red.op) {
            Some(opcode) => asm.alu_rr32(opcode, red.acc, shape.scratch),
            None => {
                return Err(VecEmitRefusal::UnsupportedReduction {
                    elem: plan.elem,
                    op: red.op,
                })
            }
        }
        pool.free(tmp);
        pool.free(acc);
    }

    // Only 256-bit registers leave the upper halves dirty; VEX-128 zeroes them.
    if l {
        asm.vzeroupper();
    }

    let remainder_entry = asm.len();
    let mut clobbered_gprs = vec![shape.scratch];
    if let Some(r) = shape.reduction {
        clobbered_gprs.push(r.acc);
    }

    Ok(VecLoopCode {
        code: asm.out,
        fallback_sites,
        remainder_entry,
        clobbered_vector_regs: pool.clobbered(),
        clobbered_gprs,
        lanes: plan.lanes,
        width_bytes: plan.width_bytes,
        max_remainder_iterations: match plan.tail {
            TailStrategy::None => 0,
            TailStrategy::ScalarRemainder { max_iterations } => max_iterations,
        },
    })
}

/// The byte displacement of element `iv + index_offset` from the array base.
fn element_disp(index_offset: i32, elem_size: usize) -> Result<i64, VecEmitRefusal> {
    let scaled = (index_offset as i64).checked_mul(elem_size as i64).ok_or(
        VecEmitRefusal::DisplacementOutOfRange {
            disp: index_offset as i64,
        },
    )?;
    scaled
        .checked_add(HEADER_SIZE as i64)
        .ok_or(VecEmitRefusal::DisplacementOutOfRange { disp: scaled })
}

fn value_reg(regs: &[Option<u8>], v: VecValueId) -> Result<u8, VecEmitRefusal> {
    match regs.get(v) {
        Some(Some(r)) => Ok(*r),
        _ => Err(VecEmitRefusal::ValueNotDefined { value: v }),
    }
}

fn release_if_dead(
    pool: &mut VecRegPool<'_>,
    regs: &mut [Option<u8>],
    lives: &[ValueLife],
    v: VecValueId,
    at: usize,
) {
    if matches!(lives.get(v).and_then(|l| l.last_use), Some(last) if last == at) {
        if let Some(Some(reg)) = regs.get(v).copied() {
            pool.free(reg);
        }
        if let Some(slot) = regs.get_mut(v) {
            *slot = None;
        }
    }
}

/// Emit one pre-header guard as a compare and a branch to the scalar fallback.
///
/// The comparison is derived from the guard variant. `jcc` fires on the
/// *failing* condition, so the fallback edge is taken exactly when the guard
/// does not hold.
fn emit_guard(
    asm: &mut Asm,
    index: usize,
    guard: &PreheaderGuard,
    binding: VecGuardValues,
    sites: &mut Vec<usize>,
) -> Result<(), VecEmitRefusal> {
    let cc = match (guard, binding) {
        // `term >= 0`; fails when `term < 0`.
        (PreheaderGuard::NonNegative(_), VecGuardValues::Term { term }) => {
            check_gpr(term)?;
            asm.cmp_ri(term, 0)?;
            CC_LESS
        }
        // `length >= term`; fails when `length < term`.
        (PreheaderGuard::LengthAtLeast(_), VecGuardValues::LengthAtLeast { length, term }) => {
            check_gpr(length)?;
            check_gpr(term)?;
            asm.cmp_rr(length, term);
            CC_LESS
        }
        // `term <= limit`; fails when `term > limit`.
        (PreheaderGuard::AtMost { limit, .. }, VecGuardValues::Term { term }) => {
            check_gpr(term)?;
            asm.cmp_ri(term, *limit as i64)?;
            CC_GREATER
        }
        // `term >= limit`; fails when `term < limit`.
        (PreheaderGuard::AtLeast { limit, .. }, VecGuardValues::Term { term }) => {
            check_gpr(term)?;
            asm.cmp_ri(term, *limit as i64)?;
            CC_LESS
        }
        // `term >= minimum`; fails when `term < minimum`.
        (PreheaderGuard::TripCountAtLeast { minimum, .. }, VecGuardValues::Term { term }) => {
            check_gpr(term)?;
            let m = i32::try_from(*minimum)
                .map_err(|_| VecEmitRefusal::GuardImmediateOutOfRange { index })?;
            asm.cmp_ri(term, m as i64)?;
            CC_LESS
        }
        // Produced only for a runtime stride (`jit/src/scev.rs:1582`), which the
        // admission gate refuses with `VecRefusal::VariableStride`.
        (PreheaderGuard::StrideInRange { .. }, _) => {
            return Err(VecEmitRefusal::UnsupportedGuard { index })
        }
        _ => return Err(VecEmitRefusal::GuardBindingMismatch { index }),
    };
    sites.push(asm.jcc_rel32(cc));
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::NO_NODE;
    use crate::scev::{OverflowModel, SymBound, TripCount};
    // `super::super` is `x64`; `simd_analysis` is one of its private modules.
    use super::super::simd_analysis::vector_gate::{ArrayGuard, VectorIsa};

    // ── register names, for readability ─────────────────────────────────

    const RAX: u8 = 0;
    const RCX: u8 = 1;
    const RDX: u8 = 2;
    const RBX: u8 = 3;
    const RSI: u8 = 6;
    const RDI: u8 = 7;
    const R8: u8 = 8;
    const R12: u8 = 12;

    // ── fixtures ────────────────────────────────────────────────────────

    fn plan_for(elem: MemKind, lanes: usize, guards: Vec<ArrayGuard>) -> VecPlan {
        let width = lanes * elem_bytes(elem);
        VecPlan {
            isa: VectorIsa::avx2(),
            elem,
            lanes,
            width_bytes: width,
            alignment: Alignment::Unknown,
            tail: TailStrategy::ScalarRemainder {
                max_iterations: lanes - 1,
            },
            trip: TripCount { min: 64, max: 64 },
            overflow: OverflowModel::NoWrapProven,
            guards,
            dependences: Vec::new(),
            max_safe_lanes: None,
        }
    }

    /// `out[i] = a[i] + b[i]` over `int`, with `a` in RCX, `b` in RDX and
    /// `out` in RDI.
    fn elementwise_shape() -> VecLoopShape {
        VecLoopShape {
            iv: RBX,
            bound: RSI,
            scratch: RAX,
            body: vec![
                VecStep::Load {
                    dst: 0,
                    from: VecArrayOperand {
                        base: RCX,
                        index_offset: 0,
                    },
                },
                VecStep::Load {
                    dst: 1,
                    from: VecArrayOperand {
                        base: RDX,
                        index_offset: 0,
                    },
                },
                VecStep::Binary {
                    dst: 2,
                    op: VecOp::Add,
                    lhs: 0,
                    rhs: 1,
                },
                VecStep::Store {
                    to: VecArrayOperand {
                        base: RDI,
                        index_offset: 0,
                    },
                    src: 2,
                },
            ],
            reduction: None,
        }
    }

    /// `sum += a[i]` over `int`, accumulator in R8.
    fn reduction_shape() -> VecLoopShape {
        VecLoopShape {
            iv: RBX,
            bound: RSI,
            scratch: RAX,
            body: vec![
                VecStep::Load {
                    dst: 0,
                    from: VecArrayOperand {
                        base: RCX,
                        index_offset: 0,
                    },
                },
                VecStep::Accumulate { src: 0 },
            ],
            reduction: Some(VecReduction {
                op: VecOp::Add,
                acc: R8,
            }),
        }
    }

    fn request<'a>(
        plan: &'a VecPlan,
        shape: &'a VecLoopShape,
        guards: &'a [VecGuardValues],
    ) -> VecEmitRequest<'a> {
        VecEmitRequest {
            plan,
            shape,
            guards,
            host: HostVectorSupport::for_test(true),
            policy: VecEmitPolicy::Enabled,
            vector_pool: &VEC_POOL,
            frame_saved_xmms: &VEC_POOL,
        }
    }

    fn emit(plan: &VecPlan, shape: &VecLoopShape) -> Result<VecLoopCode, VecEmitRefusal> {
        emit_vector_loop(&request(plan, shape, &[]))
    }

    /// Does `haystack` contain the exact byte sequence `needle`?
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack.len() >= needle.len()
            && haystack.windows(needle.len()).any(|w| w == needle)
    }

    // ── the switch ──────────────────────────────────────────────────────

    #[test]
    fn the_default_policy_is_off() {
        assert_eq!(
            VecEmitPolicy::from_flag_value(None),
            VecEmitPolicy::Disabled
        );
        for falsey in ["", " ", "0", "false", "OFF", "No"] {
            assert_eq!(
                VecEmitPolicy::from_flag_value(Some(falsey)),
                VecEmitPolicy::Disabled,
                "{falsey:?} must not turn vectorization on"
            );
        }
        for truthy in ["1", "true", "on", "yes"] {
            assert_eq!(
                VecEmitPolicy::from_flag_value(Some(truthy)),
                VecEmitPolicy::Enabled,
                "{truthy:?} must turn vectorization on"
            );
        }
    }

    #[test]
    fn a_disabled_request_emits_nothing() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let req = VecEmitRequest {
            plan: &plan,
            shape: &shape,
            guards: &[],
            host: HostVectorSupport::for_test(true),
            policy: VecEmitPolicy::Disabled,
            vector_pool: &VEC_POOL,
            frame_saved_xmms: &VEC_POOL,
        };
        assert_eq!(emit_vector_loop(&req), Err(VecEmitRefusal::Disabled));
    }

    #[test]
    fn a_host_without_avx2_is_refused_before_anything_else() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let req = VecEmitRequest {
            plan: &plan,
            shape: &shape,
            guards: &[],
            host: HostVectorSupport::for_test(false),
            policy: VecEmitPolicy::Enabled,
            vector_pool: &VEC_POOL,
            frame_saved_xmms: &VEC_POOL,
        };
        assert_eq!(emit_vector_loop(&req), Err(VecEmitRefusal::HostLacksAvx2));
    }

    // ── encodings ───────────────────────────────────────────────────────

    #[test]
    fn two_byte_vex_matches_the_known_vpaddd_encoding() {
        // `vpaddd ymm0, ymm0, ymm1` is C5 FD FE C1.
        let mut asm = Asm::new();
        let enc = VexOpcode {
            map: 1,
            pp: 1,
            w: false,
            op: 0xFE,
        };
        asm.vec_rr(enc, true, 0, 0, 1);
        assert_eq!(asm.out, vec![0xC5, 0xFD, 0xFE, 0xC1]);

        // The 128-bit form differs only in L: C5 F9 FE C1.
        let mut asm = Asm::new();
        asm.vec_rr(enc, false, 0, 0, 1);
        assert_eq!(asm.out, vec![0xC5, 0xF9, 0xFE, 0xC1]);
    }

    #[test]
    fn an_extended_base_forces_the_three_byte_vex() {
        // `vmovdqu ymm0, [r12 + rbx*4 + 32]` — REX.B set by r12, so the
        // two-byte form is illegal.
        let mut asm = Asm::new();
        let (load, _) = move_opcodes(MemKind::Int).expect("int has a move");
        let d = Disp::encode_for_base(32, R12).expect("32 encodes");
        asm.vec_mem(load, true, 0, R12, RBX, 2, d);
        assert_eq!(
            asm.out,
            vec![0xC4, 0xC1, 0x7E, 0x6F, 0x44, 0x9C, 0x20],
            "C4 C1 7E is VEX3 with B=1, map=0F, L=1, pp=F3"
        );
    }

    #[test]
    fn the_reduction_epilogue_encodings_are_the_canonical_ones() {
        let mut asm = Asm::new();
        asm.vextracti128(1, 0, 1);
        assert_eq!(asm.out, vec![0xC4, 0xE3, 0x7D, 0x39, 0xC1, 0x01]);

        let mut asm = Asm::new();
        asm.vpshufd(1, 0, 0x4E);
        assert_eq!(asm.out, vec![0xC5, 0xF9, 0x70, 0xC8, 0x4E]);

        let mut asm = Asm::new();
        asm.vmovd_to_gpr(RAX, 0);
        assert_eq!(asm.out, vec![0xC5, 0xF9, 0x7E, 0xC0]);

        let mut asm = Asm::new();
        asm.vzeroupper();
        assert_eq!(asm.out, vec![0xC5, 0xF8, 0x77]);
    }

    #[test]
    fn scalar_helpers_encode_the_forms_the_head_test_needs() {
        let mut asm = Asm::new();
        asm.lea(RAX, RBX, 8).expect("small displacement");
        assert_eq!(asm.out, vec![0x48, 0x8D, 0x43, 0x08], "lea rax, [rbx+8]");

        let mut asm = Asm::new();
        asm.cmp_rr(RAX, RSI);
        assert_eq!(asm.out, vec![0x48, 0x3B, 0xC6], "cmp rax, rsi");

        let mut asm = Asm::new();
        asm.add_ri(RBX, 8).expect("imm8");
        assert_eq!(asm.out, vec![0x48, 0x83, 0xC3, 0x08], "add rbx, 8");

        let mut asm = Asm::new();
        asm.alu_rr32(0x01, R8, RAX);
        assert_eq!(asm.out, vec![0x41, 0x01, 0xC0], "add r8d, eax");
    }

    #[test]
    fn a_stack_pointer_base_can_never_be_encoded_as_an_index() {
        // RSP is not expressible as a SIB index at all, and clobbering it
        // destroys the frame. Every role refuses it.
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.iv = RSP;
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::ForbiddenRegister { reg: RSP })
        );
    }

    // ── the element-wise golden ─────────────────────────────────────────

    #[test]
    fn an_int_elementwise_loop_emits_the_expected_bytes() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("admitted plan emits");

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // loop_head:
            0x48, 0x8D, 0x43, 0x08,             // lea  rax, [rbx + 8]
            0x48, 0x3B, 0xC6,                   // cmp  rax, rsi
            0x0F, 0x8F, 0x20, 0x00, 0x00, 0x00, // jg   epilogue (+32)
            0xC5, 0x7E, 0x6F, 0x44, 0x99, HDR8, // vmovdqu ymm8, [rcx + rbx*4 + HEADER_SIZE]
            0xC5, 0x7E, 0x6F, 0x4C, 0x9A, HDR8, // vmovdqu ymm9, [rdx + rbx*4 + HEADER_SIZE]
            0xC4, 0x41, 0x3D, 0xFE, 0xC1,       // vpaddd  ymm8, ymm8, ymm9
            0xC5, 0x7E, 0x7F, 0x44, 0x9F, HDR8, // vmovdqu [rdi + rbx*4 + HEADER_SIZE], ymm8
            0x48, 0x83, 0xC3, 0x08,             // add  rbx, 8
            0xE9, 0xD3, 0xFF, 0xFF, 0xFF,       // jmp  loop_head (-45)
            // epilogue:
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(code.code, expected);
        assert_eq!(code.remainder_entry, expected.len());
        assert!(
            code.fallback_sites.is_empty(),
            "no guards, no fallback edges"
        );
        assert_eq!(code.lanes, 8);
        assert_eq!(code.width_bytes, 32);
        assert_eq!(code.max_remainder_iterations, 7);
        // Only two XMM registers are needed: the second load reuses nothing,
        // but the add's destination reuses the left operand's register. They
        // are the first two of `VEC_POOL` — XMM8/XMM9 — spelled through the
        // constant so a future pool move updates this alongside the emitter.
        assert_eq!(code.clobbered_vector_regs, vec![VEC_POOL[0], VEC_POOL[1]]);
        assert_eq!(code.clobbered_gprs, vec![RAX]);
    }

    #[test]
    fn the_head_test_and_back_edge_agree_with_the_layout() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("admitted plan emits");

        // The forward exit branch must land exactly on the epilogue, which for
        // a non-reduction loop is the VZEROUPPER.
        let jg_site = 9usize;
        let rel = i32::from_le_bytes([
            code.code[jg_site],
            code.code[jg_site + 1],
            code.code[jg_site + 2],
            code.code[jg_site + 3],
        ]);
        let target = (jg_site + 4) as i64 + rel as i64;
        assert_eq!(code.code.get(target as usize), Some(&0xC5));
        assert_eq!(target as usize, code.code.len() - 3);

        // The back edge must land on offset 0, the head test.
        let jmp_site = code.code.len() - 3 - 4;
        let rel = i32::from_le_bytes([
            code.code[jmp_site],
            code.code[jmp_site + 1],
            code.code[jmp_site + 2],
            code.code[jmp_site + 3],
        ]);
        assert_eq!((jmp_site + 4) as i64 + rel as i64, 0);
    }

    #[test]
    fn a_narrower_lane_count_emits_the_128_bit_form_and_no_vzeroupper() {
        // A dependence can cap the lane count below the register width.
        let plan = plan_for(MemKind::Int, 4, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("admitted plan emits");
        assert_eq!(code.width_bytes, 16);
        // L=0 spells VMOVDQU as C5 .A (not C5 .E) and VPADDD as .. .9. The
        // register half: the pool is XMM8+, so VEX.R is 0 and VMOVDQU takes the
        // two-byte form `C5 7A`, while VPADDD needs VEX.B as well and falls
        // back to the three-byte `C4 41 39`.
        assert!(
            contains(&code.code, &[0xC5, 0x7A, 0x6F]),
            "128-bit VMOVDQU into ymm8"
        );
        assert!(
            contains(&code.code, &[0xC4, 0x41, 0x39, 0xFE, 0xC1]),
            "128-bit VPADDD xmm8, xmm8, xmm9"
        );
        assert!(
            !contains(&code.code, &[0xC5, 0xF8, 0x77]),
            "VEX-128 zeroes the upper halves, so no VZEROUPPER is owed"
        );
    }

    // ── the reduction ───────────────────────────────────────────────────

    #[test]
    fn an_int_sum_reduction_emits_the_zeroing_init_and_the_fold_tree() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = reduction_shape();
        let code = emit(&plan, &shape).expect("admitted plan emits");

        // The accumulator is zeroed *before* the head test, so a loop that
        // runs zero vector passes folds an identity in.
        assert_eq!(
            &code.code[..5],
            &[0xC4, 0x41, 0x3D, 0xEF, 0xC0],
            "vpxor ymm8, ymm8, ymm8"
        );

        // The epilogue's canonical shape, in order. Every VEX prefix here
        // carries R=0 (and B=0 where the r/m operand is an XMM too) because the
        // pool is XMM8..XMM15; the opcodes, the ModRM bytes and the shuffle
        // immediates are untouched. Re-derived from the encoding rules rather
        // than transcribed from what the emitter produced, which is the only
        // version of this edit that can still catch an encoder bug.
        let tail = &code.code[code.code.len() - 43..];
        #[rustfmt::skip]
        let expected_tail: Vec<u8> = vec![
            0xC4, 0x43, 0x7D, 0x39, 0xC1, 0x01, // vextracti128 xmm9, ymm8, 1
            0xC4, 0x41, 0x39, 0xFE, 0xC1,       // vpaddd xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E, // vpshufd xmm9, xmm8, 0x4E
            0xC4, 0x41, 0x39, 0xFE, 0xC1,       // vpaddd xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0xB1, // vpshufd xmm9, xmm8, 0xB1
            0xC4, 0x41, 0x39, 0xFE, 0xC1,       // vpaddd xmm8, xmm8, xmm9
            // Two-byte VEX here, not three: the r/m operand is EAX, so VEX.B
            // and VEX.X are both 1 and only VEX.R needs clearing — which the
            // C5 form carries. (The re-derivation got this one wrong first;
            // the emitter was right. That is the value of deriving rather than
            // pasting whatever came out.)
            0xC5, 0x79, 0x7E, 0xC0,             // vmovd eax, xmm8
            0x41, 0x01, 0xC0,                   // add r8d, eax
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(tail, expected_tail.as_slice());
        assert_eq!(code.clobbered_gprs, vec![RAX, R8]);
    }

    #[test]
    fn a_128_bit_reduction_skips_the_lane_extract() {
        let plan = plan_for(MemKind::Int, 4, Vec::new());
        let shape = reduction_shape();
        let code = emit(&plan, &shape).expect("admitted plan emits");
        assert!(
            !contains(&code.code, &[0x39, 0xC1, 0x01]),
            "there is no high 128-bit lane to extract"
        );
        assert!(
            contains(&code.code, &[0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E]),
            "the two-shuffle fold still runs"
        );
    }

    #[test]
    fn only_add_or_and_xor_reductions_are_emitted() {
        for op in [VecOp::And, VecOp::Mul, VecOp::Min, VecOp::Sub] {
            let plan = plan_for(MemKind::Int, 8, Vec::new());
            let mut shape = reduction_shape();
            shape.reduction = Some(VecReduction { op, acc: R8 });
            assert_eq!(
                emit(&plan, &shape),
                Err(VecEmitRefusal::UnsupportedReduction {
                    elem: MemKind::Int,
                    op
                }),
                "{op:?} has no identity this epilogue can materialise"
            );
        }
        for op in [VecOp::Add, VecOp::Or, VecOp::Xor] {
            let plan = plan_for(MemKind::Int, 8, Vec::new());
            let mut shape = reduction_shape();
            shape.reduction = Some(VecReduction { op, acc: R8 });
            assert!(emit(&plan, &shape).is_ok(), "{op:?} reduces");
        }
    }

    #[test]
    fn a_floating_point_reduction_is_refused_even_though_the_gate_can_admit_one() {
        // The gate admits an FP reduction under `FpRelaxation::AllowReassociation`.
        // The emitter does not take the caller's word for a changed FP result.
        for elem in [MemKind::Float, MemKind::Double] {
            let plan = plan_for(elem, if elem == MemKind::Float { 8 } else { 4 }, Vec::new());
            let shape = reduction_shape();
            assert_eq!(
                emit(&plan, &shape),
                Err(VecEmitRefusal::UnsupportedReduction {
                    elem,
                    op: VecOp::Add
                })
            );
        }
    }

    #[test]
    fn an_accumulate_without_a_declared_reduction_is_refused() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = reduction_shape();
        shape.reduction = None;
        assert_eq!(emit(&plan, &shape), Err(VecEmitRefusal::ReductionMismatch));

        let mut shape = elementwise_shape();
        shape.reduction = Some(VecReduction {
            op: VecOp::Add,
            acc: R8,
        });
        assert_eq!(emit(&plan, &shape), Err(VecEmitRefusal::ReductionMismatch));
    }

    // ── the oop refusal ─────────────────────────────────────────────────

    #[test]
    fn a_reference_element_is_refused_by_the_emitter_as_well_as_the_gate() {
        // A vector store of oops bypasses the GC write barrier. This refusal is
        // deliberately independent of the gate's `GcReferenceAccess`.
        let mut plan = plan_for(MemKind::Int, 8, Vec::new());
        plan.elem = MemKind::Ref;
        let shape = elementwise_shape();
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::ObjectReferenceElement)
        );
        // And the encoding tables refuse independently of the entry point.
        assert_eq!(
            move_opcodes(MemKind::Ref),
            Err(VecEmitRefusal::ObjectReferenceElement)
        );
        assert_eq!(
            arith_opcode(MemKind::Ref, VecOp::Add, true),
            Err(VecEmitRefusal::ObjectReferenceElement)
        );
    }

    #[test]
    fn subword_elements_are_refused_because_jvm_arithmetic_is_int_width() {
        for elem in [MemKind::Byte, MemKind::Char, MemKind::Short] {
            let mut plan = plan_for(MemKind::Int, 8, Vec::new());
            plan.elem = elem;
            let shape = elementwise_shape();
            assert_eq!(
                emit(&plan, &shape),
                Err(VecEmitRefusal::SubwordElement { elem })
            );
        }
    }

    // ── op admissibility ────────────────────────────────────────────────

    #[test]
    fn integer_divide_and_remainder_have_no_lane_encoding() {
        for op in [VecOp::Div, VecOp::Rem] {
            assert_eq!(
                arith_opcode(MemKind::Int, op, true),
                Err(VecEmitRefusal::UnsupportedOp {
                    elem: MemKind::Int,
                    op
                })
            );
        }
    }

    #[test]
    fn floating_point_min_and_max_have_no_lane_encoding() {
        for elem in [MemKind::Float, MemKind::Double] {
            for op in [VecOp::Min, VecOp::Max] {
                assert_eq!(
                    arith_opcode(elem, op, true),
                    Err(VecEmitRefusal::UnsupportedOp { elem, op })
                );
            }
        }
    }

    #[test]
    fn int_multiply_needs_the_sse41_feature_the_plan_claims() {
        assert_eq!(
            arith_opcode(MemKind::Int, VecOp::Mul, false),
            Err(VecEmitRefusal::MissingIsaFeature {
                elem: MemKind::Int,
                op: VecOp::Mul
            })
        );
        assert!(arith_opcode(MemKind::Int, VecOp::Mul, true).is_ok());
        // 64-bit lane multiply is AVX-512 only, at any feature level.
        assert_eq!(
            arith_opcode(MemKind::Long, VecOp::Mul, true),
            Err(VecEmitRefusal::UnsupportedOp {
                elem: MemKind::Long,
                op: VecOp::Mul
            })
        );
    }

    /// The array data area begins at `HEADER_SIZE`, so every element access
    /// encodes that as its displacement byte. Naming it keeps these fixtures
    /// from having to be re-derived by hand each time the header moves — as
    /// they did on 2026-08-06 when it went 32 -> 24.
    const HDR8: u8 = cratonvm_types::HEADER_SIZE as u8;

    #[test]
    fn double_lanes_use_the_66_prefixed_forms() {
        let plan = plan_for(MemKind::Double, 4, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("double element-wise emits");
        // VMOVUPD ymm8, [rcx+rbx*8+32] is C5 7D 10 44 D9 20 (pp = 66, VEX.R = 0).
        assert!(
            contains(&code.code, &[0xC5, 0x7D, 0x10, 0x44, 0xD9, HDR8]),
            "vmovupd with an 8-byte SIB scale"
        );
        // VADDPD ymm8, ymm8, ymm9 is C4 41 3D 58 C1.
        assert!(contains(&code.code, &[0xC4, 0x41, 0x3D, 0x58, 0xC1]));
    }

    #[test]
    fn float_lanes_use_the_unprefixed_forms() {
        let plan = plan_for(MemKind::Float, 8, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("float element-wise emits");
        // VMOVUPS ymm8, [rcx+rbx*4+32] is C5 7C 10 44 99 20 (pp = 00, VEX.R = 0).
        assert!(
            contains(&code.code, &[0xC5, 0x7C, 0x10, 0x44, 0x99, HDR8]),
            "vmovups"
        );
        // VADDPS ymm8, ymm8, ymm9 is C4 41 3C 58 C1.
        assert!(contains(&code.code, &[0xC4, 0x41, 0x3C, 0x58, 0xC1]));
    }

    // ── the width ───────────────────────────────────────────────────────

    #[test]
    fn a_width_that_is_not_a_whole_register_is_refused() {
        let mut plan = plan_for(MemKind::Int, 8, Vec::new());
        plan.width_bytes = 24;
        let shape = elementwise_shape();
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::UnsupportedWidth {
                width_bytes: 24,
                lanes: 8
            })
        );

        // A single lane is not a vector.
        let mut plan = plan_for(MemKind::Long, 2, Vec::new());
        plan.lanes = 1;
        plan.width_bytes = 8;
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::UnsupportedWidth {
                width_bytes: 8,
                lanes: 1
            })
        );
    }

    #[test]
    fn a_strict_alignment_target_is_refused_rather_than_emitted_unaligned() {
        let mut plan = plan_for(MemKind::Int, 4, Vec::new());
        plan.isa = VectorIsa::strict_align128();
        plan.alignment = Alignment::Proven(16);
        let shape = elementwise_shape();
        assert_eq!(emit(&plan, &shape), Err(VecEmitRefusal::StrictAlignmentIsa));

        plan.alignment = Alignment::Unknown;
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::UnprovableAlignment)
        );
    }

    // ── guards ──────────────────────────────────────────────────────────

    fn array_guard(g: PreheaderGuard) -> ArrayGuard {
        ArrayGuard {
            array: NO_NODE,
            guard: g,
        }
    }

    #[test]
    fn every_plan_guard_becomes_one_fallback_edge() {
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![
                array_guard(PreheaderGuard::NonNegative(SymBound::constant(0))),
                array_guard(PreheaderGuard::LengthAtLeast(SymBound::constant(0))),
                array_guard(PreheaderGuard::TripCountAtLeast {
                    term: SymBound::constant(0),
                    minimum: 8,
                }),
            ],
        );
        let shape = elementwise_shape();
        let bindings = [
            VecGuardValues::Term { term: RCX },
            VecGuardValues::LengthAtLeast {
                length: RDX,
                term: RDI,
            },
            VecGuardValues::Term { term: RSI },
        ];
        let code =
            emit_vector_loop(&request(&plan, &shape, &bindings)).expect("all guards discharged");
        assert_eq!(code.fallback_sites.len(), 3);

        // Guards precede every vector instruction: nothing before the first
        // fallback edge may be a VEX prefix, so the fallback edge owes no
        // VZEROUPPER.
        let first = code.fallback_sites[0];
        assert!(
            !code.code[..first].iter().any(|b| *b == 0xC5 || *b == 0xC4),
            "no VEX instruction may precede a guard's fallback branch"
        );

        // `cmp rcx, 0` + `jl`, then `cmp rdx, rdi` + `jl`, then `cmp rsi, 8` + `jl`.
        #[rustfmt::skip]
        let expected_guards: Vec<u8> = vec![
            0x48, 0x83, 0xF9, 0x00,             // cmp rcx, 0
            0x0F, 0x8C, 0x00, 0x00, 0x00, 0x00, // jl  fallback
            0x48, 0x3B, 0xD7,                   // cmp rdx, rdi
            0x0F, 0x8C, 0x00, 0x00, 0x00, 0x00, // jl  fallback
            0x48, 0x83, 0xFE, 0x08,             // cmp rsi, 8
            0x0F, 0x8C, 0x00, 0x00, 0x00, 0x00, // jl  fallback
        ];
        assert_eq!(
            &code.code[..expected_guards.len()],
            expected_guards.as_slice()
        );
        assert_eq!(code.fallback_sites, vec![6, 15, 25]);
        // Every fallback site is a `rel32` field left at zero for the caller.
        for site in &code.fallback_sites {
            assert_eq!(&code.code[*site..site + 4], &[0, 0, 0, 0]);
        }
    }

    #[test]
    fn an_at_most_guard_branches_on_the_opposite_condition() {
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![array_guard(PreheaderGuard::AtMost {
                term: SymBound::constant(0),
                limit: 100,
            })],
        );
        let shape = elementwise_shape();
        let bindings = [VecGuardValues::Term { term: RCX }];
        let code = emit_vector_loop(&request(&plan, &shape, &bindings)).expect("emits");
        // `term <= 100` fails when `term > 100`, so JG, not JL.
        assert_eq!(
            &code.code[..10],
            &[0x48, 0x83, 0xF9, 0x64, 0x0F, 0x8F, 0x00, 0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn a_missing_guard_binding_voids_the_whole_plan() {
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![
                array_guard(PreheaderGuard::NonNegative(SymBound::constant(0))),
                array_guard(PreheaderGuard::NonNegative(SymBound::constant(1))),
            ],
        );
        let shape = elementwise_shape();
        let bindings = [VecGuardValues::Term { term: RCX }];
        assert_eq!(
            emit_vector_loop(&request(&plan, &shape, &bindings)),
            Err(VecEmitRefusal::GuardCountMismatch { wanted: 2, got: 1 })
        );
    }

    #[test]
    fn a_binding_of_the_wrong_shape_is_refused() {
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![array_guard(PreheaderGuard::LengthAtLeast(
                SymBound::constant(0),
            ))],
        );
        let shape = elementwise_shape();
        // A `LengthAtLeast` guard needs a length as well as a term; a bare term
        // would silently compare the term against itself.
        let bindings = [VecGuardValues::Term { term: RCX }];
        assert_eq!(
            emit_vector_loop(&request(&plan, &shape, &bindings)),
            Err(VecEmitRefusal::GuardBindingMismatch { index: 0 })
        );
    }

    #[test]
    fn a_runtime_stride_guard_is_refused() {
        // Unreachable through the gate (`VecRefusal::VariableStride` fires
        // first), but closed here anyway.
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![array_guard(PreheaderGuard::StrideInRange {
                local: 4,
                headroom: SymBound::constant(0),
            })],
        );
        let shape = elementwise_shape();
        let bindings = [VecGuardValues::Term { term: RCX }];
        assert_eq!(
            emit_vector_loop(&request(&plan, &shape, &bindings)),
            Err(VecEmitRefusal::UnsupportedGuard { index: 0 })
        );
    }

    #[test]
    fn an_unencodable_guard_minimum_is_refused_not_truncated() {
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![array_guard(PreheaderGuard::TripCountAtLeast {
                term: SymBound::constant(0),
                minimum: u64::from(u32::MAX) + 1,
            })],
        );
        let shape = elementwise_shape();
        let bindings = [VecGuardValues::Term { term: RCX }];
        assert_eq!(
            emit_vector_loop(&request(&plan, &shape, &bindings)),
            Err(VecEmitRefusal::GuardImmediateOutOfRange { index: 0 })
        );
    }

    // ── the body's SSA discipline ───────────────────────────────────────

    #[test]
    fn a_use_before_definition_is_refused() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body = vec![VecStep::Store {
            to: VecArrayOperand {
                base: RDI,
                index_offset: 0,
            },
            src: 7,
        }];
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::ValueNotDefined { value: 7 })
        );
    }

    #[test]
    fn a_redefined_value_is_refused() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body.push(VecStep::Load {
            dst: 0,
            from: VecArrayOperand {
                base: RCX,
                index_offset: 1,
            },
        });
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::ValueRedefined { value: 0 })
        );
    }

    #[test]
    fn a_value_id_beyond_the_tracked_range_is_refused() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body = vec![VecStep::Load {
            dst: MAX_VEC_VALUES,
            from: VecArrayOperand {
                base: RCX,
                index_offset: 0,
            },
        }];
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::ValueRedefined {
                value: MAX_VEC_VALUES
            })
        );
    }

    #[test]
    fn a_dead_vector_load_is_refused_rather_than_emitted() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body.insert(
            0,
            VecStep::Load {
                dst: 9,
                from: VecArrayOperand {
                    base: RCX,
                    index_offset: 4,
                },
            },
        );
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::DeadVectorValue { value: 9 })
        );
    }

    #[test]
    fn an_empty_body_is_refused() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body.clear();
        assert_eq!(emit(&plan, &shape), Err(VecEmitRefusal::EmptyBody));
    }

    // ── the register model ──────────────────────────────────────────────

    #[test]
    fn the_pool_allocates_lowest_first_and_refuses_rather_than_spills() {
        let mut pool = VecRegPool::new(&VEC_POOL);
        let mut held = Vec::new();
        for _ in 0..VEC_POOL.len() {
            held.push(pool.alloc().expect("pool has room"));
        }
        assert_eq!(held, VEC_POOL.to_vec());
        assert_eq!(pool.alloc(), Err(VecEmitRefusal::OutOfVectorRegisters));
        pool.free(VEC_POOL[2]);
        assert_eq!(
            pool.alloc(),
            Ok(VEC_POOL[2]),
            "the freed register comes back"
        );
        // Freeing something the pool never handed out is a no-op, not a panic.
        pool.free(99);
        assert_eq!(pool.clobbered(), VEC_POOL.to_vec());
    }

    /// Every register the pool names is one no scalar authority can claim.
    ///
    /// This replaced `..._are_caller_saved_on_both_abis`, which asserted
    /// `reg < 6`. That was true of the old pool and was exactly why the old
    /// pool was unsafe: XMM0..XMM5 owe no prologue save, but they are
    /// `ir_lower`'s FP scratch pair *and* its entire linear-scan file. The old
    /// pool bought freedom from a frame obligation it could have discharged
    /// mechanically, by taking on an aliasing obligation nobody could.
    #[test]
    fn the_pool_only_ever_names_registers_no_scalar_authority_claims() {
        use crate::regalloc::xmm_roles::{IR_FP_SCRATCH, IR_LINEAR_SCAN};
        for reg in VEC_POOL {
            assert!(
                !IR_FP_SCRATCH.contains(&reg) && !IR_LINEAR_SCAN.contains(&reg),
                "xmm{reg} is in the vector pool AND in a scalar file"
            );
        }
    }

    /// Encodability asks about the caller's FRAME, not about the register
    /// number — and the answer differs by target.
    ///
    /// On Windows every pool register is non-volatile, so a frame that saves
    /// nothing gets nothing. On System V every XMM is volatile and the saved
    /// set is irrelevant. Both arms are asserted here rather than only the
    /// host's, because a Linux-only check is what lets a Windows-wrong pool
    /// through green (see
    /// `feedback_a_windows_only_verification_leaves_the_linux_half_uncovered`,
    /// the same trap in the other direction).
    #[test]
    fn pool_encodability_answers_the_frame_question_not_the_register_number() {
        use crate::regalloc::xmm_roles::vector_pool_is_encodable;
        // Outside the pool: never encodable, saved or not, on any target.
        for reg in [0u8, 1, 5, 7] {
            assert!(
                !vector_pool_is_encodable(reg, &[]),
                "xmm{reg} with no saves"
            );
            assert!(!vector_pool_is_encodable(reg, &[reg]), "xmm{reg} saved");
        }
        // Inside the pool and saved by the frame: always fine.
        assert!(vector_pool_is_encodable(8, &[8]));
        // Inside the pool, frame saves nothing: target-dependent, and that
        // difference is the whole reason `frame_saved_xmms` is a field.
        assert_eq!(vector_pool_is_encodable(8, &[]), !cfg!(windows));
    }

    /// The pool is the CALLER's to supply, and a pool this emitter cannot
    /// encode refuses the whole region rather than being quietly narrowed.
    ///
    /// A narrowed pool is worse than a refusal: the caller goes on believing it
    /// handed over eight registers, and the two parties disagree about which
    /// ones are live.
    ///
    /// The exact edit that trips it: delete the `vector_pool_is_encodable` loop
    /// at the top of `emit_vector_loop`.
    #[test]
    fn a_pool_naming_a_register_outside_the_vector_region_refuses_the_region() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let mut req = request(&plan, &shape, &[]);
        // XMM7 is the top of `ir_lower`'s linear-scan file, so handing it to a
        // vector region is precisely the scalar clobber this refuses.
        let bad: [u8; 3] = [8, 9, 7];
        req.vector_pool = &bad;
        assert_eq!(
            emit_vector_loop(&req),
            Err(VecEmitRefusal::UnusableVectorPool { reg: 7 })
        );
    }

    /// On Windows, a pool the caller's own prologue does not save refuses.
    ///
    /// The other half of the trade: the pool no longer overlaps a scalar file,
    /// so what it owes now is a save area — and "owes" has to mean refused, not
    /// assumed. On System V there is nothing to owe and the region emits.
    #[test]
    fn a_pool_the_frame_does_not_save_refuses_on_windows_and_emits_on_sysv() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let mut req = request(&plan, &shape, &[]);
        req.frame_saved_xmms = &[];
        let got = emit_vector_loop(&req);
        if cfg!(windows) {
            assert_eq!(got, Err(VecEmitRefusal::UnusableVectorPool { reg: 8 }));
        } else {
            assert!(got.is_ok(), "SysV owes no save area: {got:?}");
        }
    }

    /// An EMPTY pool is legal, and refuses at the first allocation.
    ///
    /// This is the answer a caller that has done no analysis must get. The
    /// alternative — falling back to a set this module owns privately — is the
    /// silent clobber `regalloc::xmm_roles` exists to make impossible.
    #[test]
    fn an_empty_pool_refuses_instead_of_helping_itself_to_the_scalar_file() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let mut req = request(&plan, &shape, &[]);
        req.vector_pool = &[];
        assert_eq!(
            emit_vector_loop(&req),
            Err(VecEmitRefusal::OutOfVectorRegisters)
        );
    }

    /// The three XMM authorities, and they are **disjoint**.
    ///
    /// The predecessor of this test asserted the opposite — that the vector
    /// pool contained every scalar register — and said in its own doc comment:
    /// "When a prologue save area lands and the pools are separated, this test
    /// fails and says so." It did. `ir_lower::emit_prologue` gained
    /// `IR_LOWER_SAVED_XMMS` on 2026-08-04, the scalar file grew to XMM7 and
    /// the pool moved to XMM8..XMM15.
    ///
    /// Kept as a *whole-range* scan rather than three pairwise checks so a
    /// fourth authority added later is caught by the same assertion.
    #[test]
    fn the_three_xmm_authorities_are_disjoint() {
        use crate::regalloc::xmm_roles::{disjointness_violation, VECTOR_REGION_MAX};

        assert_eq!(
            disjointness_violation(),
            None,
            "two XMM authorities claim one register"
        );
        assert_eq!(VEC_POOL.to_vec(), VECTOR_REGION_MAX.to_vec());
        assert_eq!(VEC_POOL_LEN, VECTOR_REGION_MAX.len());
    }

    #[test]
    fn running_out_of_vector_registers_is_a_refusal() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        // Nine simultaneously-live loads, one more than the pool holds.
        let mut body = Vec::new();
        for k in 0..9usize {
            body.push(VecStep::Load {
                dst: k,
                from: VecArrayOperand {
                    base: RCX,
                    index_offset: k as i32,
                },
            });
        }
        for k in 0..9usize {
            body.push(VecStep::Store {
                to: VecArrayOperand {
                    base: RDI,
                    index_offset: k as i32,
                },
                src: k,
            });
        }
        shape.body = body;
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::OutOfVectorRegisters)
        );
    }

    #[test]
    fn two_roles_may_not_share_a_register() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.scratch = shape.iv;
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::RegisterConflict { reg: RBX })
        );

        // An array base may not collide with the induction variable either.
        let mut shape = elementwise_shape();
        shape.body[0] = VecStep::Load {
            dst: 0,
            from: VecArrayOperand {
                base: RBX,
                index_offset: 0,
            },
        };
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::RegisterConflict { reg: RBX })
        );
    }

    #[test]
    fn two_accesses_may_share_one_array_base() {
        // `a[i] = a[i] + b[i]` — the in-place shape the gate's element-wise
        // detector also accepts.
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = elementwise_shape();
        shape.body[3] = VecStep::Store {
            to: VecArrayOperand {
                base: RCX,
                index_offset: 0,
            },
            src: 2,
        };
        assert!(emit(&plan, &shape).is_ok());
    }

    // ── displacements ───────────────────────────────────────────────────

    #[test]
    fn element_displacements_are_computed_from_the_header_size() {
        assert_eq!(element_disp(0, 4), Ok(HEADER_SIZE as i64));
        assert_eq!(element_disp(3, 4), Ok(HEADER_SIZE as i64 + 12));
        assert_eq!(element_disp(-8, 4), Ok(HEADER_SIZE as i64 - 32));
        // The arithmetic is done in 64 bits, so a subscript that overflows a
        // 32-bit displacement is still computed exactly here and refused later
        // by `Disp::encode_for_base` — never silently narrowed.
        assert_eq!(
            element_disp(i32::MAX, 8),
            Ok(i32::MAX as i64 * 8 + HEADER_SIZE as i64)
        );
        assert!(Disp::encode_for_base(i32::MAX as i64 * 8 + HEADER_SIZE as i64, 1).is_err());
    }

    #[test]
    fn an_out_of_range_element_displacement_is_refused_not_narrowed() {
        let plan = plan_for(MemKind::Long, 4, Vec::new());
        let mut shape = elementwise_shape();
        shape.body[0] = VecStep::Load {
            dst: 0,
            from: VecArrayOperand {
                base: RCX,
                index_offset: i32::MAX,
            },
        };
        // `i32::MAX * 8 + 32` does not fit a signed 32-bit displacement.
        assert!(matches!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::DisplacementOutOfRange { .. })
        ));
    }

    #[test]
    fn a_zero_displacement_on_rbp_style_bases_still_gets_an_explicit_byte() {
        // `mod=00` with SIB base 101 decodes as "no base"; `Disp::encode_for_base`
        // promotes it. HEADER_SIZE is non-zero so this only bites for a negative
        // subscript, but the emitter must route through the checked helper.
        let d = Disp::encode_for_base(0, 5).expect("zero encodes");
        assert_eq!(d, Disp::Disp8(0));
        assert_eq!(d.mod_bits(), 0b01);
    }
}
