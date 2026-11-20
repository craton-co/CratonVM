// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Graph-coloring register allocator for JVM bytecode locals.
//!
//! Performs backward dataflow liveness analysis on the bytecode CFG,
//! builds an interference graph, and uses Chaitin-Briggs graph coloring
//! to assign callee-saved registers to locals optimally.
//!
//! ## 64-local cap
//!
//! Liveness sets, gen/kill sets, and the interference graph are all
//! represented as `u64` bitsets, one bit per local. This imposes a hard
//! ceiling of **64 locals**: only locals with index `< 64` participate in
//! allocation. Methods with more than 64 locals are not rejected — locals
//! at index `>= 64` are simply never assigned a register and fall back to
//! the unallocated (frame-slot / spill) path, which is always correct.
//!
//! This cap is a deliberate simplification: methods with >64 locals are
//! rare and the bitset representation keeps liveness analysis fast for the
//! common case. Raising it would require switching every `u64` bitset (and
//! the `interference: Vec<u64>` graph) to a wider/dynamic bitset type.

use super::x64::{LOCAL_REGS, LOCAL_XMMS};
use crate::bytecode_analysis;
// Used only by the IR-level linear-scan allocator at the bottom of this file.
use crate::bailout::{Bailout, BailoutReason, CompileResult};
use crate::ir::{Graph, IrType, NodeId, Op, NO_NODE};
use crate::ir_schedule::Schedule;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap};

// j27 wave 2 (lane RESIDENCY): the loop half of the spill cost
// (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`, opt-in).
mod loop_residency;
pub(crate) use loop_residency::{loop_phi_residency_enabled, ranking_weight};

mod value_flow;
pub(crate) use value_flow::{GraphValues, ValueFlow, ValueGroup, ValueSource};

// j27 wave 4 (lane REMAT): `x + c` recomputed at a split reload
// (`CRATONVM_JIT_IR_LS_REMAT_ADD`, opt-in; consumer `ir_lower::ls_remat_add`).
mod remat_add;
pub use remat_add::{remat_add_of, RematAdd};
/// Run register allocation for a method (x86-64), without the exception
/// table. Test-only (round 11 wave 4): production goes through
/// [`allocate_registers_with_handlers`].
#[cfg(test)]
pub fn allocate_registers(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_with(
        code,
        code_len,
        num_locals,
        num_params,
        &[],
        loops,
        local_gpr_pool(),
        &LOCAL_XMMS,
        &[],
    )
}

/// Compute, for every bytecode PC, the set of locals live-IN at that
/// instruction (bit `i` set ⇒ local `i` may still be read before its next
/// definition). Unlike [`RegAllocResult::block_live_in`] (block-boundary
/// granularity only), this walks each basic block backward at *instruction*
/// granularity — the same per-instruction gen/kill walk [`build_interference`]
/// already does, just kept instead of discarded after building interference
/// edges — so a caller can ask "is local `i` still live at this exact bci"
/// rather than only "at this block's start".
///
/// Written for the OSR-exit deopt snapshot (`x64.rs`'s
/// `build_and_record_deopt_point`): a local whose machine location can't be
/// decoded at a snapshot bci (`FrameValue::Unsupported`) used to force the
/// WHOLE snapshot to reject, even when that local is provably dead there
/// (e.g. a loop induction variable read for the last time inside the loop,
/// snapshotted at a trap several instructions after the loop exits). A
/// rejected OSR-exit snapshot falls back to "continue interpreting the
/// pre-OSR-entry frame", which re-runs every loop iteration the OSR-compiled
/// code already executed — silently duplicating side effects (see
/// `testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`).
/// Knowing a local is dead at the snapshot bci lets the caller substitute a
/// safe placeholder instead of rejecting outright.
///
/// Same 64-local cap as the rest of this module (bit `i` only meaningful for
/// `i < 64`); a caller must treat locals `>= 64` as conservatively live (as
/// they already do for `block_live_in`).
///
/// Test-only (round 11 wave 5): the deopt snapshot now reads
/// [`live_locals_per_pc_all`], which models handlers and every 64-slot window.
#[cfg(test)]
pub fn live_locals_per_pc(code: &[u8], code_len: usize, num_params: usize) -> Vec<u64> {
    live_locals_per_pc_with_coverage(code, code_len, num_params).0
}

