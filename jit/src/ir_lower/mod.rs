// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Lower the scheduled IR graph to x86-64 machine code.
//!
//! Walks the scheduled basic blocks, emits native instructions for each
//! IR node, and patches forward branches.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::num::NonZeroU32;

use super::ir::{
    CmpOp, Graph, IrType, MemKind, Node, NodeId, Op, SafepointSnapshot, ScalarOp, NO_NODE,
};
use super::ir_schedule::Schedule;
use super::{CompiledMethod, ExecutableBuffer, JitInvokeInfo, JitRuntimeHelpers};
use crate::bailout::{
    record_bailout, Bailout, BailoutReason, CompileResult, DEFAULT_MAX_FRAME_BYTES,
    DEFAULT_MAX_NODES,
};
use crate::deopt::{
    ir_deopt_entry, DeoptAction, DeoptReason, DeoptVerifier, DeoptimizationPoint, EliminatedValue,
    EliminationCause, FrameState, FrameValue, MethodFrameLimits, OopCoverage, ResumeSemantics,
    VirtualObjectState,
};
use crate::regalloc::{resolve_parallel_copy, CopyOp, ValueLoc, ValueSource};
use cratonvm_types::narrow_oop::narrow_base;
use cratonvm_types::{
    narrow_oops_enabled, ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, HEADER_SIZE, SLOT_SIZE,
};

/// Round 15 wave 1 (lane sync, SS8-2): why a self-locking body's deopt frame
/// `fs` would NOT hand its method monitor to the resumed interpreter frame, or
/// `None`. The shape `deopt::handed_method_monitor_index` recognises (the VM
/// sinks make that hold the frame's `monitor_on_exit`, and every sink that
/// abandons the frame releases it): one scope, exactly one monitor, one taken
/// level (`relock == false`), its object and local 0 each located as a
/// reference -- the VM compares the two addresses. A frame that failed it
/// would leave the hold with nobody to release it (or release it twice).
///
/// Round 15 wave 3 (lane sync2): the instance form of
/// [`self_lock_frame_refusal_for`], which production asks since S15-3.
#[cfg(test)]
fn self_lock_frame_refusal(fs: &FrameState) -> Option<&'static str> {
    self_lock_frame_refusal_for(fs, true)
}


#[cfg(test)]
mod r15w1_sync_self_lock_lowering_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod prim_arraycopy_tests;
#[cfg(test)]
mod r15w4_sync3_self_lock_refusal_cause_tests;
#[cfg(all(test, target_arch = "x86_64"))]
mod r23w12_l6_byte_arraycopy_tests;
#[cfg(test)]
mod xthrow_ir_defer_bit_tests;

#[cfg(test)]
mod r23w11_l6_checked_ref_arraycopy_tests;
/// The union of `named[b]` over every bytecode pc `b` reachable from `start`
/// over normal edges, `start` included. Test-only since gen r5w5/oomjit9:
/// production asks [`snapshot_colours_reachable_with_splices`].
#[cfg(test)]
fn snapshot_colours_reachable_from(
    verified: &cratonvm_reader::VerifiedCode,
    named: &HashMap<usize, Vec<u64>>,
    words: usize,
    start: usize,
) -> Vec<u64> {
    snapshot_colours_reachable_with_splices(verified, named, &HashMap::new(), words, start)
}

#[cfg(test)]
mod dead_ref_slot_clear_tests;

/// JIT round 25 item 2 (`CRATONVM_JIT_IR_SHADOW_LIVE_ACROSS_CALL`): the
/// prologue words a map leaves out, and the stores that make leaving them out
/// sound.
#[cfg(test)]
mod r25_item2_shadow_live_tests;
/// JIT round 19 wave 6 (lane osr6, OSR5-4): the write-back pad's XMM save,
/// RUN. Every register a frame names must reach the deopt stub as the guard
/// left it, whatever the field helper did to the caller-saved XMM file.
#[cfg(all(test, target_arch = "x86_64"))]
mod r19_osr6_pad_xmm_save_tests;

/// JIT round 19 wave 4 (lane osr4, OSR2-1): the poll exit's cell write-back,
/// RUN. A deopt at a promoted loop's poll must observe every store the loop
/// made: the pad stores the cell's back-edge value through the field helper
/// when it differs from the value read on entry, and leaves the field alone
/// when it does not (never writing back over another thread's store a value
/// this thread only read).
#[cfg(all(test, target_arch = "x86_64"))]
mod r19_osr4_cell_writeback_tests;

/// JIT round 20 wave 11 (lane pLoop, OSR4-2): the poll pad's static-cell
/// write-back, RUN over a fake statics block: a changed cell gets the inline
/// `putstatic`'s two words, an unchanged one is left alone, and the block is
/// found through its base cell.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr20_ploop_static_cell_writeback_tests;

/// JIT round 21 (lane promote, TYPECHECK21-9; re-landed t1 wave 8): the
/// pending plan's cell reader names a STATIC cell (value slot 2, no base)
/// beside a field one (slot 4, its base), the pins read a static silent
/// store's value from slot 2, and (PROMOTE21-2) only an all-static pad skips
/// the XMM refusal.
#[cfg(test)]
mod t1_promote_static_pending_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod w6_gc_alloc_tests;

#[cfg(test)]
mod w6_ir_lower_tests;

