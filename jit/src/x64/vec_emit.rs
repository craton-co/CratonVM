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
//!   <accumulator init>             ; reduction only: VPXOR / VPCMPEQD acc,acc,acc
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
//! `int` and `long` reductions with `+`, `|`, `^` and `&` are emitted. All four
//! are associative *and* commutative over the whole two's-complement domain, so
//! reassociating them into lanes is exact rather than approximate, and each has
//! an identity this module can put in a register with one self-referential
//! instruction: `0` for `+`/`|`/`^` (`VPXOR acc, acc, acc`) and all-ones for `&`
//! (`VPCMPEQD acc, acc, acc`). The accumulator is initialised to that identity
//! before the head test and folded into the incoming scalar accumulator at the
//! end. The argument is identical for both widths, because JVM `int` *and*
//! `long` arithmetic are modular two's-complement; the accumulator width changes
//! the fold, never its legality.
//!
//! Which operators are admitted is decided by **two** tables read together —
//! [`scalar_fold_opcode`] for the final GPR fold and [`accumulator_identity`]
//! for the register init — because a row in one without a row in the other is
//! silent wrong code rather than a refusal (an `&` reduction over a
//! `VPXOR`-zeroed accumulator answers `0` for every loop).
//!
//! The fold is the standard log2(lanes) tree, and it has two shapes:
//!
//! * 32-bit lanes: `VEXTRACTI128` (256-bit only), two `VPSHUFD` steps (`0x4E`
//!   then `0xB1`), `VMOVD` into the scratch GPR, one scalar `add`/`or`/`xor`,
//!   then `MOVSXD` to restore the sign a 32-bit ALU op zero-extended away.
//! * 64-bit lanes: `VEXTRACTI128` (256-bit only), **one** `VPSHUFD 0x4E` —
//!   which read as qwords is a half-swap — `VMOVQ` into the scratch GPR, and
//!   one `REX.W` `add`/`or`/`xor`. **No `MOVSXD`**: a 64-bit fold writes the
//!   whole register, and re-extending from bit 31 would corrupt any sum outside
//!   the `i32` range.
//!
//! Because the vector accumulator starts at the identity, the epilogue is also
//! correct when the head test fails on its very first evaluation: zero vector
//! passes fold an identity into the accumulator, which is a no-op.
//!
//! `*` is refused: its identity is `1`, and the cheapest way to materialise that
//! is `VPCMPEQD` followed by `VPSRLD acc, acc, 31`, whose
//! `VEX.NDD.128.66.0F 72 /2 ib` form puts the destination in `vvvv` and the
//! opcode extension in ModRM `reg` — a different operand layout from every form
//! [`Asm`] has, so a new encoder family rather than a table row. `long *` could
//! not follow in any case: `VPMULLQ` is AVX-512. `-`, `min` and `max` are
//! refused for want of associativity or of an identity.
//!
//! Floating-point reductions are refused **unconditionally here** even when the
//! gate admitted one under [`FpRelaxation::AllowReassociation`] — the emitter
//! does not take the caller's word for a changed FP result.
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
//! reason, and since round 10 wave 8 the refusal is **conditional on the body
//! computing something**. JVM arithmetic on them is performed in `int` on sign-
//! (or, for `char`, zero-) extended operands and narrowed only at the store. For
//! `+ - * & | ^` whose result goes *straight* to a same-width store, a lane-wise
//! `PADDB`/`PADDW` is in fact exact — the low bits of those operations depend
//! only on the low bits of their operands. But the moment the `int` value is
//! used any other way (a shift right, a compare, a divide, an `int` reduction,
//! a store to a wider array) the 8/16-bit lane is wrong, and [`VecStep`] has no
//! way to say "this value is only ever narrowed". So sub-word **arithmetic**
//! stays refused; see `docs/jit/vectorization-emitter.md`.
//!
//! A sub-word **copy** — `b[i] = a[i]` over `byte[]`/`char[]`/`short[]`, a body
//! of `Load` and `Store` steps and nothing else — is emitted. There is no
//! arithmetic node to be computed at the wrong width: the lanes are moved bit
//! for bit by `VMOVDQU`, which has no lane width, and the only element-dependent
//! part of the address is the SIB scale, which `scale_log2` already answers for
//! 1 and 2. The condition is a scan of the body **this module was handed**, not
//! a deduction about what the gate would have admitted, and `arith_opcode`'s
//! sub-word arm is an unconditional second refusal so that a scan widened by a
//! later edit still cannot reach a lane-wise sub-word opcode.
//!
//! # Registers
//!
//! `jit/src/regalloc.rs` has no vector register class, but
//! `regalloc::xmm_roles` is the authority on who owns which XMM register. The
//! pool is **caller-supplied** ([`VecEmitRequest::vector_pool`]) and may be at
//! most [`VEC_POOL`] — XMM8..XMM15, disjoint from `ir_lower`'s FP scratch pair
//! and its linear-scan file. Every register in it is callee-saved on Windows,
//! so the caller also names the ones its own prologue saves
//! ([`VecEmitRequest::frame_saved_xmms`]); an unsaved one refuses the whole
//! region. Allocation is lowest-free-index first, freeing happens at each
//! value's last use, and exhaustion is a refusal rather than a spill.
//!
//! # Caller obligations the emitter cannot check
//!
//! * Every array base in the body is **non-null** (it is dereferenced with no
//!   null check).
//! * `bound` is the loop's **exclusive, increasing** limit: the head test runs a
//!   pass only while `iv + lanes <= bound`. An inclusive source loop
//!   (`i <= n`) passes `n + 1`, computed in 64 bits.
//! * The loop's subscripts are `iv + index_offset` with unit scale and a `+1`
//!   step — the only shape `vector_gate` admits.
//!
//! # Off by default
//!
//! [`VecEmitPolicy::from_flags`] answers [`VecEmitPolicy::Disabled`] unless
//! `CRATONVM_JIT_VECTORIZE` is set to something other than a falsey word, and
//! [`emit_vector_loop`] refuses a `Disabled` request before looking at anything
//! else. Tests pass `VecEmitPolicy::Enabled` explicitly, so no test mutates
//! process-global environment state.
//!
//! # Nothing calls any of this yet, and that is measurable
//!
//! Stated here because it is the thing most likely to be misread as "a
//! feature that is off" rather than "a feature that is absent". Searched
//! 2026-09-21 across the whole tree (read, not executed):
//!
//! * [`emit_vector_loop`] — no caller outside this file. `jit/src/regalloc.rs`
//!   mentions it only in prose, as the thing `xmm_roles` is a prerequisite for.
//! * [`VecEmitPolicy::from_flags`] — no caller at all, in this file or any
//!   other. `types/src/flag_groups.rs` already records it as "no production
//!   caller". The `CRATONVM_JIT_VECTORIZE` key it reads *is* live, but through
//!   `simd_analysis::simd_sum_forms_enabled`, which has its own default-ON
//!   rule and does not consult this function. Round 10 wave 6 gave it its
//!   first caller of any kind — `reading_the_flag_and_reading_the_value_agree`
//!   — which pins the key and the access path. Still no production caller.
//! * [`HostVectorSupport::detect`] — no caller at all either, not even a test
//!   until round 10. It is the module's only production constructor, so until
//!   `the_gate_only_offers_an_isa_this_emitter_can_encode` was added nothing
//!   anywhere checked that it agrees with `super::cpu_features`. An always-
//!   zero reading is indistinguishable from "it never happened"; this one was
//!   "it was never asked".
//! * `simd_analysis::vector_gate::admit_vectorization` — likewise, no caller
//!   outside its own file.
//!
//! So a call site that lands must pass [`VecEmitPolicy::from_flags`] and
//! [`HostVectorSupport::detect`] rather than asserting either, and the first
//! thing it should do is make those four searches return something. See
//! `docs/feature-designs/jit-r10-vecplan-proposals.md`.
//!
//! **Why round 10 wave 6 did not give any of them a production caller**, having
//! looked for one: these are not five independent orphans but one connected
//! component — `VectorIsa::detect` and [`VecEmitPolicy::from_flags`] feed
//! `admit_vectorization`, which feeds [`emit_vector_loop`], which needs
//! [`HostVectorSupport::detect`] — and the component is orphaned at its
//! **root**, not at a leaf. A root has to be something that drives a
//! compilation, and neither of these two files is. Adding a constructor here
//! that calls `detect()` for a caller that does not exist would move the zero
//! one level and make the search *stop returning* it, which is worse than
//! leaving it visible. The honest fix is the `x64.rs` call site
//! (`jit/src/x64/driver.rs`), which is not this module's file.
//! `docs/internal/retired/r10-vecwidth-the-gate-admits-five-classes-this-emitter-cannot-encode-20260921-RETIRED-20260922.md`
//! carries the rest of the accounting.
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
use cratonvm_types::ARRAY_DATA_OFFSET;

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
///
/// Two readers, two defaults. [`VecEmitPolicy::from_flags`] (this module,
/// which has no production caller yet) keeps its default-OFF rule: unset is
/// [`VecEmitPolicy::Disabled`]. The single-pass SIMD detectors' extra sum
/// forms and `a.length` headers (`simd_analysis::simd_sum_forms_enabled`) read
/// it default-ON since round 9 wave 4, with `0`/`false`/`off`/`no` as the
/// kill switch — measured, see that function.
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
/// The byte address is `base + ARRAY_DATA_OFFSET + (iv + index_offset) * elem_bytes`,
/// which is the layout `super::simd_analysis::vector_gate::analyze_alignment`
/// and `cratonvm_types::heap_types` both describe: array elements are natural
/// width and contiguous from `ARRAY_DATA_OFFSET`.
///
/// `ARRAY_DATA_OFFSET`, not `HEADER_SIZE`: the two are equal today, but the
/// planned array-length prefix (`heap_types.rs`, "objects 16, array data still
/// at 24") separates them, and an element address spelled with the object
/// header size would then read eight bytes into the array's own length prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VecArrayOperand {
    /// 64-bit GPR holding the array reference. The caller must have proven it
    /// non-null: the emitted region dereferences it with no null check, and
    /// the `LengthAtLeast` guard binding already needed its length loaded.
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
    /// The reduction operator. Only `Add`, `Or`, `Xor` and `And` are emitted,
    /// and only for an `int` or `long` `plan.elem`; those four are the
    /// operators whose identity this module can materialise in a register with
    /// one self-referential instruction — `VPXOR` for the three whose identity
    /// is `0`, `VPCMPEQD` for `&`'s all-ones. See [`accumulator_identity`].
    pub op: VecOp,
    /// 64-bit GPR holding the scalar accumulator.
    ///
    /// How much of it the epilogue touches depends on `plan.elem`: an `int`
    /// reduction reads and writes the low 32 bits and then re-sign-extends the
    /// register, so the whole 64 bits end up defined and the value is a
    /// sign-extended `int`; a `long` reduction reads and writes all 64 bits with
    /// a `REX.W` fold and does **not** re-extend.
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
    ///
    /// **`EFLAGS` is destroyed too and is not listed here**, because it is not
    /// a GPR number: every guard is a `cmp`, the head test is a `cmp`, and the
    /// back edge is preceded by an `add`. A caller that had a live condition
    /// code across the loop pre-header must re-materialise it. Stated rather
    /// than modelled, because the only two exits from this region — the
    /// patched fallback edges and `remainder_entry` — both land on code that
    /// recomputes its own test.
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
    /// `lanes * elem_bytes(elem)` is not the plan's `width_bytes`, the width is
    /// wider than the plan's own register, or it is not one
    /// `emitter_encodes_width` admits.
    ///
    /// A plan built by `simd_analysis::vector_gate::admit_vectorization` cannot
    /// reach this variant: it derives the width from the ISA, caps it only
    /// downwards, and since round 10 wave 6 refuses a cap that lands below
    /// `VectorIsa::min_width_bytes` itself. So this fires only for a
    /// hand-assembled plan — which is precisely what it is for.
    UnsupportedWidth {
        /// The width asked for.
        width_bytes: usize,
        /// The lane count asked for.
        lanes: usize,
    },
    /// The element type is a **reference**. A vector store of oops bypasses the
    /// GC write barrier; there is no lane count that rescues it.
    ObjectReferenceElement,
    /// The element type is `byte`, `char` or `short` **and the body computes**.
    /// JVM arithmetic on those is performed in `int` and narrowed at the store,
    /// so a lane-wise sub-word operation wraps at the wrong width.
    ///
    /// A sub-word body of `Load`/`Store` steps only — a copy — does not reach
    /// this: it has no arithmetic node, `VMOVDQU` has no lane width, and it is
    /// emitted. `arith_opcode` still answers this variant unconditionally for a
    /// sub-word element, which is the backstop that makes the entry point's
    /// conditional safe.
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
    /// The reduction operator is not one of the four whose identity this
    /// module can materialise (`+`, `|`, `^` — identity `0` — and `&` —
    /// identity all-ones), or the accumulator type is not `int` or `long`.
    ///
    /// Both halves of the operator test produce this variant:
    /// [`scalar_fold_opcode`] having no row and [`accumulator_identity`] having
    /// no row are equally a refusal, so a future operator added to one table
    /// and not the other is declined rather than emitted with the wrong
    /// starting value.
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
    /// The caller's [`VecEmitRequest::vector_pool`] is one this emitter cannot
    /// take, for one of four reasons:
    ///
    /// * a register outside [`VEC_POOL`];
    /// * on Windows, where the whole pool is callee-saved, one the caller's
    ///   own prologue does not save (`VecEmitRequest::frame_saved_xmms`);
    /// * more than [`VEC_POOL_LEN`] entries (`reg` is the first one past the
    ///   cap) — the allocator would silently ignore the surplus;
    /// * the same register named twice (`reg` is the repeat) — the allocator
    ///   tracks occupancy per *slot*, so a duplicate would be handed to two
    ///   simultaneously-live values.
    ///
    /// Refused for the whole region rather than skipped, because a pool the
    /// caller believes it handed over and this module quietly narrowed is a
    /// pool nobody is reasoning about correctly.
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