/// [`live_locals_per_pc_with_coverage`] with EXCEPTION EDGES modelled.
///
/// `handlers` is `(start_pc, end_pc, handler_pc)` per exception-table entry.
/// Every instruction inside `[start_pc, end_pc)` can transfer control to
/// `handler_pc`, so each block overlapping that range gains the handler's
/// block as a successor and the handler's `live_in` flows back into the
/// protected code.
///
/// Without this, a local that ONLY the handler reads is computed dead at every
/// pc in the protected range — and that is precisely where the precise
/// exceptional-frame snapshot is taken, so the reconstructed handler frame
/// dropped the value it was about to read. With a reference local that also
/// unroots a live object: the collector reclaims it, the address is recycled,
/// and an unrelated object shows up in its place (observed as json-smart
/// re-parses returning a key `String`, and as
/// `ClassCastException: java.lang.Object cannot be cast to JSONArray`).
///
/// Test-only (round 11 wave 5): production reads window 0 of
/// [`live_locals_per_pc_all`], which is bit-for-bit this answer.
#[cfg(test)]
pub fn live_locals_per_pc_with_handlers(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    param_slots: &[usize],
    handlers: &[(usize, usize, usize)],
) -> (Vec<u64>, Vec<bool>) {
    live_locals_per_pc_inner(code, code_len, num_params, param_slots, handlers)
}

/// [`live_locals_per_pc`] plus the parallel *coverage* bitmap: `covered[pc]` is
/// `true` exactly when `pc` is an instruction start inside some basic block, so
/// `live_at[pc]` is a computed answer rather than the `0` default.
///
/// The distinction matters for any consumer that treats "nothing live" as
/// permission to drop state: an *uncovered* pc (code no CFG path reaches —
/// genuinely dead bytecode, or the interior of a mis-decoded region) also reads
/// as `0`, and acting on that would silently discard live values. Every such
/// consumer must fall back to its conservative answer instead.
#[cfg(test)]
fn live_locals_per_pc_with_coverage(
    code: &[u8],
    code_len: usize,
    num_params: usize,
) -> (Vec<u64>, Vec<bool>) {
    live_locals_per_pc_inner(code, code_len, num_params, &[], &[])
}

/// Run register allocation for a method (ARM64) with the identity argument
/// layout. Test-only (round 11 wave 4): the ARM64 backend calls
/// [`allocate_registers_arm64_with_param_slots`].
#[cfg(test)]
pub fn allocate_registers_arm64(
    code: &[u8],
    code_len: usize,
    num_locals: usize,
    num_params: usize,
    loops: &[(usize, usize)],
) -> RegAllocResult {
    allocate_registers_arm64_with_param_slots(code, code_len, num_locals, num_params, &[], loops)
}

/// Who owns which XMM register, in one place.
///
/// Three components hand out XMM registers, and until this module existed each
/// held its own private answer: `ir_lower`'s FP value tier (XMM0/XMM1),
/// `ir_lower`'s linear-scan file, and the vector pool (reserved for a future
/// vector emitter; `x64/vec_emit.rs`, its first consumer, was deleted in round
/// 12 wave 3 with no production caller, so nothing hands it out today). The three
/// **overlap**, and the failure that overlap produces is silent — a scalar
/// `double` living in XMM3 destroyed by a vector region that took XMM3 for a
/// lane accumulator, with nothing to notice but a wrong number much later.
///
/// The overlap is **gone as of 2026-08-04**: `ir_lower::emit_prologue` now
/// carries a callee-saved XMM save area ([`IR_PROLOGUE_SAVED`]), which bought
/// the two registers that let the scalar file grow and moved the vector pool
/// entirely out of the scalar range. What this module keeps is the property
/// that made the fix checkable — the ranges are declared *together*, so a
/// wiring that puts two authorities on one register is a visible fact rather
/// than a discovery, and [`disjointness_violation`] is the enforced half.
///
/// `docs/jit/vectorization-emitter.md` named this the single highest-risk
/// prerequisite for wiring `emit_vector_loop` (deleted, unwired, in round 12
/// wave 3), and
/// `docs/feature-designs/jit-machine-level-and-instruction-selection.md`
/// increment 4 is the item that closed it.
pub mod xmm_roles {
    /// `ir_lower`'s FP value tier: the scratch pair every float/double
    /// arithmetic arm computes in — XMM0 the first operand and the result,
    /// XMM1 the second.
    pub const IR_FP_SCRATCH: [u8; 2] = [0, 1];