/// Does lowering `op` write its result home EXACTLY ONCE, through `store_rax`,
/// with RAX holding the value at that store?
///
/// The list is an audit of `lower_data_node`'s arms, not a guess, and it is the
/// reason a dropped home is safe for a value that is not a phi. Two shapes are
/// deliberately absent:
///
/// * arms with more than one home write, or a home write on one path and not
///   another (`Op::Load`, `Op::CheckCast`, `Op::Call`) -- the register would be
///   published on one path and stale on the other;
/// * arms that reach the home by a route other than `store_rax`
///   (`Op::Const`'s immediate store, `Op::Param`'s `gp_store_value`, every FP
///   result's `fp_store_value`) -- nothing would publish the register at all.
///
/// `Op::Cmp`/`Op::LCmp`/`Op::FCmp` are absent for a third reason: a comparison
/// fused into its consuming `Op::If` emits NOTHING, so its home write is
/// conditional on a fusion decision made elsewhere. They are single-use by
/// construction and the residency file does not promote them, so the omission
/// costs nothing.
///
/// Getting this list wrong is not silent: `lower_data_node_tracked` refuses the
/// compile when a value whose home was dropped reaches the end of its own
/// lowering unpublished.
/// Let a SINGLE-USE intermediate reach its one consumer in a register instead
/// of through its home word -- **default ON** since 2026-09-05;
/// `CRATONVM_JIT_IR_CARRY_SINGLE_USE=0` is the kill switch.
///
/// The residency work removed the frame traffic it could and then measured what
/// was left: the loop's remaining `[rbp-...]` operations are pairs of the shape
///
/// ```text
/// mov [rbp-0C0h],rax      ; store the result of an Add
/// mov rax,[rbp-0C0h]      ; ...and read the same word straight back
/// ```
///
/// `plan_register_residency` skips every one of them (`single_use` in its
/// census) and is right to: they do not want a register for their whole live
/// range, they want not to be written to memory at all. Their range is one
/// instruction long.
///
/// So this is not a promotion policy. It is the same move `fused_cmp` already
/// makes for a comparison consumed by its branch, generalised: when a value has
/// exactly one use and its consumer is the very next node in the same block,
/// the value can stay in the register the arm computed it in and the consumer
/// can read it there.
///
/// **Two decisions, deliberately separate.** Eliding the LOAD asks nothing of
/// the frame — the home word is still written and every frame state still
/// resolves through it — so it applies to every carried value. Dropping the
/// STORE needs the stronger claim that nothing else can want the word, and
/// `graph.safepoints` records the FULL OPERAND STACK at every bci, so an
/// intermediate is named by a frame state from its definition until its
/// consumer pops it. On a loop kernel that answers no every time, which the
/// census reports as `still_deopt_named` — and it is worth reading beside
/// `ir_osr_sentinel_free`, because such a method emits no deopt stub at all.
/// Every one of those frame states is unreachable, and they still pin every
/// intermediate to memory.
#[cfg(test)]
thread_local! {
    /// Test-only override of [`ir_carry_single_use_enabled`], the same shape as `LS_FORCE` and for the
    /// same reason: a byte-comparison lane has to hold both of its arms equal.
    static CARRY_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test-only RAII override of [`ir_carry_single_use_enabled`] on this thread.
#[cfg(test)]
struct CarryForce;

#[cfg(test)]
impl CarryForce {
    fn off() -> CarryForce {
        CARRY_FORCE.with(|c| c.set(Some(false)));
        CarryForce
    }
}

#[cfg(test)]
impl Drop for CarryForce {
    fn drop(&mut self) {
        CARRY_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only, per-thread twin of [`IR_CARRY_DEFERRED`]. The process-wide
    /// census also counts every compile the OTHER test threads run, so a test
    /// that reads it as a before/after delta around its own `lower` measures
    /// whatever else the harness was lowering in that window (a `5 != 6`
    /// "planned vs read" that no single body produced). A lowering runs on its
    /// caller's thread, so this delta is that test's compiles alone.
    ///
    /// Words 0..4; words 4..10 are [`ir_elide_homes_census_this_thread`]'s,
    /// per thread for the same reason (one static for both: JIT r25 wave 8,
    /// W8-2).
    static IR_CENSUS_THIS_THREAD: std::cell::Cell<[u64; 10]> =
        const { std::cell::Cell::new([0; 10]) };
}

/// [`ir_carry_deferred_census`] for the compiles THIS thread ran (test only).
#[cfg(test)]
fn ir_carry_deferred_census_this_thread() -> (u64, u64, u64, u64) {
    let v = IR_CENSUS_THIS_THREAD.with(std::cell::Cell::get);
    (v[0], v[1], v[2], v[3])
}

#[cfg(test)]
thread_local! {
    /// Test-only override of `Lowerer::frame_block_enabled`, so a test can pin
    /// either publication protocol without touching the process environment.
    static FRAME_BLOCK_FORCE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only RAII override of `Lowerer::frame_block_enabled`.
#[cfg(test)]
struct FrameBlockForce;

#[cfg(test)]
impl FrameBlockForce {
    fn set(on: bool) -> FrameBlockForce {
        FRAME_BLOCK_FORCE.with(|c| c.set(Some(on)));
        FrameBlockForce
    }
}

#[cfg(test)]
impl Drop for FrameBlockForce {
    fn drop(&mut self) {
        FRAME_BLOCK_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override of [`ir_alu_imm_enabled`], the same shape as `LS_FORCE` and for the
    /// same reason: a byte-comparison lane has to hold both of its arms equal.
    static ALU_IMM_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test-only RAII override of [`ir_alu_imm_enabled`] on this thread.
#[cfg(test)]
struct AluImmForce;

#[cfg(test)]
impl AluImmForce {
    fn off() -> AluImmForce {
        ALU_IMM_FORCE.with(|c| c.set(Some(false)));
        AluImmForce
    }
}

#[cfg(test)]
impl Drop for AluImmForce {
    fn drop(&mut self) {
        ALU_IMM_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
mod jr20_memory_byte_array_phi_tests;
#[cfg(test)]
mod nio21w1_scoped_arena_table_tests;

#[cfg(test)]
mod nio21w5_scoped_screen_cse_tests;

#[cfg(test)]
mod nio20w3_scoped_prim_array_screen_tests;
#[cfg(test)]
mod t1_argfix_splice_local_liveness_tests;
#[cfg(test)]
mod service_arg_resume_only_slot_tests;
/// Does some frame this optimizing artifact can resume show a local a debugger
/// can read as `0`? (Interpreter round i1 wave 22, lane L2;
/// `i19-L1-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger`, the
/// optimizing tier.)
///
/// True when a point's frame (in `deopt_points` or the boxes: guard traps,
/// exception frames, poll exits) publishes a local `Undefined` although the
/// method's bytecode definitely assigns it at the point's bci
/// (`regalloc::assigned_locals_per_pc_all`, parameters included). Every resume
/// maps `Undefined` to `Value::Int(0)`, which is exact for the program (the
/// interpreter never reads a dead slot before writing it) but not for a JDWP
/// `StackFrame.GetValues`, which HotSpot answers with the value (it keeps
/// every local alive while an agent can access locals). The upper half of a
/// `long` / `double` (the slot after a two-slot value) is `Undefined` by
/// design and is not asked. Only the method's own scope is asked here: a
/// spliced callee's scope has no bytecode in `bytecode`. Round 13 wave 13
/// (lane jitfix): [`ir_artifact_hides_an_assigned_local_scoped`] asks every
/// scope of a caller chain too, given the combined buffer and the body table;
/// this is it with neither.
///
/// `lib.rs` asks it of every optimizing artifact compiled while a debugger may
/// read locals (`CompileRequest::debugger_observes_locals`) and discards one
/// that answers `true`, so the single-pass body, which keeps and describes
/// every assigned local under a debugger since wave 21, runs instead. The IR
/// builder names every local in every snapshot and the lowerer keeps the home
/// of every value a reachable snapshot names, so in practice this fires for a
/// snapshot slot an optimization pass cleared (`NO_NODE`) or a merge of two
/// kinds; `CRATONVM_DBG_JITC=1` (or `CRATONVM_DBG_IR_COMPILES=1`) names each
/// discard. Fails closed: a `code_len` past the buffer answers `true`.
#[cfg(test)]
pub(crate) fn ir_artifact_hides_an_assigned_local(
    cm: &CompiledMethod,
    bytecode: PollExitBytecode<'_>,
) -> bool {
    ir_artifact_hides_an_assigned_local_scoped(cm, bytecode, None)
}

/// Test-only, per-thread census of [`ir_elide_unnamed_homes_enabled`]'s
/// work, summed over every lowering ATTEMPT on this thread: `[tail carries
/// planned, home stores elided, tail carries refused, compares fused
/// unnamed, site-named resident homes, inspection spills]`, words 4..10 of
/// `IR_CENSUS_THIS_THREAD` (per thread for the reason its carry words are).
#[cfg(test)]
fn ir_elide_homes_census_this_thread() -> [u64; 6] {
    let v = IR_CENSUS_THIS_THREAD.with(std::cell::Cell::get);
    [v[4], v[5], v[6], v[7], v[8], v[9]]
}

/// Round 12 wave 2 (lane irback): the poisoning sequence, EXECUTED. It must
/// keep bits 0..31 exact, make bits 32..63 neither extension, and leave the
/// flags and every other register (R11 included) alone -- for each register
/// class `lea_reg_base_index_bytes` encodes differently (RAX, a SIB-base R12,
/// a disp8-base R13, a REX-less RBX/RSI/RDI and R15).
#[cfg(all(test, target_arch = "x86_64"))]
mod r12w2_irback_int_poison_tests;
/// The ops this allowlist deliberately does NOT claim, and why -- written down
/// because the mechanical test (`every_droppable_op_writes_its_home_once_
/// through_store_rax`) admits them, so a reader who runs the scan needs to know
/// why each one the array below names was left out. (`LambdaIntToDouble` is
/// listed but no longer a scan candidate at all: since round 9 its arm ends in
/// `emit_call_return_check`, not a `store_rax`.)
///
/// * `Op::Cmp`, `Op::LCmp`, `Op::FCmp` -- the home write is CONDITIONAL on a
///   fusion decision `lower_terminator` makes: a comparison feeding a branch
///   that fuses emits no arm at all. The source scan sees the unfused path and
///   counts one store; the fused path stores nothing.
/// * `Op::New`, `Op::NewArray`, `Op::ConstString`, `Op::ConstClass`,
///   `Op::LambdaIntToDouble` -- `IrType::Ref` results (`LambdaIntToDouble` is
///   `Double`). A reference's HOME word may never be dropped: the GC root walk
///   reads frame slots through RBP and `OopMapEntry` names slots only. The
///   opt-in reference residency (`ir_ref_residency_enabled`) caches a
///   reference in a register but keeps its home authoritative, which is why
///   `value_home_droppable` refuses the type either way.
/// * `Op::InstanceOf` -- an `Int` result, and mechanically eligible. Excluded
///   because it is a HELPER CALL with a sentinel-return path, which is the
///   shape `Op::Call` is excluded for; nobody has checked whether the sentinel
///   path leaves RAX holding the value.
/// * `Op::ConstF` -- FP, refused by `value_home_droppable` like every other FP
///   type, so claiming it would be inert rather than wrong.
///
/// Two of these are worth revisiting and one is not: the comparisons need the
/// fusion decision to be visible at the arm, which is a real change; the
/// reference results need the deopt register image to be able to name a `Ref`,
/// which is a GC contract change and not a lowering one.
#[cfg(test)]
const DELIBERATELY_NOT_DROPPABLE: [&str; 24] = [
    // Keep the joined result in the guard's home.
    "VthreadBoundaryTest",
    // nio21 wave 19 (lane L3, `ThreadLocal.get()` expansion): the current
    // thread's mirror is a `Ref`, and a reference home may never be dropped
    // (the `New` reason).
    "CurrentThreadMirror",
    // JIT round 20 wave 11 (lane pIndy, SPLICE19-1): a VM root word's object,
    // a `Ref` -- the `CurrentThreadMirror` reason.
    "IndyRootRef",
    // nio21 wave 13 (lane L1; the nio21 wave 10/11 L3 constant-word probe):
    // `IndyBridge` now ends its hit path in one `store_rax`, so the scan sees
    // it, and it is rejected on purpose. Its result is a `Ref` (the `New`
    // reason: a reference home may never be dropped), and the arm is a probe
    // joined to a CALL of `jit_indy_bridge` -- which, for an unlinked site,
    // runs a bootstrap (arbitrary Java, side effects, a sentinel exception
    // path): the `Call` / `InstanceOf` shape, never a droppable value.
    "IndyBridge",
    // Round nio21 wave 3 (lane L2, default OFF): the call-site exception
    // edge's flag is the OSR flag's twin -- read only by its `If`, which an
    // ARMED site does not even read -- and the caught throwable is a `Ref`,
    // whose home may never be dropped (the `New` reason).
    "CallThrew",
    // JIT round 19 wave 5 (lane handler5, SPLICE4-1, opt-in
    // `ir-compiled-handler-entry`): the flag shares `OsrEntryFlag`'s arm and
    // reason; the caught throwable is a `Ref`, whose home is never dropped,
    // and the probe writes the word it reads before the arm runs.
    "CatchEntryFlag",
    "CaughtException",
    // JIT round 16 (lane matrix, `CRATONVM_JIT_IR_MULTIANEWARRAY`, default ON): a
    // `Ref` result from a helper call, the same two reasons as `New`/`NewArray`
    // (a reference HOME may never be dropped) and `InstanceOf` (a helper call
    // with a null-result exception path).
    "MultiNewArray",
    // nio21 wave 15 (lane L3): a deferred `new`, `MultiNewArray`'s shape -- a
    // `Ref` result from a helper call with a null-result exception path.
    "NewCp",
    // Round 11 wave 14 (lane `osrmerge`, opt-in `osr-opaque-entry`). The OSR
    // entry flag is read once, by the `If` of the opaque entry merge, and it
    // is materialised on both the normal and the OSR path; claiming it buys
    // one store per loop entry and would make the entry stub's seeding depend
    // on a register the stub does not set up.
    "OsrEntryFlag",
    // Round 11 wave 13 (lane `cha`). One `store_rax`, but three branches reach
    // it (null, array, class) and the only consumer is a guard that reads the
    // home word; claiming it would buy one store on a once-per-call test.
    "ExactClassIs",
    // JIT round 19 (lane osr19, opt-in `ir-class-word-chain`). One `store_rax`,
    // reached by two paths (null / array, and the class word); the same
    // reasoning as `ExactClassIs`, and its readers are the chain's compares.
    "ClassWordOf",
    // JIT round 19 wave 3 (lane eager3, opt-in `uninit-new-guard`). One
    // `store_rax`, but its only reader is the `Op::Guard` of an uninitialized
    // `new`, once per arrival; claiming it buys one store on a cold-ish test.
    "ClassInitTest",
    // `Unbox` ends in exactly one `store_rax`, so it LOOKS claimable, and it is
    // rejected on purpose. Unlike every claimed op its arm is not straight-line:
    // it deopts twice (null, then the receiver class guard) and branches on the
    // per-object layout before reaching that store. Dropping the home would
    // leave the value live only in RAX across those edges, and reasoning about
    // what each deopt sees is precisely what this list exists to stop being done
    // casually. Claimable later WITH a measurement; not worth a wrong answer to
    // save one store.
    "Unbox",
    "Cmp",
    "LCmp",
    "FCmp",
    "New",
    "NewArray",
    "ConstString",
    "ConstClass",
    "LambdaIntToDouble",
    "InstanceOf",
    "ConstF",
];

#[cfg(test)]
thread_local! {
    /// Test-only override of [`ir_osr_entry_enabled`], the same shape as
    /// `LS_FORCE` and `DEOPT_REGS_FORCE` and for the same reason.
    static OSR_ENTRY_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    /// Test-only override of [`ir_osr_entry_always_enabled`].
    static OSR_ENTRY_ALWAYS_FORCE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test-only RAII override of [`ir_osr_entry_enabled`] on this thread.
#[cfg(test)]
struct OsrEntryForce;

#[cfg(test)]
impl OsrEntryForce {
    /// The stubs are default-ON since 2026-09-05, so the test that checks an
    /// artifact carries none has to ask for the kill switch.
    fn off() -> OsrEntryForce {
        OSR_ENTRY_FORCE.with(|c| c.set(Some(false)));
        OsrEntryForce
    }

    fn on() -> OsrEntryForce {
        OSR_ENTRY_FORCE.with(|c| c.set(Some(true)));
        OSR_ENTRY_ALWAYS_FORCE.with(|c| c.set(Some(true)));
        OsrEntryForce
    }

    /// Stubs on, but ONLY where the door could enter them — i.e. the
    /// production rule, with the `ALWAYS` escape hatch explicitly off.
    ///
    /// [`Self::on`] sets `ALWAYS` too, because the twenty-odd tests that
    /// predate the sentinel-free skip assert on entries in bodies that carry
    /// calls, and they are asserting on the STUB's shape rather than on the
    /// door's admission. A test about the skip itself wants this instead.
    fn on_but_not_always() -> OsrEntryForce {
        OSR_ENTRY_FORCE.with(|c| c.set(Some(true)));
        OSR_ENTRY_ALWAYS_FORCE.with(|c| c.set(Some(false)));
        OsrEntryForce
    }
}

#[cfg(test)]
impl Drop for OsrEntryForce {
    fn drop(&mut self) {
        OSR_ENTRY_FORCE.with(|c| c.set(None));
        OSR_ENTRY_ALWAYS_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override of [`ir_deopt_regs_enabled`], the same shape (and for
    /// the same reason) as `LS_FORCE`: the production answer latches a
    /// process-wide `OnceLock` off the environment, so a test that did not
    /// override it would measure the developer's shell.
    static DEOPT_REGS_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Test-only (JIT round 25 wave 3): pin the per-body claim's switches on a
/// built lowerer. `Lowerer::new` reads them from the environment, so a shape
/// assertion about the fresh-frame clear would otherwise depend on whether
/// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM` (or a wave-2/3 class) is set in the
/// test process. `prim` is item 3's claim; every wave-2/3 class is off.
#[cfg(test)]
fn pin_body_claim_arm(lo: &mut Lowerer<'_>, prim: bool) {
    lo.prim_colour_claim = prim;
    lo.pinned_prim_slot_offsets.clear();
    lo.deopt_image_claim = false;
    lo.helper_area_claim = false;
}

/// Test-only: the `[rbp - off]` offsets, ascending, a fresh-frame skip mask
/// leaves out (word `w` is `[rbp - 8(w + 1)]`).
#[cfg(test)]
fn skip_mask_offsets(skip: &[bool]) -> Vec<i32> {
    skip.iter()
        .enumerate()
        .filter(|(_, s)| **s)
        // Cast: a word index below 4096.
        .map(|(w, _)| (w as i32 + 1) * 8)
        .collect()
}

/// Test-only: what the clear leaves out with `_SKIP_UNROOTED`'s fixed words
/// `fixed`, `_SKIP_WRITTEN` off and the lowerer's per-body claim: `fixed`, plus
/// every claimed word but one alone between two cleared words (re-included,
/// as `fresh_frame_clear_skip_with` does; the frame's first and last words are
/// never re-included), as ascending offsets.
#[cfg(test)]
fn expected_unrooted_and_claim_skip(lo: &Lowerer<'_>, fixed: &[i32]) -> Vec<i32> {
    let in_frame = |o: i32| o > 0 && o % 8 == 0 && o <= lo.frame_size;
    let claim: Vec<i32> = lo
        .body_claim_offsets_with(lo.prim_colour_claim)
        .iter()
        .map(|&o| i32::from(o))
        .filter(|&o| in_frame(o))
        .collect();
    let all: std::collections::BTreeSet<i32> = fixed
        .iter()
        .copied()
        .filter(|&o| in_frame(o))
        .chain(claim.iter().copied())
        .collect();
    let mut out = all.clone();
    for &o in &claim {
        if fixed.contains(&o) || o == 8 || o == lo.frame_size {
            continue;
        }
        if !all.contains(&(o - 8)) && !all.contains(&(o + 8)) {
            out.remove(&o);
        }
    }
    out.into_iter().collect()
}

/// Test-only RAII override of [`ir_deopt_regs_enabled`] on this thread.
#[cfg(test)]
struct DeoptRegsForce;

#[cfg(test)]
impl DeoptRegsForce {
    /// The region is default-ON since 2026-09-05, so the test that checks the
    /// historical two-argument stub shape has to ask for it explicitly.
    fn off() -> DeoptRegsForce {
        DEOPT_REGS_FORCE.with(|c| c.set(Some(false)));
        DeoptRegsForce
    }

    fn on() -> DeoptRegsForce {
        DEOPT_REGS_FORCE.with(|c| c.set(Some(true)));
        DeoptRegsForce
    }
}

#[cfg(test)]
impl Drop for DeoptRegsForce {
    fn drop(&mut self) {
        DEOPT_REGS_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only override for the two machine-level flags, so a unit test never
    /// depends on the process environment. Thread-local, so parallel tests
    /// cannot see each other's setting.
    static MIR_FORCE: std::cell::Cell<Option<MirMode>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn mir_forced() -> Option<MirMode> {
    MIR_FORCE.with(|c| c.get())
}

/// Scoped override of the machine-level mode for one test.
#[cfg(test)]
struct MirForce;

#[cfg(test)]
impl MirForce {
    fn set(mode: MirMode) -> MirForce {
        MIR_FORCE.with(|c| c.set(Some(mode)));
        MirForce
    }
}

#[cfg(test)]
impl Drop for MirForce {
    fn drop(&mut self) {
        MIR_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
thread_local! {
    /// Test-only: make the level-2 encoder produce a deliberately wrong byte,
    /// so the byte-equality oracle can be observed *failing*. A guard nobody
    /// has ever seen fail is a guard of unknown polarity.
    static MIR_INJECT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn mir_injecting_a_wrong_byte() -> bool {
    MIR_INJECT.with(|c| c.get())
}

/// Scoped byte injection for one test.
#[cfg(test)]
struct MirInject;

#[cfg(test)]
impl MirInject {
    fn on() -> MirInject {
        MIR_INJECT.with(|c| c.set(true));
        MirInject
    }
}

#[cfg(test)]
impl Drop for MirInject {
    fn drop(&mut self) {
        MIR_INJECT.with(|c| c.set(false));
    }
}

/// Process totals for the level-2 machine list, so a corpus run has a number
/// rather than a stream of per-compile lines.
///
/// Read them with [`mir_totals::read`]; the `--dump-jit-stats` style consumers
/// and the corpus probe both go through it. Plain atomics rather than a mutex:
/// this runs inside the compile path, on the compiler thread, and a torn read
/// of a diagnostic counter is not worth a lock.
pub mod mir_totals {
    use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

    static METHODS: AtomicU64 = AtomicU64::new(0);
    static TILES: AtomicU64 = AtomicU64::new(0);
    static MISMATCHES: AtomicU64 = AtomicU64::new(0);
    static SHADOW_TILES: AtomicU64 = AtomicU64::new(0);
    static ARM_BYTES: AtomicU64 = AtomicU64::new(0);
    static ENC_BYTES: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VERIFIED: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VALUES: AtomicU64 = AtomicU64::new(0);
    static ALLOC_UNLOCATED_ROOTS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_VACUOUS: AtomicU64 = AtomicU64::new(0);
    static ALLOC_INDESCRIBABLE: AtomicU64 = AtomicU64::new(0);
    static ALLOC_REJECTED: AtomicU64 = AtomicU64::new(0);

    /// Record one method's [`super::MirAllocVerdict`], keeping the three states
    /// apart.
    ///
    /// Collapsing them is the specific failure this counter set exists to
    /// prevent: a corpus where every method came back `NothingToCover` and one
    /// where every method verified thousands of values are indistinguishable in
    /// a single "ok" tally, and the first proves nothing at all.
    pub(super) fn record_alloc(verdict: super::MirAllocVerdict) {
        match verdict {
            super::MirAllocVerdict::Verified {
                located,
                unlocated_tile_roots,
            } => {
                ALLOC_VERIFIED.fetch_add(1, Relaxed);
                ALLOC_VALUES.fetch_add(located as u64, Relaxed);
                ALLOC_UNLOCATED_ROOTS.fetch_add(unlocated_tile_roots as u64, Relaxed);
            }
            super::MirAllocVerdict::NothingToCover => {
                ALLOC_VACUOUS.fetch_add(1, Relaxed);
            }
            super::MirAllocVerdict::Indescribable(_) => {
                ALLOC_INDESCRIBABLE.fetch_add(1, Relaxed);
            }
        }
    }

    /// One method whose allocation `verify_allocation` REJECTED. A compiler
    /// bug, and non-zero is a stop-and-look number, not a ratio to watch.
    pub(super) fn record_alloc_rejected() {
        ALLOC_REJECTED.fetch_add(1, Relaxed);
    }

    /// `(verified, values, nothing_to_cover, indescribable, rejected)` — the
    /// increment-2 allocation verdicts since process start.
    pub fn read_alloc() -> (u64, u64, u64, u64, u64) {
        (
            ALLOC_VERIFIED.load(Relaxed),
            ALLOC_VALUES.load(Relaxed),
            ALLOC_VACUOUS.load(Relaxed),
            ALLOC_INDESCRIBABLE.load(Relaxed),
            ALLOC_REJECTED.load(Relaxed),
        )
    }

    /// Tile roots the allocation could give no home word, summed over every
    /// verified method.
    ///
    /// Increment 3's sizing, taken from real compiles instead of from a
    /// synthetic shape count: every one of these is a value that must live in a
    /// register because there is no memory for it to live in.
    pub fn read_unlocated_tile_roots() -> u64 {
        ALLOC_UNLOCATED_ROOTS.load(Relaxed)
    }

    pub(super) fn record(
        tiles: usize,
        mismatches: usize,
        shadow_tiles: usize,
        arm_bytes: usize,
        enc_bytes: usize,
    ) {
        METHODS.fetch_add(1, Relaxed);
        TILES.fetch_add(tiles as u64, Relaxed);
        MISMATCHES.fetch_add(mismatches as u64, Relaxed);
        SHADOW_TILES.fetch_add(shadow_tiles as u64, Relaxed);
        ARM_BYTES.fetch_add(arm_bytes as u64, Relaxed);
        ENC_BYTES.fetch_add(enc_bytes as u64, Relaxed);
    }

    /// `(methods, tiles, mismatches)` since process start.
    pub fn read() -> (u64, u64, u64) {
        (
            METHODS.load(Relaxed),
            TILES.load(Relaxed),
            MISMATCHES.load(Relaxed),
        )
    }

    /// `(shadow_tiles, arm_bytes, encoder_bytes)` — verify mode's sizing of the
    /// increment that would emit the rules byte equality cannot cover.
    ///
    /// `arm_bytes - encoder_bytes` is what those tiles would save, over exactly
    /// the nodes they cover, on this workload. A zero `shadow_tiles` means the
    /// question does not arise; it does not mean the saving is zero.
    pub fn read_shadow() -> (u64, u64, u64) {
        (
            SHADOW_TILES.load(Relaxed),
            ARM_BYTES.load(Relaxed),
            ENC_BYTES.load(Relaxed),
        )
    }

    /// Zero the accumulator. Tests only — hold [`TEST_LOCK`] across the reset
    /// AND the read, or two tests reading one global see each other's counts.
    #[cfg(test)]
    pub fn reset() {
        for c in [
            &METHODS,
            &TILES,
            &MISMATCHES,
            &SHADOW_TILES,
            &ARM_BYTES,
            &ENC_BYTES,
            &ALLOC_VERIFIED,
            &ALLOC_VALUES,
            &ALLOC_UNLOCATED_ROOTS,
            &ALLOC_VACUOUS,
            &ALLOC_INDESCRIBABLE,
            &ALLOC_REJECTED,
        ] {
            c.store(0, Relaxed);
        }
    }

    /// Serialises the tests that read the counters above. Its own lock, not
    /// `isel::SHADOW_TEST_LOCK`: these are different globals, and sharing one
    /// mutex between two unrelated accumulators is how a later test ends up
    /// holding the wrong one.
    #[cfg(test)]
    pub static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
}

/// TOK10-1 (JIT round 19 proposal wave 11, lane lv11): the strict test mode
/// of the token-read guard, `CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD=strict`
/// (any `strict` spelling; still an "on" value for the guard itself). Test
/// builds only; see [`Lowerer::token_read_check`].
#[cfg(test)]
fn token_read_guard_strict() -> bool {
    Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
        .is_ok_and(|v| v.trim().eq_ignore_ascii_case("strict"))
}

#[cfg(test)]
thread_local! {
    /// Test-only override for [`linear_scan_enabled`], so a unit test never
    /// depends on the process environment or on whether the flag has been
    /// declared yet. Thread-local, so parallel tests cannot see each other's
    /// setting.
    static LS_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn ls_forced() -> Option<bool> {
    LS_FORCE.with(|c| c.get())
}

/// Test-only RAII override of [`linear_scan_enabled`] on this thread.
#[cfg(test)]
struct LsForce;

#[cfg(test)]
impl LsForce {
    fn on() -> LsForce {
        LS_FORCE.with(|c| c.set(Some(true)));
        LsForce
    }

    /// Force the cache OFF for the duration.
    ///
    /// Needed by the level-2 tests: residency and the selector are mutually
    /// exclusive in production (see `lower_inner_with_array_list`), so a test that
    /// compares a MIR-mode body against a no-mode body is otherwise comparing
    /// two different backends and not the selector at all.
    fn off() -> LsForce {
        LS_FORCE.with(|c| c.set(Some(false)));
        LsForce
    }
}

#[cfg(test)]
impl Drop for LsForce {
    fn drop(&mut self) {
        LS_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
mod r14w3_fixup_sync_window_blame_tests;
#[cfg(test)]
mod r23w10_l6_direct_call_imm_align_tests;
#[cfg(test)]
thread_local! {
    /// Test hook: size the FIRST attempt's buffer at this many bytes instead
    /// of the estimate, so the overflow-retry path can be driven on purpose.
    static IR_FIRST_ATTEMPT_CAPACITY_FOR_TEST: std::cell::Cell<Option<usize>> =
        std::cell::Cell::new(None);
}

#[cfg(test)]
mod i49_l3_inline_row_census_tests;
/// `which` argument of `DirectHelperTable::well_known_class_id`:
/// `java/lang/Long`'s `ClassId` (round 12 wave 2, lane `calls`, for the inline
/// out-of-cache `Long.valueOf` box of both tiers). Answered by the VM's
/// `jit_well_known_class_id` exactly as `WELL_KNOWN_CLASS_INTEGER` is: the
/// unique class of that name, or `0`. Here beside
/// [`WELL_KNOWN_INTEGER_CACHE_HIGH`] for the reason given there.
pub use crate::direct_helpers::WELL_KNOWN_CLASS_LONG;

#[cfg(test)]
mod r25_instanceof_proxy_ways_tests;

/// Round rx27 (lane compile): the two opt-in switches, the fill bytes run on
/// a real cell, and the lowered `instanceof` / `checkcast` arms executed
/// against a counting fake helper.
#[cfg(all(test, target_arch = "x86_64"))]
mod rx27_proxy_ways_tests;
/// JIT r20 wave 18 (lane xVhArr, AT-2): the array arm's recogniser, plan word
/// and shared bytes, EXECUTED against fake `int[]` / `long[]` headers.
#[cfg(all(test, target_arch = "x86_64"))]
mod r20w18_vh_const_array_tests;

/// nio21 wave 11 L5 (W9L2-2): the constant-`VarHandle` site recogniser, the
/// plan word, the compile-time refusals, and the shared bytes EXECUTED against
/// a fake compact object.
/// JIT round 21 wave 16 (lane perf-cf-lean,
/// `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`): the VM answer word round-trips,
/// and the guard bytes, EXECUTED against fake objects, skip the call only for
/// a non-null, plain, compact receiver whose field reads null.
#[cfg(all(test, target_arch = "x86_64"))]
mod r21w16_call_entry_null_guard_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod nio21w11_vh_const_tests;

/// JIT round 20 wave 19, lane zAssign: the `Class.isAssignableFrom` /
/// `Class.isInstance` subtype arms, EXECUTED over a fake compiled-`getClass()`
/// table (class words + mirror hints) and fake supers displays. The mirror
/// "addresses" are never dereferenced (the arms only hash and compare them).
/// JIT r23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`):
/// [`reflect_new_array_resolve_bytes`] EXECUTED over a fake compiled-`getClass()`
/// table: a proven hint answers its class id, everything else declines.
#[cfg(all(test, target_arch = "x86_64"))]
mod r23w8_reflect_new_array_resolve_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod jr20_zassign_class_assignable_tests;
#[cfg(test)]
mod nio21w4_unsafe_offheap_address_bytes_tests;

#[cfg(test)]
mod nio21w3_scoped_arena_decline_bytes_tests;
#[cfg(test)]
mod nio20w4_scoped_session_admission_tests;

#[cfg(test)]
mod scoped_shared_zgc_tests;
#[cfg(test)]
mod nio20w5_getclass_lambda_words_tests;

#[cfg(test)]
mod nio21w1_getclass_prim_array_words_tests;

#[cfg(test)]
mod nio21w2_getclass_ref_array_words_tests;
#[cfg(test)]
mod nio20w5_identity_hash_inline_tests;
#[cfg(test)]
// x86-64 ONLY. These assert x86-64 encodings and several EXECUTE the code
// they emit, which on another architecture is an illegal instruction that
// takes the whole test binary down with it -- `cargo test -p cratonvm-jit`
// died at `a_previously_declined_large_method_now_compiles` with SIGILL the
// first time it was ever run on aarch64. Gated at the MODULE, because the
// property is "this module is about x86-64", not a per-test accident.
#[cfg(target_arch = "x86_64")]
mod tests;
/// Round 11 (lane `irlower`) executed tests for this file's round-11 edits.
///
/// A module of its own at the END of the file, so none of the source scans
/// that split on `mod tests {` or on `fn lower_data_node` sees it. x86-64 only,
/// for the reason the main test module states: these execute what they emit.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r11_irlower_tests;

/// Round 11 wave 12, lane `fib`: the phi-home residency rules.
///
/// A module of its own at the END of the file, for the same reason as
/// `r11_irlower_tests`: no source scan that splits on `mod tests {` sees it.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r11w12_fib_phi_home_tests;

/// Interpreter round i1 wave 15, lane L2
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`): an OSR-door
/// compile's back-edge poll leaves at its loop header, with the state that back
/// edge carries, from the very poll whose slow-path verdict turned non-zero —
/// for a `goto` back edge and for a conditional one (whose taken edge is its
/// own `Proj` block in this tier); a method-entry compile and a zero verdict
/// leave nothing; and the exit changes no OSR admission fact.
#[cfg(test)]
mod i15_l2_poll_mode_exit_tests;

/// Interpreter round i1 wave 19, lane L2
/// (`i18-L2-ir-entry-exits-refused-for-monitor-methods`, item 1): an optimizing
/// METHOD-ENTRY body of a method with a `synchronized` block leaves at a loop
/// header whose snapshot holds no monitor, and then vouches for its frames
/// (`can_deopt_resume`), which is the arm the interpreter and first-call doors
/// resume such a method on; a loop inside the block keeps a verdict-blind
/// poll.
#[cfg(test)]
mod i19_l2_monitor_block_exit_tests;

/// Interpreter round i1 wave 23, lane L2
/// (`i9-L5-jvmti-frames-already-compiled-finish-compiled`, optimizing item
/// (b)): a METHOD-ENTRY back-edge exit at a loop header inside a nested
/// `synchronized` region on one object, whose inner level the nested-lock
/// elision deleted, leaves with the split the resume sinks replay: the level
/// the compiled code holds (`relock == false`) and the elided level
/// (`relock`), both on the object's frame word. It used to refuse
/// (`Unlocatable`) and keep a verdict-blind poll.
#[cfg(test)]
mod i23_l2_elided_lock_exit_tests;

/// Interpreter round i1 wave 22, lane L2:
///
/// * `i18-L2-ir-entry-exits-refused-for-monitor-methods`, remains (a): an
///   `ACC_SYNCHRONIZED` method's optimizing method-entry body gets its
///   back-edge exits, unless its graph can neither trap nor call, the one body
///   a caller-held sync-direct site binds ([`super::ir_entry_poll_mode_exits`]);
/// * `i19-L1-deopt-resumed-frames-show-dead-locals-as-zero-to-a-debugger`, the
///   optimizing tier: the IR door discards, under a debugger, an artifact one
///   of whose frames publishes a definitely assigned local `Undefined`
///   ([`super::ir_artifact_hides_an_assigned_local`]), and a debugger's
///   method-entry compile (no `PollExitBytecode`) describes a dead assigned
///   local from its home or gives no exit.
#[cfg(test)]
mod i22_l2_synchronized_and_debugger_tests;

/// Round 12 wave 8, lane `callcost`: the entry fast return and the dominated
/// self-call sample (`r12w8-callcost-fib-call-anatomy-FIXED-20261001.md`). Every
/// body here is EXECUTED: the guard helper is a real function, so a sample
/// that trips on the test thread's stack calls it and carries on.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r12w8_callcost_tests;

/// Interpreter round i1 wave 28, lane L6 (stage 3 of
/// `interpreter-L6-proposal-a-patchable-post-call-exit-for-withdrawn-bodies-FIXED-20261022.md`):
/// an OSR-door compile puts a post-call exit site after a real call -- a
/// five-byte `NOP` inside one 8-aligned word, an `OsrExit` point at the
/// invoke's SUCCESSOR the planless transfer accepts, and a stub the force
/// pass's shape check accepts (the site becomes `JMP` to it); a compile with
/// no poll mode exits gets none.
#[cfg(test)]
mod i28_l6_post_call_exit_tests;

/// Round 13 wave 3 (lane framestate, W5-3 stage 2): a guard inside a spliced
/// body publishes the callee's own frame over its parked callers when the
/// build recorded chain snapshots (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`).
/// x86-64 only, like the other lowering modules.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r13w3_framestate_lower_tests;

/// Round 13 wave 6 (lane chain2): a chain snapshot's scalar-replaced objects
/// are described at the sites that publish IT, found at their raw pc
/// (`Lowerer::chain_deopt_block_and_span`). The flat frames' site index keys
/// spliced sites by their resume bci and publishes flat frames only, so it
/// found no site for any chain snapshot, and every object a chain named was
/// `MaterializationRequired`.
#[cfg(test)]
mod r13w6_chain2_lower_tests;

/// Round 16 (lane codegen): executed bodies for CG16-2, the integer
/// divisions' fixups after the body (`CRATONVM_JIT_IR_DIV_COLD_FIXUPS`).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r16_codegen_tests;

// ── Round 16 (lane matrix, MX16-1): OSR entries re-check hoisted reads ──

#[cfg(test)]
mod r16_matrix_osr_tests;

/// Round 16 wave 7, lane frames: the fresh-frame clear's skips
/// (`CRATONVM_JIT_IR_ZERO_FRESH_FRAMES_SKIP_UNROOTED`, `_SKIP_WRITTEN`), its
/// 32-byte form (`_AVX`) and the OSR entry stub's clear
/// (`CRATONVM_JIT_IR_ZERO_FRESH_OSR_FRAMES`). Byte-level checks on a poisoned
/// private stack, and executed bodies under every arm.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r16w7_frames_tests;

#[cfg(test)]
// Round 16 wave 11 (lane frames2): the sized deopt register image (FR16P-1,
// `CRATONVM_JIT_IR_DEOPT_IMAGE_SIZED`) and the published end of the Java
// locals (FR-B1, `CRATONVM_JIT_IR_PUBLISH_LAST_LOCAL`). See
// `docs/feature-designs/jit-ir-optimizations.md`.
mod r16w11_frames2_tests;

#[cfg(test)]
// Round 16 wave 12 (lane polyclear): no deopt register image for a body no
// exit can read one from (FR2P-1, `CRATONVM_JIT_IR_DEOPT_IMAGE_EXITLESS`). See
// `docs/feature-designs/jit-ir-optimizations.md`.
mod r16w12_polyclear_tests;

#[cfg(test)]
// Round 16 wave 17 (lane frames2): the callee-saved save bands sized to what a
// body saves (PC-1, `CRATONVM_JIT_IR_SAVE_BANDS_SIZED`), the planned staging
// clears moved with every layout cut (`CRATONVM_JIT_IR_STAGE_CLEAR_REBASE`), and
// the prologue's watermark capture reading the thread pointer from RAX
// (FR16P-3, `CRATONVM_JIT_IR_PROLOGUE_THREAD_IN_RAX`). See
// `docs/feature-designs/jit-ir-optimizations.md`.
mod r16w17_frames2_tests;

#[cfg(test)]
mod r18w11_osrfollow_class_test_tests;

// ── Round 17 (`r16-codegen-loop-and-straight-line-anatomy-FIXED-20261005.md`): loop codegen ──

#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r17_loop_codegen_tests;

/// Interpreter round i2 wave 64, lane L2: the slot-plan token-edge rule
/// (`CRATONVM_JIT_IR_SLOTS_TOKEN_EDGES_NOT_USES`,
/// `i63-L2-proposal-frame-home-ranges-skip-memory-token-reads`) and its safety
/// net, the token-read guard (`CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD`,
/// `i63-L2-proposal-the-lowering-refuses-a-token-slot-operand-read`).
#[cfg(test)]
mod i64_l2_token_slot_tests;

/// JIT round 19 proposal wave 10, lane tok10: the token-read guard's `term`
/// and `mir` scopes
/// (`i64-L2-proposal-the-token-read-guard-covers-terminators-and-fused-compares`).
/// Same switch as the `data` scope (`CRATONVM_JIT_IR_LOWER_TOKEN_READ_GUARD`):
/// the scopes only refuse, so a compile the guard accepts emits the bytes it
/// emits with the guard off -- checked end to end below.
#[cfg(test)]
mod r19_tok10_token_scope_tests;

/// Interpreter round i2 wave 65, lane L2: one liveness core for the slot
/// plan and the allocator's model
/// (`i64-L2-proposal-one-liveness-core-for-plan-slots-and-the-live-model`,
/// `CRATONVM_JIT_IR_SHARED_LIVENESS_CORE`) and the φ edge rule for the slot
/// plan (`i64-L2-ranges-agree-is-false-for-every-graph-with-a-phi`,
/// `CRATONVM_JIT_IR_SLOTS_PHI_EDGE_RANGES`).
#[cfg(test)]
mod i65_l2_shared_liveness_tests;

/// Interpreter round i2 wave 66, lane L2: one liveness pass per compile
/// (`i65-L2-proposal-one-liveness-pass-per-compile`,
/// `CRATONVM_JIT_IR_ONE_LIVENESS_PASS`). The shared pass must give the slot
/// plan and the live model the two-pass path gives, field for field.
#[cfg(test)]
mod i66_l2_one_liveness_pass_tests;

/// Shape 2 (b) of the i41 page: a compile whose optimizer dropped snapshot
/// values keeps its back-edge exits, and one whose state lost a LIVE value
/// refuses.
#[cfg(test)]
mod i41_dropped_snapshot_value_tests;

/// JIT round 19 proposal wave (lane osr19, OSRPOLY19-2): the `Op::ClassWordOf`
/// arm and its OSR recomputation, and the chain's per-class compare, EXECUTED
/// against hand-made headers (the class id word at offset 0, the kind byte).
#[cfg(test)]
mod r19_osr19_class_word_tests;

/// JIT round 19 wave 4 (lane irops3): IROPS19-5 (the branch-free AVX FP
/// `min`/`max`) and IROPS2-8 (the FMA's register-resident addend, under the
/// linear-scan file).
#[cfg(test)]
mod r19w4_irops3_tests;

#[cfg(test)]
// JIT round 19 wave 5 (lane handler5, SPLICE4-1): the lowering half of the
// compiled handler entry (`Lowerer::catch_arm_wire`, `emit_catch_probes`).
mod r19_handler5_catch_probe_tests;

/// Interpreter round i2 wave 70, lane L2: the slot plan's legacy liveness
/// copies through the dirty-pass driver
/// (`i69-L2-proposal-one-dirty-pass-driver-for-every-liveness-copy`,
/// `CRATONVM_JIT_IR_LEGACY_LIVENESS_WORKLIST`). `pub(crate)` so the wave-69
/// bench in `regalloc` can time these copies too.
#[cfg(test)]
pub(crate) mod i70_l2_legacy_liveness_tests;

#[cfg(test)]
mod nio21w2_l3_noncloseable_model_fold_tests;

/// JIT round 19 proposal wave 10, lane lv10: the liveness infrastructure
/// items the interpreter round handed over -- the four legacy round robins
/// through the shared driver (`CRATONVM_JIT_IR_LIVENESS_ONE_DRIVER`,
/// i71-L2), one liveness scratch per compile
/// (`CRATONVM_JIT_IR_LIVENESS_SHARED_SCRATCH`, i71-L2) and the dead-ref clear
/// planner's fixed points through the closure-transfer driver
/// (`CRATONVM_JIT_IR_DEAD_REF_LIVENESS_WORKLIST`, i70-L2). Every test here is
/// a differential: the old code is the oracle, the new path must give its
/// answer exactly (and, for the drivers, the same verdict at every cap).
#[cfg(test)]
mod lv10_liveness_tests;

#[cfg(test)]
mod lv11_tests;

// j27 wave 2 (lane RESIDENCY): the loop-aware spill cost on a real loop
// (`CRATONVM_JIT_IR_LOOP_PHI_RESIDENCY`); executes what it emits.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod loop_phi_residency_tests;

// JIT rx27 wave 4 (lane regalloc): `(x & mask) == 0` branches as one `TEST`
// (`CRATONVM_JIT_IR_AND_ZERO_TEST_BRANCH`); executes what it emits.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod and_zero_test_tests;

// JIT rx27 wave 6 (lane regalloc): a pruned guard's `TEST` and the pad that
// rebuilds the `And` its frame names (`CRATONVM_JIT_IR_AND_ZERO_TEST_GUARD`).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod and_zero_guard_tests;

// JIT rx27 (lane regalloc): the executed pressure loop the regalloc tests
// lower (`edge_ref`, `lower_edge_loop`, `run`).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod edge_loop_fixture;

// JIT rx27 wave 8 (lane regalloc): second operands read in place
// (`CRATONVM_JIT_IR_OPERAND_IN_PLACE`).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod operand_in_place_tests;

// JIT rx27 wave 9 (lane regalloc): a read right behind its own register
// publish (`CRATONVM_JIT_IR_PUBLISH_RELOAD_ELIDE`).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod publish_reload_elide_tests;

// j27 wave 1 (lane LOOPCG): the select-then-test form of the thread-word
// safepoint poll (`CRATONVM_JIT_IR_POLL_WORD_SELECT`).
mod poll_word_select;

// j27 wave 5 (lane UNROLLRES): the page form of the IR back-edge poll
// (`CRATONVM_JIT_IR_PAGE_POLL`, opt-in).
mod ir_page_poll;

// j27 wave 2 (lane VHCOMP): the static-field constant-`VarHandle` arm
// (`CRATONVM_JIT_VH_CONST_STATIC`); public for the VM's cell table.
pub mod vh_static_const;

// j27 wave 2 (lane VHCOMP): the generational card edge of the instance
// constant-`VarHandle` reference writes (`CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`).
// j27 wave 4 (lane VHCARD): `pub(crate)` for the single-pass twin's switch.
pub(crate) mod vh_gen_card_edge;

// j27 wave 4 (lane REMAT): a split reload of `x + c` recomputed from a
// resident `x` (`CRATONVM_JIT_IR_LS_REMAT_ADD`, opt-in).
mod ls_remat_add;

// j27 wave 4 (lane DEOPTROWS, CH16-5): forced exits at flat guards inside
// the eager-chains soak (`CRATONVM_DEOPT_EAGER_GUARDS`, diagnostic, opt-in).
mod forced_flat_guard;

// j27 eager-guards soak close: a deopting field guard publishes a frame only
// through values already defined where it sits (a hoisted read's in-loop frame
// names the header phi, whose register the pre-header has not written).
mod frame_defined_at_site;

// j27 wave 4 (lane IRROWS, IRLOWER17-3): the census and opt-in refusal of the
// by-bci anchors that stayed undominated
// (`CRATONVM_JIT_IR_REFUSE_UNDOMINATED_ANCHOR`).
mod undominated_anchor;

// j27 wave 4 (lane CLONEIR): an array `clone()` expanded inline as a prefix of
// its thin-helper call (`CRATONVM_JIT_IR_CLONE_INLINE`, opt-in).
mod clone_inline;

// All-primitive inline `System.arraycopy` fast prefix (`[Z`, `[B`, `[C`, `[S`,
// `[I`, `[F`, `[J`, `[D`), including self-array overlapping copies.
mod prim_arraycopy;

// j27 wave 5 (lane JMMHUNT): `Thread.onSpinWait()` as a compiler barrier and
// one `PAUSE` (`CRATONVM_JIT_IR_ON_SPIN_WAIT_BARRIER`); public for the VM's
// site resolver.
pub mod on_spin_wait;

// j27 wave 4 (lane REMAT): the remat on an installed split plan and on a real
// loop; executes what it emits.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod ls_remat_add_tests;

#[cfg(test)]
mod gci_i9ng_shadow_push_gate_tests;

/// JIT round 20 wave 5 (lane loopopt): a guard whose condition has no home
/// word (a dropped constant: a site trap or a folded guard) reads it through
/// `gp_load_value` on both of its routes, instead of `slot_of`, which refused
/// and cost the attempt a second lowering with every home kept.
#[cfg(test)]
mod jr20_guard_homeless_condition_tests;

// JIT round 20 wave 20 (lane refspin): the spliced `Reference.get` loop of
// `jr20-spliced-reference-get-loop-never-sees-the-clear-20261013.md`.
#[cfg(test)]
mod jr20_refspin_referent_loop_tests;

// JIT round 20 wave 21 (lane pollroot): the general back-edge dead-home clears
// of `jr20-back-edge-poll-publishes-dead-reference-homes-20261013.md`.
#[cfg(test)]
mod jr20_pollroot_back_edge_tests;

/// zt t9/ca (carried from zs2rv, zt2ca page STATUS t5/pg3): a lowering-level
/// test of the IR tier's ZGC fast-join gate and of the fall-through split's
/// admission, both of which require `pending_shadow` to be empty -- the
/// condition under which the stub's shadow reload that the inline arm jumps
/// over emits nothing.
#[cfg(test)]
mod zt9ca_fast_join_gate_tests;

/// zt t23/zb6 (stage 2d-ii): the optimizing tier's compact reference field
/// load under a colored-store ZGC load barrier. The colored state is opened on
/// the test's own thread (`x64::ZgcColoredFieldLoadTestOverride`): nothing
/// process-wide is armed or published. A module of its own at the END of the
/// file, like `r11_irlower_tests`.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod zt23zb6_ir_field_load_tests;

/// JIT r21 wave 1 (lane typecheck, `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM`): the
/// `aastore` store-check arm, EXECUTED as `extern "C" fn(value_id,
/// component_id) -> 1 | DECLINED` over fake displays of the VM's layout
/// (`[len, chain.., n, ifaces.., verdict]`). A subclass of a component class,
/// at any depth, and an implementor of a component interface, direct or
/// inherited, are `1`; a shallower or unrelated class, a non-implementor, a
/// value with no display or past the table, and a component past the table
/// that no interface set lists all decline -- never a `0`, so a refusal stays
/// the helper's.
#[cfg(all(test, target_arch = "x86_64"))]
mod r21_typecheck_aastore_display_tests;

/// JIT r21 wave 1 (lane typecheck): the final-miss verdict screen
/// (`CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT`) and the array-target arm
/// (TYPECHECK21-3, `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`), EXECUTED over fake
/// displays of the VM's layout and fake object headers.
#[cfg(all(test, target_arch = "x86_64"))]
mod r21_typecheck_verdict_and_array_tests;

/// tca21 (TCARRAY21-1, second half): the component miss of an `instanceof T[]`
/// ([`typecheck_array_component_miss_bytes`]) behind the hit arm
/// ([`typecheck_array_hit_bytes`]), EXECUTED over fake displays of the VM's
/// layout and fake reference-array headers. The composite is built exactly as
/// `Lowerer::emit_typecheck_array_arm` lays it out: the hit arm's declines land
/// on the miss bytes, whose declines are the helper.
///
/// Expected values are derived from JVMS 6.5 / JLS 10.8 (`S[]` is a `T[]` iff
/// `S` is assignable to `T`; every array class has `Object`, `Cloneable` and
/// `Serializable` as its supertypes), not measured.
#[cfg(all(test, target_arch = "x86_64"))]
mod tca21_typecheck_array_component_miss_tests;

/// r21 barriers (BARRIERS21-2, `CRATONVM_JIT_IR_G1_GATED_AASTORE`): the G1
/// `aastore` screen (`g1_aastore_screen_bytes`), EXECUTED as `extern "C"
/// fn(array, value) -> 0 | 1 | -1`.
///
/// * 0: the fall-through, a store with no barrier.
/// * 1: the cross-region edge, the store and then the lean G1 barrier.
/// * -1: the decline edge, the whole `jit_aastore`.
///
/// The fixture is a fake SATB gate byte and a fake `JIT_G1_BARRIER` table
/// whose arena base is NOT region-aligned. The pairs on both sides of an
/// aligned 1 MiB boundary are the ones a base-free test would misjudge.
#[cfg(all(test, target_arch = "x86_64"))]
mod r21_barriers_g1_aastore_tests;

/// JIT round 22 wave 1, lane memory: ALLOC21-1 (`CRATONVM_JIT_IR_NEWARRAY_MAP_SINK`).


/// JIT round 22 wave 12, lane runtime (`CRATONVM_JIT_IR_SWEEP_ENTRY_COUNTER`):
/// an optimizing body carries the entry counter only while the cold-body
/// sweeper is armed, emitted right after the not-entrant pad, and its box is
/// handed to the body. Unarmed (this test process), nothing is emitted, so the
/// layout tests above see the old bytes.
#[cfg(test)]
mod r22w12_runtime_ir_sweep_entry_counter_tests;

/// JIT round 23 wave 3d (lane L1, proxy proposal 4e;
/// `CRATONVM_JIT_IR_DIRECT_INSTANCE_SELF_CALL`): a statically bound instance
/// self-call lowered as a receiver-carrying direct self-call (`invoke_kind`
/// 4 with the receiver in argument 0).
#[cfg(test)]
mod r23_l1_direct_instance_self_call_tests;

/// JIT round 23 wave 9 (lane W9-L7, `CRATONVM_JIT_IR_ANCHOR_TRAP_FREE_SKIP`):
/// [`can_deopt`] over each class of `Op`. A node that can trap -- a field read
/// with its null test, an array access, a division by a variable, a guard, a
/// store into a receiver that may be null, a helper `getstatic` -- answers
/// `true`; pure arithmetic, compares, conversions, constants, phis, a division
/// by a non-zero constant and a DIRECT `getstatic` answer `false`.
#[cfg(test)]
mod r23_w9_l7_can_deopt_tests;

/// JIT round 23 wave 10 (lane L7, `CRATONVM_JIT_IR_RESIDENCY_FRAME_PINNED`):
/// the population [`single_use_frame_pinned_in_loop`] admits.


/// JIT round 23 wave 10 (lane L7, `CRATONVM_JIT_IR_DEOPT_NAME_PAST_PRELOOP`):
/// a loop value owns its register for every point a frame can name it at
/// when every other holder lives and dies before the loop.
#[cfg(test)]
mod r23_w10_l7_preloop_owner_tests;
#[cfg(test)]
mod r23_w11b_l5_synthetic_store_row_tests;

/// Volatile stores share a fence only across operations that preserve ordering.
#[cfg(all(test, target_arch = "x86_64"))]
mod r23_close_volatile_fence_sink_tests;

/// JIT round 25 (item 3 of the round-24 redesign,
/// `CRATONVM_JIT_IR_PRIMITIVE_COLOUR_CLAIM`): the JIT half of the per-body
/// primitive-colour claim -- the grouped colouring, the clear's skip, the
/// published claim, and an executed body under it. The collector half is
/// `vm/src/jit/conservative_roots.rs`, `mod r25_prim_colour_claim_tests`.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_prim_colour_claim_tests;

/// JIT round 25 wave 2 (page `docs/feature-designs/real-frame-deopt.md`):
/// smaller optimizing-tier frames and fresh-frame clears --
/// `CRATONVM_JIT_IR_PINNED_PRIM_COLOUR_CLAIM` (claimable `Pinned` colours),
/// `CRATONVM_JIT_IR_DEOPT_IMAGE_CLAIM` (the deopt GPR image) and
/// `CRATONVM_JIT_IR_FRAME_PARAM_LOCALS_ONLY` (parameter local words only).
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_frame_anatomy_tests;
/// JIT round 25 wave 6 (lane W6-A): the fresh-frame clear as a store cover
/// (`CRATONVM_JIT_IR_FRESH_CLEAR_COVER`) and its census. Every test pins the
/// switches it reads, so each passes with the switch unset and set.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25w6a_fresh_clear_cover_tests;

#[cfg(test)]
mod r25w6d_leaf_entry_tests;

/// JIT round 25 wave 6 (lane W6-E): the co-establishment (default ON since j27 2026-10-28) of the item-4
/// field proofs, `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_COESTABLISH` (an
/// establishing read on another value proves `this` too). W6-E's second
/// switch, `_OSR_RECHECK`, was retired in the merge with dev's item-4
/// residual 2 (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS_PREHEADER`, default ON),
/// which implements the same pre-loop reach with its own tests. Every test
/// pins the switches it depends on, so it passes whatever the environment
/// sets.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_w6e_proven_fields_reach_tests;

// JIT round 25 wave 6, lane W6-B (`CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES`):
// site-named loop values (headers by liveness, phis, back-edge spills in the
// poll's slow path) and the `ir-home-why` census.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_w6b_site_named_loop_tests;
mod r25_w7a_reg_alloc {
    use super::{
        branch_targets_except, for_each_branch_field, isel_emit_enabled, isel_verify_enabled,
        site_value_live_at, op_home_is_one_store_rax,
        op_traps_only_through_pads, single_use_is_in_the_carry_window, Bailout, BailoutReason,
        Graph, IrType, Lowerer, MemKind, NodeId, Op, Schedule, SiteLive, HK_NAMES, HK_OP,
        IR_LOWER_LS_GPRS, IR_LS_CALLER_SAVED_GPRS, NO_NODE, RAX, RCX,
    };

    pub(super) fn effective(keep_deopt_image: bool) -> bool {
        !keep_deopt_image && !(isel_emit_enabled() || isel_verify_enabled())
    }

    /// The three switches this one implies, each unless its own key is set.
    pub(super) const ELIDE_KEY: &str = "CRATONVM_JIT_IR_ELIDE_UNNAMED_HOMES";
    pub(super) const SPILL_KEY: &str = "CRATONVM_JIT_IR_ELIDE_HOMES_SPILL_AT_INSPECTION";
    pub(super) const LOOP_KEY: &str = "CRATONVM_JIT_IR_SITE_NAMED_LOOP_VALUES";
    /// W8-4: the parameter prologue copy, implied too.
    pub(super) const PARAM_COPY_KEY: &str = "CRATONVM_JIT_IR_PARAM_COPY";


    /// W9-2: the integer ops [`Lowerer::reg_alloc_direct_alu`] emits straight
    /// into the result's register.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum DirectAlu {
        Add,
        Sub,
        Mul,
        And,
        Or,
        Xor,
        Shl,
        Shr,
        UShr,
    }

    impl DirectAlu {
        pub(super) fn of(op: &Op) -> Option<DirectAlu> {
            Some(match op {
                Op::Add => DirectAlu::Add,
                Op::Sub => DirectAlu::Sub,
                Op::Mul => DirectAlu::Mul,
                Op::And => DirectAlu::And,
                Op::Or => DirectAlu::Or,
                Op::Xor => DirectAlu::Xor,
                Op::Shl => DirectAlu::Shl,
                Op::Shr => DirectAlu::Shr,
                Op::UShr => DirectAlu::UShr,
                _ => return None,
            })
        }
    }

    /// W9-2: the second operand of a direct definition.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum DirectB {
        Reg(u8),
        Imm(i32),
        Frame(i32),
    }

    /// W9-2: the bytes of `d = ra <kind> b` written straight into `d`, or
    /// `None` when this form cannot express it (a shift by a register, a
    /// non-commutative op whose result register is the second operand's).
    /// 32-bit forms for `int` (the upper half zeroed, as the accumulator's
    /// `op eax` and 64-bit `mov` leave it), REX.W forms for `long`:
    ///
    /// ```text
    /// d == ra:          op d, rb | op d, imm
    /// d == rb:          op d, ra                      (commutative only)
    /// otherwise:        mov d, ra ; op d, rb | imm
    /// Mul by imm:       imul d, ra, imm                (one instruction)
    /// shifts by imm:    [mov d, ra ;] shl|sar|shr d, imm & (31|63)
    /// ```
    ///
    /// The accumulator form is `mov eax, ra ; op eax, b ; mov d, rax`.
    pub(super) fn direct_alu_bytes(
        kind: DirectAlu,
        wide: bool,
        d: u8,
        ra: u8,
        b: DirectB,
    ) -> Option<Vec<u8>> {
        use crate::x64::isel::{encode_allocated_alu, Mem, Op as MachineOp, Operand, Ty};

        let op = match kind {
            DirectAlu::Add => MachineOp::Add,
            DirectAlu::Sub => MachineOp::Sub,
            DirectAlu::And => MachineOp::And,
            DirectAlu::Or => MachineOp::Or,
            DirectAlu::Xor => MachineOp::Xor,
            DirectAlu::Mul => MachineOp::Imul,
            DirectAlu::Shl => MachineOp::Shl,
            DirectAlu::Shr => MachineOp::Sar,
            DirectAlu::UShr => MachineOp::Shr,
        };
        let rhs = match b {
            DirectB::Reg(reg) => Operand::Gpr(reg),
            DirectB::Imm(value) => Operand::Imm(i64::from(value)),
            DirectB::Frame(offset) => Operand::Mem(Mem::base_disp(
                5, // RBP is the frame base.
                i64::from(offset.checked_neg()?),
            )),
        };
        encode_allocated_alu(op, if wide { Ty::I64 } else { Ty::I32 }, d, ra, rhs)
    }


    /// Does the allocator imply sub-switch `key` for this compile? Only when
    /// the allocator is on and `key` is not set at all: an explicit value of
    /// the sub-switch, `=0` included, is that switch's own answer.
    pub(super) fn implied(reg_alloc_on: bool, key: &str) -> bool {
        reg_alloc_on && cratonvm_types::flags::runtime_var_os(key).is_none()
    }

    /// Direct arithmetic has no accumulator-to-result publish to amortize.
    /// Carry-served values stay with the carry; ordinary homes remain written.
    pub(super) fn direct_single_use_candidate(
        graph: &Graph,
        schedule: &Schedule,
        id: usize,
        uses: u32,
    ) -> bool {
        if uses != 1 || !super::ir_operand_in_place_enabled() {
            return false;
        }
        let Some(node) = graph.nodes.get(id) else {
            return false;
        };
        let selectable = match DirectAlu::of(&node.op) {
            Some(DirectAlu::Shl | DirectAlu::Shr | DirectAlu::UShr) => node.inputs.get(1)
                .and_then(|&b| graph.nodes.get(b as usize))
                .is_some_and(|b| matches!(b.op, Op::Const(_))),
            Some(_) => true,
            None => false,
        };
        selectable && single_use_candidate(graph, schedule, id, uses)
    }

    /// A single-use value the allocator keeps in a register although the
    /// pays rule refuses it: `int` / `long`, its home written by one
    /// `store_rax` (so a site-named plan can drop it,
    /// `Lowerer::plan_site_named_homes`), and its one reader out of the RAX
    /// carry's one-node window (inside it the carry is already free).
    pub(super) fn single_use_candidate(
        graph: &Graph,
        schedule: &Schedule,
        id: usize,
        uses: u32,
    ) -> bool {
        if uses != 1 {
            return false;
        }
        let Some(node) = graph.nodes.get(id) else {
            return false;
        };
        matches!(node.ty, IrType::Int | IrType::Long)
            && (op_home_is_one_store_rax(&node.op)
                || load_writes_its_home_once(&node.op, node.ty)
                || routed_result(&node.op, node.ty))
            && !single_use_is_in_the_carry_window(graph, schedule, id)
            && !carry_reaches(graph, schedule, id)
    }

    /// W7-A fix 2: an `int` / `long` field read (`Op::Load` of a primitive
    /// kind) writes its home exactly once, through `store_rax`, with RAX
    /// holding the value, on every route -- audited arm by arm, which is why
    /// it is claimed here, under the switch, and not in
    /// `op_home_is_one_store_rax` (whose source scan counts `store_rax` per
    /// arm and sees the exclusive routes as two):
    ///
    /// * the inline compact read (`emit_inline_compact_getfield`): the compact
    ///   read, the legacy read and the checked helper (inline, or the
    ///   W9-L7 cold tail past the body) all jump to one `done`, followed by
    ///   one `store_rax(slot)`; its proven-receiver form
    ///   (`emit_proven_receiver_getfield`) likewise joins before its one
    ///   `store_rax`; it emits nothing before returning `false`;
    /// * the checked-helper arm: `CALL`, sentinel check, one `store_rax`;
    /// * the no-helper arm: null deopt, `MOVSXD`, one `store_rax`;
    /// * a non-constant offset deopts unconditionally and defines nothing (a
    ///   dropped home there refuses the attempt: "published no register").
    ///
    /// So `publish_def_at_store` publishes the register on every route. A
    /// reference or FP load is not claimed (their homes are never dropped).
    /// W8-4 (increment 2): an `int` / `long` call result or static read. Its
    /// arm may reach the home by more than one route (an inline prefix's hit
    /// path, a sentinel tail, a cold tail past the body), so it is admitted
    /// only with an emission-time proof per value instead of an audit: the
    /// node's lowering must publish its register (`cur_def_published`), no
    /// prefix may join after it, and after the body no branch may land between
    /// the publish and the node's end
    /// ([`Lowerer::verify_reg_alloc_routed_defs`]). A route that writes the
    /// home and skips the publish either never publishes (refused at the
    /// node) or rejoins past the publish (refused at the scan); the refusal
    /// names the value and the retry keeps its home.
    pub(super) fn routed_result(op: &Op, ty: IrType) -> bool {
        matches!(op, Op::Call { .. } | Op::LoadStatic { .. })
            && matches!(ty, IrType::Int | IrType::Long)
    }

    pub(super) fn load_writes_its_home_once(op: &Op, ty: IrType) -> bool {
        matches!(
            op,
            Op::Load(MemKind::Int | MemKind::Long | MemKind::Byte | MemKind::Char | MemKind::Short)
        ) && matches!(ty, IrType::Int | IrType::Long)
    }

    /// W7-A fix: the two carries beyond the one-node window that already serve
    /// a single-use value for free, so a register would only add a publish
    /// (and leave a planned carry unread, which refuses the compile):
    ///
    /// * the deferred RCX carry, whose consumer is TWO nodes later
    ///   (`carry_deferred`; asked conservatively, by position only);
    /// * the tail carry of item 1a, whose consumer is the block's `If` /
    ///   `Return` terminator right behind the value (`plan_tail_carries`).
    pub(super) fn carry_reaches(graph: &Graph, schedule: &Schedule, id: usize) -> bool {
        let reads = |reader: NodeId| {
            graph
                .nodes
                .get(reader as usize)
                .is_some_and(|n| n.inputs.iter().any(|&i| i != NO_NODE && i as usize == id))
        };
        for block in &schedule.blocks {
            let Some(w) = block.nodes.iter().position(|&n| n as usize == id) else {
                continue;
            };
            if block.nodes.get(w + 2).is_some_and(|&c| reads(c)) {
                return true;
            }
            return w + 1 == block.nodes.len() && block.terminator.is_some_and(reads);
        }
        false
    }

    /// The site-liveness facts of EVERY `int` / `long` value, not only the
    /// plan's resident ones: a back-edge source the plan left unassigned can
    /// take its phi's register later (`coalesce_back_edge_phi_sources`), and
    /// the site naming of it is checked against these facts.
    pub(super) fn extract_every_int(
        live: &crate::regalloc::LiveModel,
        schedule: &Schedule,
        graph: &Graph,
    ) -> Option<SiteLive> {
        let every: Vec<Option<u8>> = graph
            .nodes
            .iter()
            .map(|node| matches!(node.ty, IrType::Int | IrType::Long).then_some(0))
            .collect();
        SiteLive::extract(live, schedule, graph, &every)
    }

    impl Lowerer<'_> {
        /// Is `v` a site-named coalesced back-edge phi source, asked at the
        /// outgoing edge of `block_idx`, its own block? Then its register holds
        /// it there: `coalesce_back_edge_phi_sources` admits a source only in a
        /// block with no terminator and one successor (the header), with every
        /// node after it `op_cannot_deopt`, no node after it reading the phi,
        /// and no other holder of the register live in its range. The edge
        /// copy is a self-copy, so the phi takes the register over unchanged.
        /// The site model counts the phi's read AT the edge as not strictly
        /// after it, which is the one position this answers for.
        pub(super) fn reg_alloc_coalesced_live_at_edge(&self, v: NodeId, block_idx: usize) -> bool {
            let id = v as usize;
            self.reg_alloc_on
                && self.phi_coalesced_src.get(id).copied().unwrap_or(false)
                && self.site_named.get(id).copied().unwrap_or(false)
                && self.schedule.node_to_block.get(id).copied() == Some(block_idx)
                && self
                    .schedule
                    .blocks
                    .get(block_idx)
                    .is_some_and(|b| b.terminator.is_none() && b.successors.len() == 1)
        }

        /// A carry source the direct ALU selector may read without moving it.
        /// It is available only to the exact planned consumer.
        fn reg_alloc_carried_gpr(&self, v: NodeId, consumer: NodeId) -> Option<u8> {
            if let Some((prod, cons, reg, pos)) = self.live_carry {
                if prod == v && cons == consumer && self.cur_def == Some(consumer)
                    && (reg != RAX || pos == self.buf.pos()) { return Some(reg); }
            }
            if let Some((prod, cons, _)) = self.deferred_rcx {
                if prod == v && cons == consumer && self.cur_def == Some(consumer) { return Some(RCX); }
            }
            if let Some((prod, cons, pos)) = self.tail_carry {
                if prod == v && cons == consumer && self.cur_def == Some(consumer)
                    && pos == self.buf.pos() { return Some(RAX); }
            }
            None
        }

        /// Consume a carry only after an encoding is selected.
        fn reg_alloc_consume_carried_gpr(&mut self, v: NodeId, consumer: NodeId) {
            if matches!(self.live_carry, Some((prod, cons, _, _)) if prod == v && cons == consumer) {
                self.live_carry = None;
                self.carries_read += 1;
            } else if matches!(self.deferred_rcx, Some((prod, cons, _)) if prod == v && cons == consumer) {
                self.deferred_rcx = None;
                self.carry_deferred_read += 1;
                self.carries_read += 1;
            } else if let Some((prod, cons, pos)) = self.tail_carry {
                if prod == v && cons == consumer && pos == self.buf.pos() {
                    self.tail_carry = None;
                    self.tail_carry_points.push((pos, prod));
                    self.carries_read += 1;
                }
            }
        }

        fn reg_alloc_selected_lea(
            &mut self,
            id: NodeId,
            slot: i32,
            addr: crate::x64::isel::IrAddr,
            wide: bool,
        ) -> bool {
            use crate::x64::isel::{select, Mem, Op as MachineOp, Operand, Req, Ty};
            let Some(base) = addr.base else { return false };
            let assigned = self.assigned_gpr(id);
            let dst = assigned.unwrap_or(RAX);
            if self.ls_managed.get(id as usize).copied().unwrap_or(false)
                || (assigned.is_some() && self.carry_at_store(slot).is_some())
            {
                return false;
            }
            let base_source = self
                .reg_alloc_carried_gpr(base, id)
                .or_else(|| self.resident_gpr(base));
            let index_source = addr.index.map(|index| {
                (
                    index,
                    self.reg_alloc_carried_gpr(index, id)
                        .or_else(|| self.resident_gpr(index)),
                )
            });
            let available = |input, reg: Option<u8>| {
                reg.is_some()
                    || matches!(
                        self.graph.nodes.get(input as usize).map(|node| &node.op),
                        Some(Op::Const(_))
                    )
                    || self.slot_of_checked(input).is_ok()
            };
            if !available(base, base_source)
                || index_source.is_some_and(|(index, reg)| !available(index, reg))
            {
                return false;
            }
            // The two reserved scratch registers bind spilled address inputs.
            // Choose the loads so neither destroys an already available input.
            let base_reg =
                base_source.unwrap_or(if index_source.is_some_and(|(_, reg)| reg == Some(RAX)) {
                    RCX
                } else {
                    RAX
                });
            let index_reg = index_source.map(|(index, reg)| {
                if index == base {
                    base_reg
                } else {
                    reg.unwrap_or(if base_reg == RCX { RAX } else { RCX })
                }
            });
            let mem = match index_reg {
                Some(index) => Mem::base_index(base_reg, index, addr.scale, addr.disp),
                None => Mem::base_disp(base_reg, addr.disp),
            };
            let Ok(instruction) = select(&Req::new(
                MachineOp::Lea,
                if wide { Ty::I64 } else { Ty::I32 },
                Operand::Gpr(dst),
                Operand::Mem(mem),
            )) else {
                return false;
            };
            for input in addr.base.into_iter().chain(addr.index) {
                self.reg_alloc_consume_carried_gpr(input, id);
            }
            if base_source.is_none() {
                self.gp_load_value(base_reg, base);
            }
            if let (Some((index, None)), Some(reg)) = (index_source, index_reg) {
                if index != base {
                    self.gp_load_value(reg, index);
                }
            }
            self.buf.emit(&instruction.encoded.bytes);
            if assigned.is_some() {
                self.publish_integer_result(dst, slot);
                if !wide && !self.poisons_int_def(id) {
                    self.gp_zx32_mask |= 1u16 << dst;
                }
            } else {
                self.rax_home_mirror = None;
                self.rax_reg_mirror = None;
                self.store_rax(slot);
            }
            true
        }

        fn reg_alloc_selected_array(&mut self, slot: i32, tile: crate::x64::isel::MInst) -> bool {
            use crate::x64::isel::{encode_allocated_alu, AddrSource, Mem, MInst, Operand, Ty};
            let MInst::AluRM { op, ty, dst: id, lhs, addr: AddrSource::Expr(addr), load } = tile else {
                return false;
            };
            let (Some(base), Some(index)) = (addr.base, addr.index) else {
                return false;
            };
            let assigned = self.assigned_gpr(id);
            if self.ls_managed.get(id as usize).copied().unwrap_or(false)
                || self.deferred_rcx.is_some()
                || self.live_carry.is_some()
                || self.tail_carry.is_some()
                || self.implicit_throw_plans.contains_key(&load)
                || (assigned.is_some() && (self.carry_at_store(slot).is_some()
                    || self.tail_carry_at_store(slot).is_some()))
            {
                return false;
            }
            let base_source = self.resident_gpr(base);
            let index_source = self.resident_array_index(index);
            let lhs_source = self.resident_gpr(lhs);
            let ready = |input: NodeId, reg: Option<u8>| {
                reg.map_or_else(
                    || self.slot_of_checked(input).is_ok()
                        || matches!(self.graph.nodes[input as usize].op, Op::Const(_)),
                    |reg| !matches!(reg, RAX | RCX | super::RDX | super::RSP),
                )
            };
            if !ready(base, base_source) || !ready(index, index_source) || !ready(lhs, lhs_source) {
                return false;
            }
            let base_reg = base_source.unwrap_or(super::RDX);
            let index_reg = index_source.unwrap_or(RCX);
            let dst = assigned.unwrap_or(RAX);
            let work = if lhs_source != Some(dst) && matches!(dst, r if r == base_reg || r == index_reg) {
                RAX
            } else {
                dst
            };
            let addr = Operand::Mem(Mem::base_index(base_reg, index_reg, addr.scale, addr.disp));
            let Some(bytes) = encode_allocated_alu(op, ty, work, lhs_source.unwrap_or(work), addr)
            else {
                return false;
            };
            if base_source.is_none() {
                self.gp_load_value(super::RDX, base);
            } else {
                self.gp_reg_reads += 1;
            }
            if index_source.is_none() {
                self.gp_load_index_rcx(index);
            } else {
                self.gp_reg_reads += 1;
            }
            if lhs_source.is_none() {
                self.gp_load_value(work, lhs);
            } else {
                self.gp_reg_reads += 1;
            }
            self.buf.emit(&bytes);
            self.emit_mov_reg_reg64(dst, work);
            if assigned.is_some() {
                self.publish_integer_result(dst, slot);
                if ty == Ty::I32 && !self.poisons_int_def(id) {
                    self.gp_zx32_mask |= 1u16 << dst;
                }
            } else {
                self.rax_home_mirror = None;
                self.rax_reg_mirror = None;
                self.store_rax(slot);
            }
            true
        }

        /// Bind an integer operation to its assigned operands and result.
        /// Selected roots may publish a spilled result through RAX and the
        /// existing home/carry protocol. Destructive aliases use the reserved
        /// scratch pair. Split-managed results retain their existing path.
        /// These operations cannot trap or safepoint, and every refusal occurs
        /// before emission. The existing home-dropping and publication proofs
        /// remain responsible for the result's final location.
        pub(super) fn reg_alloc_direct_alu(&mut self, id: NodeId, slot: i32) -> bool {
            if !self.reg_alloc_on && !self.operand_in_place_on {
                return false;
            }
            let graph = self.graph;
            let Some(node) = graph.nodes.get(id as usize) else {
                return false;
            };
            let wide = match node.ty {
                IrType::Int => false,
                IrType::Long => true,
                _ => return false,
            };
            use crate::x64::isel::{MInst, Op as MachineOp};
            let selected = self
                .mir
                .as_ref()
                .and_then(|plan| plan.allocated_instruction(id));
            if let Some(MInst::Lea { addr, .. }) = selected {
                return self.reg_alloc_selected_lea(id, slot, addr, wide);
            }
            if let Some(tile @ MInst::AluRM { .. }) = selected {
                return self.reg_alloc_selected_array(slot, tile);
            }
            let machine_kind = |op| {
                Some(match op {
                    MachineOp::Add => DirectAlu::Add,
                    MachineOp::Sub => DirectAlu::Sub,
                    MachineOp::And => DirectAlu::And,
                    MachineOp::Or => DirectAlu::Or,
                    MachineOp::Xor => DirectAlu::Xor,
                    MachineOp::Imul => DirectAlu::Mul,
                    MachineOp::Shl => DirectAlu::Shl,
                    MachineOp::Sar => DirectAlu::Shr,
                    MachineOp::Shr => DirectAlu::UShr,
                    _ => return None,
                })
            };
            let (kind, mut a, mut b, selected_imm) = match selected {
                Some(MInst::AluRR { op, lhs, rhs, .. }) => {
                    let Some(kind) = machine_kind(op) else {
                        return false;
                    };
                    (kind, lhs, rhs, None)
                }
                Some(MInst::AluRI { op, lhs, imm, .. }) => {
                    let (Some(kind), Ok(imm)) = (machine_kind(op), i32::try_from(imm)) else {
                        return false;
                    };
                    let Some(&other) = node
                        .inputs
                        .iter()
                        .find(|&&input| input != lhs)
                        .or(node.inputs.first())
                    else {
                        return false;
                    };
                    (kind, lhs, other, Some(imm))
                }
                _ => {
                    let (Some(kind), Some(&a), Some(&b)) = (
                        DirectAlu::of(&node.op),
                        node.inputs.first(),
                        node.inputs.get(1),
                    ) else {
                        return false;
                    };
                    (kind, a, b, self.alu_imm32(b))
                }
            };
            let assigned = self.assigned_gpr(id);
            let d = match assigned {
                Some(reg) => reg,
                None => RAX,
            };
            if assigned.is_none()
                && selected.is_none()
                && matches!(
                    kind,
                    DirectAlu::Add | DirectAlu::Mul | DirectAlu::And | DirectAlu::Or | DirectAlu::Xor
                )
                && self.resident_gpr(a).is_some_and(|r| r != RAX)
                && self.resident_gpr(b) != Some(RAX)
                && selected_imm.is_none()
                && self.alu_imm32(a).is_none()
            {
                std::mem::swap(&mut a, &mut b);
            }
            let in_file = |r: &u8| IR_LOWER_LS_GPRS.contains(r) || IR_LS_CALLER_SAVED_GPRS.contains(r);
            if (!in_file(&d) && !(assigned.is_none() && d == RAX))
                || self.ls_managed.get(id as usize).copied().unwrap_or(false)
                || (assigned.is_some() && self.carry_at_store(slot).is_some())
            {
                return false;
            }
            if let Some(MInst::AluRR { op, ty, .. }) = selected {
                if matches!(op, MachineOp::Shl | MachineOp::Shr | MachineOp::Sar) {
                    use crate::x64::isel::{select, Operand, Req};
                    // An incoming carry has its own load order. Let the legacy
                    // shift arm consume it until the selected copy plan covers it.
                    for input in [a, b] {
                        if self.carry_names(input)
                            || self.tail_carry.is_some_and(|(prod, _, _)| prod == input)
                            || (self.resident_gpr(input).is_none()
                                && self.slot_of_checked(input).is_err()
                                && !matches!(graph.nodes[input as usize].op, Op::Const(_)))
                        {
                            return false;
                        }
                    }
                    let Ok(shift) = select(&Req::new(op, ty, Operand::Gpr(d), Operand::None))
                    else {
                        return false;
                    };
                    if self.resident_gpr(b) == Some(d) {
                        self.gp_load_value(RCX, b);
                        self.gp_load_value(d, a);
                    } else {
                        self.gp_load_value(d, a);
                        self.gp_load_value(RCX, b);
                    }
                    self.buf.emit(&shift.encoded.bytes);
                    if assigned.is_some() {
                        self.publish_integer_result(d, slot);
                        if !wide && !self.poisons_int_def(id) {
                            self.gp_zx32_mask |= 1u16 << d;
                        }
                    } else {
                        self.rax_home_mirror = None;
                        self.rax_reg_mirror = None;
                        self.store_rax(slot);
                    }
                    return true;
                }
            }
            let carry_a = self.reg_alloc_carried_gpr(a, id);
            let carry_b = self.reg_alloc_carried_gpr(b, id);
            let ra = carry_a
                .or_else(|| self.resident_gpr(a))
                .filter(|r| in_file(r) || carry_a == Some(*r));
            if ra.is_none()
                && !matches!(graph.nodes[a as usize].op, Op::Const(_))
                && self.slot_of_checked(a).is_err()
            {
                return false;
            }
            // A spilled constant can be rematerialized in the instruction's
            // immediate field without removing its snapshot-visible definition.
            let rb = match selected_imm.or_else(|| self.alu_imm32(b)) {
                Some(k) => DirectB::Imm(k),
                None => match carry_b
                    .or_else(|| self.resident_gpr(b))
                    .filter(|r| in_file(r) || carry_b == Some(*r))
                {
                    Some(r) => DirectB::Reg(r),
                    None => match self.slot_of_checked(b) {
                        Ok(off) => DirectB::Frame(off),
                        Err(_) => return false,
                    },
                },
            };
            // A two-address destination may alias the right operand. Resolve
            // that copy through the reserved scratch pair before publication.
            let needs_temporary =
                rb == DirectB::Reg(d) && ra != Some(d) && (kind == DirectAlu::Sub || ra.is_none());
            let compute_dst = if needs_temporary {
                if d == RAX {
                    RCX
                } else {
                    RAX
                }
            } else {
                d
            };
            let Some(bytes) = direct_alu_bytes(kind, wide, compute_dst, ra.unwrap_or(compute_dst), rb)
            else {
                return false;
            };
            if carry_a.is_some() {
                self.reg_alloc_consume_carried_gpr(a, id);
            }
            if carry_b.is_some() && b != a {
                self.reg_alloc_consume_carried_gpr(b, id);
            }
            if ra.is_none() {
                self.gp_load_value(compute_dst, a);
            }
            if compute_dst == RCX {
                self.note_rcx_written();
            }
            self.buf.emit(&bytes);
            if compute_dst == RAX || d == RAX {
                self.rax_home_mirror = None;
                self.rax_reg_mirror = None;
            }
            if compute_dst != d {
                self.emit_mov_reg_reg64(d, compute_dst);
            }
            if assigned.is_none() {
                self.store_rax(slot);
                return true;
            }
            if self.poisons_int_def(id) {
                self.emit_poison_int_high(d);
            }
            self.mark_gp_reg_live(id);
            if !wide && !self.poisons_int_def(id) {
                self.gp_zx32_mask |= 1u16 << d;
            }
            self.reg_publishes_at_def += 1;
            self.cur_def_published = true;
            if self.home_dropped.get(id as usize).copied().unwrap_or(false) {
                self.homes_dropped_at_def += 1;
                self.home_stores_dropped += 1;
            } else {
                self.store_abi_reg(d, slot);
            }
            self.reg_alloc_direct_defs += 1;
            true
        }

        /// W8-4: after lowering node `id` (its prefix join and post-call /
        /// post-static sites included): for a site-named routed result
        /// ([`routed_result`]) whose home was dropped, refuse the value when an
        /// inline prefix joined after the arm or no route published its
        /// register, else record `(id, publish end, node end)` for
        /// [`Self::verify_reg_alloc_routed_defs`]. A no-op with the allocator
        /// off or for any other node.
        pub(super) fn reg_alloc_check_routed_def(&mut self, id: NodeId, prefixed: bool) {
            let publish = self.reg_alloc_publish_at.take();
            if !self.reg_alloc_on {
                return;
            }
            let Some(node) = self.graph.nodes.get(id as usize) else {
                return;
            };
            if !routed_result(&node.op, node.ty)
                || !self.home_dropped.get(id as usize).copied().unwrap_or(false)
                || !self.site_named.get(id as usize).copied().unwrap_or(false)
            {
                return;
            }
            let why = match publish {
                _ if prefixed => Some("an inline prefix's hit route joins after its publish"),
                Some((p, _)) if p != id => Some("its lowering published another value"),
                None => Some("no route of its lowering published its register"),
                Some(_) => None,
            };
            if let Some(why) = why {
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: id },
                    format!("n{id} (a site-named routed result): {why}"),
                ));
                return;
            }
            if let Some((_, at)) = publish {
                self.reg_alloc_routed.push((id, at, self.buf.pos()));
            }
        }

        /// W8-4: after every branch of the body is patched, no instruction may
        /// transfer control into `(publish end, node end]` of a recorded routed
        /// result -- such a route would arrive with the home written and the
        /// register stale. Asked with [`branch_targets_except`] (W7 fix 4):
        /// the tail carries' conservative scan with its rel32 half narrowed to
        /// fields behind a branch, call or RIP-relative `LEA` byte (rel8
        /// branches and absolute words as before), with the post-call and
        /// post-static exit stubs' way back exempt (their verdict tail
        /// preserves every GPR). A hit refuses the value; a false hit only
        /// keeps its home.
        pub(super) fn verify_reg_alloc_routed_defs(&mut self) {
            if self.reg_alloc_routed.is_empty() || self.buf.overflowed() {
                return;
            }
            let len = self.buf.pos();
            let base = self.buf.as_ptr() as u64;
            let bad: Vec<NodeId> = if let Some(code) =
                self.buf.as_slice().get(..len).filter(|_| len <= crate::ir_lower::MAX_OFFSET_BITSET_INDEX)
            {
                let last = self
                    .reg_alloc_routed
                    .iter()
                    .map(|&(_, _, to)| to)
                    .max()
                    .unwrap_or(0)
                    .min(len);
                let words = (last >> 6) + 1;
                let mut wanted_bits = vec![0u64; words];
                for &(_, from, to) in &self.reg_alloc_routed {
                    let lo = from + 1;
                    let hi = to.min(last);
                    for off in lo..=hi {
                        wanted_bits[off >> 6] |= 1u64 << (off & 63);
                    }
                }
                let exempt_bits = if self.post_call_exit_backs.is_empty() {
                    Vec::new()
                } else {
                    let mut eb = vec![0u64; (len >> 6) + 1];
                    for &off in &self.post_call_exit_backs {
                        if off <= len {
                            eb[off >> 6] |= 1u64 << (off & 63);
                        }
                    }
                    eb
                };
                let has_exempt = !exempt_bits.is_empty();
                let mut hit_bits = vec![0u64; words];
                for i in 0..len {
                    let rel32_owner = (i >= 1 && matches!(code[i - 1], 0xE8 | 0xE9))
                        || (i >= 2 && code[i - 2] == 0x0F && matches!(code[i - 1], 0x80..=0x8F))
                        || (i >= 2 && code[i - 2] == 0x8D && code[i - 1] & 0xC7 == 0x05);
                    if rel32_owner && i + 4 <= len {
                        let rel = i32::from_le_bytes([code[i], code[i + 1], code[i + 2], code[i + 3]]);
                        let t = (i as i64 + 4 + i64::from(rel)) as u64;
                        if t <= last as u64 {
                            let tu = t as usize;
                            if (wanted_bits[tu >> 6] & (1u64 << (tu & 63))) != 0
                                && (!has_exempt || (exempt_bits[i >> 6] & (1u64 << (i & 63))) == 0)
                            {
                                hit_bits[tu >> 6] |= 1u64 << (tu & 63);
                            }
                        }
                    }
                    if i >= 1 && matches!(code[i - 1], 0x70..=0x7F | 0xE0..=0xE3 | 0xEB) {
                        let t = (i as i64 + 1 + i64::from(code[i] as i8)) as u64;
                        if t <= last as u64 {
                            let tu = t as usize;
                            if (wanted_bits[tu >> 6] & (1u64 << (tu & 63))) != 0 {
                                hit_bits[tu >> 6] |= 1u64 << (tu & 63);
                            }
                        }
                    }
                    if i + 8 <= len {
                        let mut w = [0u8; 8];
                        w.copy_from_slice(&code[i..i + 8]);
                        let v = u64::from_le_bytes(w);
                        let off = v.wrapping_sub(base);
                        if off <= last as u64 {
                            let tu = off as usize;
                            if (wanted_bits[tu >> 6] & (1u64 << (tu & 63))) != 0 {
                                hit_bits[tu >> 6] |= 1u64 << (tu & 63);
                            }
                        }
                    }
                }
                self.reg_alloc_routed
                    .iter()
                    .filter(|&&(_, from, to)| {
                        ((from + 1)..=to.min(last))
                            .any(|t| (hit_bits[t >> 6] & (1u64 << (t & 63))) != 0)
                    })
                    .map(|&(id, _, _)| id)
                    .collect()
            } else {
                let mut wanted: std::collections::HashSet<usize> = std::collections::HashSet::new();
                for &(_, from, to) in &self.reg_alloc_routed {
                    wanted.extend((from + 1)..=to);
                }
                let exempt: std::collections::HashSet<usize> =
                    self.post_call_exit_backs.iter().copied().collect();
                let hits = match self.buf.as_slice().get(..len) {
                    Some(code) => branch_targets_except(code, base, &wanted, &exempt),
                    None => wanted.clone(),
                };
                self.reg_alloc_routed
                    .iter()
                    .filter(|&&(_, from, to)| hits.iter().any(|&t| t > from && t <= to))
                    .map(|&(id, _, _)| id)
                    .collect()
            };
            let exempt: std::collections::HashSet<usize> = if bad.is_empty() {
                std::collections::HashSet::new()
            } else {
                self.post_call_exit_backs.iter().copied().collect()
            };
            for id in bad {
                // Name the field the scan counted, for the census: the op,
                // the window, and each branch field landing in it.
                let window = self
                    .reg_alloc_routed
                    .iter()
                    .find(|&&(v, _, _)| v == id)
                    .map(|&(_, from, to)| (from, to));
                let mut fields: Vec<String> = Vec::new();
                if let (Some((from, to)), Some(code)) = (window, self.buf.as_slice().get(..len)) {
                    for_each_branch_field(code, base, &exempt, |at, t, kind| {
                        if t > from && t <= to && fields.len() < 4 {
                            let op = code.get(at.saturating_sub(2)..at).unwrap_or(&[]);
                            fields.push(format!("{kind} at {at} (after {op:02x?}) -> {t}"));
                        }
                    });
                }
                let op = self
                    .graph
                    .nodes
                    .get(id as usize)
                    .map(|n| format!("{:?}", n.op))
                    .unwrap_or_default();
                let op = op.split([' ', '{', '(']).next().unwrap_or("?").to_string();
                self.latch_bailout(Bailout::with_context(
                    BailoutReason::UnallocatedValue { node: id },
                    format!(
                        "n{id} (a site-named routed result): a branch lands past its publish \
                         ({op}, window {window:?}: {})",
                        fields.join("; ")
                    ),
                ));
            }
        }

        /// W7 fix 5: per node, does a fall-through back-edge poll name it
        /// while it is dead at that latch's edge? Such a site-named value is
        /// refused there (`emit_fallthrough_poll_spills`: "cannot be stored
        /// there") and the attempt is lowered again without it, so the plan
        /// leaves it out from the start. The poll records `cur_bci`, which
        /// for a latch whose nodes carry a bytecode pc is the last one's
        /// (`lower_data_node` sets it per node); a latch with none inherits
        /// the bci of whatever was emitted before it, which the plan cannot
        /// know, and is left to the emission's refusal as before. A coalesced
        /// back-edge source is never left out here: its register holds it at
        /// its own edge (`reg_alloc_coalesced_live_at_edge`). Only ever keeps a
        /// home that the retry would have kept; all `false` with the
        /// allocator off.
        pub(super) fn reg_alloc_fallthrough_poll_dead(&self) -> Vec<bool> {
            let mut dead = vec![false; self.graph.nodes.len()];
            if !self.reg_alloc_on {
                return dead;
            }
            let Some(sl) = self.site_live.as_ref() else {
                return dead;
            };
            let blocks = &self.schedule.blocks;
            for (b, block) in blocks.iter().enumerate() {
                if block.terminator.is_some() || block.successors.first().is_none_or(|&s| s > b) {
                    continue;
                }
                let Some(bci) = block
                    .nodes
                    .iter()
                    .rev()
                    .find_map(|&id| self.graph.nodes.get(id as usize).and_then(|n| n.bytecode_pc))
                else {
                    continue;
                };
                let Some(&(_, edge)) = sl.span.get(b) else {
                    continue;
                };
                let resume = self.resume_bci(bci);
                for sp in &self.graph.safepoints {
                    if sp.bci != bci && sp.bci != resume {
                        continue;
                    }
                    for v in sp
                        .locals
                        .iter()
                        .chain(sp.stack.iter())
                        .chain(sp.monitors.iter())
                        .copied()
                        .chain(sp.sb_virtual_nodes())
                    {
                        let i = v as usize;
                        if v == NO_NODE
                            || self.phi_coalesced_src.get(i).copied().unwrap_or(false)
                            || site_value_live_at(sl, blocks, v, edge)
                        {
                            continue;
                        }
                        if let Some(cell) = dead.get_mut(i) {
                            *cell = true;
                        }
                    }
                }
            }
            dead
        }

        /// W7 increment 6: does a phi edge copy into `phi` stage the value in
        /// `phi`'s own register although `phi`'s home is KEPT, and then write
        /// the home from that register (`emit_copy_op`)? Only under the
        /// allocator, for an `int` / `long` phi with a callee-saved register of
        /// the file that is not split-managed.
        ///
        /// Why it is the RAX route's program: the RAX route reads the source
        /// into RAX, stores RAX into the home and copies RAX into the phi's
        /// register, all inside one `CopyOp`. Staging reads the source into the
        /// phi's register, then stores that register into the home: the same
        /// two locations end up holding the same value, both written inside
        /// the same `CopyOp`, so `resolve_parallel_copy`'s cross-op order (every
        /// source read before anything writes it) is untouched -- the argument
        /// the home-dropped staging already rests on. A phi whose register is
        /// another copy's source on this edge is deferred and never staged
        /// (`defer_publish`). RAX is no longer written; nothing on the edge
        /// reads RAX after a copy (each op stages afresh).
        pub(super) fn reg_alloc_stages_kept_phi(&self, phi: NodeId) -> bool {
            if !self.reg_alloc_on {
                return false;
            }
            let Some(node) = self.graph.nodes.get(phi as usize) else {
                return false;
            };
            matches!(node.op, Op::Phi)
                && matches!(node.ty, IrType::Int | IrType::Long)
                && self
                    .assigned_gpr(phi)
                    .is_some_and(|r| IR_LOWER_LS_GPRS.contains(&r))
                && !self.ls_managed.get(phi as usize).copied().unwrap_or(false)
        }

        /// W7-A fix 2: `home_is_one_store_rax`, plus (allocator on) the
        /// primitive field reads [`load_writes_its_home_once`] audits.
        ///
        /// A call an inline prefix may claim is not one (W7 fix 3): every
        /// prefix declines a call whose result home is dropped, so dropping
        /// it turned the prefix back into the call (`regex-find-ci`'s
        /// `String.charAt` went from 26 248 to 40 533 instructions per op).
        pub(super) fn reg_alloc_home_is_one_store(&self, node: &super::Node) -> bool {
            self.home_is_one_store_rax(node)
                || (self.reg_alloc_on
                    && (load_writes_its_home_once(&node.op, node.ty)
                        || (routed_result(&node.op, node.ty)
                            && !self.reg_alloc_call_may_take_prefix(node))))
        }

        /// W7 fix 3: may an inline prefix claim `node` (an `Op::Call`)? The
        /// prefixes' own site screens (`call_info_admits_inline_prefix`, a
        /// superset).
        fn reg_alloc_call_may_take_prefix(&self, node: &super::Node) -> bool {
            match node.op {
                Op::Call { info_ptr } => self.call_info_admits_inline_prefix(info_ptr),
                _ => false,
            }
        }

        /// The census key of a kept home: its `ir-home-why` reason, and for
        /// `op` the defining op's kind (`op(Call)`, `op(Load)`, ...).
        fn reg_alloc_why_key(&self, id: usize, node: &super::Node) -> Option<String> {
            let k = *self.home_keep_why.get(id)?;
            let name = HK_NAMES.get(usize::from(k)).copied().unwrap_or("other");
            if k == HK_OP {
                let full = format!("{:?}", node.op);
                let kind = full
                    .split([' ', '{', '('])
                    .next()
                    .unwrap_or("?")
                    .to_string();
                return Some(format!("op({kind})"));
            }
            Some(name.to_string())
        }

        /// W7-A fix: the summed block frequency of phi `phi`'s coalesced
        /// back-edge sources that are still site-naming candidates -- the
        /// definition stores the phi's site naming removes besides its own
        /// edge copies, for `plan_site_named_homes`' spill-cost rule. `0` for
        /// anything else and with the allocator off (the rule unchanged).
        pub(super) fn reg_alloc_coalesced_source_freq(
            &self,
            phi: NodeId,
            candidate: &[bool],
        ) -> f64 {
            if !self.reg_alloc_on {
                return 0.0;
            }
            let Some(node) = self.graph.nodes.get(phi as usize) else {
                return 0.0;
            };
            if !matches!(node.op, Op::Phi) {
                return 0.0;
            }
            let freq = &self.schedule.freq.freq;
            node.inputs
                .iter()
                .skip(1)
                .filter(|&&s| {
                    s != NO_NODE
                        && self
                            .phi_coalesced_src
                            .get(s as usize)
                            .copied()
                            .unwrap_or(false)
                        && candidate.get(s as usize).copied().unwrap_or(false)
                })
                .filter_map(|&s| {
                    self.schedule
                        .node_to_block
                        .get(s as usize)
                        .copied()
                        .filter(|&b| b < self.schedule.blocks.len())
                })
                .filter_map(|b| freq.get(b).copied())
                .filter(|w| w.is_finite())
                .sum()
        }

        /// W7-A fix: is `v` read by the PAD site `at` itself (the snapshot at
        /// `at`'s own bci), and still in its register there? The site model
        /// asks for a read STRICTLY after the site, because at a value's last
        /// read the node being lowered may take its register for its own
        /// result. That cannot happen when the node's own result is not
        /// assigned that register: a pad-only op's arm
        /// (`op_traps_only_through_pads`) reads its operands into scratch
        /// registers, jumps to its pads before it defines anything, and writes
        /// no allocatable register but its own result's. So at every pad of
        /// `at`, `v`'s register is `v`. This names a single-use value at its
        /// consumer's own guard (an index at the bounds check, a divisor at
        /// the zero check).
        pub(super) fn reg_alloc_read_at_site(&self, v: NodeId, at: NodeId) -> bool {
            if !self.reg_alloc_on || at == NO_NODE || v == NO_NODE {
                return false;
            }
            let (Some(node), Some(vn)) = (
                self.graph.nodes.get(at as usize),
                self.graph.nodes.get(v as usize),
            ) else {
                return false;
            };
            if matches!(node.op, Op::Phi)
                || !op_traps_only_through_pads(&node.op)
                || !matches!(vn.ty, IrType::Int | IrType::Long)
                || !node.inputs.contains(&v)
            {
                return false;
            }
            let Some(reg) = self.gp_reg_of.get(v as usize).copied().flatten() else {
                return false;
            };
            self.gp_reg_of.get(at as usize).copied().flatten() != Some(reg)
                && !self.ls_managed.get(v as usize).copied().unwrap_or(false)
                && !self.ls_managed.get(at as usize).copied().unwrap_or(false)
        }

        /// The `CRATONVM_DBG_JITC` engagement line, printed per lowering with
        /// the switch on or off:
        ///
        /// * `resident`: `int` / `long` values holding a GP register
        ///   (coalesced sources included), `single_use_resident` of them read
        ///   once;
        /// * `site_named` / `phis` / `coalesced`: values whose home is written
        ///   only where a reader needs it, and how many are phis and coalesced
        ///   back-edge sources;
        /// * `def_stores`: definition home stores emitted; `def_stores_dropped`
        ///   and `tail_elided`: the ones not emitted;
        /// * `need_point_stores`: homes written at the points that need them,
        ///   as `inspect_spills` (inline, before a call / allocation / monitor
        ///   / helper) plus `slow_spills` (a back-edge poll's slow path);
        ///   `spills_deduped` were covered on every path already;
        /// * `excluded`: values an earlier attempt refused;
        /// * `phis_int` / `phis_resident` / `phis_home_dropped` (dropped by any
        ///   mechanism, the default `phi_home_droppable` one included),
        ///   `coalesced_src` / `coalesced_home_dropped`,
        ///   `single_use_site_named` / `single_use_home_dropped`, and
        ///   `kept=reason:count,..`: resident `int` / `long` values whose home
        ///   is still written, by their `ir-home-why` reason (`HK_NAMES`;
        ///   `op(<kind>)` names the defining op; empty without the census),
        ///   and `single_use_kept` the same for those read once.
        pub(super) fn reg_alloc_census_line(&self) -> String {
            let mut resident = 0usize;
            let mut single_use = 0usize;
            let (mut phis, mut phis_resident, mut phis_dropped) = (0usize, 0usize, 0usize);
            let (mut coalesced, mut coalesced_dropped) = (0usize, 0usize);
            let (mut su_named, mut su_dropped) = (0usize, 0usize);
            let mut kept: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut su_kept: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            for (id, node) in self.graph.nodes.iter().enumerate() {
                if !matches!(node.ty, IrType::Int | IrType::Long) {
                    continue;
                }
                let dropped = self.home_dropped.get(id).copied().unwrap_or(false);
                let named = self.site_named.get(id).copied().unwrap_or(false);
                let is_phi = matches!(node.op, Op::Phi);
                if is_phi {
                    phis += 1;
                }
                if self.phi_coalesced_src.get(id).copied().unwrap_or(false) {
                    coalesced += 1;
                    coalesced_dropped += usize::from(dropped);
                }
                if self.gp_reg_of.get(id).copied().flatten().is_none() {
                    continue;
                }
                resident += 1;
                if is_phi {
                    phis_resident += 1;
                    phis_dropped += usize::from(dropped);
                }
                let once = self.use_count.get(id).copied() == Some(1);
                if once {
                    single_use += 1;
                    su_named += usize::from(named);
                    su_dropped += usize::from(dropped);
                }
                if !dropped {
                    if let Some(key) = self.reg_alloc_why_key(id, node) {
                        if once {
                            *su_kept.entry(key.clone()).or_default() += 1;
                        }
                        *kept.entry(key).or_default() += 1;
                    }
                }
            }
            let render = |m: &std::collections::BTreeMap<String, usize>| -> String {
                m.iter()
                    .map(|(k, c)| format!("{k}:{c}"))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            format!(
                "[cratonvm-jitc] ir-reg-alloc method={} resident={resident} \
                 single_use_resident={single_use} site_named={} phis={} coalesced={} \
                 def_stores={} def_stores_dropped={} tail_elided={} need_point_stores={} \
                 inspect_spills={} slow_spills={} spills_deduped={} excluded={} \
                 phis_int={phis} phis_resident={phis_resident} phis_home_dropped={phis_dropped} \
                 coalesced_src={coalesced} coalesced_home_dropped={coalesced_dropped} \
                 single_use_site_named={su_named} single_use_home_dropped={su_dropped} \
                 single_use_kept={} kept={} direct_defs={} kept_phi_stages={} reg_alloc={}",
                self.graph.scope_method_key,
                self.site_named_count,
                self.site_named_phis,
                self.reg_alloc_coalesced_named,
                self.def_home_stores,
                self.homes_dropped_at_def + self.carry_stores_dropped,
                self.tail_stores_elided,
                self.inspection_spills_emitted + self.poll_slow_spills_emitted,
                self.inspection_spills_emitted,
                self.poll_slow_spills_emitted,
                self.inspection_spills_deduped,
                self.elide_excluded.iter().filter(|e| **e).count(),
                render(&su_kept),
                render(&kept),
                self.reg_alloc_direct_defs,
                self.reg_alloc_kept_phi_stages,
                if self.reg_alloc_on { "on" } else { "off" },
            )
        }
    }
}

// JIT round 25 wave 7, lane W7-A: the register-allocator increment's tests.
// Every arm pins `CRATONVM_JIT_IR_REG_ALLOC` and the three switches it implies,
// so the suite passes with any of them exported.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_w7a_reg_alloc_tests;
/// W7-C: the planner, the gate and the sunk return check. Every test pins the
/// switch it reads, so each passes with the switch unset and set.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25w7c_tail_republish_sink_tests;

// JIT round 25 wave 8, lane W8-4 (`CRATONVM_JIT_IR_REG_ALLOC`, increment 2):
// call results and static reads as site-named producers, verified per value at
// emission, and the parameter copy implied by the allocator.
#[cfg(test)]
#[cfg(target_arch = "x86_64")]
mod r25_w8_4_reg_alloc_tests;

/// JIT round 25 wave 8 (lane W8-7): an ordinary OSR entry at an INNER loop
/// header must not skip a pre-header guard of the enclosing loop
/// (`osr_entries_inside_a_predicated_loop`,
/// `CRATONVM_JIT_IR_OSR_INNER_PREDICATE_REFUSAL`).
#[cfg(test)]
mod r25w8_inner_header_predicate_refusal_tests;

/// JIT round 25 wave 9 (lane W9-5, `CRATONVM_JIT_IR_OSR_RECHECK_CLASS_PREDICATES`):
/// a loop class predicate planted at the OUTER header's pre-header, the way
/// `ir_optimize::class_predicate` plants it (`Guard{bci: header}(ExactClassIs(o,
/// C))` on the loop's entry control, under the loop-entry frame). W8-7 refuses
/// the inner header's entry; with the re-check the entry is admitted and its
/// stub re-tests the seeded receiver: a `C` runs the body, anything else
/// (another class, null) leaves for the interpreter at the entry's header.


#[cfg(test)]
mod r22_ffm_const_tests;

/// t1 wave 7 lane NBD, DISPATCH21-4 (`CRATONVM_JIT_IR_IC_CAST_KIND_SCREEN_ELIDE`):
/// which receivers a dominating `checkcast` proves plain objects.
#[cfg(test)]
mod t1_nbd_ic_cast_kind_screen_tests;

/// JIT round 21, lane smallfix (REVJIT221-2, 574bc22865), re-landed by t1
/// wave 8 lane RL-MISC: a certainly stub-only `Op::NewArray` route (no bump
/// offered) emits an armed site's held dead-home clears in front of its map,
/// as `Op::New`'s stub-only route does (`NewSiteClears::FrontOfMap`). Text pin
/// over the default arm (the route needs a full lowering to reach).
#[cfg(test)]
mod t1w8_rlmisc_newarray_stub_only_clear_tests;

/// t1 wave 8 (lane OSRREFS, OSRREFS21-1/-2): the census key the
/// `CRATONVM_JIT_IR_OSR_DATAFLOW_LIVE_IN_REFS=0` arm counts for what the
/// reference half would open.
#[cfg(test)]
mod t1w8_osrrefs_census_tests;

/// JIT round 21, lane argwin (OOM21-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`): the optimizing tier's
/// owned-arguments hand-off and its callee window -- the site rule, the
/// switches, and the bytes.
#[cfg(test)]
mod jr21_argwin_lower_tests;

/// JIT round 21, lane g1vh (CF21-1): the G1 plan of the constant-`VarHandle`
/// reference write modes ([`VhG1Gates`]), its bytes EXECUTED against fake
/// compact objects in a fake G1 arena (256-byte regions), with the post edge
/// patched to the access followed by a flag store, as the optimizing tier's
/// prefix follows it with `jit_g1_post_write_barrier`.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_g1vh_vh_const_g1_tests;

#[cfg(test)]
mod jr21_osrrefs_tests;

/// JIT round 21 lane stress (FUZZ21-1 / FUZZ21-3 / FUZZ21-7): the stress
/// emitters and the parsing of their switches.
#[cfg(test)]
mod r21_stress_lower_tests;

/// t1 wave 10 (lane REFS3, `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE`): the
/// `refersTo` expansion's compare-only referent read reaches the checked
/// `jit_getfield` helper with `GETFIELD_NO_KEEPALIVE` (HotSpot's `refersTo0`
/// load is `AS_NO_KEEPALIVE`); with the switch at `0` it does not.
#[cfg(test)]
mod t1w10_refs3_no_keepalive_tests;

/// JIT round 21, lanes alloc + oom: the held dead-home clears of an `Op::New`
/// / `Op::NewArray` site run exactly once in front of every map of its route,
/// and never on a sunk fast path, under every combination of
/// `CRATONVM_JIT_IR_ZGC_NEW_MAP_SINK` / `CRATONVM_JIT_IR_NEWARRAY_MAP_SINK`
/// (sunk or not) and `CRATONVM_JIT_IR_HELPER_SITE_DEAD_HOME_CLEAR`.
#[cfg(test)]
mod jr21_alloc_oom_clear_tests;

/// JIT round 21, lane hashlock (`CRATONVM_INFLATED_MARK_HASH`): the compiled
/// identity-hash read answers an INFLATED compact word that carries its hash
/// bits exactly as `ObjectHeader::identity_hash` does, and still declines an
/// INFLATED word without them and every thin-locked word.
#[cfg(test)]
mod jr21_hashlock_inflated_hash_read_tests;
#[cfg(test)]
mod jr21_hash2_class_table_tests;

/// JIT round 21, lane strings2 (STRINGS21-1): the inline
/// `StringLatin1.toLowerCase` / `toUpperCase` prefix
/// (`Lowerer::try_emit_ir_latin1_case_map_prefix`) through the real lowering,
/// EXECUTED. This harness has no TLAB and no compact-strings flag, so the
/// allocation is refused at compile time and the prefix answers only the
/// unchanged string (`str` itself, the JDK's `first == len` identity) and
/// declines everything else to the call -- which pins the operand order, the
/// null / non-`byte[]` / null-`Locale` declines and every Latin-1 byte's scan
/// verdict. The passes themselves are executed in `x64::latin1_case_emit`.
#[cfg(test)]
mod jr21_strings2_latin1_case_map_prefix_tests;

/// JIT round 21, lane vhcae (`CRATONVM_JIT_VH_CONST_RET_SAME_LOADER`; page
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// a reference `compareAndExchange` whose return is cast to the field's own
/// class (`(Node) NEXT.compareAndExchange(h, a, b)`) is served when the site's
/// class has the coordinate class's loader; the reference exchange's bytes,
/// EXECUTED under the generational gates, hand over CMPXCHG's witness.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_vhcae_vh_const_cae_tests;

/// JIT round 21 (lane tl3, TL221-1, `CRATONVM_JIT_IR_REFERENT_GATED_INLINE`):
/// a spliced `Reference.referent` read is inline behind the SATB pre-gate byte.
#[cfg(test)]
mod jr21_tl3_referent_gate_tests;

/// JIT round 21, lane tcmiss (TCPERF21-1, `CRATONVM_JIT_TYPECHECK_NONCALL`):
/// a type check no longer destroys the XMM bank for the register allocator,
/// and its slow path gives every caller-saved XMM resident back.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcmiss_typecheck_noncall_tests;

/// JIT round 21 (lane lambdacap; `CRATONVM_JIT_IR_LAMBDA_PROXY_KEEPS_CONTEXT`):
/// a body that built a capturing lambda's proxy keeps the context ABI of its
/// single-pass twin after escape analysis removed the proxy, so the supersede
/// forward and the late-static cells can take it; `=0` restores the old
/// answer, and a body that never built one is unchanged.
#[cfg(test)]
mod jr21_lambdacap_context_tests;

/// TLGET21-3 close (`CRATONVM_JIT_IR_LOOP_ENTRY_DEAD_HOME_CLEARS`): a home an
/// in-loop site clears moves to the loop's entry block when nothing in the
/// loop writes its colour, and stays when something does.
#[cfg(test)]
mod loop_entry_dead_home_clear_tests;

/// JIT round 21, lane charat (STRINTR21-5,
/// `jr21-strintr2-charat-loop-per-char-node-cost-FIXED-20261101.md`): the String
/// char read's lowering, executed, and the guard a dominating fact proves.
#[cfg(test)]
mod jr21_charat_tests;

/// Round rx27 wave 1 (lane alloc): the `call=` field of the `ir new-site` /
/// `ir newarray-site` census lines names, per compiled allocation site, the
/// one reason its fast path calls Rust per allocation, in the order the
/// lowering decides it (a stub route first, then the post-init and announce
/// choices of an emitted bump).
#[cfg(test)]
mod rx27_alloc_site_census_tests;
#[cfg(test)]
mod rx27_frame_block_descriptor_tests;

/// JIT r21 tc21b: the one target-side rule of the compiled `instanceof`
/// display MISS arm, shared by the IR and the single-pass tier.


#[cfg(test)]
mod loop_return_splice_tests;

#[cfg(test)]
mod allocated_result_tests;

#[cfg(test)]
mod selected_value_flow_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod outlined_field_result_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod poll_safepoint_map_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod exact_floor_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod memory_default_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod epoch_param_proof_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod dead_home_clear_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod colored_field_proof_tests;

#[cfg(test)]
mod allocated_selection_tests;

#[cfg(all(test, target_arch = "x86_64"))]
mod read_bounds_live_tests;

#[cfg(test)]
mod netty_branch_scan_boundaries;

mod virtual_object_info;
#[allow(unused_imports)]
pub use virtual_object_info::*;
#[allow(unused_imports)]
use virtual_object_info::*;
mod mir_plan_and_init;
#[allow(unused_imports)]
pub use mir_plan_and_init::*;
#[allow(unused_imports)]
use mir_plan_and_init::*;
mod vector_preloop;
#[allow(unused_imports)]
pub use vector_preloop::*;
#[allow(unused_imports)]
use vector_preloop::*;
mod value_phi_copies;
#[allow(unused_imports)]
pub use value_phi_copies::*;
#[allow(unused_imports)]
use value_phi_copies::*;
mod field_proof_entry_is_rechecked_osr;
#[allow(unused_imports)]
pub use field_proof_entry_is_rechecked_osr::*;
#[allow(unused_imports)]
use field_proof_entry_is_rechecked_osr::*;
mod int_expr_bounds;
#[allow(unused_imports)]
pub use int_expr_bounds::*;
#[allow(unused_imports)]
use int_expr_bounds::*;
mod character_to_chars_prefix;
#[allow(unused_imports)]
pub use character_to_chars_prefix::*;
#[allow(unused_imports)]
use character_to_chars_prefix::*;
mod getfield_legacy_read;
#[allow(unused_imports)]
pub use getfield_legacy_read::*;
#[allow(unused_imports)]
use getfield_legacy_read::*;
mod post_call_exit_site_for;
#[allow(unused_imports)]
pub use post_call_exit_site_for::*;
#[allow(unused_imports)]
use post_call_exit_site_for::*;
mod entry_fold_node;
#[allow(unused_imports)]
pub use entry_fold_node::*;
#[allow(unused_imports)]
use entry_fold_node::*;
mod call_loaded_ic_entry_releasing;
#[allow(unused_imports)]
pub use call_loaded_ic_entry_releasing::*;
#[allow(unused_imports)]
use call_loaded_ic_entry_releasing::*;
mod fallthrough_poll_spills;
#[allow(unused_imports)]
pub use fallthrough_poll_spills::*;
#[allow(unused_imports)]
use fallthrough_poll_spills::*;
mod ir_ic_cascade_admits_kind;
#[allow(unused_imports)]
pub use ir_ic_cascade_admits_kind::*;
#[allow(unused_imports)]
use ir_ic_cascade_admits_kind::*;
mod implicit_kind_matches_catch;
#[allow(unused_imports)]
pub use implicit_kind_matches_catch::*;
#[allow(unused_imports)]
use implicit_kind_matches_catch::*;
mod scoped_raw_tail;
#[allow(unused_imports)]
pub use scoped_raw_tail::*;
#[allow(unused_imports)]
use scoped_raw_tail::*;
mod frame_needs;
#[allow(unused_imports)]
pub use frame_needs::*;
#[allow(unused_imports)]
use frame_needs::*;
mod back_edge_dead_ref_homes;
#[allow(unused_imports)]
pub use back_edge_dead_ref_homes::*;
#[allow(unused_imports)]
use back_edge_dead_ref_homes::*;
mod block_lazy_clear_survivors;
#[allow(unused_imports)]
pub use block_lazy_clear_survivors::*;
#[allow(unused_imports)]
use block_lazy_clear_survivors::*;
mod promoted_pending_mark;
#[allow(unused_imports)]
pub use promoted_pending_mark::*;
#[allow(unused_imports)]
use promoted_pending_mark::*;
mod site_value_live_at;
#[allow(unused_imports)]
pub use site_value_live_at::*;
#[allow(unused_imports)]
use site_value_live_at::*;
mod plan_register_residency_with;
#[allow(unused_imports)]
pub use plan_register_residency_with::*;
#[allow(unused_imports)]
use plan_register_residency_with::*;
mod inner_attempt;
#[allow(unused_imports)]
pub use inner_attempt::*;
#[allow(unused_imports)]
use inner_attempt::*;
mod g1_aastore_screen;
#[allow(unused_imports)]
pub use g1_aastore_screen::*;
#[allow(unused_imports)]
use g1_aastore_screen::*;
mod scoped_session_confined_rows;
#[allow(unused_imports)]
pub use scoped_session_confined_rows::*;
mod fast_barriers;
#[allow(unused_imports)]
use fast_barriers::*;
mod block_cse;
#[allow(unused_imports)]
use block_cse::*;
#[cfg(test)]
mod opcost_barriers_tests;
mod w7_deferred_tails;
mod w7_epoch_cover;
#[cfg(test)]
mod w7_ir_lower_tests;
#[allow(unused_imports)]
use scoped_session_confined_rows::*;