/// Whether this module has an encoding for a vector `width_bytes` wide.
///
/// The width table, spelled once. Two things consult it — the width check in
/// [`emit_vector_loop`] and the test
/// `the_narrowest_width_each_isa_declares_is_one_this_emitter_encodes`, which
/// pins it against every `VectorIsa::min_width_bytes` the gate carries. A
/// second copy of `matches!(w, 16 | 32)` is exactly how those two would drift
/// apart, and drifting apart is the defect the pairing exists to catch: the
/// gate admits plans, this module encodes them, and a width one side believes
/// in and the other does not is a loop analysed in full and then thrown away.
///
/// 8 is absent rather than forgotten. The half-register move is `VMOVQ`, whose
/// load (`VEX.128.F3.0F 7E`) and store (`VEX.128.66.0F D6`) directions do not
/// share a mandatory prefix — `move_opcodes` returns one `pp` for both
/// directions and cannot express it — so it is a new encoder family rather
/// than a table row. See `VectorIsa::min_width_bytes`, which is the number the
/// gate reads so that a plan this returns `false` for is refused before it is
/// ever built.
fn emitter_encodes_width(width_bytes: usize) -> bool {
    matches!(width_bytes, 16 | 32)
}

/// The unaligned packed move for `elem`: `(load, store)`.
///
/// Integer lanes move with `VMOVDQU`; `float`/`double` lanes move with
/// `VMOVUPS`/`VMOVUPD` so the data does not cross the integer/FP domain on
/// every iteration.
///
/// The sub-word types share the `Int`/`Long` row, and that is a statement about
/// the instruction rather than a convenience: `VMOVDQU` moves a whole register
/// and has no lane width to get wrong, so one row serves 1-, 2-, 4- and 8-byte
/// elements. What differs per element type is the *SIB scale* of the address
/// ([`scale_log2`]) and the arithmetic ([`arith_opcode`]), and only the second
/// of those is unsafe for sub-word lanes. Returning a row here therefore does
/// **not** admit sub-word arithmetic: [`arith_opcode`] still refuses every
/// sub-word operation unconditionally, and `emit_vector_loop` refuses a
/// sub-word body that contains a `Binary` or `Accumulate` step before reaching
/// either table. Two independent checks, as for oop stores.
fn move_opcodes(elem: MemKind) -> Result<(VexOpcode, VexOpcode), VecEmitRefusal> {
    let (pp, load, store) = match elem {
        // VEX.F3.0F 6F /r, VEX.F3.0F 7F /r — VMOVDQU
        MemKind::Int | MemKind::Long => (2u8, 0x6Fu8, 0x7Fu8),
        // The same VMOVDQU row. A 16- or 32-byte integer move does not know
        // its lane width, so `byte`/`char`/`short` copies need no new opcode.
        MemKind::Byte | MemKind::Char | MemKind::Short => (2, 0x6F, 0x7F),
        // VEX.0F 10 /r, VEX.0F 11 /r — VMOVUPS
        MemKind::Float => (0, 0x10, 0x11),
        // VEX.66.0F 10 /r, VEX.66.0F 11 /r — VMOVUPD
        MemKind::Double => (1, 0x10, 0x11),
        MemKind::Ref => return Err(VecEmitRefusal::ObjectReferenceElement),
        // No wildcard arm. Every `MemKind` is now named, so a new element type
        // is a compile error here rather than a silent `SubwordElement`
        // refusal for something that is not sub-word.
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
            // VEX.66.0F38 39 /r  VPMINSD and VEX.66.0F38 3D /r  VPMAXSD —
            // SSE4.1, arriving with `VPMULLD` and gated on the same claim,
            // which is what `VectorIsa::int32_mul_minmax`'s name says.
            //
            // **Signed**, which is the only thing to get wrong here: the
            // unsigned forms are the neighbouring opcodes (`PMINUD` is `3B`,
            // `PMAXUD` is `3F`) and `Math.min`/`Math.max` on a JVM `int` is
            // signed. `39`/`3D` are the `S` forms.
            VecOp::Min => {
                if !int32_mul_minmax {
                    return Err(VecEmitRefusal::MissingIsaFeature { elem, op });
                }
                (2, 1, 0x39)
            }
            VecOp::Max => {
                if !int32_mul_minmax {
                    return Err(VecEmitRefusal::MissingIsaFeature { elem, op });
                }
                (2, 1, 0x3D)
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
            // No `VPMULLQ` outside AVX-512, and no `VPMINSQ`/`VPMAXSQ` either —
            // 64-bit lane multiply and 64-bit signed lane min/max are all
            // AVX-512 instructions. The gate cannot express that distinction:
            // `VectorIsa::int32_mul_minmax` is a 32-bit claim by name and its
            // use is guarded by `elem_bytes(a.elem) == 4`, so an `int` min/max
            // clears the gate and a `long` one clears it too. This refusal is
            // the only thing that says no to the `long` case, and it explains
            // itself, which is why it is left as the only thing.
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
        // The sub-word backstop, and since round 10 wave 8 it is load-bearing
        // rather than belt-and-braces: `move_opcodes` now has a `byte`/`char`/
        // `short` row, so a sub-word **copy** is emittable and the entry point's
        // refusal is conditional on the body. This arm is what makes that
        // conditional safe — a body scan widened by a later edit still cannot
        // reach a lane-wise sub-word opcode, because there is none to reach.
        // Unconditional, and it must stay unconditional: JVM `+ - * & | ^` on
        // `byte`/`char`/`short` is computed in `int` on extended operands and
        // narrowed at the store, and `VecStep` cannot say "only ever narrowed".
        MemKind::Byte | MemKind::Char | MemKind::Short => {
            return Err(VecEmitRefusal::SubwordElement { elem })
        }
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
///
/// The same opcode byte serves both widths; `REX.W` is the only difference and
/// [`Asm::alu_rr64`] supplies it.
///
/// This table is **half** of the reduction admission test. The other half is
/// [`accumulator_identity`], and `reduction_enc` requires both to answer. See
/// that function's doc for why a row here without a row there is wrong code
/// rather than a refusal.
fn scalar_fold_opcode(op: VecOp) -> Option<u8> {
    match op {
        VecOp::Add => Some(0x01),
        VecOp::Or => Some(0x09),
        VecOp::Xor => Some(0x31),
        // `and r/m, r`. Added round 10 wave 9, in the same change as `&`'s
        // accumulator init — see `accumulator_identity`, which is the half of
        // this pair that makes the row correct rather than zero-answering.
        VecOp::And => Some(0x21),
        _ => None,
    }
}

/// The instruction that materialises `op`'s identity in the vector accumulator,
/// emitted once before the head test as `enc acc, acc, acc`.
///
/// # Why this is a table and not a literal
///
/// The accumulator is initialised *before* the loop and folded into the caller's
/// scalar accumulator *after* it, so the value it starts at has to be `op`'s
/// identity — otherwise a loop that runs zero vector passes (the head test can
/// fail on its first evaluation) corrupts a scalar result that was already
/// correct, and a loop that runs some passes starts from the wrong element.
///
/// Until round 10 wave 9 this was an unconditional `VPXOR` written inline at the
/// allocation site, and [`scalar_fold_opcode`] was the only thing deciding which
/// reductions were admitted. That pairing is a wrong-code trap rather than a
/// missing feature: adding `And => 0x21` to `scalar_fold_opcode` alone admits an
/// `&` reduction whose accumulator starts at `0`, and `0 & anything` is `0`, so
/// every such loop answers zero — silently, with no refusal, and invisibly to a
/// suite whose reductions all sum. So the two tables are now consulted together
/// (`reduction_enc` requires *both* to answer) and a missing row on either side
/// is a [`VecEmitRefusal::UnsupportedReduction`], never a wrong answer.
/// `the_two_reduction_tables_admit_exactly_the_same_operators` pins that.
///
/// # The rows
///
/// * `+`, `|`, `^` — identity `0`. `VPXOR acc, acc, acc`
///   (`VEX.66.0F EF /r`), which is zero whatever the register held.
/// * `&` — identity all-ones. `VPCMPEQD acc, acc, acc` (`VEX.66.0F 76 /r`):
///   comparing a register with itself is true in every lane regardless of its
///   contents, so this needs no constant pool and no memory operand. The
///   element width in the mnemonic is irrelevant here — all-ones dwords are
///   all-ones qwords and all-ones bytes — so the one row serves `Int` and
///   `Long` alike. `VPCMPEQD ymm` is AVX2, which `emit_vector_loop` has already
///   required of the host.
/// * `*` — identity `1`, and deliberately absent. The cheapest sequence is
///   `VPCMPEQD` followed by `VPSRLD acc, acc, 31`, and
///   `VPSRLD xmm, xmm, imm8` is `VEX.NDD.128.66.0F 72 /2 ib`: the destination
///   is in `vvvv`, the source in `r/m`, and the opcode extension in ModRM
///   `reg`. That is a different operand layout from every form [`Asm`] has
///   ([`Asm::vec_rr`] puts the destination in ModRM `reg`), so it is a new
///   encoder family rather than a table row. `long *` cannot follow at all —
///   `VPMULLQ` is AVX-512.
/// * `-`, `min`, `max` — `-` is not associative and the other two have no
///   identity in a fixed-width lane that is not a materialised extremum.
fn accumulator_identity(op: VecOp) -> Option<VexOpcode> {
    let enc = match op {
        // VEX.66.0F EF /r  VPXOR
        VecOp::Add | VecOp::Or | VecOp::Xor => 0xEFu8,
        // VEX.66.0F 76 /r  VPCMPEQD
        VecOp::And => 0x76,
        _ => return None,
    };
    Some(VexOpcode {
        map: 1,
        pp: 1,
        w: false,
        op: enc,
    })
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

    /// `VMOVQ r64, xmm` — VEX.128.66.0F.**W1** 7E /r. The XMM register is the
    /// ModRM `reg` field, exactly as in [`Asm::vmovd_to_gpr`].
    ///
    /// Same opcode byte as `VMOVD`, and `W` is the whole difference: `W0` moves
    /// 32 bits, `W1` moves 64. Because `W` is set, [`Asm::vex_prefix`] cannot
    /// take its two-byte shortcut, so this is always the three-byte `C4` form
    /// even for XMM0..7 with a low GPR.
    ///
    /// Not to be confused with the *other* `VMOVQ`, `VEX.128.F3.0F 7E /r`,
    /// which moves `xmm <- xmm/m64` and is the load direction
    /// `emitter_encodes_width` does not offer. Different `pp`, same opcode; the
    /// pair is why `move_opcodes`'s single-`pp` return type cannot express an
    /// 8-byte element.
    fn vmovq_to_gpr(&mut self, dst_gpr: u8, src_xmm: u8) {
        self.vex_prefix(src_xmm >= 8, false, dst_gpr >= 8, 1, true, 0, false, 1);
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

    /// `op r/m64, r64` — the 64-bit reduction fold. `reg` = source.
    ///
    /// The same opcode bytes as [`Asm::alu_rr32`] (`01` add, `09` or, `31`
    /// xor); `REX.W` is the only difference and it is what makes
    /// [`Asm::movsxd_self`] unnecessary afterwards rather than merely
    /// redundant. A 64-bit ALU op writes all 64 bits of its destination, so
    /// re-sign-extending from bit 31 would *destroy* any accumulator whose
    /// value does not fit an `i32` — the one trap in the `long` epilogue.
    ///
    /// `REX` is emitted unconditionally here, unlike in `alu_rr32`, because
    /// `W` always needs a byte to live in.
    fn alu_rr64(&mut self, opcode: u8, dst: u8, src: u8) {
        self.rex_w(src >= 8, false, dst >= 8);
        self.byte(opcode);
        self.byte(0xC0 | ((src & 7) << 3) | (dst & 7));
    }

    /// `movsxd r64, r32` on one register — `REX.W 63 /r`, `reg` = `rm` = `r`.
    fn movsxd_self(&mut self, r: u8) {
        self.rex_w(r >= 8, false, r >= 8);
        self.byte(0x63);
        self.byte(0xC0 | ((r & 7) << 3) | (r & 7));
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
    // Every encoding below is VEX — `Asm` has exactly one prefix emitter and
    // every vector helper routes through it — so a host without AVX2 would
    // take a SIGILL. This is deliberately a check on the HOST and not on
    // `plan.isa`: the 16-byte paths here are VEX.128, which is AVX rather than
    // SSE2/SSE4.1, so "honouring a 128-bit plan.isa" on a non-AVX host would
    // emit the illegal instruction rather than avoid it. Since round 10
    // `simd_analysis::vector_gate::VectorIsa::detect` no longer offers those
    // two ISAs on x86-64 at all, which is the other half of this coupling; the
    // test `the_gate_only_offers_an_isa_this_emitter_can_encode` pins it.
    if !req.host.has_avx2() {
        return Err(VecEmitRefusal::HostLacksAvx2);
    }
    // The caller's pool, checked before anything is emitted. A pool longer
    // than `VEC_POOL_LEN` is refused rather than truncated: `VecRegPool::alloc`
    // scans only the first `VEC_POOL_LEN` slots, so the surplus would be a
    // quiet narrowing — the exact failure `UnusableVectorPool`'s doc says must
    // never happen.
    if let Some(&reg) = req.vector_pool.get(VEC_POOL_LEN) {
        return Err(VecEmitRefusal::UnusableVectorPool { reg });
    }
    // Three more ways to fail, per register: one outside `VEC_POOL` is not
    // this emitter's to give; one the caller's own prologue does not save
    // would corrupt a caller's floating-point state on Windows and not on
    // Linux — the worst shape a bug can have; and a REPEATED register is the
    // one pool defect that produces wrong code instead of a refusal.
    // `VecRegPool` keys `in_use` by *slot*, not by register number, so a pool
    // of `[xmm8, xmm8]` hands xmm8 out twice and two simultaneously-live
    // vector values end up sharing it — and `free` then resolves the register
    // back to the *first* matching slot, so the bookkeeping cannot recover
    // either. Refused here, where the caller still learns which register it
    // named twice.
    for (i, &reg) in req.vector_pool.iter().enumerate() {
        if !crate::regalloc::xmm_roles::vector_pool_is_encodable(reg, req.frame_saved_xmms) {
            return Err(VecEmitRefusal::UnusableVectorPool { reg });
        }
        // `get(..i)` rather than `[..i]`: this module never panics.
        if matches!(req.vector_pool.get(..i), Some(earlier) if earlier.contains(&reg)) {
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
    // Sub-word elements: refused when the body *computes*, emitted when it only
    // moves. The reason the two differ is entirely about arithmetic. JVM `+ - *
    // & | ^` on `byte`/`char`/`short` is performed in `int` on sign- (or, for
    // `char`, zero-) extended operands and narrowed only at the store, and
    // `VecStep` has no way to say "this value is only ever narrowed", so a
    // lane-wise `PADDB`/`PADDW` can wrap at the wrong width. **A copy has no
    // arithmetic node at all**: `b[i] = a[i]` over `byte[]` moves the lanes bit
    // for bit, and `VMOVDQU` does not know its lane width, so there is nothing
    // for a width to be wrong about.
    //
    // The condition is a scan of the body this module was *handed*, not a
    // deduction from what the gate would have admitted. The gate does refuse
    // sub-word arithmetic — `VecRefusal::MixedElementWidths`, because the
    // arithmetic node's `elem` is `Int` while the accesses' is `Byte` — but
    // that refusal depends on the *producer* labelling the node `Int`, and a
    // producer that labelled it `Byte` would clear the gate. This module
    // validates the plan it is handed, exactly as it does for `lanes`, `elem`
    // and the guard list, so the scan is local and the gate's behaviour is not
    // load-bearing for it.
    //
    // `arith_opcode` is the second, independent check: its sub-word arm is an
    // unconditional refusal, so even a scan widened by accident cannot encode a
    // sub-word lane-wise operation. Two gates, for the same reason oop stores
    // have two.
    //
    // Deliberately still *before* the width check, so that a sub-word plan
    // whose `width_bytes` was computed for a wider element (a hand-built plan,
    // or a fixture that mutates `plan.elem`) is refused with the element-type
    // reason rather than an arithmetically-true width complaint.
    if matches!(plan.elem, MemKind::Byte | MemKind::Char | MemKind::Short)
        && shape
            .body
            .iter()
            .any(|s| matches!(s, VecStep::Binary { .. } | VecStep::Accumulate { .. }))
    {
        return Err(VecEmitRefusal::SubwordElement { elem: plan.elem });
    }
    let elem_size = elem_bytes(plan.elem);
    let scale = match scale_log2(elem_size) {
        Some(s) => s,
        None => return Err(VecEmitRefusal::SubwordElement { elem: plan.elem }),
    };

    // ---- the width ---------------------------------------------------------
    // `plan.width_bytes > plan.isa.width_bytes` is the plan contradicting
    // itself: `VecPlan::width_bytes`'s own doc says it "may be narrower than
    // the register when a dependence capped the lane count" — narrower, never
    // wider. `admit_vectorization` cannot produce such a plan (it derives the
    // width from `isa.lanes_for(elem)` and only ever caps it downwards), but
    // this module validates the plan it is handed rather than the plan it
    // assumes was built, exactly as it does for `lanes`, `elem` and the guard
    // list. Without the check a caller could hand `isa: sse2()` with a 32-byte
    // width and get VEX.256 integer bytes emitted for a target that claims
    // 128-bit registers.
    //
    // The floor is the other direction, and since round 10 wave 6 it is no
    // longer reachable from a gate-produced plan either: a dependence-capped
    // width below `plan.isa.min_width_bytes` — two `int` lanes, eight bytes,
    // from a backward distance of 2 or 3 — is now refused by
    // `admit_vectorization` as `VecRefusal::WidthBelowIsaMinimum`, where the
    // dependence that caused it can be named. `emitter_encodes_width` stays
    // here regardless, for the same reason as the line above it: a hand-built
    // plan is still a plan this module must not emit garbage for. The two
    // numbers are tied together by
    // `the_narrowest_width_each_isa_declares_is_one_this_emitter_encodes`.
    if plan.lanes < 2
        || elem_size == 0
        || plan.lanes.checked_mul(elem_size) != Some(plan.width_bytes)
        || plan.width_bytes > plan.isa.width_bytes
        || !emitter_encodes_width(plan.width_bytes)
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

    // The reduction epilogue exists for exactly four operators — `+`, `|`, `^`
    // and `&` — on the two integer accumulator widths, `int` and `long`.
    //
    // The operator set is decided by the *identity*, not by the fold: the vector
    // accumulator is initialised to `op`'s identity before the head test and
    // folded into the caller's scalar accumulator after the loop, so an operator
    // is emittable exactly when this module can materialise its identity in a
    // register. That is also what makes a loop running zero vector passes
    // correct — it folds an identity in, a no-op.
    //
    // Two tables say so and **both** are consulted here: `scalar_fold_opcode`
    // for the final GPR fold and `accumulator_identity` for the register init.
    // Requiring both is not belt and braces, it is the thing that keeps this a
    // refusal: a fold row without an init row gives `&` a `VPXOR`-zeroed
    // accumulator and the answer `0` for every loop. See
    // `accumulator_identity`'s doc. `*` is refused by both tables (identity `1`
    // needs an operand layout `Asm` does not have, and `VPMULLQ` is AVX-512),
    // and `-`/`min`/`max` by both as well.
    //
    // `long` is admitted as of round 10 wave 8 and needs no new identity
    // argument: `long +`, `long |` and `long ^` are exactly associative and
    // commutative over the whole two's-complement domain (JVM `long` arithmetic
    // is modular, so this is the *same* argument as for `int`, not a weaker
    // one), their identity is the same `0`, and `VPADDQ`/`VPOR`/`VPXOR` are all
    // already in `arith_opcode`'s `Long` arm. What it needed was the epilogue's
    // fold, which is width-dependent; see below.
    //
    // `&` is admitted as of round 10 wave 9, at both widths, for the same
    // reason at one remove: it is associative, commutative and idempotent over
    // every bit pattern, and its identity — all-ones — is one `VPCMPEQD acc,
    // acc, acc` away rather than a constant pool away. `VPAND` was already in
    // both arms of `arith_opcode`, so nothing inside the loop or in the
    // horizontal tree changed.
    //
    // Floating-point reductions are refused here even when the gate admitted
    // one under `FpRelaxation::AllowReassociation`. That is deliberate and
    // independent: the emitter does not take the caller's word for a changed FP
    // result. `plan.elem` is matched positively — `Int | Long` — rather than
    // excluding `Float`/`Double`, so a future element type is refused by
    // default instead of being admitted by an unrevised `!=`.
    let reduction_enc = match shape.reduction {
        None => None,
        Some(r) => {
            let init = match (
                matches!(plan.elem, MemKind::Int | MemKind::Long),
                scalar_fold_opcode(r.op),
                accumulator_identity(r.op),
            ) {
                // Both tables answer, and the element is an integer width.
                (true, Some(_), Some(init)) => init,
                _ => {
                    return Err(VecEmitRefusal::UnsupportedReduction {
                        elem: plan.elem,
                        op: r.op,
                    })
                }
            };
            Some((
                r,
                arith_opcode(plan.elem, r.op, plan.isa.int32_mul_minmax)?,
                init,
            ))
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
        Some((_, _, init)) => {
            let reg = pool.alloc()?;
            // `init acc, acc, acc` — `VPXOR` for `+`/`|`/`^`, `VPCMPEQD` for
            // `&`. Both are self-referential by construction, so neither reads
            // whatever the pool's register happened to hold. Emitted *before*
            // the head test so a loop that runs zero vector passes still folds
            // an identity into the caller's scalar accumulator.
            asm.vec_rr(init, l, reg, reg, reg);
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
                    (Some(acc), Some((_, enc, _))) => asm.vec_rr(enc, l, acc, acc, sreg),
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

    if let (Some(acc), Some((red, enc, _))) = (acc_reg, reduction_enc) {
        // 64-bit lanes take a shorter tree and a different final move. Derived
        // from `plan.elem`, which `reduction_enc` has already restricted to
        // `Int | Long`, so these two arms are exhaustive over what can get here.
        let long_lanes = plan.elem == MemKind::Long;
        let tmp = pool.alloc()?;
        // 256-bit: fold the high 128 bits into the low ones first. Lane width
        // does not matter to this step — `VEXTRACTI128` moves 128 bits and the
        // following `enc` is the element's own lane-wise operation.
        if l {
            asm.vextracti128(tmp, acc, 1);
            asm.vec_rr(enc, false, acc, acc, tmp);
        }
        // `VPSHUFD 0x4E` selects dwords `[2, 3, 0, 1]`, which read as qwords is
        // exactly a 64-bit half-swap. So the *same* instruction is step one of
        // the four-dword tree and the *whole* of the two-qword tree.
        asm.vpshufd(tmp, acc, 0x4E);
        asm.vec_rr(enc, false, acc, acc, tmp);
        if !long_lanes {
            // The second dword step: `0xB1` selects `[1, 0, 3, 2]`, swapping
            // adjacent dwords, which brings lane 1 alongside lane 0. It must
            // **not** run for 64-bit lanes: it would swap the two halves
            // *inside* each qword and fold the result back in, which is not a
            // horizontal sum of anything.
            asm.vpshufd(tmp, acc, 0xB1);
            asm.vec_rr(enc, false, acc, acc, tmp);
        }
        match scalar_fold_opcode(red.op) {
            Some(opcode) if long_lanes => {
                // The whole 64-bit lane 0, and a fold that writes all 64 bits
                // of the accumulator.
                asm.vmovq_to_gpr(shape.scratch, acc);
                asm.alu_rr64(opcode, red.acc, shape.scratch);
                // No `movsxd`, and this is the trap the `int` arm below makes
                // look like an omission. A `REX.W` ALU op writes the full
                // register, so the accumulator is already exact; sign-extending
                // it from bit 31 would corrupt every `long` sum outside the
                // `i32` range — which is most of the values a `long`
                // accumulator exists for.
            }
            Some(opcode) => {
                asm.vmovd_to_gpr(shape.scratch, acc);
                asm.alu_rr32(opcode, red.acc, shape.scratch);
                // A 32-bit ALU op zero-extends into the full register, so the
                // accumulator now holds a ZERO-extended `int`. The shape's
                // contract (and the single-pass backend's local
                // representation) is sign-extended; a negative sum left
                // zero-extended would read back as a large positive `long`
                // through any 64-bit consumer (`i2l`, a 64-bit compare, a
                // frame spill reloaded as a `long`). Restore it.
                asm.movsxd_self(red.acc);
            }
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
        .checked_add(ARRAY_DATA_OFFSET as i64)
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

    // Here, not in the production `impl` above: a test gate above production
    // code stops the panic-free ratchet's scan (round 9 wave 8, review8c F2).
    // A child module may read the private `avx2` field.
    impl HostVectorSupport {
        /// A stated capability, for tests only, so the encoding tests run on
        /// hosts without AVX2 (the bytes are checked, never executed).
        pub(crate) const fn for_test(avx2: bool) -> HostVectorSupport {
            HostVectorSupport { avx2 }
        }
    }

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

    /// `b[i] = a[i]` — a pure copy, `a` in RCX and `b` in RDI, with no
    /// arithmetic node anywhere in the body.
    ///
    /// The element type lives in the *plan*, not here: this body is exactly as
    /// valid for `byte[]` as for `int[]`, which is the whole point of the
    /// sub-word copy case. What changes with the element type is the SIB scale
    /// of the two addresses.
    fn copy_shape() -> VecLoopShape {
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
                VecStep::Store {
                    to: VecArrayOperand {
                        base: RDI,
                        index_offset: 0,
                    },
                    src: 0,
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

    /// [`VecEmitPolicy::from_flags`]'s first caller of any kind.
    ///
    /// Round 10 wave 6. The module doc recorded it as the one symbol here with
    /// *no caller at all, not even a test* — which is the round's recurring
    /// defect class: an entry point nothing reaches is indistinguishable from
    /// one that is merely never taken. This does not give it a production
    /// caller (that needs the `x64.rs` call site, which is another lane's file
    /// and does not exist yet), but it does mean the function is executed
    /// somewhere.
    ///
    /// It is deliberately close to the body, because what it pins is not the
    /// falsey-word table — `the_default_policy_is_off` already owns that,
    /// against the pure half — but the two things a future edit gets wrong:
    /// the **key** (`VECTORIZE_FLAG`, the same constant
    /// `simd_analysis::simd_sum_forms_enabled` reads, with its own opposite
    /// default documented on the constant) and the **access path**
    /// (`cratonvm_types::flags::runtime_var_os`, the VM's flag layer, not
    /// `std::env::var` — this tree distinguishes them; see
    /// `docs/internal/feature-designs/flag-declaration-audit.md`).
    ///
    /// It reads process-global environment state and mutates none, so it is
    /// order-independent under a parallel test runner whatever the flag is set
    /// to — which is why the assertion is an agreement between the two halves
    /// rather than a fixed verdict.
    #[test]
    fn reading_the_flag_and_reading_the_value_agree() {
        let raw = cratonvm_types::flags::runtime_var_os(VECTORIZE_FLAG);
        assert_eq!(
            VecEmitPolicy::from_flags(),
            VecEmitPolicy::from_flag_value(raw.as_ref().and_then(|v| v.to_str())),
            "from_flags must be from_flag_value applied to {VECTORIZE_FLAG} \
             as the flag layer sees it"
        );
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

    /// The gate offers exactly the ISAs this emitter can encode, and it is
    /// this test that keeps the two ends of that coupling from drifting apart.
    ///
    /// The round-10 finding
    /// (`docs/known-issues/jit/r10-intr-vecplan-non-avx2-isa-branch-is-unreachable-20260920.md`)
    /// was that `VectorIsa::detect` picked `sse41()`/`sse2()` on the `else`
    /// side of the same `has_avx2()` test this function hard-fails on, so
    /// those plans could never be emitted. `detect` was narrowed to
    /// AVX2-or-`None` on x86-64; this asserts the property that narrowing
    /// bought, from both sides.
    ///
    /// The contracts the assertions rest on, stated rather than observed:
    ///
    /// * `VectorIsa::detect()` is `Option<VectorIsa>` and on x86-64 its body
    ///   is `crate::x64::has_avx2().then(VectorIsa::avx2)`. `bool::then`
    ///   yields `Some` when the receiver is **true**, so `is_some()` means the
    ///   host HAS AVX2.
    /// * `HostVectorSupport::detect()` reads `cpu_features::has_avx2()`, and
    ///   `crate::x64::has_avx2` is that same function re-exported (`x64.rs`,
    ///   `pub use cpu_features::{has_avx2, …}`) with a cached answer. So the
    ///   two queries cannot disagree.
    /// * `emit_vector_loop` refuses with `HostLacksAvx2` when
    ///   `!req.host.has_avx2()` — note the negation: the refusal fires on the
    ///   feature being ABSENT, the opposite polarity from `is_some()` above.
    ///
    /// Composing those three: on x86-64, `detect().is_some()` and "emission is
    /// not refused for lack of AVX2" are the same proposition, and both arms
    /// are asserted so the test says something on an AVX2 host and on a
    /// non-AVX2 one alike.
    ///
    /// `#[cfg(target_arch = "x86_64")]` because off x86-64 the two sides are
    /// deliberately not coupled: `detect` answers `neon128()` on aarch64,
    /// where the emitter for it is `jit/src/aarch64.rs`, not this module.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn the_gate_only_offers_an_isa_this_emitter_can_encode() {
        let host = HostVectorSupport::detect();
        let offered = VectorIsa::detect();
        assert_eq!(
            offered.is_some(),
            host.has_avx2(),
            "the gate offers an ISA exactly when this emitter's host gate passes"
        );

        let shape = elementwise_shape();
        match offered {
            Some(isa) => {
                // Build the plan out of the ISA that was actually offered, not
                // out of `plan_for`'s hard-coded AVX2, so this fails if
                // `detect` ever starts offering something narrower again.
                let lanes = isa.lanes_for(MemKind::Int);
                let mut plan = plan_for(MemKind::Int, lanes, Vec::new());
                plan.isa = isa;
                let mut req = request(&plan, &shape, &[]);
                req.host = host;
                assert_ne!(
                    emit_vector_loop(&req),
                    Err(VecEmitRefusal::HostLacksAvx2),
                    "an offered ISA must not be refused for lack of AVX2"
                );
            }
            None => {
                // Nothing was offered, so nothing was analysed — and had a
                // plan been built anyway, this is the refusal it would meet.
                let plan = plan_for(MemKind::Int, 8, Vec::new());
                let mut req = request(&plan, &shape, &[]);
                req.host = host;
                assert_eq!(
                    emit_vector_loop(&req),
                    Err(VecEmitRefusal::HostLacksAvx2),
                    "no AVX2 means no emission, which is why nothing is offered"
                );
            }
        }
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
            0xC5, 0x7E, 0x6F, 0x44, 0x99, HDR8, // vmovdqu ymm8, [rcx + rbx*4 + ARRAY_DATA_OFFSET]
            0xC5, 0x7E, 0x6F, 0x4C, 0x9A, HDR8, // vmovdqu ymm9, [rdx + rbx*4 + ARRAY_DATA_OFFSET]
            0xC4, 0x41, 0x3D, 0xFE, 0xC1,       // vpaddd  ymm8, ymm8, ymm9
            0xC5, 0x7E, 0x7F, 0x44, 0x9F, HDR8, // vmovdqu [rdi + rbx*4 + ARRAY_DATA_OFFSET], ymm8
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
        let tail = &code.code[code.code.len() - 46..];
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
            // The 32-bit add zero-extended R8; the accumulator's contract is
            // a sign-extended `int`, so it is re-extended before anyone reads
            // the full register.
            0x4D, 0x63, 0xC0,                   // movsxd r8, r8d
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

    /// The `long` reduction epilogue: a shorter tree, a 64-bit move, a `REX.W`
    /// fold, and **no** `MOVSXD`.
    ///
    /// Round 10 wave 8. Before it, `reduction_enc` opened `if plan.elem !=
    /// MemKind::Int` and every `long[]` reduction the gate admitted — which is
    /// every one of them, since nothing in `admit_vectorization` looks at the
    /// accumulator width — was refused with `UnsupportedReduction { Long, Add }`.
    ///
    /// Why the identity argument needs nothing new: JVM `long` arithmetic is
    /// modular two's-complement, so `+` is exactly associative and commutative
    /// over the whole domain and its identity is `0` — the *same* argument the
    /// `int` case rests on, at a different width. `VPXOR` still initialises
    /// correctly and `VPADDQ` was already in `arith_opcode`.
    ///
    /// Every byte below is re-derived from the VEX encoding rules, not
    /// transcribed from output:
    ///
    /// * `VPADDQ` is `VEX.66.0F D4 /r`, so map 1 and `pp = 1`. With `dst = xmm8`
    ///   and `src2 = xmm9` both extended, VEX.B is needed, which rules out the
    ///   two-byte `C5` form: `C4 41 3D` at 256-bit, `C4 41 39` at 128-bit — the
    ///   same prefix bytes as the `int` test's `VPADDD`, which differs only in
    ///   the opcode (`D4` against `FE`).
    /// * `VPSHUFD 0x4E` selects dwords `[2, 3, 0, 1]`. Read as qwords that is a
    ///   half-swap, so one step is the whole two-lane tree. The `int` tree's
    ///   second step, `0xB1` (`[1, 0, 3, 2]`), must be absent: it swaps the
    ///   halves *inside* each qword.
    /// * `VMOVQ r64, xmm` is `VEX.128.66.0F.W1 7E /r` — the same opcode byte as
    ///   `VMOVD` with `W` set. `W = 1` forbids the two-byte prefix, so this is
    ///   `C4 61 F9 7E C0` where the `int` test has the two-byte `C5 79 7E C0`.
    ///   `61` is `R̄=0` (xmm8 in the `reg` field), `X̄=1`, `B̄=1` (RAX in `r/m`),
    ///   map 1; `F9` is `W=1`, `vvvv=1111`, `L=0`, `pp=01`.
    /// * `add r8, rax` is `REX.W 01 /r` with `reg = RAX`: `49 01 C0`. `49` is
    ///   `REX.W|B` — B because the *destination* R8 is the `r/m` operand.
    #[test]
    fn a_long_sum_reduction_folds_in_64_bits_and_never_re_extends() {
        let plan = plan_for(MemKind::Long, 4, Vec::new());
        assert_eq!(plan.width_bytes, 32, "four long lanes is a whole YMM");
        let shape = reduction_shape();
        let code = emit(&plan, &shape).expect("a long reduction now emits");

        // Same zeroing init as the `int` case: identity `0`, same instruction.
        assert_eq!(
            &code.code[..5],
            &[0xC4, 0x41, 0x3D, 0xEF, 0xC0],
            "vpxor ymm8, ymm8, ymm8"
        );
        // The accumulator holds XMM8, so the load lands in XMM9 and the address
        // scales by 8.
        assert!(
            contains(&code.code, &[0xC5, 0x7E, 0x6F, 0x4C, 0xD9, HDR8]),
            "vmovdqu ymm9, [rcx + rbx*8 + ARRAY_DATA_OFFSET]"
        );
        assert!(
            contains(&code.code, &[0xC4, 0x41, 0x3D, 0xD4, 0xC1]),
            "vpaddq ymm8, ymm8, ymm9 accumulates 64-bit lanes"
        );

        let tail = &code.code[code.code.len() - 33..];
        #[rustfmt::skip]
        let expected_tail: Vec<u8> = vec![
            0xC4, 0x43, 0x7D, 0x39, 0xC1, 0x01, // vextracti128 xmm9, ymm8, 1
            0xC4, 0x41, 0x39, 0xD4, 0xC1,       // vpaddq xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E, // vpshufd xmm9, xmm8, 0x4E
            0xC4, 0x41, 0x39, 0xD4, 0xC1,       // vpaddq xmm8, xmm8, xmm9
            0xC4, 0x61, 0xF9, 0x7E, 0xC0,       // vmovq rax, xmm8   (W1, not W0)
            0x49, 0x01, 0xC0,                   // add   r8, rax     (REX.W)
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(tail, expected_tail.as_slice());

        // The two things whose *absence* is the correctness claim.
        assert!(
            !contains(&code.code, &[0x70, 0xC8, 0xB1]),
            "the second dword shuffle would fold each qword's own halves together"
        );
        assert!(
            !contains(&code.code, &[0x4D, 0x63, 0xC0]),
            "movsxd r8, r8d would truncate every long sum outside the i32 range"
        );
        assert_eq!(code.clobbered_gprs, vec![RAX, R8]);
    }

    /// Two `long` lanes: no lane extract, and still exactly one shuffle.
    #[test]
    fn a_128_bit_long_reduction_needs_one_shuffle_and_no_lane_extract() {
        let plan = plan_for(MemKind::Long, 2, Vec::new());
        assert_eq!(plan.width_bytes, 16, "two long lanes is a whole XMM");
        let shape = reduction_shape();
        let code = emit(&plan, &shape).expect("a 128-bit long reduction emits");

        let tail = &code.code[code.code.len() - 19..];
        #[rustfmt::skip]
        let expected_tail: Vec<u8> = vec![
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E, // vpshufd xmm9, xmm8, 0x4E
            0xC4, 0x41, 0x39, 0xD4, 0xC1,       // vpaddq xmm8, xmm8, xmm9
            0xC4, 0x61, 0xF9, 0x7E, 0xC0,       // vmovq rax, xmm8
            0x49, 0x01, 0xC0,                   // add   r8, rax
        ];
        assert_eq!(tail, expected_tail.as_slice());
        assert!(
            !contains(&code.code, &[0x39, 0xC1, 0x01]),
            "there is no high 128-bit lane to extract"
        );
        assert!(
            !contains(&code.code, &[0xC5, 0xF8, 0x77]),
            "VEX-128 zeroes the upper halves, so no VZEROUPPER is owed"
        );
    }

    /// The `long` operator set is the `int` one, for the same reason: an
    /// identity this module can materialise in one instruction.
    ///
    /// `Mul` is refused although a 32-bit `VPMULLD` exists — there is no
    /// `VPMULLQ` outside AVX-512 *and* no one-instruction `1` — and `Min`/`Sub`
    /// for want of an identity and of associativity respectively. All three are
    /// `UnsupportedReduction` and not `UnsupportedOp`, because what is missing
    /// is the accumulator's starting value rather than the lane-wise opcode.
    #[test]
    fn long_reductions_are_the_four_operators_with_a_materialisable_identity() {
        for op in [VecOp::Add, VecOp::Or, VecOp::Xor, VecOp::And] {
            let plan = plan_for(MemKind::Long, 4, Vec::new());
            let mut shape = reduction_shape();
            shape.reduction = Some(VecReduction { op, acc: R8 });
            assert!(emit(&plan, &shape).is_ok(), "long {op:?} reduces");
        }
        for op in [VecOp::Mul, VecOp::Min, VecOp::Max, VecOp::Sub] {
            let plan = plan_for(MemKind::Long, 4, Vec::new());
            let mut shape = reduction_shape();
            shape.reduction = Some(VecReduction { op, acc: R8 });
            assert_eq!(
                emit(&plan, &shape),
                Err(VecEmitRefusal::UnsupportedReduction {
                    elem: MemKind::Long,
                    op
                }),
                "{op:?} has no identity this epilogue can materialise"
            );
        }
    }

    #[test]
    fn only_add_or_xor_and_and_reductions_are_emitted() {
        for op in [VecOp::Mul, VecOp::Min, VecOp::Max, VecOp::Sub, VecOp::Div] {
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
        for op in [VecOp::Add, VecOp::Or, VecOp::Xor, VecOp::And] {
            let plan = plan_for(MemKind::Int, 8, Vec::new());
            let mut shape = reduction_shape();
            shape.reduction = Some(VecReduction { op, acc: R8 });
            assert!(emit(&plan, &shape).is_ok(), "{op:?} reduces");
        }
    }

    /// The anti-drift pin for the pair of tables that decide which reductions
    /// are emitted.
    ///
    /// `scalar_fold_opcode` and `accumulator_identity` are consulted together by
    /// `reduction_enc`, and the whole reason they are consulted together is that
    /// a row in one without a row in the other is **not** a refusal unless
    /// something makes it one. A `VecOp::And => Some(0x21)` added to the fold
    /// table alone would give every `&` reduction a `VPXOR`-zeroed accumulator
    /// and the answer `0` for every loop: silent wrong code, invisible to every
    /// other test in this module, all of which reduce with `+`.
    ///
    /// So this asserts the two tables agree *as tables*, over the whole
    /// `VecOp` vocabulary, rather than over the handful of operators the other
    /// tests happen to exercise. It fails the moment one side gains a row the
    /// other does not — which is the edit that would otherwise ship the bug.
    #[test]
    fn the_two_reduction_tables_admit_exactly_the_same_operators() {
        for op in [
            VecOp::Add,
            VecOp::Sub,
            VecOp::Mul,
            VecOp::Div,
            VecOp::Rem,
            VecOp::And,
            VecOp::Or,
            VecOp::Xor,
            VecOp::Min,
            VecOp::Max,
            VecOp::Neg,
        ] {
            assert_eq!(
                scalar_fold_opcode(op).is_some(),
                accumulator_identity(op).is_some(),
                "{op:?}: one reduction table answers and the other does not. A \
                 fold with no identity starts the accumulator at the wrong \
                 value; an identity with no fold cannot finish. Add both rows \
                 or neither."
            );
        }
        // And the set itself, so that widening it is a deliberate edit here
        // rather than a side effect somewhere else.
        for op in [VecOp::Add, VecOp::Or, VecOp::Xor, VecOp::And] {
            assert!(accumulator_identity(op).is_some(), "{op:?} is emitted");
        }
        for op in [
            VecOp::Sub,
            VecOp::Mul,
            VecOp::Div,
            VecOp::Rem,
            VecOp::Min,
            VecOp::Max,
            VecOp::Neg,
        ] {
            assert!(accumulator_identity(op).is_none(), "{op:?} is not emitted");
        }
    }

    /// `&` at both widths: the all-ones init, the `VPAND` horizontal tree, and
    /// the scalar `and` fold.
    ///
    /// Round 10 wave 9. Every byte re-derived from the encoding rules:
    ///
    /// * `VPCMPEQD` is `VEX.66.0F 76 /r` — the same map, `pp` and `W` as the
    ///   `VPXOR` init this replaces, so the prefix bytes are *identical* to the
    ///   `int` sum test's `C4 41 3D … C0` and only the opcode differs
    ///   (`76` against `EF`). That identity is the point: the init site is one
    ///   `vec_rr` with a table-chosen opcode, not two code paths.
    /// * `VPAND` is `VEX.66.0F DB /r`, already in both arms of `arith_opcode`.
    /// * `and r/m32, r32` is `21 /r`, so `and r8d, eax` is `41 21 C0` where the
    ///   sum test has `41 01 C0`; the `REX.W` form `and r8, rax` is `49 21 C0`.
    /// * The `int` arm still re-extends with `MOVSXD` and the `long` arm still
    ///   must not — that is a property of the fold width, not of the operator.
    #[test]
    fn an_and_reduction_starts_at_all_ones_and_folds_with_and() {
        // ---- int ----------------------------------------------------------
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = reduction_shape();
        shape.reduction = Some(VecReduction {
            op: VecOp::And,
            acc: R8,
        });
        let code = emit(&plan, &shape).expect("an `&` reduction now emits");

        assert_eq!(
            &code.code[..5],
            &[0xC4, 0x41, 0x3D, 0x76, 0xC0],
            "vpcmpeqd ymm8, ymm8, ymm8 — all-ones, `&`'s identity, and NOT the \
             vpxor (opcode EF) that would make this loop answer 0"
        );
        assert!(
            !contains(&code.code[..5], &[0xEF]),
            "the zeroing init must not be what an `&` reduction starts from"
        );

        let tail = &code.code[code.code.len() - 46..];
        #[rustfmt::skip]
        let expected_tail: Vec<u8> = vec![
            0xC4, 0x43, 0x7D, 0x39, 0xC1, 0x01, // vextracti128 xmm9, ymm8, 1
            0xC4, 0x41, 0x39, 0xDB, 0xC1,       // vpand xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E, // vpshufd xmm9, xmm8, 0x4E
            0xC4, 0x41, 0x39, 0xDB, 0xC1,       // vpand xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0xB1, // vpshufd xmm9, xmm8, 0xB1
            0xC4, 0x41, 0x39, 0xDB, 0xC1,       // vpand xmm8, xmm8, xmm9
            0xC5, 0x79, 0x7E, 0xC0,             // vmovd eax, xmm8
            0x41, 0x21, 0xC0,                   // and  r8d, eax
            0x4D, 0x63, 0xC0,                   // movsxd r8, r8d
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(tail, expected_tail.as_slice());

        // ---- long ---------------------------------------------------------
        let plan = plan_for(MemKind::Long, 4, Vec::new());
        let mut shape = reduction_shape();
        shape.reduction = Some(VecReduction {
            op: VecOp::And,
            acc: R8,
        });
        let code = emit(&plan, &shape).expect("a long `&` reduction emits");
        assert_eq!(
            &code.code[..5],
            &[0xC4, 0x41, 0x3D, 0x76, 0xC0],
            "all-ones dwords are all-ones qwords: one init row serves both widths"
        );
        let tail = &code.code[code.code.len() - 33..];
        #[rustfmt::skip]
        let expected_tail: Vec<u8> = vec![
            0xC4, 0x43, 0x7D, 0x39, 0xC1, 0x01, // vextracti128 xmm9, ymm8, 1
            0xC4, 0x41, 0x39, 0xDB, 0xC1,       // vpand xmm8, xmm8, xmm9
            0xC4, 0x41, 0x79, 0x70, 0xC8, 0x4E, // vpshufd xmm9, xmm8, 0x4E
            0xC4, 0x41, 0x39, 0xDB, 0xC1,       // vpand xmm8, xmm8, xmm9
            0xC4, 0x61, 0xF9, 0x7E, 0xC0,       // vmovq rax, xmm8
            0x49, 0x21, 0xC0,                   // and   r8, rax   (REX.W)
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(tail, expected_tail.as_slice());
        assert!(
            !contains(&code.code, &[0x4D, 0x63, 0xC0]),
            "a 64-bit `and` writes the whole register; re-extending would corrupt it"
        );
        assert!(
            !contains(&code.code, &[0x70, 0xC8, 0xB1]),
            "the second dword shuffle is not a horizontal step at 64-bit lanes"
        );
    }

    /// The zero-vector-pass case for `&`, which is the one the identity exists
    /// for and the one a wrong init would silently break.
    ///
    /// With all-ones in the accumulator and no vector pass, the epilogue folds
    /// all-ones into the caller's scalar accumulator with `and` — a no-op on the
    /// low 32 bits — and `MOVSXD` restores the sign, exactly as for `+`, because
    /// the incoming accumulator was a sign-extended `int`. A `VPXOR`-zeroed
    /// accumulator would instead `and` a zero in and destroy a scalar result
    /// that was already correct, on a loop that ran no vector code at all.
    ///
    /// The shape of that claim is structural — the init is emitted before the
    /// head test, so it is on the zero-pass path — and this asserts the two
    /// facts that make it so: the init is the first instruction, and the
    /// head-test branch that can skip every pass is emitted after it.
    #[test]
    fn the_and_identity_is_in_place_before_the_head_test_can_skip_every_pass() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let mut shape = reduction_shape();
        shape.reduction = Some(VecReduction {
            op: VecOp::And,
            acc: R8,
        });
        let code = emit(&plan, &shape).expect("emits");
        // The init is first; the `jg epilogue` that skips the body follows it.
        assert_eq!(&code.code[..5], &[0xC4, 0x41, 0x3D, 0x76, 0xC0]);
        let jg = code
            .code
            .windows(2)
            .position(|w| w == [0x0F, CC_GREATER])
            .expect("the head test's exit branch");
        assert!(
            jg >= 5,
            "the accumulator identity must be materialised before the branch \
             that can skip every vector pass"
        );
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

    /// Sub-word **arithmetic** is refused. Unchanged since the module was
    /// written, and `elementwise_shape` is what makes this the arithmetic case:
    /// its body's third step is a `Binary`.
    ///
    /// The refusal must fire *before* the width check, which is what this
    /// fixture happens to prove: the plan's `width_bytes` is 32 because
    /// `plan_for` computed it for `Int`, while `lanes * elem_bytes(Byte)` is 8,
    /// so a reordering would answer `UnsupportedWidth { 32, 8 }` — true, and
    /// silent about the element type that is the actual reason.
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
        // An `Accumulate` step is the other half of the condition: a sub-word
        // reduction is refused for the element type, not for the reduction.
        for elem in [MemKind::Byte, MemKind::Char, MemKind::Short] {
            let mut plan = plan_for(MemKind::Int, 8, Vec::new());
            plan.elem = elem;
            let shape = reduction_shape();
            assert_eq!(
                emit(&plan, &shape),
                Err(VecEmitRefusal::SubwordElement { elem }),
                "{elem:?}: an accumulate is a computation"
            );
        }
        // The backstop, independent of the entry point: there is no lane-wise
        // sub-word opcode to reach even if the body scan above is ever widened.
        // Unconditional in the ISA claim, because no feature level adds one that
        // would be correct for JVM semantics.
        for elem in [MemKind::Byte, MemKind::Char, MemKind::Short] {
            for op in [VecOp::Add, VecOp::Sub, VecOp::And, VecOp::Or, VecOp::Xor] {
                for feature in [false, true] {
                    assert_eq!(
                        arith_opcode(elem, op, feature),
                        Err(VecEmitRefusal::SubwordElement { elem }),
                        "{elem:?} {op:?} (sse41={feature}) has no lane encoding here"
                    );
                }
            }
        }
    }

    /// A sub-word **copy** emits. Round 10 wave 8.
    ///
    /// `b[i] = a[i]` over `byte[]` has no arithmetic node at all, so the reason
    /// sub-word arithmetic is refused — JVM `+ - * & | ^` on `byte`/`char`/
    /// `short` is computed in `int` on extended operands and narrowed at the
    /// store — does not apply to it. The lanes are moved bit for bit.
    ///
    /// What makes it cost no new encoder, each part checked against the code
    /// rather than assumed:
    ///
    /// * `VMOVDQU` is element-agnostic. `move_opcodes` returns the same
    ///   `(pp = 2, 0x6F, 0x7F)` row for `Int`, `Long` and now the three sub-word
    ///   types, because a 16- or 32-byte integer move does not know its lane
    ///   width. This test asserts that identity directly, so the claim is pinned
    ///   rather than restated in a comment.
    /// * `scale_log2` already answered `Some(0)` for one byte and `Some(1)` for
    ///   two; the SIB scale needed nothing.
    /// * 32 lanes of `byte` is 32 bytes, a whole YMM, so `emitter_encodes_width`
    ///   is satisfied without a new width.
    #[test]
    fn a_subword_copy_emits_because_a_copy_has_no_arithmetic() {
        // The row identity, first: this is the fact the whole case rests on.
        for elem in [MemKind::Byte, MemKind::Char, MemKind::Short] {
            assert_eq!(
                move_opcodes(elem),
                move_opcodes(MemKind::Int),
                "{elem:?} moves with the same VMOVDQU row as int"
            );
        }

        // `byte[]`: 32 lanes, SIB scale 0.
        let plan = plan_for(MemKind::Byte, 32, Vec::new());
        assert_eq!(plan.width_bytes, 32, "32 byte lanes is a whole YMM");
        let shape = copy_shape();
        let code = emit(&plan, &shape).expect("a sub-word copy emits");

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // loop_head:
            0x48, 0x8D, 0x43, 0x20,             // lea  rax, [rbx + 32]
            0x48, 0x3B, 0xC6,                   // cmp  rax, rsi
            0x0F, 0x8F, 0x15, 0x00, 0x00, 0x00, // jg   epilogue (+21)
            // scale 0, so the SIB is `00 011 001` = 0x19 and `00 011 111` = 0x1F,
            // where the scale-4 int fixtures have 0x99 and 0x9F.
            0xC5, 0x7E, 0x6F, 0x44, 0x19, HDR8, // vmovdqu ymm8, [rcx + rbx*1 + ARRAY_DATA_OFFSET]
            0xC5, 0x7E, 0x7F, 0x44, 0x1F, HDR8, // vmovdqu [rdi + rbx*1 + ARRAY_DATA_OFFSET], ymm8
            0x48, 0x83, 0xC3, 0x20,             // add  rbx, 32
            0xE9, 0xDE, 0xFF, 0xFF, 0xFF,       // jmp  loop_head (-34)
            // epilogue:
            0xC5, 0xF8, 0x77,                   // vzeroupper
        ];
        assert_eq!(code.code, expected);
        assert_eq!(code.lanes, 32);
        assert_eq!(code.width_bytes, 32);
        assert_eq!(code.max_remainder_iterations, 31);
        assert_eq!(code.clobbered_vector_regs, vec![VEC_POOL[0]]);
        assert_eq!(code.clobbered_gprs, vec![RAX]);

        // `char[]`/`short[]`: 16 lanes, SIB scale 1 (0x59 / 0x5F). Both are the
        // same two-byte element; the copy does not care which, because nothing
        // is sign- or zero-extended when the bits are only moved.
        for elem in [MemKind::Char, MemKind::Short] {
            let plan = plan_for(elem, 16, Vec::new());
            assert_eq!(plan.width_bytes, 32, "16 two-byte lanes is a whole YMM");
            let code = emit(&plan, &shape).expect("a two-byte copy emits");
            assert!(
                contains(&code.code, &[0xC5, 0x7E, 0x6F, 0x44, 0x59, HDR8]),
                "{elem:?}: vmovdqu ymm8, [rcx + rbx*2 + ARRAY_DATA_OFFSET]"
            );
            assert!(
                contains(&code.code, &[0xC5, 0x7E, 0x7F, 0x44, 0x5F, HDR8]),
                "{elem:?}: vmovdqu [rdi + rbx*2 + ARRAY_DATA_OFFSET], ymm8"
            );
            assert_eq!(code.lanes, 16);
        }

        // The 128-bit half of the same case, because a dependence can cap the
        // lane count: 16 `byte` lanes is a whole XMM and no VZEROUPPER is owed.
        let narrow = plan_for(MemKind::Byte, 16, Vec::new());
        assert_eq!(narrow.width_bytes, 16);
        let code = emit(&narrow, &shape).expect("a 128-bit sub-word copy emits");
        assert!(
            contains(&code.code, &[0xC5, 0x7A, 0x6F, 0x44, 0x19, HDR8]),
            "L=0 spells the two-byte VEX as C5 7A, not C5 7E"
        );
        assert!(!contains(&code.code, &[0xC5, 0xF8, 0x77]));
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

    /// `VPMINSD` / `VPMAXSD`, the two rows the gate's own refusal string has
    /// been advertising.
    ///
    /// Round 10 wave 8. `admit_vectorization` pushes
    /// `MissingIsaFeature("32-bit integer multiply / min / max (PMULLD, PMINSD,
    /// PMAXSD — SSE4.1)")` only when the target *lacks* the feature — true about
    /// the CPU, since SSE4.1 brings all three together — so on every target
    /// `VectorIsa::detect` can offer (AVX2 only, since wave 5) the gate cleared
    /// `Math.max` over `int[]` and this module refused it with `UnsupportedOp`.
    /// A reader who trusted the string concluded this tree could vectorize it.
    /// Now it can.
    ///
    /// The bytes, derived rather than captured: both are in the `0F 38` map with
    /// the `66` prefix, so `map = 2` and `pp = 1` — the same tuple shape as the
    /// `VPMULLD` row above them, differing only in the opcode byte. `VEX.66.0F38
    /// 39 /r` is `VPMINSD` and `3D /r` is `VPMAXSD`; the map nibble is what turns
    /// the `int` add's `C4 41 3D` prefix into `C4 42 3D`.
    ///
    /// **Signed, and the neighbours are the trap.** `38`/`3C` are the byte forms
    /// (`PMINSB`/`PMAXSB`) and `3B`/`3F` are the *unsigned* dword forms
    /// (`PMINUD`/`PMAXUD`). `Math.min`/`Math.max` on a JVM `int` is signed, so
    /// `39`/`3D` are the only two correct opcodes in that block of eight.
    #[test]
    fn int_min_and_max_encode_the_signed_sse41_dword_forms() {
        // The table, asserted through the field values rather than the bytes, so
        // a wrong map or prefix is a named failure.
        assert_eq!(
            arith_opcode(MemKind::Int, VecOp::Min, true),
            Ok(VexOpcode {
                map: 2,
                pp: 1,
                w: false,
                op: 0x39
            }),
            "VPMINSD is VEX.66.0F38 39 /r"
        );
        assert_eq!(
            arith_opcode(MemKind::Int, VecOp::Max, true),
            Ok(VexOpcode {
                map: 2,
                pp: 1,
                w: false,
                op: 0x3D
            }),
            "VPMAXSD is VEX.66.0F38 3D /r"
        );

        // Both are SSE4.1, arriving with PMULLD, so both are gated on the same
        // claim the gate's refusal string names.
        for op in [VecOp::Min, VecOp::Max] {
            assert_eq!(
                arith_opcode(MemKind::Int, op, false),
                Err(VecEmitRefusal::MissingIsaFeature {
                    elem: MemKind::Int,
                    op
                }),
                "{op:?} needs the SSE4.1 claim, exactly as Mul does"
            );
        }

        // And through the entry point, as emitted bytes.
        for (op, opcode) in [(VecOp::Min, 0x39u8), (VecOp::Max, 0x3D)] {
            let plan = plan_for(MemKind::Int, 8, Vec::new());
            assert!(
                plan.isa.int32_mul_minmax,
                "avx2() claims SSE4.1's dword ops"
            );
            let mut shape = elementwise_shape();
            shape.body[2] = VecStep::Binary {
                dst: 2,
                op,
                lhs: 0,
                rhs: 1,
            };
            let code = emit(&plan, &shape).expect("int min/max now emits");
            assert!(
                contains(&code.code, &[0xC4, 0x42, 0x3D, opcode, 0xC1]),
                "{op:?}: vpminsd/vpmaxsd ymm8, ymm8, ymm9 — three-byte VEX \
                 because the 0F38 map cannot use the C5 form"
            );
        }
    }

    /// 64-bit lane min/max stays refused, and this refusal is the *only* thing
    /// saying no.
    ///
    /// `VPMINSQ`/`VPMAXSQ` are AVX-512. The gate cannot express that: its
    /// `MissingIsaFeature` check is guarded by `elem_bytes(a.elem) == 4`, so a
    /// `long` `Math.max` is admitted on every target it models. That is fine —
    /// `UnsupportedOp { Long, Max }` explains itself exactly, which is the test
    /// the five-classes page sets for leaving a refusal at the emitter.
    #[test]
    fn long_min_and_max_are_avx512_and_stay_refused() {
        for op in [VecOp::Min, VecOp::Max] {
            assert_eq!(
                arith_opcode(MemKind::Long, op, true),
                Err(VecEmitRefusal::UnsupportedOp {
                    elem: MemKind::Long,
                    op
                }),
                "{op:?} on 64-bit lanes is AVX-512, at any SSE/AVX feature level"
            );
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

    /// The array data area begins at `ARRAY_DATA_OFFSET`, so every element access
    /// encodes that as its displacement byte. Naming it keeps these fixtures
    /// from having to be re-derived by hand each time the header moves — as
    /// they did on 2026-08-06 when it went 32 -> 24.
    const HDR8: u8 = cratonvm_types::ARRAY_DATA_OFFSET as u8;

    #[test]
    fn double_lanes_use_the_66_prefixed_forms() {
        let plan = plan_for(MemKind::Double, 4, Vec::new());
        let shape = elementwise_shape();
        let code = emit(&plan, &shape).expect("double element-wise emits");
        // VMOVUPD ymm8, [rcx + rbx*8 + ARRAY_DATA_OFFSET] is
        // C5 7D 10 44 D9 <HDR8> (pp = 66, VEX.R = 0). The displacement is
        // spelled through `HDR8`, never as a literal: this comment used to say
        // "+32 … 20", which was already two header shrinks out of date
        // (32 -> 24 on 2026-08-06, then 24 -> 16) while the assertion below
        // stayed right.
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
        // VMOVUPS ymm8, [rcx + rbx*4 + ARRAY_DATA_OFFSET] is
        // C5 7C 10 44 99 <HDR8> (pp = 00, VEX.R = 0). Same stale-literal note
        // as `double_lanes_use_the_66_prefixed_forms`.
        assert!(
            contains(&code.code, &[0xC5, 0x7C, 0x10, 0x44, 0x99, HDR8]),
            "vmovups"
        );
        // VADDPS ymm8, ymm8, ymm9 is C4 41 3C 58 C1.
        assert!(contains(&code.code, &[0xC4, 0x41, 0x3C, 0x58, 0xC1]));
    }

    // ── the width ───────────────────────────────────────────────────────

    /// A plan may be NARROWER than its own register (a dependence caps the
    /// lane count) but never wider.
    ///
    /// The exact edit that trips it: delete the `plan.width_bytes >
    /// plan.isa.width_bytes` term from the width check. Before round 10 this
    /// combination emitted: a plan claiming a 128-bit target got 256-bit
    /// VEX bytes, because `l` is computed from `plan.width_bytes` alone and
    /// nothing compared the two widths.
    ///
    /// What the callee does, so the expected payload follows from the
    /// contract: `emit_vector_loop` reports `UnsupportedWidth` with the plan's
    /// *own* `width_bytes` and `lanes` echoed back, not with the ISA's width —
    /// so the expected value is `{ width_bytes: 32, lanes: 8 }`, the numbers
    /// this fixture set, and not `16`.
    #[test]
    fn a_plan_wider_than_the_isa_it_was_decided_against_is_refused() {
        let mut plan = plan_for(MemKind::Int, 8, Vec::new());
        assert_eq!(plan.width_bytes, 32, "the fixture is a 256-bit plan");
        // SSE4.1 is 128-bit and `UnalignedOk`, so this reaches the width check
        // rather than being caught by the strict-alignment arm above it.
        plan.isa = VectorIsa::sse41();
        assert_eq!(plan.isa.width_bytes, 16);
        let shape = elementwise_shape();
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::UnsupportedWidth {
                width_bytes: 32,
                lanes: 8
            })
        );

        // The narrower-than-the-register direction stays admissible: that is
        // what a dependence-capped lane count looks like, and it is the case
        // `a_narrower_lane_count_emits_the_128_bit_form_and_no_vzeroupper`
        // already emits.
        let narrow = plan_for(MemKind::Int, 4, Vec::new());
        assert_eq!(narrow.width_bytes, 16);
        assert_eq!(narrow.isa.width_bytes, 32);
        assert!(emit(&narrow, &shape).is_ok());
    }

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

    /// The gate's floor and this module's width table are the same number.
    ///
    /// This is the pairing that stops the round-10 defect family from coming
    /// back: `simd_analysis`'s `VectorIsa::min_width_bytes` is what
    /// `admit_vectorization` refuses below, and `emitter_encodes_width` is what
    /// this module refuses outside. If a target ever declares a floor this
    /// module cannot encode, the gate will admit plans that die here — which is
    /// exactly
    /// `r10-vecplan-dependence-capped-widths-are-admitted-but-never-emittable-20260921-RETIRED-20260922.md`
    /// and, one level up, the `VectorIsa::detect` page wave 5 closed.
    ///
    /// What the callees return, so the assertions follow from the contracts:
    /// `emitter_encodes_width` is `matches!(width_bytes, 16 | 32)`, so it is
    /// `true` for exactly those two values; each `VectorIsa` constructor is a
    /// `const fn` whose `min_width_bytes` is 16 (the three x86 ones inherit it
    /// through `..VectorIsa::sse2()`).
    ///
    /// The loop is over the x86 targets only, because "this module encodes it"
    /// is a claim about the x86 emitter. `neon128`'s floor is asserted
    /// separately, against the file that would have to encode it.
    #[test]
    fn the_narrowest_width_each_isa_declares_is_one_this_emitter_encodes() {
        for isa in [
            VectorIsa::sse2(),
            VectorIsa::sse41(),
            VectorIsa::avx2(),
            VectorIsa::strict_align128(),
        ] {
            assert!(
                emitter_encodes_width(isa.min_width_bytes),
                "{}: the gate refuses below {} bytes, but this emitter has no \
                 encoding for {} either — the gate would admit plans that die \
                 in the width check",
                isa.name,
                isa.min_width_bytes,
                isa.min_width_bytes
            );
            assert!(
                isa.min_width_bytes <= isa.width_bytes,
                "{}: a floor above the register would refuse every plan",
                isa.name
            );
        }

        // The table, stated positively and negatively: 8 is the value a
        // dependence-capped `int` plan lands on, and it is the one this module
        // must keep answering `false` for until `VMOVQ` exists here.
        assert!(emitter_encodes_width(16));
        assert!(emitter_encodes_width(32));
        assert!(!emitter_encodes_width(8));
        assert!(!emitter_encodes_width(24));
        assert!(!emitter_encodes_width(64));

        // NEON's floor is about `jit/src/aarch64.rs`, which carries only the
        // `Q=1` `.4S` forms (`ld1_4s`, `st1_4s`, `add_v4s`, `mul_v4s`). Read,
        // not executed — nothing in this crate can run an AArch64 vector.
        assert_eq!(VectorIsa::neon128().min_width_bytes, 16);
    }

    /// The dependence-capped 8-byte plan: refused by the gate now, and still
    /// refused here.
    ///
    /// Two lanes of `int` is the shape a backward dependence at distance 2 or 3
    /// produces. Since round 10 wave 6 `admit_vectorization` never builds it —
    /// it answers `VecRefusal::WidthBelowIsaMinimum` instead — so this end is
    /// now reachable only from a hand-assembled plan. It must still refuse,
    /// because this module validates the plan it is handed rather than the plan
    /// it assumes was built.
    ///
    /// What the callee returns: `emit_vector_loop` echoes the *plan's* own
    /// numbers into `UnsupportedWidth`, not the ISA's, so the expected payload
    /// is `{ width_bytes: 8, lanes: 2 }` — the values `plan_for(Int, 2, …)`
    /// sets — and not `{ 16, 4 }`. The checks above the width one do not fire
    /// first: `avx2()` is `UnalignedOk` (so the strict-alignment arm is
    /// skipped) and `Int` is neither `Ref` nor sub-word.
    #[test]
    fn the_dependence_capped_width_is_refused_at_both_ends() {
        let plan = plan_for(MemKind::Int, 2, Vec::new());
        assert_eq!(plan.width_bytes, 8, "2 int lanes is the capped shape");
        assert_eq!(plan.isa.min_width_bytes, 16, "…below the target's floor");
        assert!(!emitter_encodes_width(plan.width_bytes));
        let shape = elementwise_shape();
        assert_eq!(
            emit(&plan, &shape),
            Err(VecEmitRefusal::UnsupportedWidth {
                width_bytes: 8,
                lanes: 2
            })
        );

        // Must emit: the first width at or above the floor. Four `int` lanes is
        // 16 bytes, the VEX.128 path, which is the width a dependence distance
        // of 4 or more leaves.
        let at_the_floor = plan_for(MemKind::Int, 4, Vec::new());
        assert_eq!(at_the_floor.width_bytes, at_the_floor.isa.min_width_bytes);
        assert!(emit(&at_the_floor, &shape).is_ok());
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

    /// `alu_ri`'s **32-bit** immediate form, which nothing asserted the bytes
    /// of before round 10.
    ///
    /// Every guard the existing tests emit has a limit inside `i8`, so only
    /// the `83 /ext ib` arm was ever pinned; the `81 /ext id` arm — the one a
    /// real `AtMost` limit takes, since a loop bound past 127 is the normal
    /// case — was emitted by no test at all. An `81` form with the operand
    /// size or the ModRM extension wrong compares against the wrong value and
    /// takes the fallback edge (or, worse, does not) with nothing to say so.
    ///
    /// Re-derived from the encoding rules rather than transcribed:
    /// `cmp_ri(r, imm)` is `alu_ri(r, 7, imm)`, which emits `rex_w(false,
    /// false, r >= 8)` = `0x48` for RCX, then — `i8::try_from(1000)` having
    /// failed and `i32::try_from(1000)` succeeded — opcode `0x81`, ModRM
    /// `0xC0 | (7 << 3) | (1 & 7)` = `0xF9`, then `1000i32.to_le_bytes()` =
    /// `E8 03 00 00`. `PreheaderGuard::AtMost` fails when `term > limit`, so
    /// the branch is `JG` = `0F 8F`, and its `rel32` stays zero because a
    /// fallback site is the CALLER's to patch.
    #[test]
    fn a_guard_limit_past_the_imm8_range_uses_the_imm32_form() {
        // The boundary, on the assembler directly: 127 fits `i8`, 128 does not.
        let mut asm = Asm::new();
        asm.cmp_ri(RCX, 127).expect("imm8");
        assert_eq!(asm.out, vec![0x48, 0x83, 0xF9, 0x7F], "cmp rcx, 127");

        let mut asm = Asm::new();
        asm.cmp_ri(RCX, 128).expect("imm32");
        assert_eq!(
            asm.out,
            vec![0x48, 0x81, 0xF9, 0x80, 0x00, 0x00, 0x00],
            "cmp rcx, 128 — one past the imm8 range, not truncated to 0x80"
        );

        // …and end to end, through the guard the shape actually produces.
        let plan = plan_for(
            MemKind::Int,
            8,
            vec![array_guard(PreheaderGuard::AtMost {
                term: SymBound::constant(0),
                limit: 1000,
            })],
        );
        let shape = elementwise_shape();
        let bindings = [VecGuardValues::Term { term: RCX }];
        let code = emit_vector_loop(&request(&plan, &shape, &bindings)).expect("emits");
        assert_eq!(
            &code.code[..13],
            &[
                0x48, 0x81, 0xF9, 0xE8, 0x03, 0x00, 0x00, // cmp rcx, 1000
                0x0F, 0x8F, 0x00, 0x00, 0x00, 0x00, // jg <fallback, unpatched>
            ]
        );
        // The site is the offset of the `rel32` field, i.e. just past `0F 8F`.
        assert_eq!(code.fallback_sites, vec![9]);
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

    /// A pool that names one register twice refuses the whole region.
    ///
    /// This is the only pool defect that produced **wrong code** rather than a
    /// refusal. `VecRegPool` keys `in_use` by slot index, so `[xmm8, xmm9,
    /// xmm8]` hands xmm8 out at slot 0 and again at slot 2; two
    /// simultaneously-live vector values would then share one register and the
    /// second load would destroy the first. `free` makes it worse rather than
    /// better: it resolves a register back with `position`, i.e. to the
    /// *first* matching slot, so returning slot 2 releases slot 0.
    ///
    /// Why this assertion and not another: the validation loop walks the pool
    /// in order and returns on the first fault it finds, so the register it
    /// names is the REPEAT (index 2), not the original (index 0) — and both
    /// spellings are `8`, so the test also fixes the ordering by putting a
    /// clean register between them.
    ///
    /// The exact edit that trips it: delete the `get(..i)` duplicate test at
    /// the top of `emit_vector_loop`.
    #[test]
    fn a_pool_naming_the_same_register_twice_refuses_the_region() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let mut req = request(&plan, &shape, &[]);
        // Every entry is individually fine: 8 and 9 are both in `VEC_POOL` and
        // both in `frame_saved_xmms` (`request` passes the whole pool), so the
        // encodability test cannot be what refuses this.
        let repeated: [u8; 3] = [8, 9, 8];
        for reg in repeated {
            assert!(
                crate::regalloc::xmm_roles::vector_pool_is_encodable(reg, &VEC_POOL),
                "xmm{reg} is individually encodable, so only the duplicate can refuse"
            );
        }
        req.vector_pool = &repeated;
        assert_eq!(
            emit_vector_loop(&req),
            Err(VecEmitRefusal::UnusableVectorPool { reg: 8 })
        );
    }

    /// A pool longer than the region refuses rather than being truncated.
    ///
    /// `VecRegPool::alloc` scans `.take(VEC_POOL_LEN)` and its occupancy array
    /// is `[bool; VEC_POOL_LEN]`, so entries past the cap would be silently
    /// unusable — the quiet narrowing `UnusableVectorPool`'s doc forbids.
    ///
    /// Honest note on what this pins: today the check is over-determined.
    /// `VEC_POOL_LEN == VECTOR_REGION_MAX.len() == 8` (asserted by
    /// `the_three_xmm_authorities_are_disjoint`) and every legal entry must be
    /// one of those eight, so a ninth entry is necessarily either a duplicate
    /// or outside the region and the per-register loop would refuse it too.
    /// The length test is kept because it is the only one that stays correct
    /// if `VECTOR_REGION_MAX` is ever widened without `VEC_POOL_LEN` — the
    /// drift that would make `alloc` truncate and `in_use` under-size at once.
    #[test]
    fn a_pool_longer_than_the_region_refuses_rather_than_being_truncated() {
        let plan = plan_for(MemKind::Int, 8, Vec::new());
        let shape = elementwise_shape();
        let mut req = request(&plan, &shape, &[]);
        let mut oversized: Vec<u8> = VEC_POOL.to_vec();
        assert_eq!(oversized.len(), VEC_POOL_LEN);
        oversized.push(VEC_POOL[0]);
        req.vector_pool = oversized.as_slice();
        // The length check runs first and names the entry past the cap, which
        // here is `VEC_POOL[0]`.
        assert_eq!(
            emit_vector_loop(&req),
            Err(VecEmitRefusal::UnusableVectorPool { reg: VEC_POOL[0] })
        );
        // The unextended pool is accepted, so length is what refused above and
        // not the contents.
        req.vector_pool = &VEC_POOL;
        assert!(emit_vector_loop(&req).is_ok());
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
        assert_eq!(element_disp(0, 4), Ok(ARRAY_DATA_OFFSET as i64));
        assert_eq!(element_disp(3, 4), Ok(ARRAY_DATA_OFFSET as i64 + 12));
        assert_eq!(element_disp(-8, 4), Ok(ARRAY_DATA_OFFSET as i64 - 32));
        // The arithmetic is done in 64 bits, so a subscript that overflows a
        // 32-bit displacement is still computed exactly here and refused later
        // by `Disp::encode_for_base` — never silently narrowed.
        assert_eq!(
            element_disp(i32::MAX, 8),
            Ok(i32::MAX as i64 * 8 + ARRAY_DATA_OFFSET as i64)
        );
        assert!(Disp::encode_for_base(i32::MAX as i64 * 8 + ARRAY_DATA_OFFSET as i64, 1).is_err());
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
        // promotes it. ARRAY_DATA_OFFSET is non-zero so this only bites for a negative
        // subscript, but the emitter must route through the checked helper.
        let d = Disp::encode_for_base(0, 5).expect("zero encodes");
        assert_eq!(d, Disp::Disp8(0));
        assert_eq!(d.mod_bits(), 0b01);
    }
}