    /// `ir_lower`'s linear-scan file — the registers a *long-lived* FP value
    /// may be promoted into for the whole method.
    ///
    /// Disjoint from [`IR_FP_SCRATCH`] by construction, which is why residency
    /// and the arithmetic arms can coexist.
    ///
    /// XMM6/XMM7 are here because [`IR_PROLOGUE_SAVED`] pays for them.
    ///
    /// # Why the ceiling is XMM7, and what it is NOT (R14)
    ///
    /// It **was** an encoder limit: `ir_lower`'s `fp_load` / `fp_store` /
    /// `fp_binop` emitted ModRM with `(xmm & 7) << 3` and no REX byte, so XMM8
    /// encoded as XMM0 and the pool could not be widened without producing
    /// silently wrong machine code. That is fixed as of 2026-09-17 — those
    /// three emit REX, and so does `ir_lower::emit_xmm_frame_move`, the fourth
    /// one (it takes its register from [`IR_PROLOGUE_SAVED`], so it would have
    /// been the one to break first).
    /// `every_xmm_encoder_reaches_xmm8_through_rex` pins all four.
    ///
    /// What the ceiling is now is **ownership**, which is a policy decision and
    /// not a defect. XMM8–XMM15 belongs to [`VECTOR_REGION_MAX`]; growing this
    /// array into that range trips [`disjointness_violation`], and the right
    /// answer to that is to decide which authority owns those eight registers,
    /// not to widen both and route around the check. Two things fall out of any
    /// such decision and neither is discretionary:
    ///
    ///   1. every register added here must be added to [`IR_PROLOGUE_SAVED`] on
    ///      **Windows**, which makes XMM6–XMM15 non-volatile — otherwise the
    ///      first use corrupts the caller's floating-point state on one
    ///      platform and not the other;
    ///   2. `ir_lower`'s frame pays 16 bytes per saved register
    ///      (`ir_saved_xmm_bytes`), on every IR-compiled method, used or not.
    ///
    /// `docs/jit/vectorization-emitter.md` is where the ownership question
    /// belongs.
    pub const IR_LINEAR_SCAN: [u8; 6] = [2, 3, 4, 5, 6, 7];

    /// The XMM registers `ir_lower::emit_prologue` saves and every exit
    /// restores — empty on System V, where the ABI makes every XMM volatile
    /// and there is nothing to save.
    ///
    /// This is the list that makes the two ranges above and below able to
    /// coexist with a *caller*. Win64 makes XMM6–XMM15 non-volatile; without a
    /// save area, any use of one silently corrupts the caller's floating-point
    /// state on one platform and not the other — the worst possible shape for
    /// a bug, and the reason `IR_LINEAR_SCAN` stopped at XMM5 until now.
    ///
    /// Only XMM6/XMM7 appear, not XMM6–XMM15: they are what `IR_LINEAR_SCAN`
    /// hands out, and a frame does not pay 16 bytes to save a register nothing
    /// can name. (Until 2026-09-17 the reason was stronger — the emitter could
    /// not *encode* anything above XMM7 — and that half is now false; see the
    /// R14 note on [`IR_LINEAR_SCAN`].) [`VECTOR_REGION_MAX`] is XMM8–XMM15 and
    /// owes its own save area — see [`vector_pool_is_encodable`].
    ///
    /// `ir_lower::emit_xmm_frame_move` is the emitter that consumes this list,
    /// and it encodes REX, so a wider list would be *emitted* correctly. What
    /// it would not be is free: 16 frame bytes per entry, on every method.
    #[cfg(windows)]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[6, 7];
    /// System V: every XMM is caller-saved, so the prologue saves none.
    #[cfg(not(windows))]
    pub const IR_PROLOGUE_SAVED: &[u8] = &[];

    /// VEC21-7 (the r23 L8 close, L8-2): the save list of a method whose
    /// optimizing body carries a vector pre-loop
    /// (`ir_vector_loop::plan_vector_loops` admitted a loop). On Windows
    /// XMM8-XMM11 are non-volatile, so [`vector_pool_is_encodable`] gives the
    /// pre-loop none of them unless the frame saves them; with them an FP
    /// min/max step (four registers) and a multi-compare scan fit. The method
    /// pays 64 more frame bytes and four `MOVUPS` pairs; every other method
    /// keeps [`IR_PROLOGUE_SAVED`]. System V saves nothing either way.
    #[cfg(windows)]
    pub const IR_PROLOGUE_SAVED_VECTOR: &[u8] = &[6, 7, 8, 9, 10, 11];
    /// System V: as [`IR_PROLOGUE_SAVED`].
    #[cfg(not(windows))]
    pub const IR_PROLOGUE_SAVED_VECTOR: &[u8] = &[];

    /// The widest pool a vector region may be given.
    ///
    /// **Disjoint from [`IR_FP_SCRATCH`] and [`IR_LINEAR_SCAN`]**, which is the
    /// whole point: a vector region can no longer destroy a scalar `double` a
    /// caller left live, so a vector emitter's caller owes no
    /// prove-the-scalars-are-dead argument. It owes a different one, stated by
    /// [`vector_pool_is_encodable`]: on Windows every register here is
    /// callee-saved, so a frame that hands one out must save it.
    ///
    /// XMM8–XMM15 rather than the low half because a vector emitter encodes
    /// with VEX (the deleted `x64/vec_emit.rs` did),
    /// which carries the high bit for free. `IR_LINEAR_SCAN`'s own encoder used
    /// to have no such luxury; since R14 it does (REX rather than VEX), so this
    /// range is now held by **ownership** — the two authorities must be
    /// disjoint and this one claimed it first — rather than by the scalar
    /// emitter being unable to name it.
    pub const VECTOR_REGION_MAX: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

    /// May a vector region take `reg`, given the set of XMM registers the
    /// calling frame's prologue saves?
    ///
    /// Two conditions, and the second is the one that used to be prose:
    ///
    ///   1. `reg` is in [`VECTOR_REGION_MAX`] — no other register is the
    ///      vector pool's to give;
    ///   2. it is caller-saved on this target, **or** `frame_saved` says the
    ///      caller's prologue saved it. On System V (1) implies (2); on
    ///      Windows every register in the pool is non-volatile, so a caller
    ///      that saves nothing gets an empty pool and a vector emitter
    ///      must refuse (the deleted `emit_vector_loop` did) — which is the
    ///      correct answer, not a missed optimization.
    pub fn vector_pool_is_encodable(reg: u8, frame_saved: &[u8]) -> bool {
        if !VECTOR_REGION_MAX.contains(&reg) {
            return false;
        }
        #[cfg(windows)]
        {
            frame_saved.contains(&reg)
        }
        #[cfg(not(windows))]
        {
            let _ = frame_saved;
            true
        }
    }

    /// `ir_lower`'s **general-purpose** linear-scan file — the registers a
    /// long-lived `int`/`long` value may be promoted into for a whole method.
    ///
    /// RBX and R12–R15, and every part of that choice is forced:
    ///
    ///   * **Callee-saved on the target ABI.** `ir_lower` emits calls
    ///     constantly — runtime helpers, inline-cache dispatch, JIT-to-JIT
    ///     direct calls — and this wiring has no reload machinery, so a
    ///     value's register must survive a call by the calling convention
    ///     rather than by analysis. That rules out every caller-saved
    ///     register.
    ///
    ///     RSI/RDI are caller-saved on **System V** and callee-saved on
    ///     **Win64**, so they are in the file on Windows and out of it
    ///     elsewhere — the same platform split [`IR_PROLOGUE_SAVED`] already
    ///     makes for XMM6/XMM7, and for the same reason. This constant said
    ///     they were caller-saved unconditionally until 2026-09-10: a System V
    ///     fact stated as an ABI-independent one, and it cost the optimizing
    ///     tier two of the seven callee-saved registers the single-pass
    ///     backend has been colouring locals into all along (`x64::LOCAL_REGS`
    ///     is `[u8; 7]` on Windows and `[u8; 5]` elsewhere).
    ///   * **Untouched by this emitter.** The value tier is RAX/RCX/RDX, the
    ///     safepoint and shadow-stack scratch is R10/R11, and call arguments go
    ///     in `ENTRY_ABI_REGS`. None of those overlaps this set.
    ///   * RBP and RSP are excluded for the obvious reason. R13 is *included*:
    ///     it is only awkward as an addressing BASE (no `mod=00` form), and
    ///     nothing here uses one of these as a base.
    ///
    /// The frame stays authoritative — this is a write-through read cache,
    /// exactly like [`IR_LINEAR_SCAN`] — so no oop map, deopt frame state or
    /// phi copy changes. What does change is that the prologue must save these
    /// and every exit restore them: see [`IR_GP_PROLOGUE_SAVED`].
    ///
    /// **`IrType::Ref` is never promoted**, and that is not a tuning choice. A
    /// GC safepoint walks a frame it did not stop, through RBP, and
    /// `OopMapEntry` can only name frame slots — so no reference may be
    /// register-resident at one. The XMM file discharged that obligation
    /// structurally, by having no register a `Ref` could occupy; a GP file has
    /// to discharge it by refusing the type, which `plan_register_residency`
    /// does and which its own test pins.
    /// Win64: RBX, R12–R15 **and RSI/RDI**, which this ABI makes
    /// callee-saved. The two extra registers are appended rather than
    /// interleaved so that a method whose peak live set fits in five is
    /// allocated exactly as it was before they existed.
    #[cfg(windows)]
    pub const IR_GP_LINEAR_SCAN: [u8; 7] = [3, 12, 13, 14, 15, 6, 7];
    /// System V: RSI/RDI are argument registers and caller-saved, so the file
    /// is RBX and R12–R15 alone.
    #[cfg(not(windows))]
    pub const IR_GP_LINEAR_SCAN: [u8; 5] = [3, 12, 13, 14, 15];

    /// The first [`IR_GP_LINEAR_SCAN`] entries that were the file before the
    /// Win64 widening — what `CRATONVM_JIT_IR_GP_WIDE=0` restores. Five on
    /// every platform, which on System V is the whole file.
    pub const IR_GP_LINEAR_SCAN_NARROW: usize = 5;

    /// JIT round 25 (item 1b, `CRATONVM_JIT_IR_LS_CALLER_SAVED`, default ON since j27): the
    /// CALLER-saved GPRs `ir_lower` may hand to a value whose live range lies
    /// inside one block and crosses only nodes whose lowering writes none of
    /// them (`ir_lower::ir_op_caller_saved_transparent`). R8-R11 on both ABIs.
    ///
    /// * Never saved by the prologue: they are not in [`IR_GP_LINEAR_SCAN`],
    ///   so `IR_GP_PROLOGUE_SAVED`, the save bands and `ir_gp_file()` are
    ///   unchanged, and a value that uses one costs no save and no restore.
    /// * Not RAX/RCX/RDX (the value tier, `IDIV`, `LCmp`/`FCmp`'s `SETcc DL`),
    ///   not RSI/RDI (callee-saved and in the wide file on Win64; argument
    ///   registers on System V, whose byte forms also need a REX prefix this
    ///   emitter's byte paths were never audited for).
    /// * R8/R9 are argument and deopt-trampoline registers and R10/R11 the
    ///   poll / shadow-stack / epoch-guard scratch, but every one of those
    ///   uses is inside a CALL, a poll, a guard pad or a field / allocation
    ///   sequence -- none of which a transparent node emits, and every one of
    ///   which ends a caller-saved value's residency (`Lowerer::
    ///   invalidate_caller_saved_residency`). The poison stress mode's
    ///   `PUSH R11 ... POP R11` preserves R11.
    ///
    /// Never named by a deopt frame, a pad write-back or a mode exit: every
    /// such site filters on `IR_GP_LINEAR_SCAN`, and the home word stays
    /// written (`Lowerer::install_caller_saved_residency`).
    pub const IR_GP_LINEAR_SCAN_CALLER_SAVED: [u8; 4] = [8, 9, 10, 11];

    /// The GP registers `ir_lower::emit_prologue` saves and every exit
    /// restores — all of [`IR_GP_LINEAR_SCAN`], because every one of them is
    /// callee-saved on the target ABI and that is exactly why they were
    /// chosen.
    ///
    /// Platform-conditional only through [`IR_GP_LINEAR_SCAN`]: System V and
    /// Win64 agree about RBX and R12–R15 and disagree about RSI/RDI.
    /// `Lowerer::saved_gpr_regs` emits a save only for a register the
    /// residency plan actually handed out, so a widened file that goes unused
    /// costs frame bytes and no instructions.
    pub const IR_GP_PROLOGUE_SAVED: &[u8] = &IR_GP_LINEAR_SCAN;

    /// The first register claimed by two of the three authorities, if any.
    ///
    /// The invariant this module exists to make checkable, as a value rather
    /// than a paragraph. `None` is the healthy answer and
    /// `the_three_xmm_authorities_are_disjoint` pins it.
    pub fn disjointness_violation() -> Option<u8> {
        for reg in 0u8..16 {
            let claims = u8::from(IR_FP_SCRATCH.contains(&reg))
                + u8::from(IR_LINEAR_SCAN.contains(&reg))
                + u8::from(VECTOR_REGION_MAX.contains(&reg));
            if claims > 1 {
                return Some(reg);
            }
        }
        None
    }
}

#[cfg(test)]
impl LsLegacyRun {
    /// Everything but the visit count: what the two fixed points must agree on.
    pub(crate) fn answer(&self) -> (bool, bool, &[u64], &[u64]) {
        (self.ran, self.converged, &self.live_in, &self.live_out)
    }
}

/// Choose what loses its register when nothing is free.
///
/// Maximises `next_use_distance × SPILL_DISTANCE_SCALE / frequency_weight`, so
/// a value whose next use is far away and whose uses are cold goes first, and a
/// value used on the next instruction inside a nested loop goes last.
/// Rematerializable values score [`SPILL_SCORE_REMAT`] and always go first:
/// evicting one costs no store at all.
///
/// `only_reg`, when set, restricts the candidates to the active holding that
/// register — the caller passes the register `current` is REQUIRED to take,
/// because evicting anything else cannot make room for it. With no such
/// holder (the register is blocked rather than busy) the answer is
/// [`Victim::Current`].
///
/// Test-only since j27 wave 2: the scan calls [`ls_pick_victim_with`] with the
/// switch it read once; this is the switch-off form the fixtures pin.
#[cfg(test)]
fn ls_pick_victim(
    graph: &Graph,
    live: &LiveModel,
    active: &[Active],
    class: RegClass,
    current: Pending,
    only_reg: Option<PhysReg>,
) -> Victim {
    ls_pick_victim_with(graph, live, active, class, current, only_reg, false)
}

// ── Metrics ──────────────────────────────────────────────────────────
//
// # R5: `record_allocation_metrics` was deleted here, and that was the decision
//
// It read:
//
// ```ignore
// pub fn record_allocation_metrics(recorder: &CompileRecorder, alloc: &Allocation) {
//     recorder.set_peak_live_values(alloc.peak_live);
//     recorder.set_spills(alloc.spills);
//     recorder.set_reloads(alloc.reloads);
// }
// ```
//
// and it had **no caller anywhere in the workspace** — not in production, not
// in this module's tests, despite its own doc comment claiming the latter. What
// it was waiting for arrived from the other direction: `ir_lower` now calls
// `metrics::note_current_spills` / `note_current_reloads` with its own
// `ls_spills` / `ls_reloads` under `ls_active`, so `CompilationReport::spills`
// and `::reloads` are measured on the linear-scan path and left `NotMeasured`
// on the colourer-only path — which is the correct report, not a missing one.
//
// The two producers do not measure the same thing:
//
// * this one reported what the ALLOCATION **planned** — `Allocation::spills` /
//   `::reloads`, the whole split/spill/remat schedule;
// * the ambient form reports what the backend **emitted**, which is the smaller
//   number, because `plan_register_residency` executes only the subset of the
//   plan it can prove.
//
// Two different numbers pointed at one `Measured<u32>` field is a report whose
// meaning depends on which producer ran last, and only one of the two describes
// machine code that exists. `NOTES-misc.md` §2 offered the alternative — keep
// it and give `CompilationReport` a second pair of fields, `planned_spills` /
// `planned_reloads`, so the gap between plan and emission is the thing being
// measured. That is a `metrics.rs` change and `metrics.rs` is not this lane's
// file; re-adding this function without it would re-create the collision.
//
// So: deleted, with the gap recorded at `Allocation::spills`.
//
// # Where that stands now (2026-09-17)
//
// **The `metrics.rs` half landed** (`NOTES-opts7.md` §2):
// `CompilationReport::planned_spills` / `::planned_reloads` exist, with
// `CompileRecorder::set_planned_spills` / `set_planned_reloads` and their own
// JSON keys, and `::spills` / `::reloads` are documented as the EMITTED figures
// and only those. So the collision above no longer forbids a producer — a
// re-added function writing the PLANNED pair would be correct.
//
// It would also still have no caller, which is why it has not been re-added.
// The only holder of an `Allocation` is `ir_lower::plan_register_residency`,
// and it holds no `CompileRecorder`; that is the open question, not which
// setter to call. `CompilationReport::planned_spills` states it, with the three
// candidate shapes. The stale prose mentions this section used to list are
// gone from `metrics.rs`.

#[cfg(test)]
pub(crate) mod linear_scan_tests;

// ── REVIEW-NOTE ──────────────────────────────────────────────────────
//
// Changes this pass could not make in place, recorded here rather than made
// silently. Nothing below is applied. Each is either a statement about a file
// outside the two this pass was allowed to touch, or a deliberate deviation
// found inside them and left alone on purpose.
//
// REVIEW-NOTE 1 — `docs/JIT_OPTIMIZATION.md` is stale in two load-bearing
// places, and one of them is the paragraph this pass was pointed at.
//
//   (a) The closing paragraph of "Register residency in the optimizing tier"
//       reads:
//
//         > **Known limit, named rather than guessed at:** a loop counter and
//         > a loop accumulator are `Op::Phi` at the loop header, and phis are
//         > excluded — the allocator refuses them […] Reaching loop-carried
//         > values therefore needs the allocator to admit phis and
//         > `emit_phi_copies` to publish
//
//       Phis have been admitted since 2026-09-02. `ir_op_defines_value` lists
//       `Op::Phi`, `LiveModel::release_phi_pins` drops the structural pin,
//       `ir_lower::plan_register_residency` admits them behind
//       `ir_phi_residency_enabled` (default ON *within* the outer flag), and
//       `emit_phi_copies` publishes at every incoming edge. Replace that
//       paragraph with the state of the code:
//
//         **Loop-carried values reach the file.** A loop counter and a loop
//         accumulator are `Op::Phi` at the loop header, and phis are admitted
//         (`CRATONVM_JIT_IR_PHI_RESIDENCY`, default ON within the outer flag).
//         The publish site a phi never had is `emit_phi_copies`: it writes the
//         home word at every incoming edge exactly as before and then moves
//         the value into the phi's register, so the write-through contract is
//         unchanged and the register is a cache. The allocator's structural
//         pin is dropped by `release_phi_pins` on the same condition. A
//         `Ref`-typed phi is still refused, by type — `OopMapEntry` names
//         frame slots only, and a phi's publish site sits outside the per-node
//         invalidation loop that `CRATONVM_JIT_IR_REF_RESIDENCY` rests on.
//         `[ir-ls] phis=N: resident=… disabled=… ref_type=… no_register=…
//         no_home=… wrong_bank=… demoted=…` is the per-cause census, and its
//         `disabled=` field is what the old `phi=` field measured.
//
//   (b) The same section says `CRATONVM_JIT_IR_LINEAR_SCAN` is "default off"
//       and that "the flip is a separate decision that still wants a
//       measurement on a quiet host". `ir_lower::linear_scan_enabled` has been
//       default ON since 2026-09-02 and its own doc comment says so. "Off is
//       exactly the pre-change emission" is still true and should stay;
//       "default off" should become "default ON since 2026-09-02; `=0` is the
//       kill switch". This matters because the section is what a reader
//       consults before deciding what a measurement is measuring, and it
//       currently states the opposite default.
//
// REVIEW-NOTE 2 — the write-through contract has a deliberate exception, and
// this pass did not introduce it and did not remove it.
//
//   The contract as the doc states it — "the home word is written at every
//   definition, so `emit_safepoint_map`, `build_deopt_points` and
//   `emit_phi_copies` read exactly what they always did" — is no longer
//   unconditional in `ir_lower`. `Lowerer::phi_home_droppable` (which requires
//   `CRATONVM_JIT_IR_DROP_PHI_HOME`, `ir-deopt-regs`, `ir-phi-copy-regs` and
//   `ir-skip-republish` together) lets `emit_copy_op` SKIP a resident phi's
//   home store, and `value_home_droppable` does the same for ordinary values.
//
//   Left alone, deliberately. It is confined to `Int`/`Long`, so no oop map
//   entry and no `Ref` root is affected; the one reader that used to trust the
//   home unconditionally is guarded (`ir_phi_home_publish_guard_enabled` in
//   `emit_phi_copies`, added after `Select.processGroupResult` published a phi
//   from a word nothing wrote and collapsed H2 window-query result sets); and
//   that guard has a source-scanning test pinning its reader set. Restoring
//   the literal contract would delete a measured optimisation and its safety
//   net in one move.
//
//   What is worth doing is a documentation change, not a code one: the
//   sentence quoted above should read "the home word is written at every
//   definition **unless `ir-drop-phi-home` / `ir-drop-home` prove no reader
//   needs it** — an `Int`/`Long`-only rule that never applies to a `Ref`."
//
// REVIEW-NOTE 3 — `plan_register_residency` and the skip census live in
// `jit/src/ir_lower.rs`, not here.
//
//   The brief this pass worked from names `regalloc::plan_register_residency`,
//   `regalloc::wants_loc` and "the skip census" as this module's. `wants_loc`
//   is this module's (a `LiveModel` field built by `build_live_model`);
//   `plan_register_residency` and both censuses are `ir_lower`'s, and that is
//   the right home for them — the census describes what *that* emitter could
//   take, and this module must not know which registers that emitter reserves
//   for its own tiers. Recorded so the next reader of that brief does not go
//   looking here.

#[cfg(test)]
mod tests;

#[cfg(test)]
mod handler_liveness_tests;

/// `assigned_locals_per_pc_all` (interpreter round i1 wave 19, lane L1).
#[cfg(test)]
mod assigned_locals_tests;

mod local_gpr_pool;
#[allow(unused_imports)]
pub use local_gpr_pool::*;
#[allow(unused_imports)]
use local_gpr_pool::*;
mod build_live_model_legacy_impl;
#[allow(unused_imports)]
pub use build_live_model_legacy_impl::*;
#[allow(unused_imports)]
use build_live_model_legacy_impl::*;
mod verify_allocation;
#[allow(unused_imports)]
pub use verify_allocation::*;
#[allow(unused_imports)]
use verify_allocation::*;
