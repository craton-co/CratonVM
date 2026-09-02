// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! x86-64 JIT code emitter for JVM bytecode methods.
//!
//! Compiles JVM bytecode directly to x86-64 machine code.
//!
//! # Error-handling discipline (NEW-7)
//!
//! This module is on the JIT compilation hot path and must never panic
//! in production builds. A stray panic during codegen would tear down
//! the interpreter caller and leak machine code pages.
//!
//! The `#![cfg_attr(not(test), deny(...))]` gate below makes clippy
//! refuse to compile this module in a release build when any of the
//! following appear in non-test code:
//!
//! - `.unwrap()` / `.expect()` — return `None` from `jit_scan` or the
//!   `compile` entry to let the caller fall back to the interpreter.
//! - `panic!()` / `unimplemented!()` / `todo!()` — same treatment.
//!
//! Tests inside `#[cfg(test)] mod tests { ... }` are exempt — assertion
//! panics are the standard test failure mechanism.

#![cfg_attr(not(test), deny(clippy::panic, clippy::unimplemented, clippy::todo,))]
//!
//! ## Calling Convention (matches C ABI)
//!
//! **Windows x64**: args in rcx, rdx, r8, r9; return in rax
//! **SysV (Linux/Mac)**: args in rdi, rsi, rdx, rcx, r8, r9; return in rax
//!
//! All JVM values are passed as i64 (ints are sign-extended to 64 bits).
//!
//! ## Register Allocation
//!
//! - Locals 0..N are stored in a stack frame area, addressed via `[rbp - offset]`
//! - The operand stack is simulated at compile time (static stack mapping)
//! - Most values flow through rax/rcx/rdx as temporaries
//!
//! ## Stack Layout (after prologue)
//!
//! ```text
//! [rbp + 16]    = return address (pushed by call)
//! [rbp + 8]     = saved rbp
//! [rbp]         = ← rbp points here
//! [rbp - 8]     = local 0
//! [rbp - 16]    = local 1
//! [rbp - 24]    = local 2
//! ...
//! [rbp - N*8]   = local N-1
//! [rbp - (N+1)*8 .. ] = operand stack spill area
//! ```

use super::{CompiledMethod, ExecutableBuffer, JitInvokeInfo};
use cratonvm_jit_api::JitRuntimeHelpers;
// JEP 358 (helpful NPE) inline-codegen path: the canonical operation-kind
// vocabulary baked into the inline null-check failure stubs. Single source of
// truth in jit-api (the VM crate maps these same codes to HotSpot strings).
use cratonvm_jit_api::npe_action;
#[allow(unused_imports)]
use cratonvm_types::narrow_oop::{narrow_base, narrow_oops_enabled};
use cratonvm_types::{
    ARRAY_DATA_OFFSET, ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET,
    FIELD_CELL_PAYLOAD64_OFFSET, FIELD_CELL_TAG_OFFSET, HEADER_SIZE, SLOT_SIZE,
};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};

mod cpu_features;
// Re-exported at the visibility these queries had before the split: the
// intrinsic-registration tests (`jit/tests/intrinsic_crc32.rs`) assert that a
// hardware intrinsic registers exactly when the host reports the feature.
pub use cpu_features::{
    has_avx2, has_bmi1, has_lzcnt, has_pclmulqdq, has_popcnt, has_sse41, has_sse42,
};

// ---------------------------------------------------------------------------
// Switch-instruction validation helpers (HIGH security, task #8)
// ---------------------------------------------------------------------------
//
// Moved to `x64/switch_validation.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod switch_validation;
pub use switch_validation::*;
// ---------------------------------------------------------------------------
// Register encoding for x86-64
// ---------------------------------------------------------------------------
//
// Moved to `x64/reg_encoding.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod reg_encoding;
pub use reg_encoding::*;
// ---------------------------------------------------------------------------
// Checked memory-operand displacement encoding
// ---------------------------------------------------------------------------
//
// Every ModRM/SIB displacement this file emits should be built by
// `disp::Disp` rather than by narrowing an offset into a literal byte:
// `x as u8` on a value the CPU reads back as a SIGNED disp8 addresses memory
// BEFORE the base register the moment `x` exceeds 127, with no fault and no
// assembler complaint. See `x64/disp.rs` for the hazard, the RBP/R13 and
// RSP/R12 addressing special cases, and the boundary tests.
//
// Declared `pub mod` (not `mod` + glob re-export like its siblings) so the
// fully-qualified `disp::Disp` path stays available to the other backends;
// the named re-export below keeps `Disp` spellable bare inside this file.
pub mod disp;
pub use disp::{base_requires_displacement, base_requires_sib, disp8_const, Disp, DispOutOfRange};
// ---------------------------------------------------------------------------
// Instruction patterns and instruction selection
// ---------------------------------------------------------------------------
//
// This declaration was missing until 2026-08-01, so `x64/isel.rs` had never
// been compiled: its declarative pattern table, and the byte-for-byte
// equivalence sweep against the hand-written emitters below that is the only
// reason to trust that table, had never been seen by a compiler or run.
//
// Declared `pub mod` rather than `mod` + glob re-export because the table and
// the selector are addressed by qualified path (`isel::select_block`), and a
// glob would drop several hundred pattern-row constants into this file's
// namespace.
pub mod isel;
// ---------------------------------------------------------------------------
// SIMD loop analysis and vectorization
// ---------------------------------------------------------------------------
//
// Moved to `x64/simd_analysis.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod simd_analysis;
pub use simd_analysis::*;
// ---------------------------------------------------------------------------
// Bytecode compatibility check
// ---------------------------------------------------------------------------
//
// Moved to `x64/bytecode_compat.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod bytecode_compat;
pub use bytecode_compat::*;
// ---------------------------------------------------------------------------
// Loop-Invariant Code Motion (LICM)
// ---------------------------------------------------------------------------
//
// Moved to `x64/licm.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod licm;
pub use licm::*;
pub(crate) mod stack_kinds;
// ---------------------------------------------------------------------------
// HIGH-1 / Fix 1 — null-check elimination helper
// ---------------------------------------------------------------------------
//
// Moved to `x64/null_check_elim.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod null_check_elim;
pub use null_check_elim::*;
// ---------------------------------------------------------------------------
// Escape analysis
// ---------------------------------------------------------------------------
//
// Moved to `x64/escape_analysis.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod escape_analysis;
pub use escape_analysis::*;

// PERF-01: the enumeration of what the single-pass backend can do that the
// optimizing tier cannot. The admission chain consults it.
mod single_pass_only;
pub use single_pass_only::*;
// ---------------------------------------------------------------------------
// Integer-arithmetic LICM
// ---------------------------------------------------------------------------
//
// Moved to `x64/licm_int.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod licm_int;
pub use licm_int::*;
// ---------------------------------------------------------------------------
// Array Bounds Check Elimination (BCE)
// ---------------------------------------------------------------------------
//
// Moved to `x64/bce.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod bce;
pub use bce::*;
// ---------------------------------------------------------------------------
// Vector (SIMD) emission
// ---------------------------------------------------------------------------
//
// Lives in `x64/vec_emit.rs`. The `pub use` keeps every
// existing path resolving; a glob re-export caps each item at its own
// declared visibility, so nothing here became more public than it was.
mod vec_emit;
pub use vec_emit::*;
mod driver;
pub use driver::*;
mod loop_rewrite;
pub use loop_rewrite::*;
pub mod bytecode_walk;
/// Test-only view of the E27-1 N2b compile-time needle screen.
///
/// The screen decides which `String.indexOf(int)` sites the backend will
/// inline, and it is the reason N2b is not a deopt cliff — so it is worth a
/// test, and a test needs a way in. See
/// `bytecode_walk::prev_insn_int_const` for the reasoning.
#[doc(hidden)]
pub fn prev_insn_int_const_for_test(code: &[u8], code_len: usize, pc: usize) -> Option<i32> {
    bytecode_walk::prev_insn_int_const(code, code_len, pc)
}

mod inlining;
/// Engagement count for the splice cursor clamp, for `jit-method-stats`.
/// A number beside a result is what says whether the guard ran at all.
pub(crate) use inlining::inline_live_slot_clamps;
/// The PC -> inline-chain map, and the per-compile session that records it.
///
/// NAMED rather than glob re-exported, unlike the ~15 `pub use foo::*;`
/// siblings above. A glob would be capped at each item's own declared
/// visibility and so would be sound, but `inlining` is not a lowering module
/// with one entry point: it is the splice emitter, and most of what is `pub`
/// in it is a hook the walk calls. These five items ARE its interface to the
/// rest of the tree -- `jit/src/lib.rs` names `InlineFrameMap` for the
/// `CompiledMethod` field, `x64/driver.rs` opens and closes the session
/// around codegen, and `vm/src/jit/conservative_roots.rs` reads
/// `InlineFrameLevel` out of the finished map to expand a compiled frame into
/// the inlined callees it is standing inside.
///
/// Without this line none of them can name the types at all: the module was
/// private and unexported, which is why the whole producer shipped inert and
/// every item in it still carries `#[allow(dead_code)]`. `InlineFrameRow` is
/// deliberately NOT exported -- it is the emission-order form, consumed by
/// `finish_inline_frame_recording` and meaningless outside it.
pub use inlining::{
    begin_inline_frame_recording, begin_npe_trap_recording, finish_inline_frame_recording,
    finish_npe_trap_recording, inline_call_map_at_return_counts, inline_frame_map_enabled,
    inline_miss_edge_poison_counts, npe_trap_lines_enabled, InlineFrameLevel, InlineFrameMap,
    NpeTrapMap, NpeTrapSite,
};
mod arith;
mod arrays;
mod deopt_stubs;
mod objects;
pub(crate) use objects::note_ungated_ref_store;
pub use objects::ref_store_site_counts;
pub(crate) use objects::note_gated_ref_store;
pub use null_check_elim::receiver_null_check_counts;
pub use null_check_elim::receiver_null_check_implicit_count;
mod osr;
mod simd;

/// Test-only switch that makes every inlined body publish deopt metadata.
///
/// `try_emit_inline` refuses a splice whose body published any (PGO-02 §3 —
/// an inlined scope is not representable in deopt metadata, so a point
/// recorded inside one describes a stack that never existed). A guard nobody
/// can make fire is a guard nobody has tested, and no production emitter
/// produces this state today; this is how
/// `inline_publishing_a_deopt_point_is_refused` produces it deliberately.
#[cfg(test)]
thread_local! {
    pub(crate) static INLINE_TEST_PUBLISHES_DEOPT: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}
mod emit;
mod frames;
mod operand_stack;
pub mod safepoint;
// ---------------------------------------------------------------------------
// Compile bytecode to x86-64
// ---------------------------------------------------------------------------

/// Simulated operand stack slot — tracks where each value is.
/// During compilation, the operand stack is mapped to stack frame offsets.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum StackSlot {
    /// Value is at [rbp - offset]. Offset is always positive (below rbp).
    Frame(i32),
    /// Value is in a callee-saved register (R12-R15). Zero-cost push: no code
    /// emitted until the value is consumed. This eliminates the store+load
    /// round-trip when a register-mapped local is loaded and then immediately
    /// used by an arithmetic or branch operation.
    CalleeSaved(u8),
    /// Value is in a caller-saved scratch register (R8/R9). Used as a
    /// deferred-spill cache: `push_from_rax` moves the result into a scratch
    /// register instead of storing to the frame, avoiding the store+load
    /// round-trip when the value is consumed by the very next operation.
    /// Scratch slots MUST be flushed before any call, backward branch, or return.
    ///
    /// **A home offset was carried here for one day and reverted.** The idea
    /// was to bound the spill region -- `flush_scratch_registers` reserves a
    /// fresh word per flushed value, so a stretch with several calls grows it
    /// once per call. Reserving the home at PUSH time instead made
    /// `push_from_rax` advance the spill cursor where it previously did not,
    /// and that shipped a nondeterministic heap corruption:
    /// `RMapGcStress` went from PASS to "duplicate insert" / an
    /// `ArrayIndexOutOfBoundsException` inside `String.equals`, and
    /// `CRATONVM_JIT_KERNEL_REG_LOCALS=0` -- which makes this whole path inert
    /// -- was what made it pass again. The frame-growth defect is real and
    /// still open; whatever fixes it must not move this cursor, because the
    /// OSR entry's local homes are derived from the same layout.
    Scratch(u8),
    /// Value is in an XMM register (XMM0-XMM15). Used for FP locals loaded via
    /// dload/fload from XMM-allocated locals. Avoids the XMM→RAX→frame round-trip
    /// when the value is immediately consumed by a double/float arithmetic op.
    /// Like Scratch, these MUST be flushed before calls.
    Xmm(u8),
}

/// Scratch registers available for deferred-spill caching.
/// R8 and R9 are caller-saved on both Windows x64 and SysV ABIs.
/// R10 is excluded because it is used internally by bounds checks and SIMD loops.
const SCRATCH_REGS: [u8; 2] = [R8, R9];

/// Where a live oop resides at a shadow-stack safepoint — the source the push
/// reads from and the destination the post-call reload writes back to.
/// `Reg` is a callee-saved home register (R12–R15/RBX/RSI/RDI); `Frame(off)`
/// is the canonical frame slot `[rbp - off]`. (Scratch/XMM entries never hold a
/// live oop across a call: scratch is flushed pre-call and XMM holds FP data.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ShadowHome {
    Reg(u8),
    Frame(i32),
}

/// Register mapping for locals → callee-saved registers.
/// R12-R15 + RBX on all platforms. On Windows, RSI and RDI are also callee-saved.
#[cfg(target_os = "windows")]
pub const LOCAL_REGS: [u8; 7] = [R12, R13, R14, R15, RBX, RSI, RDI];

/// XMM register numbers for float/double local allocation.
/// XMM8-XMM15 are used to avoid conflicts with arithmetic temporaries (XMM0/XMM1).
/// On Windows x64: XMM6-15 are callee-saved; we use XMM8-15 (8 registers).
/// On Linux x86-64: all XMMs are caller-saved in SysV ABI; we save/restore them anyway
/// since Rust may hold live values there when calling JIT-compiled functions.
pub const LOCAL_XMMS: [u8; 8] = [8, 9, 10, 11, 12, 13, 14, 15];

/// Scratch XMM registers for FP intermediate persistence across bytecodes.
/// XMM2-XMM7 bridge the gap between temporaries (XMM0-1) and locals (XMM8-15).
/// When a FP binop produces a result in XMM0, it can be promoted to a scratch XMM
/// to free XMM0 for the next operation, avoiding the XMM0→frame spill/reload cycle.
/// These are caller-saved and must be flushed before calls and backward branches.
const SCRATCH_XMMS: [u8; 6] = [2, 3, 4, 5, 6, 7];

#[cfg(not(target_os = "windows"))]
pub const LOCAL_REGS: [u8; 5] = [R12, R13, R14, R15, RBX];

/// JIT compiler state.
struct Compiler {
    /// Human-readable method key used only by env-gated diagnostics.
    method_label: String,
    buf: ExecutableBuffer,
    /// Simulated operand stack — maps JVM stack positions to frame offsets.
    stack: Vec<StackSlot>,
    /// Next available frame offset (negative, below locals area).
    next_spill_offset: i32,
    /// Operand-spill cursor captured by the most recent
    /// `emit_pre_safepoint_spill`, consumed by the matching
    /// `emit_oop_map_for_safepoint` as `OopMapEntry::live_frame_hi`. `0` when
    /// no spill was emitted for this safepoint, which the GC reads as
    /// "unknown" and handles by scanning the whole frame.
    pending_live_frame_hi: i32,
    /// Base spill offset (first slot after locals).
    base_spill_offset: i32,
    /// Exclusive end of the operand-stack spill area.
    spill_limit_offset: i32,
    /// Number of local variable slots.
    num_locals: usize,
    /// Number of parameter slots.
    num_params: usize,
    /// Number of locals mapped to callee-saved registers.
    num_reg_locals: usize,
    /// Per-local register assignment from graph-coloring allocator.
    /// `local_assignments[i] = Some(reg)` means local i is in that register.
    local_assignments: Vec<Option<u8>>,
    /// Which register-homed locals a GC-capable safepoint actually has to
    /// publish to their canonical frame slots (see
    /// [`super::regalloc::SafepointPublishPlan`]).
    ///
    /// `None` on paths that do not build it — the legacy [`compile`] test
    /// wrapper, OSR artifact compilation, and (as a deliberate cost gate) any
    /// method where no local received a register home, since there the plan
    /// provably cannot change any consumer's answer. Every consumer must fall
    /// back to the conservative "any register home at all" behaviour, which is
    /// what the whole file did before 2026-07-26.
    ///
    /// Set by `compile_with_param_slots` immediately after `Compiler::new`
    /// rather than being threaded through that already-18-argument constructor;
    /// `local_assignments` is final once `new` returns and is never mutated
    /// afterwards, so the plan cannot go stale.
    safepoint_publish: Option<super::regalloc::SafepointPublishPlan>,
    /// One-shot, and TWO independent claims because the two registers are
    /// published by different code:
    ///
    /// * `.0` — every Java argument of this call is in a frame slot, so
    ///   `ARG_REGS` hold nothing that is not already published;
    /// * `.1` — the site's own staging loop wrote through `RAX`, so RAX's last
    ///   value is one of those staged arguments.
    ///
    /// The direct-call sites stage through `R11`, not RAX, so they claim `.0`
    /// only — and only when `reserve_direct_call_service_slots` actually
    /// reserved (`service_args_base.is_some()`); with no service slots the
    /// arguments live in `ARG_REGS` and NOWHERE else, and the claim is false.
    /// The dispatch and MIC/PIC sites stage through RAX and claim both, `.1`
    /// only when `n > 0` so the loop really ran.
    ///
    /// Set ONLY by `emit_pre_safepoint_spill_args_published`, which calls the
    /// spill immediately afterwards, and taken at the top of the spill — so
    /// there is no path on which a claim survives into a later safepoint that
    /// staged nothing. See `spill_args_published_enabled`.
    args_published_for_next_spill: (bool, bool),
    /// Narrowed blind-spill selection captured at the safepoint that raised
    /// `sink_alloc_blind_spill`, as a bitmask over `ALL_SPILL_GPRS` positions
    /// (`None` = spill every register). The inline-TLAB `new` site splits its
    /// blind spill across two program points; the deferred half must use the
    /// SAME selection the safepoint used, or the union of the two halves would
    /// not equal what one unsplit spill wrote. Recomputing at the slow-path
    /// label would not do: the operand-stack model has moved on by then.
    pending_narrow_spill: Option<u16>,
    /// Per-basic-block live-in local sets `(block_start_pc, live_in_bitset)`
    /// from the allocator — used to build per-OSR-entry-PC dead-local masks so
    /// the OSR trampoline skips loading locals dead at the entry (a dead local
    /// would otherwise clobber a live one that shares its coalesced register).
    osr_block_live_in: Vec<(usize, u64)>,
    /// Callee-saved GPR registers actually used (for prologue/epilogue).
    alloc_used_regs: Vec<u8>,
    /// Per-local XMM register assignment. `None` means the local spills to the frame.
    xmm_assignments: Vec<Option<u8>>,
    /// XMM registers actually used for locals (for prologue/epilogue save/restore).
    alloc_used_xmms: Vec<u8>,
    /// Mapping from bytecode PC to native code offset (for branch patching).
    pc_to_native: Vec<i32>,
    /// OSR entry offset per bytecode PC.
    ///
    /// Identical to `pc_to_native` for every PC *except* loop headers that
    /// carry LICM-hoisted preheader code. `pc_to_native[header]` is set
    /// *after* the hoisted code so the in-loop back-edge skips re-running it;
    /// but an OSR entry that jumped there would run the loop body with
    /// uninitialised hoist spill slots (garbage array pointers → SIGSEGV,
    /// stale arithmetic → wrong result). `osr_entry_native[header]` therefore
    /// points *before* the hoisted preheader so an OSR entry executes the
    /// hoist initialisation exactly like a normal fall-through entry would.
    osr_entry_native: Vec<i32>,
    /// DBG (env-gated diagnostics): last bytecode pc/op the main emit loop
    /// dispatched, so a codegen bail can report where it gave up.
    dbg_last_pc: usize,
    dbg_last_op: u8,
    /// RBC.6 — an `athrow` (0xbf) was lowered in this method. Forces
    /// `has_dispatch` so the interpreter's compiled-entry paths take the
    /// dispatch-aware route that drains the pending JIT exception (the
    /// `!has_dispatch` fast path returns the raw value without draining).
    emitted_athrow: bool,
    /// A live monitorenter/monitorexit runtime call was emitted. The VM helper
    /// needs the per-thread JIT TLS even on an otherwise call-free method, so
    /// this forces dispatch-aware entry.
    emitted_monitor_call: bool,
    /// This method uses frame-preserving exception exits for handlers that
    /// read non-parameter locals (RBC.6 precise-handler continuation).
    precise_exception_frames: bool,
    /// The inlined-splice scope stack — one entry per splice currently being
    /// emitted, outermost first.
    ///
    /// `docs/jit/deopt-frame-state-interning.md` §5.1 named this as the
    /// remaining producer edit for the single-pass backend: "it needs one
    /// pushed at the splice and popped at the callee's return". Each entry is
    /// the CALLER's frame captured at the invoke — a `FrameState` whose `bci`
    /// names the call in progress and whose operand stack has already had the
    /// callee's arguments removed, which is what
    /// `ResumeSemantics::for_caller_scope()` (`RESUME`) means and what the VM's
    /// `caller_resume_pc` requires.
    ///
    /// Empty on every compile that splices nothing, so `inline_caller_chain`
    /// answers `None` and the published metadata is byte-identical to what it
    /// was before this existed.
    pub(super) inline_scope_stack: Vec<crate::deopt::FrameState>,

    /// `[start_pc, end_pc)` ranges covered by this method's exception table.
    /// Empty when the method has no handlers. Consulted only by
    /// [`Compiler::pc_is_protected`]; see `PROTECTED_RANGES_REQUEST`.
    protected_ranges: Vec<(u32, u32)>,
    /// This method's exception table as `(start_pc, end_pc, handler_pc, catch
    /// type name)`, non-empty exactly when compiled local handlers are ARMED
    /// for this compile (see the arming conditions in `driver.rs`). An empty
    /// name is a catch-all.
    ///
    /// Non-empty is what makes handler bodies live code: the walk seeds each
    /// `handler_pc` as a reachability root and as a branch target whose
    /// incoming operand stack is the JVMS `[exception]` — depth one, marked as
    /// a reference. Empty ⇒ byte-identical codegen to before the feature.
    pub(super) local_handler_table: Vec<(usize, usize, usize, &'static str)>,
    /// The compiling method's declaring class id — the loader context a catch
    /// type name resolves through at runtime. Meaningful only alongside a
    /// non-empty [`Self::local_handler_table`].
    pub(super) local_handler_class_id: u32,
    /// One [`crate::JitLocalHandlerSite`] per throwing bci that got a local
    /// dispatch, in emission order. Moved onto the published `CompiledMethod`,
    /// which is what keeps the addresses baked into the stubs valid.
    pub(super) local_handler_sites: Vec<Box<crate::JitLocalHandlerSite>>,
    /// `throw bci -> index into `local_handler_sites``, so two fallible
    /// operations at the same bci share one site and one cache.
    local_handler_site_by_bci: FxHashMap<usize, usize>,
    /// Pending local-handler stubs: `(rel32 patch offset of the guard's branch,
    /// site index, throw bci, whether the propagate edge is a reason-9 deopt
    /// rather than the shared sentinel exit)`.
    ///
    /// Emitted by `emit_local_handler_stubs` after the body walk, because a
    /// stub jumps FORWARD to handler blocks whose native offsets only exist
    /// once the walk has passed them.
    local_handler_stubs: Vec<(usize, usize, usize, bool)>,
    /// `LoopXform::bci_of` when this compile is emitting REWRITTEN bytecode
    /// (see `plan_bytecode_loop_xform`), `None` on every ordinary compile.
    ///
    /// When present, the `pc` the emitter walks is an output PC of the
    /// rewritten method and is **not** an interpreter bci — output PCs run
    /// past the end of the interpreter's method. Everything that leaves this
    /// compile as a bci must therefore go through [`Compiler::orig_bci`].
    /// There are exactly four such places, and all four bake the value as an
    /// immediate into machine code, so none of them can be fixed by a
    /// post-pass over the finished `CompiledMethod`:
    ///
    ///  * [`Compiler::emit_bounds_check_stubs`] — arg 4 of `jit_throw_aioobe`;
    ///  * [`Compiler::emit_exception_check_stub`] — the arg of `set_throw_bci`,
    ///    which `execute_jit_call` range-tests against this method's own
    ///    exception table;
    ///  * [`Compiler::emit_deopt_stubs`] — arg 3 of `jit_uncommon_trap`;
    ///  * the `athrow` (`0xbf`) lowering — arg 2 of `jit_throw_exception`,
    ///    range-tested by `route_jit_exception_through_method`.
    ///
    /// `OopMapEntry::bytecode_pc` is deliberately NOT translated: it is only
    /// ever compared against the value this same codegen stores into the
    /// frame's safepoint-id slot (`emit_pre_safepoint_spill`), so both sides
    /// live in output-PC space and translating one of them would make
    /// `find_oop_map_for_safepoint_id`'s `.find()` ambiguous across copies.
    bci_provenance: Option<Vec<u32>>,
    /// `[start, end)` of the versioning pre-header guard's SYNTHETIC bytes in
    /// the pc space being emitted, when a versioned bytecode loop rewrite is in
    /// effect. `None` on every ordinary compile and on an unversioned rewrite.
    ///
    /// Provenance is total, so these bytes answer [`Compiler::orig_bci`] with
    /// the loop header's bci — but they are an image of no original
    /// instruction: `encode_preheader_guard` synthesised them, and part-way
    /// through them the abstract operand stack is not the header's. Anything
    /// that would publish a pc to the VM *as* a bci must therefore refuse here
    /// rather than translate. Today that is exactly the OSR-entry table and the
    /// OSR-exit snapshot keyed off it; `rewritten_deopt_points_are_publishable`
    /// is the fail-closed backstop that catches a future one.
    synthetic_guard_span: Option<(usize, usize)>,
    /// An allocation OOM bail (`emit_post_alloc_oom_check`) was emitted in this
    /// method — by `newarray` (0xbc), `anewarray` (0xbd), or `new` (0xbb).
    /// Forces `has_dispatch` for the SAME thread-availability reason as
    /// `direct_calls` above: the fallible alloc helpers (`jit_newarray` /
    /// `jit_anewarray_object` / `jit_new_object`) read the per-thread
    /// `JIT_THREAD` TLS (via `jit_thread_mut()`) BOTH to run the
    /// allocation-failure STW GC and to construct the catchable
    /// `OutOfMemoryError` / `NegativeArraySizeException`. The `!has_dispatch`
    /// fast entry path skips `set_jit_thread`, so without this a JIT'd
    /// allocating method would run the helper with a null thread — no GC on
    /// young-gen pressure, and on genuine exhaustion the throwable is never
    /// created so the `i64::MIN` bail sentinel leaks as the method's (truncated)
    /// return value (e.g. `new int[N]` silently yields 0) instead of throwing.
    emitted_alloc_oom_check: bool,
    /// Residual-6 companion fix: set when a `checkcast` site emitted the
    /// post-helper sentinel check. `jit_checkcast` constructs the
    /// `ClassCastException` for a definitively-failed cast through the
    /// `JIT_THREAD` TLS (same requirement — and same `has_dispatch` forcing —
    /// as [`Self::emitted_alloc_oom_check`]); without the dispatch-aware
    /// entry the helper silently degrades to the legacy null-return.
    emitted_checkcast_throw: bool,
    /// Set when an `aastore` site emitted the element-type check call.
    /// `jit_aastore_type_check` builds the `ArrayStoreException` through the
    /// JIT_THREAD TLS and bails with the `i64::MIN` sentinel, so the method
    /// must be entered through the dispatch-aware path — same requirement,
    /// and the same reason, as `emitted_checkcast_throw`.
    emitted_aastore_throw: bool,
    /// Forward branch patches: (native offset of rel32, target bytecode PC).
    forward_patches: Vec<(usize, usize)>,
    /// Jump table patches: (native offset of i32 entry, table_base_native_offset, target bytecode PC).
    /// Each entry stores a RIP-relative offset from the table base to the target native code.
    jump_table_patches: Vec<(usize, usize, usize)>,
    /// Self-call patches: native offset of the rel32 to patch to entry point.
    self_call_patches: Vec<usize>,
    /// Frame size (total stack allocation).
    frame_size: i32,
    /// Whether method needs heap pointer (hidden first arg).
    needs_heap: bool,
    /// Frame offset where heap pointer is stored (valid when needs_heap is true).
    heap_local_offset: i32,
    /// Frame offset where allocation-heavy methods cache this invocation's
    /// `*mut JvmThread` for inline TLAB `new`. 0 when no cache slot is reserved.
    jit_thread_slot_off: i32,
    /// Frame slot holding the prologue-cached native-stack floor for the
    /// inline self-call check (`0` = not reserved -> sites emit the plain
    /// guard-helper CALL).
    stack_floor_slot_off: i32,
    /// Frame offset of the first XMM save slot (from RBP).
    xmm_saved_base: i32,
    /// Resolved multianewarray metadata: (bytecode_pc, leaf_element_type_code).
    multianewarray_info: Vec<(usize, i64)>,
    /// Resolved field access metadata: (bytecode_pc, field_index, type_tag).
    /// type_tag is b'I', b'J', b'F', b'D', b'L', or b'['.
    field_info: Vec<(usize, usize, u8)>,
    /// Compact reference-field layout: `pc -> (byte_offset, is_ref)` for inline
    /// getfield/putfield codegen (no helper call / runtime lookup). Empty when
    /// the flag is off → the inline emitters use the legacy path.
    compact_field_off: std::collections::HashMap<usize, (u32, bool)>,
    /// Resolved typecheck metadata: (bytecode_pc, class_name_ptr, class_name_len).
    /// The class name string is leaked for 'static lifetime so the JIT code can reference it.
    typecheck_info: Vec<(usize, *const u8, usize)>,
    /// Resolved static field metadata: (bytecode_pc, class_id_raw, field_index, type_tag, is_volatile).
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    /// LICM: loop-invariant aaload hoisting info.
    hoist_info: Vec<LoopHoist>,
    /// LICM: frame offsets for hoisted values (one per LoopHoist entry).
    hoist_offsets: Vec<i32>,
    /// LICM: loop-invariant integer-arithmetic hoisting info.
    arith_hoist_info: Vec<ArithLoopHoist>,
    /// LICM: frame offsets for hoisted arithmetic values (one per
    /// `arith_hoist_info` entry).
    arith_hoist_offsets: Vec<i32>,
    /// LICM: frame offset of the first shared arith-LICM scratch slot.
    arith_scratch_base: i32,
    /// LICM: loop-invariant `arraylength` hoisting info.
    array_len_hoist_info: Vec<ArrayLenHoist>,
    /// LICM: frame offsets for hoisted array lengths (one per
    /// `array_len_hoist_info` entry, shared by all of that entry's sites).
    array_len_hoist_offsets: Vec<i32>,
    /// Frame offset of the first callee-saved register slot (from RBP).
    callee_saved_base: i32,
    /// SIMD: vectorizable loops detected during analysis.
    simd_loops: Vec<SimdIntArraySum>,
    /// Guarded tight-loop lowering for read-only `int[][]` dot products.
    matrix_dot_loops: Vec<MatrixDotLoop>,
    /// Bounds check elimination: bytecode PCs where bounds checks can be skipped
    /// because loop analysis proved the access is always in-bounds.
    bounds_safe_pcs: FxHashSet<usize>,
    /// Deferred out-of-line bounds-check failure stubs: (branch_patch_offset, bc_pc).
    /// After the main bytecode loop, we emit the slow-path code for each.
    bounds_check_stubs: Vec<(usize, usize)>,
    /// Round-8 CRIT fix (audit `round8-jit.md`, "false-promise abort" item):
    /// deferred null-check failure stubs for inline array load/store/length
    /// opcodes (iastore / bastore / aastore / lastore / fastore / dastore /
    /// castore / sastore plus the load + `arraylength` siblings). The inline
    /// bounds check would otherwise dereference the null array pointer at
    /// `[NULL + ARRAY_LENGTH_OFFSET]`, hitting the signal-handler hs_err path
    /// that just re-raises and kills the VM.
    ///
    /// Each entry is `(action, patch_offset, trap_key)`: `action` is the JEP-358
    /// [`cratonvm_jit_api::npe_action`] code for the trapping opcode (so the
    /// interpreter can attach "Cannot load from int array" etc. when
    /// `-XX:+ShowCodeDetailsInExceptionMessages` is on), and `patch_offset` is
    /// the rel32 placeholder of the `TEST RAX,RAX; JZ rel32`. At method end
    /// [`Self::emit_null_check_store_stubs`] groups entries by `action` and
    /// emits ONE stub per distinct code, each calling `jit_npe_with_action(code)`
    /// (which sets `JIT_PENDING_NPE` + the action + the deopt flag) and exiting
    /// via the method epilogue with `RAX = i64::MIN`. The interpreter's post-JIT
    /// path drains the NPE flag and surfaces the (action-only) exception.
    /// (Previously a single shared stub called `jit_bastore(0)`, which set the
    /// byte-store action for *every* opcode regardless of element type.)
    ///
    /// `trap_key` is the id `x64::inlining::record_npe_trap_site` issued for
    /// this site, or `0` for "not described". A non-zero key buys the site its
    /// own ten-byte cold trampoline, which packs `action | key << 8` into the
    /// helper's single argument so the NPE snapshot can put a LINE on the frame
    /// that raised. `0` keeps the historical shape exactly: the `JZ` goes
    /// straight to the shared per-action stub.
    null_check_store_stubs: Vec<(u8, usize, u32)>,
    /// Post-invoke exception checks. After every JIT-dispatched invoke
    /// (`invoke_dispatch` / `invoke_virtual_mic`) whose callee can throw,
    /// the codegen emits a `CMP RAX, i64::MIN; JE rel32` guard. The
    /// dispatch helpers return `i64::MIN` (instead of 0) when they stash a
    /// pending Java exception into `JIT_PENDING_EXCEPTION`. Each recorded
    /// offset is the rel32 placeholder of that JE; `emit_exception_check_stub`
    /// patches them all to a single shared out-of-line stub that loads the
    /// `i64::MIN` deopt sentinel and runs the epilogue. The interpreter's
    /// post-JIT path then drains `take_jit_pending_exception()` and routes
    /// the real exception through the method's exception table — instead of
    /// letting the JIT keep running with a bogus `0` return value (which
    /// previously masked the true exception with a downstream NPE; see the
    /// Jetty `Main.main` "getClasspath on null" miscompile).
    /// `(patch_offset, throw_site_bci)` -- the bci is the bytecode pc of
    /// the fallible operation whose cold path branches here, so the stub
    /// can stamp it onto the pending-exception signal (see
    /// `JitRuntimeHelpers::set_throw_bci`).
    exception_check_stubs: Vec<(usize, usize)>,
    /// Speculative BCE: deopt guards to emit at loop headers.
    /// Each guard checks that array.length >= loop_bound before entering the loop.
    speculative_bce_guards: Vec<SpeculativeBCEGuard>,
    /// Speculative BCE guards indexed by their loop-header PC, built once from
    /// `speculative_bce_guards`. The per-header bytecode emit loop looks guards
    /// up here in O(1) instead of re-scanning the whole guard vector at every
    /// loop header.
    speculative_bce_guards_by_header: FxHashMap<usize, Vec<SpeculativeBCEGuard>>,
    /// Resolved `new` (0xbb) metadata.
    ///
    /// Tuple layout (CRIT-2):
    ///   (bytecode_pc, class_id_raw, num_fields,
    ///    has_nonzero_tag_primitive_init,
    ///    has_finalizer)
    ///
    /// The last two flags gate whether the inline TLAB fast path must
    /// invoke `jit_post_tlab_init`. When both are `false`, the JIT
    /// inlines the header completion (identity-hash + num_slots) and
    /// skips the helper call entirely. See `emit_inline_tlab_new`.
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    /// DEFERRED `new` (0xbb) sites: `(bytecode_pc, holder_class_id, cp_idx)`.
    ///
    /// A `new` whose target class was not loaded when this method was
    /// compiled. There is no class id or field count to bake, so the site
    /// compiles to a `new_object_cp` helper call carrying the *referencing*
    /// class id + the constant-pool index; the helper resolves, initialises
    /// and allocates on first execution, exactly like the interpreter's 0xbb
    /// handler. Disjoint from `new_info` by construction (a pc is in exactly
    /// one of the two). See `jit_api::JitRuntimeHelpers::new_object_cp`.
    new_deferred_info: Vec<(usize, u32, u16)>,
    /// Resolved `anewarray` (0xbd) metadata: (bytecode_pc, component_class_id_raw).
    anewarray_info: Vec<(usize, u32)>,
    /// DEFERRED `anewarray` (0xbd) sites: `(bytecode_pc, holder_class_id,
    /// cp_idx)` — the `anewarray` sibling of `new_deferred_info`, served by
    /// the `anewarray_object_cp` helper.
    anewarray_deferred_info: Vec<(usize, u32, u16)>,
    /// Invoke dispatch info: (bytecode_pc, pointer to leaked JitInvokeInfo).
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    /// Resolved `invokedynamic` (0xba) call-site info, needed ONLY for
    /// stack-effect bookkeeping (arg pop count + result push kind) at the
    /// unconditional-deopt codegen site — see the 0xba arm in
    /// `compile_bytecode`. Entry: `(bytecode_pc, arg_slot_count, return_type_tag)`
    /// where `arg_slot_count` is `count_param_slots(descriptor)` — one
    /// simulated slot per argument regardless of category, matching every
    /// other invoke-arg-popping site in this file — and `return_type_tag` is
    /// the descriptor's return type byte (`b'V'` for void). Popping is
    /// type-agnostic (`self.pop_stack()` returns any `StackSlot` kind), so
    /// only the count is needed for args; only the *push* (the call's result)
    /// needs the type, to select the correctly-shaped placeholder value.
    /// Populated during the same CP-resolution pre-pass as `invoke_info`,
    /// from `jit_scan`'s `indy_ops`. A method whose invokedynamic site cannot
    /// be resolved here (no resolver, or the resolver returns `None`) bails
    /// the whole compile (`x64::compile` returns `None`) rather than
    /// guessing — see `try_compile_inner`.
    ///
    /// The 4th tuple element (`indy_arg_type_tags`) is the descriptor's
    /// per-argument JVM type tag sequence, in push order — used ONLY by the
    /// OSR-exit/uncommon-trap deopt snapshot (`build_and_record_deopt_point`)
    /// to precisely type this call's own arguments on the operand stack
    /// instead of falling back to the coarse per-method `wide_fp` gate. See
    /// `indy_arg_type_tags`'s doc comment.
    indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)>,
    /// Direct call targets: (bytecode_pc, direct call info).
    /// For invokestatic/invokespecial where the callee is already JIT-compiled.
    direct_calls: Vec<(usize, super::JitDirectCall)>,
    /// Monomorphic inline cache slots: (bytecode_pc, MIC slot pointer).
    /// For invokevirtual/invokeinterface call sites.
    mic_slots: Vec<(usize, *const super::JitMICSlot)>,
    /// Polymorphic inline cache slots: (bytecode_pc, PIC slot pointer).
    /// For invokevirtual/invokeinterface call sites. A populated
    /// `pic_slots` entry supersedes the MIC for the same `pc` (PIC
    /// is a 4-entry superset).
    ///
    /// HIGH-7 wiring (now active): `jit/src/lib.rs::try_compile`
    /// eagerly allocates one `Box<JitPICSlot>` per polymorphic call
    /// site (invokevirtual / invokeinterface) and passes the
    /// `(pc, *const JitPICSlot)` pairs into `x64::compile()` via the
    /// new `pic_slots` parameter, which assigns this field. Slots
    /// start empty; the runtime helper populates them on miss, after
    /// which subsequent invocations take the inline 4-way cascade.
    pic_slots: Vec<(usize, *const super::JitPICSlot)>,
    /// Task #60 — Helper-call patch sites (Design B for shift-safe unrolling).
    ///
    /// Each entry is the native offset of the 4-byte rel32 immediate inside
    /// an `E8 rel32` CALL emitted by [`emit_call_absolute`]. The unroll
    /// duplicator uses this to re-resolve each copy's helper rel32 against
    /// the original helper address (which is shifted-PC-invariant): the
    /// copy's rel32 is recomputed as `helper_addr - (copy_pc + 5)` so the
    /// duplicated CALL lands on the same helper, not on `helper + shift`.
    ///
    /// Only sites that fit in ±2GB (i.e. used the rel32 form, not the
    /// 12-byte imm64-via-RAX fallback) are recorded; the fallback path
    /// already contains an absolute imm64 and is shift-safe by
    /// construction.
    helper_call_patches: Vec<usize>,
    /// Native offsets of RIP-relative `disp32` fields that address a FIXED
    /// ABSOLUTE address (today: the safepoint flag, see
    /// [`emit_test_mem8_abs_imm8`]), paired with the number of instruction
    /// bytes that follow the displacement.
    ///
    /// A RIP-relative displacement is measured from the address of the NEXT
    /// instruction, so the trailing count matters: `TEST BYTE [rip+d32], imm8`
    /// carries its `imm8` after the displacement and its reference point is
    /// `disp32_offset + 4 + 1`, not `+ 4`.
    ///
    /// Serves the same purpose as [`Self::helper_call_patches`] and for the
    /// same reason: the unroll duplicator copies body bytes verbatim, and a
    /// displacement that was right at the original site addresses
    /// `target + shift` from the copy. Each copy is re-resolved against the
    /// reconstructed absolute target.
    rip_abs_disp32_patches: Vec<(usize, usize)>,
    /// Task #60 — IC (inline cache) patch sites for per-clone slot allocation.
    ///
    /// Each entry is `(native_offset_of_imm64, kind, original_slot_ptr)` where
    /// `kind == 0` for MIC and `kind == 1` for PIC. The native offset points at
    /// the 8-byte little-endian imm64 inside a 10-byte `MOV R10, imm64`
    /// (encoded via [`emit_mov_imm64_full`] so the imm64 lives at a
    /// deterministic offset regardless of operand value). The unroll
    /// duplicator mints a fresh `Box<JitMICSlot>` / `Box<JitPICSlot>` per
    /// IC site per copy and overwrites the duplicated imm64 to point at
    /// the new slot, so per-iteration cache hits do not collide across
    /// unrolled copies. All immediates carrying the same original slot pointer
    /// are rewritten to the same clone: this includes both the inline guard and
    /// the slow helper's MIC/PIC arguments.
    ic_patches: Vec<(usize, u8, usize)>,
    /// Task #60 — clone-minted MIC/PIC slots owned by the compiler.
    ///
    /// The byte-copy unroll duplicator allocates fresh `Box<JitMICSlot>` /
    /// `Box<JitPICSlot>` for every IC site it duplicates. Those boxes must
    /// outlive the compiled method (the imm64 baked into the emitted code
    /// is a raw pointer to them), so they are stashed here at duplication
    /// time and transferred to `CompiledMethod._jit_mic_slots` /
    /// `_jit_pic_slots` at finalize. Caller-supplied slots remain owned by
    /// the caller (in `lib.rs::try_compile`); these vectors are *additive*.
    cloned_mic_slots: Vec<Box<super::JitMICSlot>>,
    cloned_pic_slots: Vec<Box<super::JitPICSlot>>,
    /// Loop unrolling: (header_pc, back_edge_pc, extra_copies).
    /// Small loops where the back-edge goto can be unrolled with extra iterations.
    /// extra_copies is the number of additional body copies (1 for 2x, 3 for 4x).
    unroll_loops: Vec<(usize, usize, usize)>,
    /// Native offset right after the prologue (for tail-call jumps).
    body_entry_offset: usize,
    /// PGO branch hints: maps bytecode PC → is_usually_taken.
    ///
    /// When present, branch emission adds an x86 branch prediction prefix:
    /// - `0x3E` (taken hint) when `is_usually_taken == true`
    /// - `0x2E` (not-taken hint) when `is_usually_taken == false`
    branch_hints: FxHashMap<usize, bool>,
    /// PGO loop unroll hints: maps back-edge bytecode PC → unroll factor.
    loop_unroll_hints: FxHashMap<usize, usize>,
    /// Resolved ldc/ldc_w constants: (bytecode_pc, i64 value).
    ldc_info: Vec<(usize, i64)>,
    /// String-`ldc` sites: `(bytecode_pc, referencing class id, CP index)`.
    /// Served by `helpers.ldc_string_cp`, the exact twin of the class row
    /// below — and CP-indexed for one more reason than the mirror is: JVMS
    /// §5.4.3 resolves a constant-pool entry ONCE and records the result, and
    /// that record is keyed `(class, cp index)`. The predecessor shape baked
    /// the literal's UTF-8 bytes, which is a key the record cannot be read
    /// with, so every execution re-derived the answer through the string
    /// pool's lock and a hash of the whole literal (18.4 ns against HotSpot's
    /// 0.2 — `probes/LdcConstCostProbe.java`).
    ldc_string_info: Vec<(usize, u32, u16)>,
    /// Class-`ldc` sites: `(bytecode_pc, referencing class id, CP index)`.
    /// Served by `helpers.ldc_class_cp`, which resolves the target and
    /// returns its mirror — the mirror is a heap object, so it can neither be
    /// baked as an immediate nor resolved once at compile time.
    ldc_class_info: Vec<(usize, u32, u16)>,
    /// Resolved ldc2_w constants: (bytecode_pc, i64 value).
    ldc2w_info: Vec<(usize, i64)>,
    /// `ldc`-family pcs whose constant is floating-point — `CONSTANT_Float` for
    /// `ldc`/`ldc_w`, `CONSTANT_Double` for `ldc2_w`.
    ///
    /// `ldc_info`/`ldc2w_info` carry only the bits, because the codegen that
    /// consumes them lets the CONSUMING opcode pick the width. The deopt
    /// operand-stack snapshot has no consuming opcode to ask, so it needs the
    /// constant-pool tag the resolver already read; without it `x64::stack_kinds`
    /// answered `Unknown` for every numeric `ldc`, the snapshot recorded
    /// `Unsupported`, and `osr_exit_policy` then refused OSR entry for the whole
    /// artifact. See `osr-refused-for-a-loop-inline-in-main-FIXED-20260818`.
    ldc_fp_pcs: FxHashSet<usize>,
    /// Runtime helper function pointers for JIT callbacks.
    helpers: JitRuntimeHelpers,
    /// Expected simulated-stack depth at each forward branch target.
    /// Used to fix up the stack when dead code becomes live at a merge point.
    branch_target_stack_depth: FxHashMap<usize, usize>,
    /// Companion to `branch_target_stack_depth`: the operand-stack oop-mark
    /// vector (`stack_oop_marks`) live at each forward branch target, recorded
    /// in lock-step via `record_branch_target_depth`.
    ///
    /// FIX: the dead-code merge reconstruction (`compile_bytecode`'s main
    /// dispatch loop, where dead code becomes live again at a branch target)
    /// used to rebuild `stack_oop_marks` from scratch as all-`false`,
    /// "relying" on the merge target's own bytecode to re-tag any slot that
    /// genuinely holds a reference as it re-executes the producing
    /// instruction. That only holds for slots actually PRODUCED between the
    /// branch and the merge point — a value pushed well BEFORE the branch
    /// (e.g. `getstatic System.out` ahead of an `if`/`else` computing a later
    /// argument) is simply carried across untouched, and its true oop-mark
    /// was silently discarded: a live reference got permanently mis-marked as
    /// a plain non-oop value for the rest of the compiled method. This is
    /// invisible in ordinary execution (a conservative frame sweep still
    /// finds the value), but it silently defeated the OSR-exit/invokedynamic
    /// uncommon-trap deopt snapshot's operand-stack decoding at any later
    /// safepoint that read the slot — see
    /// `fixed-suite-bugs/testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`.
    /// Recording the real marks here lets the reconstruction restore them
    /// instead of guessing `false`.
    branch_target_stack_oop_marks: FxHashMap<usize, Vec<bool>>,
    /// Set to true when an internal error (e.g. stack underflow) is detected
    /// during compilation.  `compile_bytecode` checks this and bails out.
    ///
    /// Raise it through [`Compiler::fail`], never by assignment — the flag on
    /// its own says nothing about WHAT refused (see `failed_site`).
    failed: bool,
    /// The FIRST site that raised `failed`, with the bytecode pc/opcode current
    /// at that moment.
    ///
    /// `failed` is a flag consulted only after the whole dispatch loop has run,
    /// so a refusal through it used to be attributed to
    /// `dbg_last_pc`/`dbg_last_op` — the last bytecode the emitter touched,
    /// which for a method that walks on to its `ireturn` is just the last
    /// instruction and names nothing. `Rbc6FieldProbe.getfieldRefHandlerLocal`
    /// reported `singlepass-codegen(pc=36,op=0xac)`, an arm that has no
    /// `return false` of its own, for an operand-stack underflow one
    /// instruction earlier. Every raising site now names itself and that name
    /// reaches the `compile-bail` line, the way `note_jit_bail_site` works
    /// everywhere else.
    failed_site: Option<(&'static str, usize, u8)>,
    /// Set by `patch_branches` when it rejects the method: the branch target
    /// PC that had no native offset, and the nearest emitted PC at or below it.
    /// The refusal named neither before, and its message asserted a cause
    /// ("malformed bytecode a verifier would reject") that is demonstrably
    /// wrong for real javac output.
    pub(crate) unresolved_branch_target: Option<(usize, i64)>,
    /// Bitmask of scratch XMM registers (2-7) currently in use on the simulated stack.
    /// Bit N corresponds to SCRATCH_XMMS[N]. Used to allocate scratch XMMs for
    /// FP intermediate persistence across bytecodes.
    scratch_xmm_in_use: u8,
    /// FP LICM: loop-invariant FP load hoisting info.
    fp_hoist_info: Vec<FpLoopHoist>,
    /// FP LICM: frame offsets for hoisted FP values (one per FpLoopHoist entry).
    _fp_hoist_offsets: Vec<i32>,
    /// SIMD: vectorizable double-array sum loops detected during analysis.
    simd_fp_loops: Vec<SimdFpArraySum>,
    /// FP strength reduction: set of bytecode PCs where dmul-by-2.0 is replaced with dadd-self.
    fp_strength_reduction_pcs: FxHashSet<usize>,
    /// Scalar replacement: non-escaping NEW PCs → frame-local field storage.
    scalar_replaced: FxHashMap<usize, ScalarReplacedObject>,
    /// Scalar replacement: putfield/getfield PCs that target a scalar-replaced object → NEW PC.
    scalar_field_ops: FxHashMap<usize, usize>,
    /// Scalar replacement: invokespecial PCs whose `<init>()V` should be skipped.
    scalar_init_skips: std::collections::HashSet<usize>,
    /// Phase B (real-frame-deopt x64 backport): `(new_pc, field_index)` →
    /// JVM field type tag (`b'I'`/`b'J'`/`b'D'`/`b'F'`/`b'L'`/…) for fields of
    /// scalar-replaced objects, built post-plan by joining `field_info` with
    /// `scalar_field_ops`. Drives the per-field `FrameValue` width/ref-ness when
    /// emitting a `VirtualObject`. A field never accessed is absent ⇒ zero default.
    sr_field_types: FxHashMap<(usize, usize), u8>,
    /// Phase B: bytecode PC → locals holding a live scalar object at that PC
    /// (`(local_index, new_pc)`), from `plan_scalar_replacement`. A deopt snapshot
    /// at `bci` reads `sr_local_prov_at[bci]` to emit `VirtualObject` slots.
    sr_local_prov_at: FxHashMap<usize, Vec<(usize, usize)>>,
    /// Phase B: set when a `monitorenter`/`monitorexit` was ELIDED over a
    /// NON-scalar object (the pre-existing blanket-elision case). An elided lock
    /// over a non-scalar object leaves no recordable trace, so the producer keeps
    /// such a method off the deopt-resume path. Phase C: an elision over a SCALAR
    /// object no longer sets this — it is recorded in `sr_monitor_at` and relocked
    /// on resume. (`ACC_SYNCHRONIZED` is separately caught by the consumer's
    /// `is_synchronized` bail.)
    has_elided_monitor: bool,
    /// Phase C (monitors): bytecode PC → scalar monitors held at that PC
    /// (`(new_pc, lock_depth)`), from `plan_scalar_replacement`. A deopt snapshot at
    /// `bci` reads `sr_monitor_at[bci]` to emit `MonitorInfo{ VirtualObjectRef(new_pc),
    /// lock_depth }` so the resume re-acquires the elided lock.
    sr_monitor_at: FxHashMap<usize, Vec<(usize, u32)>>,
    /// Phase C: monitorenter/exit PCs whose receiver is a scalar object — the
    /// codegen monitor handler consults this to decide relockable (scalar) vs
    /// `has_elided_monitor` (non-scalar).
    sr_monitor_scalar_ops: std::collections::HashSet<usize>,
    /// Inline sites: bytecode PC → resolved InlineSite for inlining callee bytecode.
    inline_sites: FxHashMap<usize, crate::InlineSite>,
    // PGO-02: see the `inline_guard_variants` parameter doc on
    // `compile_with_param_slots`.
    inline_guard_variants: FxHashMap<usize, Vec<(u32, crate::InlineSite)>>,
    /// Compile-time resolved `java/lang/String` field layout, for the String
    /// call-site intrinsics. `None` ⇒ String layout unavailable (intrinsic
    /// codegen bails to normal dispatch). See `crate::StringFieldLayout`.
    ///
    /// Threaded in now so the API is stable; the String-intrinsic codegen
    /// that reads it lands in a later wave. `#[allow(dead_code)]` until then.
    #[allow(dead_code)]
    string_layout: Option<crate::StringFieldLayout>,
    /// Deferred out-of-line deoptimization stubs: (branch_patch_offset, bci, reason_code).
    /// Speculative guards (e.g. BCE) jump here; the stub calls jit_uncommon_trap and
    /// returns i64::MIN to signal the interpreter to resume.
    deopt_stubs: Vec<(usize, usize, i64)>,
    /// T1.1.a — parallel type tracker for the simulated operand stack.
    ///
    /// `stack_oop_marks[i] == true` means the value at `self.stack[i]`
    /// is an object reference (an "oop"). Pushed in lock-step with
    /// `self.stack`. Any future push that is not explicitly tagged
    /// defaults to `false` (non-oop) via `push_nonoop_from_rax`.
    ///
    /// At every call site that may trigger GC (new, anewarray, invoke*,
    /// newarray, multianewarray, ldc-class, etc.) the compiler walks
    /// this vector alongside `self.stack`, finds every
    /// `(StackSlot::Frame(_), true)` pair, and emits an
    /// [`OopMapEntry`] recording the frame offset. The GC root walker
    /// then consults the map at the return PC for precise coverage.
    ///
    /// When empty or misaligned (a stack underflow has occurred), the
    /// walker falls back to the conservative scan — this never loses
    /// an oop, it only pins extra false positives.
    stack_oop_marks: Vec<bool>,
    /// True while `stack_oop_marks` is an exact type map for the current
    /// simulated operand stack. Reconstructed conservative merge states clear
    /// oop bits safely for non-moving GC, but moving-young must treat those
    /// safepoints as incomplete and fall back.
    stack_oop_marks_exact: bool,
    /// Frame offsets of the STAGED INVOKE-ARGUMENT buffer slots that hold
    /// references, for the safepoint map about to be emitted.
    ///
    /// Invoke arguments are popped off `self.stack` and written into a buffer
    /// in the spill reserve BEFORE the call, so by the time
    /// `emit_oop_map_for_safepoint` runs there is nothing left on the simulated
    /// stack to name them — `emit_pre_safepoint_spill` says as much where it
    /// publishes the conservative bound ("includes the staged invoke-argument
    /// buffer ... live for the duration of the call"). They were covered by that
    /// bound and by nothing else, which is why a frame could assert
    /// `fully_oop_covered` while live argument oops sat unnamed in the
    /// operand-spill region.
    ///
    /// Consumed (taken) by the next `emit_oop_map_for_safepoint`, exactly like
    /// `pending_live_frame_hi`, so a staging site that emits no map cannot leak
    /// its slots into a later safepoint's map.
    pending_staged_arg_oops: Vec<i32>,
    /// A reference argument was staged somewhere this compiler cannot name in
    /// an oop map — the native-ABI outgoing-argument area
    /// (`emit_stack_arg_setup`), the direct-call service slots, or an inlined
    /// callee's parameter locals.
    ///
    /// Those areas are covered by the conservative scan and by nothing else, so
    /// a method that stages a reference into one of them must not claim precise
    /// coverage. Fail-closed: it makes the safepoint incomplete rather than
    /// silently narrowing what the map describes.
    pending_staged_args_unmapped: bool,
    /// T1.1.a — collected oop maps, indexed by native PC offset of the
    /// instruction *after* the safepoint call. Transferred to
    /// `CompiledMethod::oop_maps` at finalize time.
    oop_maps: Vec<crate::OopMapEntry>,
    /// Stage 2 (precise oop maps) — per-bytecode-PC "must be oop" local
    /// bitmask. `local_oop_masks[pc] & (1 << k) != 0` means local slot `k`
    /// holds an object reference on EVERY path reaching `pc` (so a safepoint
    /// at `pc` can record local `k`'s canonical frame slot as a precise oop).
    /// Empty when unsupported (>64 locals); `local_oop_reached[pc]` is false
    /// for PCs the forward dataflow never reached (e.g. exception-handler-only
    /// entries), where no precise local marking is emitted and the GC falls
    /// back to the conservative frame sweep. See
    /// `fixed-suite-bugs/app-jvm-bugs/precise-jit-stack-maps-design.md` (Stage 2).
    local_oop_masks: Vec<u64>,
    /// Stage 2 — companion to `local_oop_masks`: whether the forward local-oop
    /// dataflow reached each PC. Only `reached` PCs get precise local entries.
    local_oop_reached: Vec<bool>,
    /// The forward dataflow's ENTRY state: bit `k` set ⇒ JVM local slot `k`
    /// holds a reference parameter on method entry (`compute_param_oop_mask`).
    /// It is what seeds `local_oop_masks[0]`, and it is kept separately
    /// because the METHOD-ENTRY safepoint poll
    /// (`Compiler::emit_safepoint_poll_prologue`) sits at no bytecode pc at
    /// all — see `Compiler::local_oop_mask_at_current_pc`.
    ///
    /// Deliberately NOT read back out of `local_oop_masks[0]`: when bci 0 is
    /// also a branch target that entry has been intersected with the back
    /// edge's state, which is a SUBSET of the entry state, and publishing a
    /// subset while claiming complete coverage is the unsound direction.
    param_oop_mask: u64,
    /// deopt-osr P2 — per-local JVM value kind (`classify_local_kinds`), the
    /// width/type source for the deopt snapshot so a `long`/`double`/`float`
    /// local emits a precisely-typed `FrameValue` rather than a truncating
    /// `Register`/`StackSlot`. Empty unless `deopt_real_enabled()` (the only
    /// consumer is the gated snapshot), so production compiles skip the scan.
    local_kinds: Vec<LocalKind>,
    /// Per-bci refinement of the `Ambiguous` entries of `local_kinds`. A slot
    /// the compiler reuses across two disjoint live ranges is ambiguous for the
    /// METHOD but usually not at the bci a deopt actually happens at, and an
    /// `Unsupported` slot makes precise resume impossible for the whole frame.
    /// See `refine_ambiguous_local_kinds`.
    local_kinds_refined: AmbiguousLocalKinds,
    /// deopt-osr — per-PC local liveness (`regalloc::live_locals_per_pc`),
    /// bit `i` set ⇒ local `i` may still be read at that bci. Indexed by 0's
    /// only when `deopt_real_enabled()` populates it (see `local_kinds`
    /// above); empty otherwise. Consumed by `build_and_record_deopt_point` to
    /// tell "local's machine location is unreadable because it's genuinely
    /// dead here" (safe to substitute a placeholder) apart from "unreadable
    /// and still needed" (must reject the snapshot). Without this, an
    /// OSR-exit snapshot at a trap that lands just past a hot loop — where
    /// the loop's own induction variable is provably dead — was rejected
    /// wholesale, and the safe-reject fallback re-runs the interpreter from
    /// the pre-OSR-entry frame, silently re-executing every loop iteration
    /// the OSR-compiled code already committed
    /// (`fixed-suite-bugs/testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`).
    /// (`regalloc::live_locals_per_pc_all`) — `local_liveness[pc *
    /// local_liveness_words + w]` covers slots `[w*64, w*64+64)`. Read through
    /// [`Compiler::local_live_at`], never directly: a method with more than 64
    /// locals has more than one word per pc, and indexing this by `pc` alone
    /// silently reads window 0 of the wrong instruction.
    local_liveness: Vec<u64>,
    /// Words per pc in [`Self::local_liveness`] — `ceil(num_locals / 64)`, and
    /// `1` for the overwhelming majority of methods. Zero while the vector is
    /// empty (the ungated compile), which `local_live_at` reads as "no answer".
    local_liveness_words: usize,
    /// Parallel coverage bitmap for [`Self::local_liveness`]: `false` at a pc
    /// no basic block covers, where the liveness answer is the `0` default
    /// ("nothing live") rather than a computed result. Treating that as "every
    /// local is dead" would discard the whole frame, so the snapshot builder
    /// falls back to "everything live" there.
    local_liveness_covered: Vec<bool>,
    /// Debug-only: number of exception ranges modelled by the liveness /
    /// interference analyses for this method (CRATONVM_DBG_EXCFRAME).
    exception_ranges_dbg_len: usize,
    /// Where the inline mini-emitter's walk last stood, so a rollback can name
    /// itself.
    ///
    /// `outer-splice-rolled-back=N` is a count with no subject: it says a
    /// planned splice was thrown away at emission, but not what construct did
    /// it, and all ~50 of `try_emit_inline_body`'s bails look identical from
    /// outside. A single-pass walk bails where it stands, so the (callee pc,
    /// opcode) it last reached IS the answer. Updated once per callee
    /// instruction and reported by `try_emit_inline_site` under
    /// `CRATONVM_DBG_JITC`.
    pub(super) inline_walk_at: (usize, u8),
    /// Live inline (spliced-callee) scopes, innermost LAST.
    ///
    /// A call-carrying splice (`CRATONVM_JIT_INLINE_CALLS`, default-on since
    /// 2026-08-20) puts the callee's JVM locals in the CALLER's spill area and
    /// then emits a real, GC-capable call from inside the spliced body. Those
    /// local slots were named by nothing: `local_oop_masks` describes the
    /// ENCLOSING method's locals and the operand marks describe operands -- so
    /// a reference the splice parked in a callee local (`this`, above all) was
    /// in neither the safepoint's oop map nor the shadow publication.
    ///
    /// That was sound only while a moving collector PINNED whatever the
    /// conservative frame sweep found, which is what `emit_inline_direct_call`
    /// asserts. ZGC's relocate-under-proven-JIT path (2026-08-21) SUPPRESSES
    /// that sweep whenever the per-cycle coverage proof passes, and the proof
    /// consulted a completeness claim this file made without looking at splices
    /// at all. Measured on `LongLongHashMapTest.randomOperations`: the spliced
    /// `AbstractLongAssert.<init>` holds `this` in a callee local across its
    /// super-constructor call, the slide moves the object, and the splice's
    /// trailing `putfield longs` lands on the vacated copy -- a `LongAssert`
    /// whose `longs` reads back null and NPEs on the next assertion.
    ///
    /// One entry per active splice, so a nested splice (`inline-nest`) keeps
    /// its parent's locals covered too.
    pub(super) inline_oop_scopes: Vec<InlineOopScope>,
    /// deopt-osr FU2 — whether the method touches any `long`/`float`/`double`
    /// (`code_uses_long_float_double`). The method-level gate for the operand-stack
    /// snapshot: the abstract stack has no per-entry width source, so when this is
    /// `true` a non-oop `Frame`/GPR stack slot (possibly a `long`/spilled-FP) is
    /// recorded `Unsupported` (re-run) rather than mistyped `Int`; when `false`
    /// every non-oop stack slot is provably a cat-1 `int`/`ref`. Only the gated
    /// snapshot reads it, so production compiles leave it `false`.
    uses_long_float_double: bool,
    /// Stage 2 — the bytecode PC of the instruction currently being emitted,
    /// updated at the top of the `compile_bytecode` loop so
    /// `emit_oop_map_for_safepoint` can look up the local-oop mask without
    /// threading `pc` through every safepoint call site.
    cur_bc_pc: usize,
    /// Adjacent store→load reload elision: `(frame_offset, gpr, buf_pos)`
    /// recorded by `emit_store_local`/`emit_load_local` — "register `gpr`
    /// holds the exact value of `[rbp - frame_offset]`, and the buffer stood
    /// at `buf_pos` right after that instruction". Consulted by
    /// `emit_load_local`, which substitutes a reg-reg move (or nothing) for
    /// the reload **only when `buf_pos == self.buf.pos()`** — i.e. nothing
    /// whatsoever has been emitted in between, so no instruction can have
    /// clobbered the register and no code path can have joined in between
    /// (any join at a bytecode boundary is additionally severed by the
    /// explicit invalidation at branch-target PCs in the main loop, and
    /// speculative-inline emission suppresses the mechanism entirely — its
    /// mini-emitter replays CALLEE bytecode whose internal joins this
    /// position rule cannot see). The STORE itself is never elided, so frame
    /// slots always hold canonical values for GC scans, OSR entries, deopt
    /// re-execution, and the interpreter.
    ///
    /// This kills the dominant cost of the template backend's operand-stack
    /// round-trips (`mov [rbp-X],rax; mov rax,[rbp-X]`) — on store-forwarding
    /// latency inside loop-carried dependency chains it was worth ~25-40% on
    /// pure-int array kernels (QuickBench sieve). Opt out with
    /// `CRATONVM_JIT_NO_SLOT_MIRROR=1`.
    slot_mirror: Option<(i32, u8, usize)>,
    /// Pure-kernel deferred operand cache. The general-purpose R8/R9 cache was
    /// previously disabled because call-heavy methods repeatedly paid to flush
    /// it. Pure kernels have no calls or GC-capable operations, so their only
    /// flush is at a branch/return; at a counted-loop back edge the operand
    /// stack is already empty. Keeping arithmetic intermediates in R8/R9
    /// removes the template backend's remaining frame store/load pairs without
    /// broadening the pure-kernel admission surface.
    kernel_operand_cache: bool,
    /// `true` while `try_emit_inline` replays callee bytecode (see
    /// `slot_mirror`): suppresses both recording and consumption.
    slot_mirror_suppressed: bool,
    /// Stage 3 — whether the moving-safe precise-stack-map machinery is on
    /// (gate `CRATONVM_PRECISE_JIT_MAPS`). Gates the prologue frame-record
    /// call and the per-safepoint id store. Off → byte-identical default path.
    precise_maps: bool,
    /// Step 1 (`precise-jit-maps-default.md`) — segment-relative TLS
    /// displacement of the innermost-RBP mirror slot, or 0 when inline
    /// frame-record is off / unavailable. Non-zero → the prologue emits one
    /// `mov` (`gs:` on Windows, `fs:` on Linux) instead of
    /// `call jit_frame_record`. Cached from `inline_rbp_tls_disp()` at
    /// construction so codegen reads it once.
    inline_rbp_tls_disp: usize,
    /// Segment-relative displacement of the compile-id mirror — the identity
    /// half of the frame record, written beside `inline_rbp_tls_disp` so the GC
    /// can name the method owning the innermost RBP without decoding the call
    /// that created the frame. 0 when unavailable → nothing is published and
    /// the scan keeps its old decode path.
    inline_cm_tls_disp: usize,
    /// This compilation's identity, reserved BEFORE codegen because the
    /// immediate must be encoded into the prologue while the `CompiledMethod`
    /// that will own it does not exist yet. 0 → publish nothing.
    compile_id: u32,
    /// Step 1 debug self-check (`CRATONVM_DBG_VERIFY_INLINE_FRAME_RECORD`) —
    /// when set AND inline frame-record is active, also emit the verify call.
    verify_inline_frame_record: bool,
    /// Stage 3 — frame offset (positive; slot at `[rbp - sp_id_slot_off]`) of
    /// the reserved safepoint-id slot. 0 when `precise_maps` is off.
    sp_id_slot_off: i32,
    /// SB-CRASH-04 (register-invisibility) — whether every GC-capable safepoint
    /// blind-spills the CURRENT value of each used callee-saved GPR
    /// (`alloc_used_regs`) into a reserved frame slot so the conservative
    /// `scan_active_jit_frames` walk can mark any oop that lives ONLY in a
    /// callee-saved register at the safepoint: operand-stack reference entries
    /// that survive the call un-spilled, PLUS any oop the JIT's per-slot oop
    /// tracking fails to classify. Fully conservative — the scanner re-validates
    /// each qword via `heap.is_object_address`, so non-oop register values are
    /// ignored. No post-call reload is needed under the opt-out non-moving young
    /// sweep (the object is never relocated, so the register stays valid).
    /// Gated `CRATONVM_JIT_SAFEPOINT_REG_SPILL`; off → byte-identical default
    /// path (no slots reserved, no stores).
    safepoint_reg_spill: bool,
    /// Keycloak Gap 9 (`CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`): spill the FULL
    /// GPR file ([`ALL_SPILL_GPRS`]) at each safepoint rather than only the used
    /// callee-saved set, closing the caller-saved/argument/RAX register-resident
    /// oop gap. Implies `safepoint_reg_spill`.
    safepoint_reg_spill_all: bool,
    /// Diagnostic control (`CRATONVM_JIT_SAFEPOINT_REG_SPILL=nostore`): reserve
    /// the spill slots (so the frame layout matches the spilling build) but emit
    /// NO stores — isolates the effect of the stores from the effect of the
    /// frame-size perturbation (the SB-CRASH-04 "FIX == NOSTORE" methodology).
    safepoint_reg_spill_nostore: bool,
    /// Register-only operand-stack-oop soundness (DEFAULT ON; opt out with
    /// `CRATONVM_JIT_NO_CALLEE_OOP_FLUSH`) — whether `flush_scratch_registers`
    /// also spills `StackSlot::CalleeSaved` operand-stack entries marked as
    /// references to a frame slot before each pre-call/branch/return flush, so a
    /// live oop that lives ONLY in a callee-saved register across a GC-capable
    /// call is on the stack and visible to the conservative root scan. Distinct
    /// from `safepoint_reg_spill` (which blind-spills register-mapped *locals*
    /// and is env-gated off): this targets register-resident operand-stack
    /// *temporaries* and is on by default. See `flush_callee_saved_oops_enabled`.
    flush_callee_saved_oops: bool,
    /// Frame offset (positive; first slot at `[rbp - reg_spill_base]`) of the
    /// reserved callee-saved-register spill area: one 8-byte slot per entry in
    /// `alloc_used_regs`, same order. 0 when `safepoint_reg_spill` is off.
    reg_spill_base: i32,
    /// Request half of the allocation spill sink (see `alloc_spill_sink_enabled`).
    /// Raised by the `new` bytecode arm immediately before
    /// `emit_pre_safepoint_spill` when the site will take the inline-TLAB path
    /// with no post-init helper; taken (and cleared) by that emitter.
    sink_alloc_blind_spill: bool,
    /// Acknowledgement half of the allocation spill sink. Set by
    /// `emit_pre_safepoint_spill` when it actually withheld the sinkable
    /// registers; taken by `emit_deferred_alloc_blind_spill` at the allocation's
    /// slow-path label. The two halves are separate so a request the safepoint
    /// emitter cannot honour silently degrades to the full spill rather than to
    /// a spill that is missing eleven registers.
    deferred_alloc_blind_spill: bool,
    /// Stage A.2 (precise oop maps, B-K fix) — bytecode PCs at which a
    /// GC-capable safepoint flushed register-locals via
    /// `emit_pre_safepoint_spill`. Populated only under `precise_maps`. Used at
    /// finalize to compute [`crate::CompiledMethod::fully_oop_covered`]: a method
    /// is covered only when every spilled safepoint also recorded an oop map
    /// (`safepoint_pcs ⊆ mapped_safepoint_pcs`).
    safepoint_pcs: FxHashSet<u32>,
    /// Stage A.2 — bytecode PCs at which `emit_oop_map_for_safepoint` recorded a
    /// precise oop map (possibly empty). Populated only under `precise_maps`.
    mapped_safepoint_pcs: FxHashSet<u32>,
    /// Shadow-stack precise roots (`CRATONVM_SHADOW_STACK`) — whether the
    /// per-safepoint live-oop push/reload codegen is emitted. Off →
    /// byte-identical default path.
    shadow_enabled: bool,
    /// Frame offset (positive; slot at `[rbp - shadow_thread_slot_off]`) of the
    /// reserved slot that caches this invocation's `*mut JvmThread`, set once in
    /// the prologue. 0 when `shadow_enabled` is off.
    shadow_thread_slot_off: i32,
    /// Frame offset of the reserved slot holding the shadow `top` at method
    /// entry. The epilogue restores `thread.shadow_stack.top` from it so any
    /// unbalanced safepoint push is unwound on return (correct under nesting).
    /// 0 when `shadow_enabled` is off.
    shadow_savetop_slot_off: i32,
    /// Frame offset of the reserved slot holding the shadow `top` captured by
    /// the MOST RECENT push (its base), so the matching reload restores from and
    /// resets `top` to exactly that base — independent of any intervening
    /// unbalanced push (e.g. a constructor call, or a call that threw and was
    /// caught in-method) that would otherwise leave `top` drifted high and make
    /// the reload read uninitialised slots above the real data. 0 when shadow
    /// is off. (spring-bug-10 — the drift was the shadow-reload SIGSEGV.)
    shadow_savebase_slot_off: i32,
    /// Byte offset of the `ShadowStack` from `&JvmThread` (from the helper
    /// table). The shadow `top` is at `[thread + shadow_off_in_thread + 0]`.
    shadow_off_in_thread: i32,
    /// Live-oop homes recorded by the most recent shadow push, consumed by the
    /// matching post-call reload. Cleared at each push start so an unbalanced
    /// (no-reload) safepoint cannot feed stale homes to a later reload.
    pending_shadow: Vec<ShadowHome>,
    /// Completeness proof for the most recent shadow push. The paired
    /// `OopMapEntry` carries this bit so GC can reject moving-young relocation
    /// when the active safepoint is not fully covered.
    pending_shadow_coverage_complete: bool,
    /// Lazy-prologue perf lever: byte range `[start, end)` of the prologue's
    /// (NOP-able) `get_current_thread` fetch sequence. After codegen, if
    /// `shadow_pushed_any` is still false (the method never published a
    /// register-resident oop), this range is overwritten with NOPs so the
    /// per-invocation thread-fetch CALL never runs — eliminating the ~2.8x
    /// fib44 / ~7x call-heavy shadow regression for the (vast majority of)
    /// methods that never push. Both 0 when shadow is off or the fetch was
    /// not emitted.
    shadow_fetch_start: usize,
    shadow_fetch_end: usize,
    /// Leaked NUL-terminated copy of `method_label`, materialised on first use
    /// by the overflow-bail diagnostic (`CRATONVM_SHADOW_OVERFLOW_DIAG`) so the
    /// bail can name the method without a helper call. `None` until then.
    shadow_overflow_label: Option<usize>,
    /// Set true the first time `emit_shadow_push` emits a real push (non-empty
    /// homes). Gates whether the prologue fetch above is kept or NOP'd.
    shadow_pushed_any: bool,
    /// T5.2.1 — induction variables detected in each loop.
    ///
    /// One entry per detected counted loop. Consumed by downstream
    /// passes that care about loop-structure properties (trip count,
    /// stride, bound): unroll factor decisions, SIMD remainder
    /// handling, and range-check elimination.
    induction_vars: Vec<crate::scev::InductionVar>,
    /// T5.2.14 — null-check elimination dataflow.
    ///
    /// Per-PC bitmask of locals proven non-null. Used by future null
    /// emission passes to skip the `TEST reg, reg; JZ throw_npe`
    /// sequence when the receiver is already known non-null.
    null_check_info: crate::null_check_elim::NullCheckInfo,
    /// Implicit null-check sites whose faulting instruction has been emitted
    /// but whose recovery address is not yet known — `(fault_off, bc_pc)`.
    /// Drained by `bind_implicit_null_recovery` when the slow path is bound;
    /// a non-empty vector at the end of a compile means a site was elided
    /// without a recovery address and fails the compile.
    implicit_null_pending: Vec<(usize, usize)>,
    /// Resolved `(fault_off, recover_off)` pairs, registered against the final
    /// code address once the artifact exists. Offsets rather than addresses:
    /// the buffer base is stable from allocation, but registration must not
    /// happen until the compile is known to have succeeded.
    implicit_null_sites: Vec<(usize, usize)>,
    /// T5.2.15 — SIMD element-wise loops detected via SuperWord.
    ///
    /// Each entry describes one vectorizable `out[i] = a[i] OP b[i]`
    /// loop. The JIT emitter walks this list at the loop header and
    /// replaces the scalar body with a PADDD/PMULLD (etc.) block plus
    /// a remainder loop for trip counts that aren't a multiple of 4.
    ///
    /// Detection runs unconditionally; emission is gated on `has_avx2()`.
    simd_element_wise_loops: Vec<SimdArrayElementWise>,
    /// Canonical byte/boolean-array zero-fill loops lowered to `REP STOSB`.
    bulk_zero_byte_fill_loops: Vec<BulkZeroByteFillLoop>,
    /// Canonical byte/boolean-array `a[iv] = 1; iv += step` loops.
    bulk_set_byte_stride_loops: Vec<BulkSetByteStrideLoop>,
    /// Canonical nested byte/boolean Sieve loops.
    byte_sieve_loops: Vec<ByteSieveLoop>,
    /// T5.2.17 — loop unswitch candidates.
    ///
    /// Each entry describes a loop with a loop-invariant conditional
    /// branch that can be lifted to produce two specialized loops.
    /// The emitter consumes this list to duplicate the body and hoist
    /// the branch above the header.
    loop_unswitch_candidates: Vec<LoopUnswitchCandidate>,

    // ── MED-4 / Fix 3 — PC-indexed lookup acceleration ─────────────────
    //
    // The original layout stores per-call-site metadata in `Vec<(pc, …)>`
    // arrays and queries them with `iter().find(|(p, …)| *p == pc)` on
    // every getfield/putfield/invoke* during code generation. For hot
    // methods (large generated classes, generics-heavy code) this is
    // O(N) per opcode and N²-shaped for the whole compile.
    //
    // These auxiliary maps mirror the indexed entries so the hot
    // lookup is O(1). They are populated once via [`build_pc_indices`]
    // immediately before `compile_bytecode` runs and never mutated
    // afterwards, so the borrow-checker tax is zero on the hot path.
    field_info_idx: FxHashMap<usize, usize>,
    static_field_info_idx: FxHashMap<usize, usize>,
    invoke_info_idx: FxHashMap<usize, usize>,
    indy_info_idx: FxHashMap<usize, usize>,
    /// deopt-osr indy-arg-types fix: the invokedynamic call's own per-argument
    /// type tags (`indy_arg_type_tags`), keyed by the trap's bci — populated
    /// by the `0xba` codegen arm right before it snapshots the OSR-exit deopt
    /// point. Consumed by `build_and_record_deopt_point`'s operand-stack loop
    /// to precisely type the top `tags.len()` stack entries (this call's
    /// arguments) instead of the coarse per-method `wide_fp` gate. See
    /// `indy_arg_type_tags`'s doc comment for the motivating bug.
    indy_stack_arg_types: FxHashMap<usize, Vec<u8>>,

    /// Per-bci operand-stack KINDS — the width source the stack never had.
    ///
    /// Consulted by `build_and_record_deopt_point` exactly where it would
    /// otherwise record `FrameValue::Unsupported`, and only when its depth and
    /// ref-ness agree with the emitter's own live stack. Empty when the
    /// analysis declined (an unmodelled construct poisons its successors), in
    /// which case every snapshot keeps the pre-existing coarse encoding. See
    /// `x64::stack_kinds`.
    stack_kinds: stack_kinds::StackKindMap,

    /// The same thing for ORDINARY invokes (`invoke{virtual,special,static,
    /// interface}`), keyed by call-site bci and derived once from
    /// `invoke_info`'s descriptors by [`Compiler::index_invoke_arg_types`].
    ///
    /// Why this exists: a call-site guard (`ReceiverTypeChanged`, reason 6)
    /// snapshots the frame BEFORE the argument pops, so the operand stack still
    /// holds `[.., receiver, args]`. Without a width source those argument
    /// entries fall to the coarse per-method `wide_fp` gate and are recorded
    /// `FrameValue::Unsupported` — and ONE unsupported entry anywhere in the
    /// artifact makes `osr_exit_policy` refuse OSR entry for the whole method.
    ///
    /// Measured on the regression-suite corpus (2026-08-03): after the
    /// loop-header fix, **every** remaining `osr-entry-unresumable-exit`
    /// refusal was a `ReceiverTypeChanged` snapshot blocked by exactly one
    /// stack entry, and every one of those entries was a call argument — e.g.
    /// `RJitGc.main`'s `invokestatic Double.doubleToLongBits(D)J`, whose sole
    /// stack entry is a `double`. The tags were already available; only
    /// invokedynamic was using them.
    invoke_stack_arg_types: FxHashMap<usize, Vec<u8>>,
    direct_calls_idx: FxHashMap<usize, usize>,
    mic_slots_idx: FxHashMap<usize, usize>,
    pic_slots_idx: FxHashMap<usize, usize>,
    new_info_idx: FxHashMap<usize, usize>,
    new_deferred_idx: FxHashMap<usize, usize>,
    anewarray_info_idx: FxHashMap<usize, usize>,
    anewarray_deferred_idx: FxHashMap<usize, usize>,
    typecheck_info_idx: FxHashMap<usize, usize>,
    ldc_info_idx: FxHashMap<usize, usize>,
    ldc_string_info_idx: FxHashMap<usize, usize>,
    ldc_class_info_idx: FxHashMap<usize, usize>,
    ldc2w_info_idx: FxHashMap<usize, usize>,

    /// Memo for `magic_signed_div32`: constant divisor → computed
    /// `(magic, shift)` pair. The magic-number derivation runs a Newton-style
    /// iteration; a loop body with a repeated `/ k` or `% k` on the same
    /// constant `k` would otherwise recompute it at every occurrence. The
    /// result is a pure function of the divisor, so caching is behavior-
    /// preserving.
    magic_div_memo: FxHashMap<i32, (i64, u32)>,
    /// 64-bit sibling of `magic_div_memo` for the long const-div peephole.
    magic_div64_memo: FxHashMap<i64, (i64, u32)>,

    /// JVM local slot of each incoming JIT argument, in argument order
    /// (`this` first for instance methods, then declared params). Because a
    /// category-2 (long/double) parameter occupies TWO JVM local slots while
    /// the JIT calling convention passes it in ONE argument register, an
    /// argument's JVM slot can diverge from its argument index. The prologue
    /// uses this to deposit each incoming argument register into the slot the
    /// body actually reads (e.g. `lload_2` for the second `long` parameter).
    /// Empty ⇒ legacy "argument index == slot" behavior (correct for
    /// category-1-only methods); used by the many test call sites.
    param_jvm_slots: Vec<usize>,

    /// Total JVM local slots consumed by all parameters (category-2 counted
    /// as 2, plus the implicit `this`). Used as the lower bound for prologue
    /// zero-initialization so a `long`/`double` parameter's slot is never
    /// clobbered. `0` ⇒ fall back to `num_params`.
    param_slot_span: usize,

    /// deopt-osr Step 1: precise deopt-exit snapshots (machine-state -> interpreter
    /// frame) recorded at eligible guards (currently the speculative-BCE loop-header
    /// guard), transferred to `CompiledMethod::deopt_points` at finalize.
    /// Emit-and-discard for now — no live path consumes them until the in-stub
    /// trampoline + resume land (`real-frame-deopt-x64-backport.md` Steps 2-4).
    deopt_points: Vec<crate::deopt::DeoptimizationPoint>,
    /// deopt-osr Step 1: stable boxed copies of `deopt_points`, for the
    /// imm64-baked deopt stub to load by pointer (mirrors `_deopt_point_boxes`).
    deopt_boxes: Vec<Box<crate::deopt::DeoptimizationPoint>>,
    /// The EMITTER pc each `deopt_points` entry was recorded at, one per entry
    /// and in the same order.
    ///
    /// `DeoptimizationPoint::bci` is published in interpreter-bci space (see
    /// [`Compiler::orig_bci`]), so on a rewritten method it is no longer the
    /// coordinate the point was recorded at and the translation cannot be
    /// re-derived from the published artifact alone. This keeps the input, so
    /// `compile_with_param_slots` can CHECK the coordinate change instead of
    /// trusting it — see `x64::loop_rewrite::rewritten_deopt_points_are_publishable`.
    /// Identical to each point's own `bci` on every ordinary compile.
    deopt_point_pcs: Vec<usize>,
    /// deopt-osr Step 9 follow-up (a): raw pointer to a single, process-lifetime
    /// **leaked** `DeoptEpochGuard` for this artifact, baked as the 4th arg into
    /// every frame-deopt stub so `x64_deopt_entry` can short-circuit a superseded
    /// compilation BEFORE dereferencing the box. Null until the first frame-deopt
    /// stub is emitted (`emit_deopt_stubs`, only under `deopt_real_enabled()`);
    /// copied to `CompiledMethod::deopt_epoch_guard` at finalize so the VM can
    /// stamp it at install. All deopt points in one artifact share it (one
    /// creation epoch, one live cell). `*mut` so the leaked allocation's address
    /// is stable; never written through here (the VM owns the atomic stamp).
    deopt_epoch_guard: *const crate::deopt::DeoptEpochGuard,
    /// deopt-osr Step 9 follow-up (c): this method's `"<class>.<method>:<desc>"`
    /// key, set by `compile_with_param_slots` from its `method_key` arg. Used to
    /// consult the per-bci de-spec registry (`crate::deopt::despec_contains`) so a
    /// loop-header speculation that has repeatedly deopted is NOT re-emitted on
    /// recompile. Empty (`""`) on the legacy/test `compile()` wrapper and in
    /// production (the registry is empty), so the consult is a no-op there.
    method_key: String,
    /// The common direct-recursive edge cannot allocate, call another method,
    /// or poll. See [`gc_inert_selfrec_candidate`].
    gc_inert_selfrec: bool,
    /// deopt-osr Step 2 / P2: frame offset (positive depth-from-RBP) of the
    /// DEEPEST qword of the always-reserved 256-byte
    /// `SavedRegisters{gpr:[u64;16],xmm:[u64;16]}` region the frame-deopt stub
    /// spills into. `gpr[r]` is stored at `[rbp - (deopt_regs_base - r*8)]`, so
    /// `gpr[0]=RAX` is the deepest/lowest address and
    /// `LEA [rbp - deopt_regs_base] == &gpr[0] == &SavedRegisters`; the XMM half
    /// follows (`#[repr(C)]`), so `xmm[n]` at
    /// `[rbp - (deopt_regs_base - 128 - n*8)]`. 0 unless `deopt_real_enabled()`
    /// reserved the region at construction.
    deopt_regs_base: i32,
    /// deopt-osr Step 2: bci → stable pointer to the boxed `DeoptimizationPoint`
    /// for that guard, baked as arg0 (imm64) by the frame-deopt stub. Populated
    /// by `emit_deopt_snapshot_at_guard`.
    deopt_box_ptr_by_bci: rustc_hash::FxHashMap<usize, *const crate::deopt::DeoptimizationPoint>,
    /// Reason-9 (`DeoptReason::PendingException`) snapshots, keyed by the
    /// THROWING instruction's own bci. Separate from `deopt_box_ptr_by_bci`
    /// because the two disagree about what the key means: an ordinary deopt
    /// point resumes at its bci, an exceptional one is *thrown* at its bci and
    /// is only ever used to pick a handler. Sharing one map let a reason-2/6
    /// box be handed to a reason-9 stub (and vice versa).
    exc_frame_box_ptr_by_bci:
        rustc_hash::FxHashMap<usize, *const crate::deopt::DeoptimizationPoint>,
    /// deopt-osr Step 7: bcis (loop-boundary PCs vetted by OSR-entry) that carry
    /// an OSR-exit map in `deopt_points`/`deopt_boxes` (tagged
    /// `DeoptReason::OsrExit`). Transferred to `CompiledMethod::osr_exit_points`
    /// at finalize; a non-empty set drives `can_osr_exit`. Emit-and-discard until
    /// Step 8 wires the mid-loop sink. Only populated when `deopt_real_enabled()`.
    osr_exit_points: Vec<usize>,
    /// deopt-osr Step 7: bci → stable boxed-point pointer for the OSR-exit map at
    /// that loop bci (mirrors `deopt_box_ptr_by_bci`; kept separate so an OSR-exit
    /// map and a guard snapshot at the same bci cannot collide on the key).
    osr_exit_box_ptr_by_bci: rustc_hash::FxHashMap<usize, *const crate::deopt::DeoptimizationPoint>,
    /// deopt-osr Step 8 (test trigger): the loop-header bci at which to emit a
    /// synthetic unconditional OSR-exit branch (→ the OSR-exit frame-deopt stub),
    /// so a JIT'd loop bails to the interpreter at a loop bci and resumes the loop
    /// body. `Some(_)` only under `CRATONVM_OSR_EXIT_TEST` + `CRATONVM_DEOPT_REAL`
    /// (set after construction from the detected loops); `None` in production, so
    /// no trigger is emitted and code is byte-identical. This is the deliberate
    /// "instrument a rare branch" trigger from the handoff — proves the mechanism
    /// pending a real speculation/counter trigger.
    osr_exit_test_trigger_bci: Option<usize>,
    /// deopt-osr Step 8 follow-up (P4): when `Some(N)`, the trigger at
    /// `osr_exit_test_trigger_bci` is COUNTER-GATED — it bails only on the `N`-th
    /// reach of the loop header, so the JIT advances ~`N` loop iterations (and
    /// commits their side effects) before the exit, making the reconstructed frame
    /// carry JIT-advanced state for the interpreter's true OSR-exit transfer. When
    /// `None`, the trigger is the unconditional-at-header bail (Step 8). Set from
    /// `osr_exit_after()` at finalize (only under `CRATONVM_DEOPT_REAL`).
    osr_exit_after_count: Option<usize>,
    /// deopt-osr: the through-JIT deopt-EXIT differential trigger
    /// (`CRATONVM_DEOPT_EAGER` + `deopt_real_enabled()`) — the loop-header bci at
    /// which to force a reason-2 deopt-EXIT. When `Some(pc)`, the backend records a
    /// reason-2 snapshot and emits an unconditional branch to the deopt stub at
    /// `pc_to_native[pc]` (the normal loop path), so a JIT'd loop deopts at the
    /// loop bci on entry — reconstructing the loop-header frame (incl.
    /// `long`/`double`/`float` locals) and resuming in the interpreter via
    /// `resume_real_ir_deopt`. The end-to-end exercise of the deopt-EXIT resume,
    /// independent of the (pattern-specific) speculative-BCE guard. `None`
    /// (production / no loop) ⇒ no branch ⇒ byte-identical.
    deopt_eager_bci: Option<usize>,
}

/// deopt-osr Step 1: map a frame slot's machine location + oop-ness to a
/// `FrameValue` for a deopt snapshot. Pure (no `&self`) so it is unit-testable
/// without a live compile.
///
/// `spill_off` is the positive `[rbp - spill_off]` frame offset; the resolver
/// reads `*(rbp + off)`, so a spilled slot is encoded with the negated offset.
/// A register-resident slot is `Register(r)` regardless of oop-ness — typing a
/// register slot oop-vs-primitive is the deferred width/type source (Phase A
/// `can_deopt_resume` excludes the ambiguous cases). XMM-resident slots have no
/// resolver in Phase A (`SavedRegisters` is `gpr[16]` only) -> `Unsupported`.
/// Phase B (real-frame-deopt x64 backport): build the per-field `FrameValue`s for
/// a scalar-replaced object's `VirtualObjectState`. `field_type(k)` yields the JVM
/// type tag of field `k` if the method accessed it (else `None` ⇒ `Int(0)` zero
/// default — sound: a non-escaping object's continuation reads the same bytecode,
/// so a never-accessed field is provably never read post-resume). Field `k` lives
/// in the frame at `[rbp - (field_base_offset + k*SLOT_SIZE)]` (always its live
/// value: zero-filled at `new`, overwritten by `putfield`); the resolver reads
/// `*(rbp + off)`, so the encoded `StackSlot*` offset is the negation. Pure (no
/// `&self`) so it is unit-testable.
fn sr_field_values(
    num_fields: usize,
    field_base_offset: i32,
    field_type: impl Fn(usize) -> Option<u8>,
) -> Vec<crate::deopt::FrameValue> {
    use crate::deopt::FrameValue;
    let mut field_values = Vec::with_capacity(num_fields);
    for k in 0..num_fields {
        let spill_off = field_base_offset + (k as i32) * (SLOT_SIZE as i32);
        let off = -spill_off;
        let fv = match field_type(k) {
            Some(tag) => match tag {
                // Reference field (object or array) — the raw word IS the heap
                // pointer (0 = null); resolves to a GC-tracked `Object`.
                b'L' | b'[' => FrameValue::StackSlotRef(off),
                b'J' => FrameValue::StackSlotLong(off),
                b'D' => FrameValue::StackSlotDouble(off),
                b'F' => FrameValue::StackSlotFloat(off),
                // I/B/C/S/Z (and any unexpected tag) → cat-1 int.
                _ => FrameValue::StackSlot(off),
            },
            None => FrameValue::Int(0),
        };
        field_values.push(fv);
    }
    field_values
}

fn frame_value_for_slot(
    reg: Option<u8>,
    xmm: Option<u8>,
    spill_off: i32,
    is_oop: bool,
) -> crate::deopt::FrameValue {
    use crate::deopt::FrameValue;
    if let Some(r) = reg {
        FrameValue::Register(r)
    } else if xmm.is_some() {
        FrameValue::Unsupported
    } else if is_oop {
        FrameValue::StackSlotRef(-spill_off)
    } else {
        FrameValue::StackSlot(-spill_off)
    }
}

/// deopt-osr P2: map a NON-oop local slot's machine location + classified JVM
/// `kind` to a precisely-typed `FrameValue`. Pure (no `&self`) so it is
/// unit-testable. `spill_off` is the positive `[rbp - spill_off]` frame offset
/// (the resolver reads `*(rbp + off)`, so a spilled slot encodes the negation).
///
/// The classifier ([`classify_local_kinds`]) is the width source the snapshot
/// otherwise lacks; this turns it into the right variant per provenance:
/// `long` → `RegisterLong`/`StackSlotLong`, `float` → `XmmFloat`/`StackSlotFloat`,
/// `double` → `XmmDouble`/`StackSlotDouble`, `int` → `Register`/`StackSlot`. A
/// provenance/kind contradiction (an `int` in an XMM, an FP value in a GPR) and
/// an `Ambiguous`/`Ref` kind yield `Unsupported` (safe re-run) — never a guess.
/// `HighHalf` (the dead cat-2 upper half) and a never-accessed `Unknown` slot
/// both yield `Undefined`: the only ways to read a local are the load/`iinc`
/// opcodes the scan covers, so an `Unknown` slot is provably dead — `Undefined`
/// (→ `Value::Int(0)`) is never read, and even a dead cat-2 param stays
/// alignment-correct as two `Undefined` cat-1 slots (its high-half can never be
/// the upper half of a *classified* cat-2 — those are marked `HighHalf`).
fn typed_local_frame_value(
    reg: Option<u8>,
    xmm: Option<u8>,
    spill_off: i32,
    kind: LocalKind,
) -> crate::deopt::FrameValue {
    use crate::deopt::FrameValue;
    match kind {
        LocalKind::Int => {
            if let Some(r) = reg {
                FrameValue::Register(r)
            } else if xmm.is_some() {
                FrameValue::Unsupported // an int in an XMM is a contradiction
            } else {
                FrameValue::StackSlot(-spill_off)
            }
        }
        LocalKind::Long => {
            if let Some(r) = reg {
                FrameValue::RegisterLong(r)
            } else if xmm.is_some() {
                FrameValue::Unsupported
            } else {
                FrameValue::StackSlotLong(-spill_off)
            }
        }
        LocalKind::Float => {
            if let Some(n) = xmm {
                FrameValue::XmmFloat(n)
            } else if reg.is_some() {
                FrameValue::Unsupported // a float in a GPR is a contradiction
            } else {
                FrameValue::StackSlotFloat(-spill_off)
            }
        }
        LocalKind::Double => {
            if let Some(n) = xmm {
                FrameValue::XmmDouble(n)
            } else if reg.is_some() {
                FrameValue::Unsupported
            } else {
                FrameValue::StackSlotDouble(-spill_off)
            }
        }
        // Dead cat-2 upper half (collapse skips it) or a never-accessed dead
        // slot: a harmless zero the resume never reads.
        LocalKind::HighHalf | LocalKind::Unknown => FrameValue::Undefined,
        // A reused slot (genuinely different kinds at different points) is
        // unknowable from a whole-method scan — re-run, never a guess.
        LocalKind::Ambiguous => FrameValue::Unsupported,
        // FIX (silent data corruption residual): scan says this slot is
        // UNAMBIGUOUSLY `Ref` everywhere it's ever accessed in the method, but
        // we only reach this arm when the flow-sensitive precise oop-mask
        // dataflow couldn't PROVE it's live-as-oop on every path reaching this
        // bci (the caller already routes the provably-live case through
        // `RegisterRef`/`StackSlotRef` before calling this helper). The
        // canonical reason a single-kind `Ref` slot fails that proof is a
        // narrower-scoped local (e.g. an enhanced-for loop variable) that's
        // dead on a path bypassing its only `astore` (a loop that could run
        // zero iterations) — the oop-mask dataflow is deliberately
        // conservative there because a GC root scan must never mistake a
        // genuinely-dead, possibly-garbage slot for a live oop. But the JVM
        // verifier's definite-assignment rule means that same not-provably-
        // live slot can NEVER be legally read by any bytecode reachable from
        // here (reading an unassigned local is a verification error), so for
        // THIS resume-snapshot purpose specifically — unlike the GC root
        // scan — trusting the single unambiguous scan classification is
        // sound: the resumed interpreter will never actually consume this
        // slot's value if it truly isn't live. Previously this fell to
        // `Unsupported` (safe re-run) for the exact code shape this fix
        // targets — a loop `Iterator`/boxed-element local live right after
        // the loop, at the print statement's invokedynamic trap — which
        // silently defeated the OSR-exit transfer for that common pattern.
        // Mirrors the already-trusted `RegisterRef`/`StackSlotRef` provenance
        // split the oop-mask-true branch uses (see the caller).
        LocalKind::Ref => {
            if let Some(r) = reg {
                FrameValue::RegisterRef(r)
            } else if xmm.is_some() {
                FrameValue::Unsupported
            } else {
                FrameValue::StackSlotRef(-spill_off)
            }
        }
    }
}

#[cfg(test)]
mod deopt_snapshot_tests {
    use super::frame_value_for_slot;
    use crate::deopt::FrameValue;

    #[test]
    fn frame_value_for_slot_maps_provenance() {
        // Register-resident wins (provenance is the GPR), oop-ness aside.
        assert_eq!(
            frame_value_for_slot(Some(3), None, 16, false),
            FrameValue::Register(3)
        );
        assert_eq!(
            frame_value_for_slot(Some(3), None, 16, true),
            FrameValue::Register(3)
        );
        // XMM-resident: not resolvable in Phase A.
        assert_eq!(
            frame_value_for_slot(None, Some(9), 16, false),
            FrameValue::Unsupported
        );
        // Spilled primitive vs spilled oop -> StackSlot vs StackSlotRef at [rbp-off].
        assert_eq!(
            frame_value_for_slot(None, None, 16, false),
            FrameValue::StackSlot(-16)
        );
        assert_eq!(
            frame_value_for_slot(None, None, 24, true),
            FrameValue::StackSlotRef(-24)
        );
    }

    use super::{
        classify_local_kinds, code_uses_long_float_double, opcode_touches_long_float_double,
        pure_high_halves, refine_ambiguous_local_kinds, typed_local_frame_value,
        wide_local_high_halves, LocalKind,
    };

    #[test]
    fn fu2_wide_fp_gate_detects_long_float_double() {
        // Pure int/ref method: iload_0; iconst_1; iadd; ireturn → no wide/FP.
        let int_only = [0x1a, 0x04, 0x60, 0xac];
        assert!(!code_uses_long_float_double(&int_only, int_only.len()));
        // A long: lload_0; lconst_1; ladd; lstore_0; return.
        let with_long = [0x1e, 0x0a, 0x61, 0x3f, 0xb1];
        assert!(code_uses_long_float_double(&with_long, with_long.len()));
        // A float: fload_0; freturn.
        let with_float = [0x22, 0xae];
        assert!(code_uses_long_float_double(&with_float, with_float.len()));
        // A double constant: ldc2_w #idx; dreturn.
        let with_double = [0x14, 0x00, 0x05, 0xaf];
        assert!(code_uses_long_float_double(&with_double, with_double.len()));
        // wide-prefixed dload: wide; dload idx16; dreturn.
        let wide_dload = [0xc4, 0x18, 0x00, 0x03, 0xaf];
        assert!(code_uses_long_float_double(&wide_dload, wide_dload.len()));
        // Per-opcode spot checks: i-arith excluded, l/f/d-arith included.
        assert!(!opcode_touches_long_float_double(0x60)); // iadd
        assert!(opcode_touches_long_float_double(0x61)); // ladd
        assert!(opcode_touches_long_float_double(0x62)); // fadd
        assert!(opcode_touches_long_float_double(0x63)); // dadd
        assert!(!opcode_touches_long_float_double(0x6c)); // idiv
        assert!(opcode_touches_long_float_double(0x6d)); // ldiv
        assert!(!opcode_touches_long_float_double(0x74)); // ineg
        assert!(opcode_touches_long_float_double(0x75)); // lneg
        assert!(!opcode_touches_long_float_double(0x36)); // istore
        assert!(opcode_touches_long_float_double(0x37)); // lstore
    }

    #[test]
    fn classify_local_kinds_per_opcode() {
        // Indexed load/store forms exercising each kind + cat-2 high-halves.
        // local7 is never touched -> Unknown.
        let code = [
            0x15, 0x00, // iload 0   -> Int@0
            0x37, 0x01, // lstore 1  -> Long@1, HighHalf@2
            0x38, 0x03, // fstore 3  -> Float@3
            0x39, 0x04, // dstore 4  -> Double@4, HighHalf@5
            0x3a, 0x06, // astore 6  -> Ref@6
            0xb1, // return
        ];
        let kinds = classify_local_kinds(&code, code.len(), 8);
        assert_eq!(kinds[0], LocalKind::Int);
        assert_eq!(kinds[1], LocalKind::Long);
        assert_eq!(kinds[2], LocalKind::HighHalf);
        assert_eq!(kinds[3], LocalKind::Float);
        assert_eq!(kinds[4], LocalKind::Double);
        assert_eq!(kinds[5], LocalKind::HighHalf);
        assert_eq!(kinds[6], LocalKind::Ref);
        assert_eq!(kinds[7], LocalKind::Unknown);
    }

    /// A method with MORE THAN 64 LOCALS gets NO oop mask at all — and
    /// `classify_local_kinds` is what covers for it.
    ///
    /// `compute_local_oop_masks` returns empty vectors for `max_locals > 64`, so
    /// `build_and_record_deopt_point`'s `is_oop` reads false for EVERY local in
    /// such a method (slot 0 included, not just the ones above 63). The only
    /// thing that then publishes a reference local as a reference rather than as
    /// a truncating `StackSlot` is the `LocalKind::Ref` arm — and that is sound
    /// exactly because this classifier is a per-slot `Vec`, with no bitset and
    /// no cap.
    ///
    /// Both halves are asserted together on purpose. Either one alone reads as a
    /// property of a helper; together they are the invariant a reader of that
    /// call site needs, and the pairing is what fails if someone "optimises"
    /// `classify_local_kinds` into a `u64` to match its neighbours.
    ///
    /// Witnessed end-to-end by `probes/HighLocalOopProbe.java` (84 locals,
    /// references at slots 74/75/76, OSR-entered, 100 forced collections):
    /// `oop_reached=false oop_mask=0x0` while those three slots published
    /// `StackSlotRef`. Its `probes/LowLocalOopProbe.java` twin, identical but
    /// under 64 locals, reports `oop_reached=true oop_mask=0x700000e`.
    #[test]
    fn classify_local_kinds_types_a_reference_above_slot_63() {
        // wide astore 74; wide aload 74; wide istore 70; return
        let code = [
            0xc4, 0x3a, 0x00, 0x4a, // wide astore 74 -> Ref@74
            0xc4, 0x19, 0x00, 0x4a, // wide aload  74 -> Ref@74
            0xc4, 0x36, 0x00, 0x46, // wide istore 70 -> Int@70
            0xb1, // return
        ];
        let kinds = classify_local_kinds(&code, code.len(), 84);
        assert_eq!(
            kinds[74],
            LocalKind::Ref,
            "a reference local above slot 63 must still be classified Ref — it is the              ONLY thing that publishes it as a reference in a >64-local method, because              `compute_local_oop_masks` gives that method no mask at all"
        );
        assert_eq!(kinds[70], LocalKind::Int);
        assert_eq!(kinds[83], LocalKind::Unknown);

        // The other half of the invariant: the mask really is absent, so there
        // is no second opinion to fall back on.
        let (masks, reached) = crate::x64::licm::compute_local_oop_masks(&code, code.len(), 84, 0);
        assert!(
            masks.is_empty() && reached.is_empty(),
            "compute_local_oop_masks must answer NOTHING above 64 locals; if it ever              starts answering, the deopt snapshot's `is_oop` gains a second source and              this test's premise needs rewriting rather than deleting"
        );

        // And the same method one local smaller DOES get a mask, so the cliff is
        // the local count and not something about `wide` encodings.
        let (masks64, reached64) =
            crate::x64::licm::compute_local_oop_masks(&code, code.len(), 64, 0);
        assert!(!masks64.is_empty() && !reached64.is_empty());
    }

    /// The `RowDataType.read` shape: slot 1 is an `int` in one arm of a branch
    /// and a `ref` in the other, so the whole-method classifier must call it
    /// `Ambiguous` — but at a pc only the `istore` arm reaches, the refinement
    /// must answer `Int`.
    #[test]
    fn refine_resolves_branch_disjoint_slot_reuse() {
        // 0: iconst_0        (0x03)
        // 1: ifne 9          (0x9a 0x00 0x08)  -> targets 9
        // 4: iconst_1        (0x04)
        // 5: istore_1        (0x3c)            <- Int definition
        // 6: goto 12         (0xa7 0x00 0x06)  -> targets 12
        // 9: aconst_null     (0x01)
        // 10: astore_1       (0x4c)            <- Ref definition
        // 11: nop            (0x00)
        // 12: return         (0xb1)
        let code = [
            0x03, 0x9a, 0x00, 0x08, 0x04, 0x3c, 0xa7, 0x00, 0x06, 0x01, 0x4c, 0x00, 0xb1,
        ];
        let kinds = classify_local_kinds(&code, code.len(), 2);
        assert_eq!(
            kinds[1],
            LocalKind::Ambiguous,
            "the whole-method vote must still be Ambiguous"
        );
        let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
        // pc 6 is the `goto` right after `istore_1` — only the Int arm reaches
        // it, so the slot is provably an int there.
        assert_eq!(refined.kind_at(6, 1), Some(LocalKind::Int));
        // pc 11 is the `nop` after `astore_1` — the Ref arm. `kind_at` never
        // answers `Ref`: the oop mask owns ref-typed slots.
        assert_eq!(refined.kind_at(11, 1), None);
        // pc 12 is the join of both arms — genuinely ambiguous.
        assert_eq!(refined.kind_at(12, 1), None);
        // A slot that was never ambiguous is not tracked at all.
        assert_eq!(refined.kind_at(6, 0), None);
    }

    /// A method with no ambiguous local pays nothing and answers nothing.
    #[test]
    fn refine_is_empty_without_ambiguity() {
        // iconst_0; istore_1; return
        let code = [0x03, 0x3c, 0xb1];
        let kinds = classify_local_kinds(&code, code.len(), 2);
        assert_eq!(kinds[1], LocalKind::Int);
        let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
        assert_eq!(refined.kind_at(1, 1), None);
    }

    /// An exception handler is reachable from anywhere inside its protected
    /// range, so nothing may be assumed on entry to one — or anywhere the
    /// handler flows to.
    #[test]
    fn refine_seeds_exception_handlers_top() {
        // Same shape as the branch test, but pc 6 is declared a handler entry.
        let code = [
            0x03, 0x9a, 0x00, 0x08, 0x04, 0x3c, 0xa7, 0x00, 0x06, 0x01, 0x4c, 0x00, 0xb1,
        ];
        let kinds = classify_local_kinds(&code, code.len(), 2);
        assert_eq!(kinds[1], LocalKind::Ambiguous);
        let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[(0, 13, 6)]);
        assert_eq!(
            refined.kind_at(6, 1),
            None,
            "a handler entry must not inherit the kind of its protected range"
        );
    }

    /// A cat-2 store must clear a stale kind from the slot above it, or a
    /// reused high-half keeps answering with the value it used to hold.
    #[test]
    fn refine_clears_cat2_high_half() {
        // 0: aconst_null (0x01)
        // 1: astore_2    (0x4d)          <- Ref definition of slot 2
        // 2: lconst_0    (0x09)
        // 3: lstore_1    (0x40)          <- Long at slot 1, high half at slot 2
        // 4: return      (0xb1)
        let code = [0x01, 0x4d, 0x09, 0x40, 0xb1];
        let kinds = classify_local_kinds(&code, code.len(), 3);
        assert_eq!(kinds[2], LocalKind::Ambiguous);
        let refined = refine_ambiguous_local_kinds(&code, code.len(), &kinds, &[]);
        assert_eq!(
            refined.kind_at(4, 2),
            None,
            "the high half of a live cat-2 is not an independently typed slot"
        );
    }

    fn classify_local_kinds_reuse_is_ambiguous() {
        // local0 stored as int then as float -> Ambiguous (slot reuse).
        let code = [
            0x36, 0x00, // istore 0 -> Int
            0x38, 0x00, // fstore 0 -> conflict -> Ambiguous
            0xb1,
        ];
        let kinds = classify_local_kinds(&code, code.len(), 1);
        assert_eq!(kinds[0], LocalKind::Ambiguous);
    }

    #[test]
    fn classify_local_kinds_highhalf_reuse_is_ambiguous() {
        // local0 long (hi half @1), but local1 also used as int -> Ambiguous@1.
        let code = [
            0x37, 0x00, // lstore 0 -> Long@0, would-be HighHalf@1
            0x15, 0x01, // iload 1  -> Int@1 (independently used)
            0xb1,
        ];
        let kinds = classify_local_kinds(&code, code.len(), 2);
        assert_eq!(kinds[0], LocalKind::Long);
        assert_eq!(kinds[1], LocalKind::Ambiguous);
    }

    /// REGRESSION (`arrays-sort-long-osr-miscompile`): the OSR publication
    /// strips register homes from cat-2 high-half slots so the trampoline
    /// cannot seed a dead half over a live local sharing its register. That
    /// strip must NOT touch a slot which is also a real local in a disjoint
    /// live range.
    ///
    /// `java.util.DualPivotQuicksort.mixedInsertionSort` is the shape: slot 7
    /// is `long ai`'s high half in the method's first region and the `int i`
    /// loop counter in the other two. Stripping it made the trampoline seed
    /// only its frame slot while the compiled body read its register, so an OSR
    /// entry ran with a garbage `i` and `Arrays.sort(long[])` threw an
    /// `ArrayIndexOutOfBoundsException` with an index in the hundreds of
    /// millions.
    ///
    /// The classifier already draws the distinction; this pins the predicate
    /// the OSR publication filters on, which is where the bug was.
    #[test]
    fn only_pure_high_halves_may_lose_their_osr_register_home() {
        // slot 0: long (so slot 1 would be its high half, untouched otherwise)
        // slot 2: long (so slot 3 would be its high half) — but slot 3 is also
        //         loaded as an int, exactly the reuse `mixedInsertionSort` has.
        let code = [
            0x37, 0x00, // lstore 0  -> Long@0, HighHalf@1
            0x37, 0x02, // lstore 2  -> Long@2, would-be HighHalf@3
            0x15, 0x03, // iload  3  -> Int@3, so @3 is Ambiguous, not HighHalf
            0xb1, // return
        ];
        let kinds = classify_local_kinds(&code, code.len(), 4);
        assert_eq!(kinds[1], LocalKind::HighHalf, "untouched high half");
        assert_eq!(kinds[3], LocalKind::Ambiguous, "high half reused as an int");

        // `wide_local_high_halves` names BOTH — it is a whole-method scan with
        // no notion of reuse, which is why the publication must filter it.
        let halves = wide_local_high_halves(&code, code.len());
        assert!(halves.contains(&1) && halves.contains(&3));

        // The filter the OSR publication applies — the same function it calls,
        // not a copy of its predicate: a copy passes while the call site drifts.
        assert_eq!(
            pure_high_halves(&kinds, &halves),
            vec![1],
            "a reused high half must keep its home"
        );
    }

    /// The same rule for a slot reused as a **reference**, which is the shape
    /// the Tomcat annotation-scan SIGSEGV was: every stage of
    /// `probes/AnnotationScanSplitProbe` (and of the minimised
    /// `probes/OsrRefSlotReuseProbe`) has
    ///
    ///     slot 4 : Iterator, then `lstore 4` for the trailing `long ns`
    ///     slot 5 : byte[] b inside the loop, high half after it
    ///
    /// so slot 5 is a LIVE `byte[]` at the OSR entry PC and a dead high half
    /// forty bytecodes later. Stripping its register home there left the
    /// trampoline seeding only the frame slot while the compiled body read the
    /// register: `b.length` off a garbage base (SIGSEGV on Windows, a silently
    /// wrong checksum on Linux, where slot 5 lands elsewhere in `LOCAL_REGS`).
    ///
    /// The sibling case in `only_pure_high_halves_may_lose_their_osr_register_home`
    /// reuses the slot as an `int`. Both are `Ambiguous`, but they arrive
    /// through different opcode families (`iload`/`istore` vs `aload`/`astore`)
    /// and `local_access_at` classifies them in different arms, so one passing
    /// is not evidence for the other.
    #[test]
    fn a_high_half_reused_as_a_reference_keeps_its_osr_register_home() {
        // slot 4: astore/aload (Ref) AND lstore (Long)  -> Ambiguous
        // slot 5: astore/aload (Ref), and `lstore 4`'s would-be high half
        let code = [
            0x3a, 0x04, // astore 4  -> Ref@4
            0x19, 0x04, // aload  4
            0x3a, 0x05, // astore 5  -> Ref@5
            0x19, 0x05, // aload  5
            0x37, 0x04, // lstore 4  -> @4 Ambiguous, would-be HighHalf@5
            0xb1, // return
        ];
        let kinds = classify_local_kinds(&code, code.len(), 6);
        assert_eq!(kinds[4], LocalKind::Ambiguous, "Ref then Long in slot 4");
        assert_eq!(
            kinds[5],
            LocalKind::Ref,
            "slot 5 is only ever a reference: slot 4 never settles on Long, so \
             the cat-2 high-half pass must not even reach it"
        );

        // The whole-method scan still names slot 5 — `lstore 4` is there.
        let halves = wide_local_high_halves(&code, code.len());
        assert!(
            halves.contains(&5),
            "the scan names it; the filter is the gate"
        );

        assert!(
            pure_high_halves(&kinds, &halves).is_empty(),
            "a live reference slot must keep its OSR register home"
        );
    }

    #[test]
    fn typed_local_frame_value_picks_typed_variant() {
        // long in GPR / spilled.
        assert_eq!(
            typed_local_frame_value(Some(7), None, 8, LocalKind::Long),
            FrameValue::RegisterLong(7)
        );
        assert_eq!(
            typed_local_frame_value(None, None, 16, LocalKind::Long),
            FrameValue::StackSlotLong(-16)
        );
        // float/double in XMM / spilled.
        assert_eq!(
            typed_local_frame_value(None, Some(2), 8, LocalKind::Float),
            FrameValue::XmmFloat(2)
        );
        assert_eq!(
            typed_local_frame_value(None, None, 8, LocalKind::Float),
            FrameValue::StackSlotFloat(-8)
        );
        assert_eq!(
            typed_local_frame_value(None, Some(3), 8, LocalKind::Double),
            FrameValue::XmmDouble(3)
        );
        assert_eq!(
            typed_local_frame_value(None, None, 24, LocalKind::Double),
            FrameValue::StackSlotDouble(-24)
        );
        // int unchanged.
        assert_eq!(
            typed_local_frame_value(Some(1), None, 8, LocalKind::Int),
            FrameValue::Register(1)
        );
        // HighHalf and never-accessed Unknown -> harmless Undefined.
        assert_eq!(
            typed_local_frame_value(None, None, 8, LocalKind::HighHalf),
            FrameValue::Undefined
        );
        assert_eq!(
            typed_local_frame_value(None, None, 8, LocalKind::Unknown),
            FrameValue::Undefined
        );
        // Ambiguous (slot reuse) -> re-run.
        assert_eq!(
            typed_local_frame_value(None, None, 8, LocalKind::Ambiguous),
            FrameValue::Unsupported
        );
        // Provenance/kind contradiction (float in a GPR) -> re-run.
        assert_eq!(
            typed_local_frame_value(Some(4), None, 8, LocalKind::Float),
            FrameValue::Unsupported
        );
    }
}

/// Does this method's frame need the 256-byte `SavedRegisters` region that the
/// frame-deopt stub spills 16 GPRs and 16 XMMs into?
///
/// **This must cover every stub `emit_deopt_stubs` takes the spilling path
/// for.** That path is selected by
///
/// ```text
/// deopt_real_enabled() || matches!(reason, 8 | 9 | 10)
/// ```
///
/// Reasons 9 and 10 are the precise-exception-frame stubs, which
/// `precise_exception_frames` covers. Reason 8 is the unconditional
/// `invokedynamic` trap, which is emitted whether or not `deopt_real` is on —
/// and it was covered by neither, which is not a missed optimisation but a
/// stack-corrupting bug: with the region unreserved `deopt_regs_base` is 0, so
/// the stub's `[rbp - (base - r*8)]` stores become `[rbp]`, `[rbp+8]`, … and
/// walk UP over the saved `rbp` and the return address. The epilogue's `ret`
/// then jumps to whatever register landed on the return slot.
///
/// Kept as a free function so the contract can be tested with `deopt_real`
/// OFF — the only configuration the divergence was visible in, and one no
/// in-process test can reach, because `deopt_real_enabled()` latches a
/// process-wide `OnceLock`.
///
/// See `probes/IndyDeoptProbe.java` and
/// `docs/known-issues/jit/deopt-real-off-null-entry-sigsegv-20260803.md`.
pub(crate) fn deopt_spill_region_reserved(
    deopt_real: bool,
    precise_exception_frames: bool,
    has_indy_sites: bool,
) -> bool {
    deopt_real || precise_exception_frames || has_indy_sites
}

impl Compiler {
    #[allow(clippy::too_many_arguments)]
    fn new(
        method_label: String,
        buf: ExecutableBuffer,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        needs_heap: bool,
        multianewarray_info: Vec<(usize, i64)>,
        field_info: Vec<(usize, usize, u8)>,
        typecheck_info: Vec<(usize, *const u8, usize)>,
        static_field_info: Vec<(usize, u32, usize, u8, bool)>,
        hoist_info: Vec<LoopHoist>,
        arith_hoist_info: Vec<ArithLoopHoist>,
        array_len_hoist_info: Vec<ArrayLenHoist>,
        alloc_result: super::regalloc::RegAllocResult,
        reserve_matrix_dot_scratch: bool,
        helpers: JitRuntimeHelpers,
        num_scalar_slots: usize,
        cache_jit_thread_for_inline_new: bool,
        reserve_stack_floor: bool,
        gc_inert_selfrec: bool,
        precise_exception_frames: bool,
        // Does this method contain an `invokedynamic`? The `0xba` lowering
        // emits a frame-deopt stub (reason 8) that spills 32 registers into the
        // `SavedRegisters` region, and it does so whether or not
        // `deopt_real_enabled()` — so the frame has to reserve that region on
        // the same condition. See `deopt_regs_size` below.
        has_indy_sites: bool,
        protected_ranges: Vec<(u32, u32)>,
    ) -> Self {
        // Compact arrays: byte[] uses 1-byte elements, int[] uses 4-byte, ref[] uses 8-byte.
        // Each local takes 8 bytes: [rbp - 8], [rbp - 16], ...
        // If needs_heap, reserve one extra slot for the heap pointer.
        // If LICM hoisting is active, reserve extra slots for hoisted values.
        // If scalar replacement is active, reserve extra slots for replaced object fields.
        let num_hoists = hoist_info.len();
        let num_arith_hoists = arith_hoist_info.len();
        let num_len_hoists = array_len_hoist_info.len();
        // LICM arithmetic: besides one result slot per hoist, reserve a small
        // shared scratch pool sized to the deepest hoisted expression. The
        // pool is shared because hoists execute serially (one per loop entry),
        // never nested — see `emit_arith_hoist`.
        let arith_scratch_depth: usize = arith_hoist_info
            .iter()
            .map(|h| arith_expr_max_depth(&h.steps))
            .max()
            .unwrap_or(0);
        // Stage 3 — when precise maps are enabled, reserve ONE extra frame
        // slot (the last local slot) to hold the active safepoint's bytecode
        // PC, written by `emit_pre_safepoint_spill` before each GC-capable
        // call so the GC root walker can recover the exact oop map. Off by
        // default → no slot reserved → frame layout byte-identical.
        let precise_maps = precise_jit_maps_enabled() || moving_young_enabled();
        // Step 1 (inline frame-record) — cache the validated TLS displacement
        // (0 when the opt-in flag is off or the OS probe failed → CALL path).
        let inline_rbp_tls_disp = if precise_maps {
            inline_rbp_tls_disp()
        } else {
            0
        };
        // Identity is only publishable where the RBP mirror is: the pair is
        // what makes `(rbp, id)` describe one frame. Reserve the id here, at
        // the start of this compilation, so the prologue can encode it.
        let inline_cm_tls_disp = if inline_rbp_tls_disp != 0 {
            crate::x64::inline_cm_tls_disp()
        } else {
            0
        };
        let compile_id = if inline_cm_tls_disp != 0 {
            crate::reserve_compile_id()
        } else {
            0
        };
        let verify_inline_frame_record = verify_inline_frame_record_enabled();
        let shadow_enabled = shadow_stack_maps_enabled();
        // SB-CRASH-04 (register-invisibility): blind-spill used callee-saved
        // GPRs into reserved frame slots at every GC-capable safepoint so the
        // conservative root scan can see register-only oops. The slot count =
        // `alloc_used_regs.len()`, reserved in `total` below.
        //
        // FIX (SB-CRASH-04 default-path gap, see `precise_reg_spill_disabled`
        // above for the full rationale): fold `precise_maps` — default-on —
        // into the SAME decision so the already-built, already-safe (`=all`:
        // "can only over-retain, never corrupt" under the non-moving young
        // sweep this VM runs whenever JIT frames are live) full-GPR spill
        // actually runs by default, matching what several call sites'
        // comments already claim happens. The FULL GPR file (not just the
        // callee-saved subset) is required: a receiver/args staged into
        // ARG_REGS immediately before a GC-capable call (invoke dispatch,
        // the MIC/PIC cascade, allocation helpers, checkcast/instanceof) sit
        // in caller-saved/argument registers, which the callee-saved-only
        // spill never covers.
        //
        // 2026-07-31: this used to read `precise_maps && !…`, and `precise_maps`
        // is `precise_jit_maps_enabled() || moving_young_enabled()`. So a
        // ROOT-VISIBILITY mechanism was on only because moving-young defaults
        // on, and `CRATONVM_NO_MOVING_YOUNG=1` silently withdrew it — together
        // with the scratch flush in `emit_pre_safepoint_spill_impl` and shadow
        // publication, all three keyed on the same flag. Each exists so the
        // conservative scan can SEE a register-resident oop across a
        // GC-capable call; none of them is moving-specific, and each was added
        // to fix a real reclaimed-root crash. With all three gone at once the
        // opt-out lane faulted on a zeroed heap slot within seconds of real
        // work — Hibernate `ZonedDateTimeTest` / `OffsetDateTimeTest` (1–3 s,
        // reproduced on a pristine dev build) and the Windows
        // `DateSymbolsProbe` repro. See
        // `jit-no-moving-young-opt-out-unpublishes-roots-CLOSED-20260803.md`.
        //
        // Keyed on its own opt-out alone, the DEFAULT path is byte-identical
        // (`precise_reg_spill_disabled()` is opt-in and unset), and the
        // non-moving lane gets the visibility the default lane already had.
        let reg_spill_for_root_visibility = !precise_reg_spill_disabled();
        let safepoint_reg_spill = safepoint_reg_spill_enabled() || reg_spill_for_root_visibility;
        let safepoint_reg_spill_all = safepoint_reg_spill_all() || reg_spill_for_root_visibility;
        let safepoint_reg_spill_nostore = safepoint_reg_spill_nostore();
        // Register-only operand-stack-oop soundness (DEFAULT ON): flush
        // `CalleeSaved` operand-stack reference entries in `flush_scratch_
        // registers` so a live oop held only in a callee-saved register across a
        // GC-capable call is visible to the conservative root scan.
        let flush_callee_saved_oops = flush_callee_saved_oops_enabled();
        // Shadow stack reserves TWO frame slots: the cached thread pointer and
        // a saved `top` watermark (restored in the epilogue to unwind any
        // unbalanced safepoint push — e.g. the `invokespecial <init>` push that
        // has no paired reload). OSR entries zero both slots (clobber-free) so
        // the null-guards make those frames skip shadow tracking safely.
        let shadow_slots = if shadow_enabled { 3 } else { 0 };
        let jit_thread_slots = if cache_jit_thread_for_inline_new {
            1
        } else {
            0
        };
        // Inline self-recursion check: one slot for the prologue-cached
        // native-stack floor.
        let stack_floor_slots = if reserve_stack_floor { 1 } else { 0 };
        let extra_slots = (if needs_heap { 1 } else { 0 })
            + num_hoists
            + num_arith_hoists
            + arith_scratch_depth
            + num_len_hoists
            + num_scalar_slots
            + (if precise_maps { 1 } else { 0 })
            + jit_thread_slots
            + stack_floor_slots
            + shadow_slots;
        let total_locals = num_locals.saturating_add(extra_slots);
        // Shadow-stack thread-pointer cache slot: the ABSOLUTE last reserved
        // slot (`[rbp - shadow_thread_slot_off]`), set once in the prologue.
        let shadow_thread_slot_off: i32 = if shadow_enabled {
            // Cast: value to i32 (encoding immediate/displacement)
            (total_locals as i32).saturating_mul(8)
        } else {
            0
        };
        // Shadow-stack saved-`top` watermark slot: second-to-last reserved slot.
        let shadow_savetop_slot_off: i32 = if shadow_enabled {
            // Cast: value to i32 (encoding immediate/displacement)
            (total_locals as i32 - 1).saturating_mul(8)
        } else {
            0
        };
        // Shadow-stack per-push saved-base slot: third-to-last reserved slot.
        let shadow_savebase_slot_off: i32 = if shadow_enabled {
            // Cast: value to i32 (encoding immediate/displacement)
            (total_locals as i32 - 2).saturating_mul(8)
        } else {
            0
        };
        // Byte offset of the `ShadowStack` within `JvmThread` (from the helper
        // table), captured before `helpers` is moved into the struct below.
        // Cast: value to i32 (encoding immediate/displacement)
        let shadow_off_in_thread: i32 = helpers.shadow_stack_offset_in_thread as i32;
        // The safepoint-id slot sits below the cached JIT-thread and shadow
        // slots when those gates are on, so the reserved slots never alias.
        let sp_id_slot_off: i32 = if precise_maps {
            // Cast: value to i32 (encoding immediate/displacement)
            (total_locals as i32 - shadow_slots as i32 - jit_thread_slots as i32).saturating_mul(8)
        } else {
            0
        };
        let jit_thread_slot_off: i32 = if cache_jit_thread_for_inline_new {
            (total_locals as i32 - shadow_slots as i32).saturating_mul(8)
        } else {
            0
        };
        // Inline self-recursion floor slot: sits below the sp-id slot (which
        // itself sits below the cached JIT-thread + shadow slots), so the
        // reserved tail slots never alias.
        let stack_floor_slot_off: i32 = if reserve_stack_floor {
            (total_locals as i32
                - shadow_slots as i32
                - jit_thread_slots as i32
                - (if precise_maps { 1 } else { 0 }))
            .saturating_mul(8)
        } else {
            0
        };
        let locals_size = (total_locals.min(i32::MAX as usize / 8) as i32).saturating_mul(8); // Cast: address arithmetic
                                                                                              // Headroom above the operand stack for the direct-call argument-service
                                                                                              // copy. That copy preserves a direct callee's Java arguments for the cold
                                                                                              // exception-table service, and it must NOT be placed on the argument slots
                                                                                              // themselves: `pop_stack` reclaims them but the popped `StackSlot::Frame`s
                                                                                              // stay live until `emit_stack_arg_setup` marshals them, so an aliased
                                                                                              // reservation reverses the arguments into themselves and the callee gets
                                                                                              // arg0 in every slot (fixed-suite-bugs/jit-direct-call-arg1-clobbered-by-arg0-FIXED.md).
                                                                                              //
                                                                                              // The copy needs one slot per argument, and a call site's arguments are
                                                                                              // themselves on the operand stack, so `max_stack` slots of headroom is
                                                                                              // always sufficient. Cap it so a deep-stack method does not double its
                                                                                              // frame for a service copy that can never be that wide; a call with more
                                                                                              // than `DIRECT_CALL_SERVICE_HEADROOM_SLOTS` arguments simply fails the
                                                                                              // reservation and falls back, exactly as an over-wide method does today.
        const DIRECT_CALL_SERVICE_HEADROOM_SLOTS: usize = 16;
        let spill_slots =
            max_stack.saturating_add(max_stack.min(DIRECT_CALL_SERVICE_HEADROOM_SLOTS));
        let spill_size = (spill_slots.min(i32::MAX as usize / 8) as i32).saturating_mul(8); // Cast: address arithmetic
        let shadow_space = 32i32; // Windows x64 shadow space for helper calls
                                  // Reserved bytes ABOVE the shadow region for in-frame stack args to
                                  // any helper called without `emit_stack_arg_setup` (which would
                                  // bump RSP itself). The worst-case site is the PIC/MIC slow path
                                  // that invokes `jit_invoke_virtual_mic` with 6 args: on Windows
                                  // args 5..=6 are written by the JIT to `[RSP+32]` and `[RSP+40]`
                                  // BEFORE the CALL (see the `MOV [RSP+0x28], RAX` emission in the
                                  // invokevirtual slow-path). Reserving only 8 bytes (one slot) made
                                  // the `[RSP+40]` write corrupt either a saved callee-saved GPR or
                                  // the saved XMM region, which surfaced as a delayed STATUS_ACCESS_
                                  // VIOLATION (rc=139) on Windows once the corrupted register was
                                  // restored after the helper returned — observed in Keycloak 26
                                  // `quarkus-run.jar start-dev` SEGFAULTing inside the JIT-compiled
                                  // `picocli/CommandLine$Assert.hashCode(Object)` PIC dispatch and
                                  // recurring across the W2-CHM / RBC.1 / SPB.* cascade documented
                                  // in `vm/src/jit/skip_list.rs`. Reserve 16 bytes (two slots) so
                                  // the 6th stack arg slot lands inside the allocated frame.
        let stack_arg_reserve: i32 = 16;

        // Use graph-coloring allocator results
        let raw_local_assignments = alloc_result.assignments;
        let raw_used_callee_saved = alloc_result.used_callee_saved;
        let raw_local_assignments_len = raw_local_assignments.len();
        let kernel_reg_homes_active = KERNEL_REG_HOMES_ACTIVE.with(|c| c.get());
        let gpr_local_homes_enabled =
            callee_saved_gpr_local_homes_enabled() || kernel_reg_homes_active;
        let local_assignments = if gpr_local_homes_enabled {
            raw_local_assignments
        } else {
            vec![None; raw_local_assignments_len]
        };
        let osr_block_live_in = alloc_result.block_live_in;
        let mut alloc_used_regs = if gpr_local_homes_enabled {
            raw_used_callee_saved
        } else {
            Vec::new()
        };
        // The matrix-dot preheader owns R12..R15 for the duration of its tight
        // loop.  Save them in the normal prologue/epilogue even when diagnostic
        // flags disable register-homed Java locals; unlike PUSH/POP around the
        // loop, frame saves also remain correct if a future cold path exits
        // exceptionally.
        if reserve_matrix_dot_scratch {
            alloc_used_regs.extend([R12, R13, R14, R15]);
            alloc_used_regs.sort_unstable();
            alloc_used_regs.dedup();
        }
        // The x86-64 System V ABI (Linux/macOS) makes every XMM register
        // caller-saved.  Keeping a Java float/double local in XMM8..15 across
        // an invoke therefore loses it when the callee/helper uses SIMD
        // scratch registers.  MonotonicLongValues.Builder.pack exposed this as
        // a zeroed page average after its invokespecial, corrupting Lucene's
        // document map.  Windows x64 preserves XMM6..15, so retain the local
        // allocation there; System V locals must use their canonical frame
        // homes until post-call XMM spill/reload exists.
        #[cfg(windows)]
        let (xmm_assignments, alloc_used_xmms) =
            (alloc_result.xmm_assignments, alloc_result.used_xmm_regs);
        #[cfg(not(windows))]
        let (xmm_assignments, alloc_used_xmms) =
            (vec![None; alloc_result.xmm_assignments.len()], Vec::new());
        let num_reg_locals = local_assignments.iter().filter(|a| a.is_some()).count()
            + xmm_assignments.iter().filter(|a| a.is_some()).count();
        let callee_saved_size = alloc_used_regs.len() as i32 * 8; // Cast: x86-64 immediate encoding
                                                                  // XMM save slots: 8 bytes each (we store the 64-bit value via MOVQ through RAX)
        let xmm_saved_size = alloc_used_xmms.len() as i32 * 8; // Cast: x86-64 immediate encoding

        // Callee-saved registers are saved using MOV into frame slots (not PUSH)
        // to keep RSP stable after SUB RSP. This ensures shadow space is at [RSP..RSP+31].
        let callee_saved_base = locals_size + spill_size + 8;
        // XMM save slots follow GPR save slots
        let xmm_saved_base = callee_saved_base + callee_saved_size;

        // SB-CRASH-04 — per-safepoint callee-saved-register spill area, one slot
        // per used callee-saved GPR. Placed AFTER the prologue's XMM-save region
        // (distinct slots: the prologue's `callee_saved_base` slots hold the
        // CALLER's values for epilogue restore, whereas these hold each
        // safepoint's CURRENT live values for the GC root scan). Sits above the
        // shadow space / stack-arg region (those are nearest RSP), so it never
        // overlaps the helper-call shadow space or the 6th stack-arg slot.
        let reg_spill_size = if safepoint_reg_spill_all {
            // Gap 9: one slot per GPR in the full file (caller- + callee-saved).
            // Cast: buffer position/length to encoding offset (i32/u32)
            ALL_SPILL_GPRS.len() as i32 * 8
        } else if safepoint_reg_spill {
            callee_saved_size
        } else {
            0
        };
        let reg_spill_base = xmm_saved_base + xmm_saved_size;

        // deopt-osr Step 2 / P2 — 256-byte SavedRegisters{gpr:[u64;16],xmm:[u64;16]}
        // region for the frame-deopt stub's in-stub 16-GPR + 16-XMM spill. Reserved
        // only under CRATONVM_DEOPT_REAL so the default frame is byte-identical.
        // Placed just BELOW (deeper than) the per-safepoint reg-spill region and
        // above the shadow/stack-arg region. `deopt_regs_base` is the offset of the
        // DEEPEST qword (gpr[0]=RAX, lowest address): gpr[r] at
        // [rbp - (deopt_regs_base - r*8)] (ascending with r from
        // &gpr[0] = [rbp - deopt_regs_base]); the XMM half follows the GPR half in
        // `#[repr(C)]` order, so xmm[n] at [rbp - (deopt_regs_base - 128 - n*8)].
        // The condition MUST cover every stub that spills into this region.
        // `emit_deopt_stubs` takes the spilling path when
        //
        //     deopt_real_enabled() || matches!(reason, 8 | 9 | 10)
        //
        // and reasons 9/10 are the precise-exception-frame stubs, which
        // `precise_exception_frames` already covers. Reason 8 — the
        // unconditional `invokedynamic` trap — was covered by neither, and that
        // is a stack-corrupting bug rather than a missing optimisation: with the
        // region unreserved `deopt_regs_base` is 0, so the stub's
        // `[rbp - (base - r*8)]` stores become `[rbp]`, `[rbp+8]`, … — walking
        // UP into the caller's frame, over the saved `rbp` and the return
        // address. The epilogue's `ret` then jumps to whatever register landed
        // on the return slot (`rcx`), which is how
        // `CRATONVM_JIT='deopt-real=0'` turned a hot lambda into a SIGSEGV at a
        // constant, unmapped address. Reproducer:
        // `probes/IndyDeoptProbe.java`;
        // `docs/known-issues/jit/deopt-real-off-null-entry-sigsegv-20260803.md`.
        //
        // Byte-identical whenever `deopt_real_enabled()` (the default) or when
        // the method has no `invokedynamic`: the region was already reserved in
        // the first case and is not needed in the second.
        let deopt_regs_size = if deopt_spill_region_reserved(
            crate::deopt_real_enabled(),
            precise_exception_frames,
            has_indy_sites,
        ) {
            32 * 8
        } else {
            0
        };
        debug_assert!(deopt_regs_size == 0 || deopt_regs_size == 256);
        let deopt_regs_base = if deopt_regs_size != 0 {
            reg_spill_base + reg_spill_size + deopt_regs_size
        } else {
            0
        };

        // Total frame = locals + spill + callee-saved GPRs + callee-saved XMMs
        //             + per-safepoint reg-spill (SB-CRASH-04)
        //             + frame-deopt SavedRegisters region (deopt-osr Step 2)
        //             + shadow space + room for in-frame stack args.
        let total = locals_size
            + spill_size
            + callee_saved_size
            + xmm_saved_size
            + reg_spill_size
            + deopt_regs_size
            + shadow_space
            + stack_arg_reserve;

        // After CALL entry: RSP ≡ 8 mod 16 (return addr).
        // After PUSH RBP: RSP ≡ 0 mod 16.
        // After SUB RSP, frame_size: RSP ≡ (0 - frame_size) mod 16.
        // No PUSH after SUB RSP, so just need frame_size ≡ 0 mod 16.
        let frame_size = (total + 15) & !15;

        let base_spill = locals_size + 8; // first spill slot after locals
        let spill_limit = base_spill.saturating_add(spill_size);

        // Heap pointer stored in the extra local slot (beyond max_locals)
        let heap_local_offset = if needs_heap {
            (num_locals as i32 + 1) * 8 // Cast: x86-64 immediate encoding
        } else {
            0
        };

        // LICM: compute frame offsets for hoisted values
        // They go after the heap slot (or after locals if no heap needed)
        let hoist_base = num_locals + (if needs_heap { 1 } else { 0 });
        let hoist_offsets: Vec<i32> = (0..num_hoists)
            .map(|k| ((hoist_base + k) as i32 + 1) * 8) // Cast: x86-64 immediate encoding
            .collect();

        // LICM: integer-arithmetic hoist slots follow the aaload hoist slots.
        let arith_hoist_base = hoist_base + num_hoists;
        let arith_hoist_offsets: Vec<i32> = (0..num_arith_hoists)
            .map(|k| ((arith_hoist_base + k) as i32 + 1) * 8) // Cast: x86-64 immediate encoding
            .collect();
        // Frame offset of the first arith-LICM scratch slot (shared eval stack).
        let arith_scratch_local = arith_hoist_base + num_arith_hoists;
        let arith_scratch_base: i32 = ((arith_scratch_local as i32) + 1) * 8; // Cast: x86-64 immediate encoding

        // LICM: array-length hoist slots follow the arith scratch pool, so
        // none of the four regions alias. Each slot holds a zero-extended
        // 32-bit length; nothing in it is ever a reference, so these slots are
        // deliberately absent from every oop map.
        let len_hoist_base = arith_scratch_local + arith_scratch_depth;
        let array_len_hoist_offsets: Vec<i32> = (0..num_len_hoists)
            .map(|k| ((len_hoist_base + k) as i32 + 1) * 8) // Cast: x86-64 immediate encoding
            .collect();

        Self {
            method_label,
            buf,
            stack: Vec::with_capacity(max_stack),
            next_spill_offset: base_spill,
            pending_live_frame_hi: 0,
            base_spill_offset: base_spill,
            spill_limit_offset: spill_limit,
            num_locals,
            num_params,
            num_reg_locals,
            local_assignments,
            // Built by `compile_with_param_slots` (it has `code`/`param_oop_mask`,
            // which this constructor does not). `None` = conservative fallback.
            safepoint_publish: None,
            pending_narrow_spill: None,
            args_published_for_next_spill: (false, false),
            osr_block_live_in,
            alloc_used_regs,
            xmm_assignments,
            alloc_used_xmms,
            pc_to_native: Vec::new(),
            osr_entry_native: Vec::new(),
            dbg_last_pc: 0,
            dbg_last_op: 0,
            emitted_athrow: false,
            emitted_monitor_call: false,
            precise_exception_frames,
            inline_scope_stack: Vec::new(),
            protected_ranges,
            local_handler_table: Vec::new(),
            local_handler_class_id: 0,
            local_handler_sites: Vec::new(),
            local_handler_site_by_bci: FxHashMap::default(),
            local_handler_stubs: Vec::new(),
            // Installed after construction by `compile_with_param_slots`, and
            // only when it decided to compile rewritten bytecode.
            bci_provenance: None,
            synthetic_guard_span: None,
            emitted_alloc_oom_check: false,
            emitted_checkcast_throw: false,
            emitted_aastore_throw: false,
            forward_patches: Vec::new(),
            jump_table_patches: Vec::new(),
            self_call_patches: Vec::new(),
            frame_size,
            needs_heap,
            heap_local_offset,
            jit_thread_slot_off,
            stack_floor_slot_off,
            xmm_saved_base,
            multianewarray_info,
            field_info,
            compact_field_off: std::collections::HashMap::new(),
            typecheck_info,
            static_field_info,
            hoist_info,
            hoist_offsets,
            arith_hoist_info,
            arith_hoist_offsets,
            array_len_hoist_info,
            array_len_hoist_offsets,
            arith_scratch_base,
            callee_saved_base,
            simd_loops: Vec::new(),
            matrix_dot_loops: Vec::new(),
            bounds_safe_pcs: FxHashSet::default(),
            bounds_check_stubs: Vec::new(),
            null_check_store_stubs: Vec::new(),
            exception_check_stubs: Vec::new(),
            speculative_bce_guards: Vec::new(),
            speculative_bce_guards_by_header: FxHashMap::default(),
            new_info: Vec::new(),
            new_deferred_info: Vec::new(),
            anewarray_info: Vec::new(),
            anewarray_deferred_info: Vec::new(),
            invoke_info: Vec::new(),
            indy_info: Vec::new(),
            direct_calls: Vec::new(),
            mic_slots: Vec::new(),
            pic_slots: Vec::new(),
            helper_call_patches: Vec::new(),
            rip_abs_disp32_patches: Vec::new(),
            ic_patches: Vec::new(),
            cloned_mic_slots: Vec::new(),
            cloned_pic_slots: Vec::new(),
            unroll_loops: Vec::new(),
            body_entry_offset: 0,
            branch_hints: FxHashMap::default(),
            loop_unroll_hints: FxHashMap::default(),
            ldc_info: Vec::new(),
            ldc_string_info: Vec::new(),
            ldc_class_info: Vec::new(),
            ldc2w_info: Vec::new(),
            ldc_fp_pcs: FxHashSet::default(),
            branch_target_stack_depth: FxHashMap::default(),
            branch_target_stack_oop_marks: FxHashMap::default(),
            failed: false,
            failed_site: None,
            unresolved_branch_target: None,
            helpers,
            scratch_xmm_in_use: 0,
            fp_hoist_info: Vec::new(),
            _fp_hoist_offsets: Vec::new(),
            simd_fp_loops: Vec::new(),
            fp_strength_reduction_pcs: FxHashSet::default(),
            scalar_replaced: FxHashMap::default(),
            scalar_field_ops: FxHashMap::default(),
            scalar_init_skips: std::collections::HashSet::new(),
            sr_field_types: FxHashMap::default(),
            sr_local_prov_at: FxHashMap::default(),
            has_elided_monitor: false,
            sr_monitor_at: FxHashMap::default(),
            sr_monitor_scalar_ops: std::collections::HashSet::new(),
            inline_sites: FxHashMap::default(),
            inline_guard_variants: FxHashMap::default(),
            string_layout: None,
            deopt_stubs: Vec::new(),
            stack_oop_marks: Vec::with_capacity(16),
            stack_oop_marks_exact: true,
            pending_staged_arg_oops: Vec::new(),
            pending_staged_args_unmapped: false,
            oop_maps: Vec::new(),
            local_oop_masks: Vec::new(),
            local_kinds: Vec::new(),
            local_kinds_refined: AmbiguousLocalKinds::default(),
            local_liveness: Vec::new(),
            local_liveness_words: 0,
            local_liveness_covered: Vec::new(),
            exception_ranges_dbg_len: 0,
            inline_walk_at: (usize::MAX, 0),
            inline_oop_scopes: Vec::new(),
            uses_long_float_double: false,
            local_oop_reached: Vec::new(),
            param_oop_mask: 0,
            cur_bc_pc: 0,
            slot_mirror: None,
            kernel_operand_cache: kernel_reg_homes_active,
            slot_mirror_suppressed: false,
            precise_maps,
            inline_rbp_tls_disp,
            inline_cm_tls_disp,
            compile_id,
            verify_inline_frame_record,
            sp_id_slot_off,
            safepoint_reg_spill,
            safepoint_reg_spill_all,
            safepoint_reg_spill_nostore,
            flush_callee_saved_oops,
            reg_spill_base,
            sink_alloc_blind_spill: false,
            deferred_alloc_blind_spill: false,
            safepoint_pcs: FxHashSet::default(),
            mapped_safepoint_pcs: FxHashSet::default(),
            shadow_enabled,
            shadow_thread_slot_off,
            shadow_savetop_slot_off,
            shadow_savebase_slot_off,
            shadow_off_in_thread,
            pending_shadow: Vec::new(),
            pending_shadow_coverage_complete: false,
            shadow_fetch_start: 0,
            shadow_fetch_end: 0,
            shadow_overflow_label: None,
            shadow_pushed_any: false,
            induction_vars: Vec::new(),
            null_check_info: crate::null_check_elim::NullCheckInfo::default(),
            implicit_null_pending: Vec::new(),
            implicit_null_sites: Vec::new(),
            simd_element_wise_loops: Vec::new(),
            bulk_zero_byte_fill_loops: Vec::new(),
            bulk_set_byte_stride_loops: Vec::new(),
            byte_sieve_loops: Vec::new(),
            loop_unswitch_candidates: Vec::new(),
            field_info_idx: FxHashMap::default(),
            static_field_info_idx: FxHashMap::default(),
            invoke_info_idx: FxHashMap::default(),
            indy_info_idx: FxHashMap::default(),
            indy_stack_arg_types: FxHashMap::default(),
            stack_kinds: stack_kinds::StackKindMap::default(),
            invoke_stack_arg_types: FxHashMap::default(),
            direct_calls_idx: FxHashMap::default(),
            mic_slots_idx: FxHashMap::default(),
            pic_slots_idx: FxHashMap::default(),
            new_info_idx: FxHashMap::default(),
            new_deferred_idx: FxHashMap::default(),
            anewarray_info_idx: FxHashMap::default(),
            anewarray_deferred_idx: FxHashMap::default(),
            typecheck_info_idx: FxHashMap::default(),
            ldc_info_idx: FxHashMap::default(),
            ldc_string_info_idx: FxHashMap::default(),
            ldc_class_info_idx: FxHashMap::default(),
            ldc2w_info_idx: FxHashMap::default(),
            magic_div_memo: FxHashMap::default(),
            magic_div64_memo: FxHashMap::default(),
            // Set by `compile_with_param_slots` after construction; empty/0
            // here preserves legacy "arg index == slot" behavior.
            param_jvm_slots: Vec::new(),
            param_slot_span: 0,
            deopt_points: Vec::new(),
            deopt_boxes: Vec::new(),
            deopt_point_pcs: Vec::new(),
            deopt_epoch_guard: std::ptr::null(),
            method_key: String::new(),
            gc_inert_selfrec,
            deopt_regs_base,
            deopt_box_ptr_by_bci: FxHashMap::default(),
            exc_frame_box_ptr_by_bci: FxHashMap::default(),
            osr_exit_points: Vec::new(),
            osr_exit_box_ptr_by_bci: FxHashMap::default(),
            osr_exit_test_trigger_bci: None,
            osr_exit_after_count: None,
            deopt_eager_bci: None,
        }
    }

    /// MED-4 / Fix 3 — populate the pc → array-index maps used by hot
    /// codegen lookups. Call once after all the `*_info` Vecs are
    /// installed and before `compile_bytecode` walks the bytecode.
    fn build_pc_indices(&mut self) {
        self.field_info_idx.clear();
        self.field_info_idx.reserve(self.field_info.len());
        for (i, e) in self.field_info.iter().enumerate() {
            self.field_info_idx.insert(e.0, i);
        }
        self.static_field_info_idx.clear();
        self.static_field_info_idx
            .reserve(self.static_field_info.len());
        for (i, e) in self.static_field_info.iter().enumerate() {
            self.static_field_info_idx.insert(e.0, i);
        }
        self.invoke_info_idx.clear();
        self.invoke_info_idx.reserve(self.invoke_info.len());
        for (i, e) in self.invoke_info.iter().enumerate() {
            self.invoke_info_idx.insert(e.0, i);
        }
        self.indy_info_idx.clear();
        self.indy_info_idx.reserve(self.indy_info.len());
        for (i, e) in self.indy_info.iter().enumerate() {
            self.indy_info_idx.insert(e.0, i);
        }
        self.direct_calls_idx.clear();
        self.direct_calls_idx.reserve(self.direct_calls.len());
        for (i, e) in self.direct_calls.iter().enumerate() {
            self.direct_calls_idx.insert(e.0, i);
        }
        self.mic_slots_idx.clear();
        self.mic_slots_idx.reserve(self.mic_slots.len());
        for (i, e) in self.mic_slots.iter().enumerate() {
            self.mic_slots_idx.insert(e.0, i);
        }
        self.pic_slots_idx.clear();
        self.pic_slots_idx.reserve(self.pic_slots.len());
        for (i, e) in self.pic_slots.iter().enumerate() {
            self.pic_slots_idx.insert(e.0, i);
        }
        self.new_info_idx.clear();
        self.new_info_idx.reserve(self.new_info.len());
        for (i, e) in self.new_info.iter().enumerate() {
            self.new_info_idx.insert(e.0, i);
        }
        self.new_deferred_idx.clear();
        self.new_deferred_idx.reserve(self.new_deferred_info.len());
        for (i, e) in self.new_deferred_info.iter().enumerate() {
            self.new_deferred_idx.insert(e.0, i);
        }
        self.anewarray_info_idx.clear();
        self.anewarray_info_idx.reserve(self.anewarray_info.len());
        for (i, e) in self.anewarray_info.iter().enumerate() {
            self.anewarray_info_idx.insert(e.0, i);
        }
        self.anewarray_deferred_idx.clear();
        self.anewarray_deferred_idx
            .reserve(self.anewarray_deferred_info.len());
        for (i, e) in self.anewarray_deferred_info.iter().enumerate() {
            self.anewarray_deferred_idx.insert(e.0, i);
        }
        self.typecheck_info_idx.clear();
        self.typecheck_info_idx.reserve(self.typecheck_info.len());
        for (i, e) in self.typecheck_info.iter().enumerate() {
            self.typecheck_info_idx.insert(e.0, i);
        }
        self.ldc_info_idx.clear();
        self.ldc_info_idx.reserve(self.ldc_info.len());
        for (i, e) in self.ldc_info.iter().enumerate() {
            self.ldc_info_idx.insert(e.0, i);
        }
        self.ldc_string_info_idx.clear();
        self.ldc_string_info_idx.reserve(self.ldc_string_info.len());
        for (i, e) in self.ldc_string_info.iter().enumerate() {
            self.ldc_string_info_idx.insert(e.0, i);
        }
        self.ldc_class_info_idx.clear();
        self.ldc_class_info_idx.reserve(self.ldc_class_info.len());
        for (i, e) in self.ldc_class_info.iter().enumerate() {
            self.ldc_class_info_idx.insert(e.0, i);
        }
        self.ldc2w_info_idx.clear();
        self.ldc2w_info_idx.reserve(self.ldc2w_info.len());
        for (i, e) in self.ldc2w_info.iter().enumerate() {
            self.ldc2w_info_idx.insert(e.0, i);
        }
    }

    /// T5.2.1 — lookup the induction variable for a loop header, if any.
    ///
    /// Returns `Some` if SCEV analysis recognized the loop starting at
    /// `header_pc` as a counted loop with a single IV. Callers use the
    /// stride/bound fields to make unrolling and vectorization
    /// decisions.
    #[allow(dead_code)]
    pub(crate) fn induction_var_for(&self, header_pc: usize) -> Option<&crate::scev::InductionVar> {
        self.induction_vars
            .iter()
            .find(|iv| iv.header_pc == header_pc)
    }

    /// T5.2.14 — query the null-check elimination info.
    ///
    /// Returns `true` when the JIT can statically prove that local
    /// `local` is non-null at the instruction starting at `pc`. Uses
    /// the forward-dataflow result computed by
    /// [`crate::null_check_elim::analyze`].
    ///
    /// Round-11 HIGH-1 fix: the round-7 safe-stub (`return false`)
    /// is replaced with a real query against the meet-over-paths
    /// dataflow result computed by [`crate::null_check_elim::analyze`].
    /// The analysis is sound by construction (intersection at every
    /// join point, monotone-descending lattice, fixpoint iteration
    /// with a hard budget), so reporting `true` here is safe to use
    /// for branch elision at `ifnull`/`ifnonnull` and for skipping
    /// inline null-check stubs at array store/load sites.
    pub(crate) fn is_local_nonnull(&self, pc: usize, local: usize) -> bool {
        self.null_check_info.is_nonnull(pc, local)
    }

    /// Any implicit null-check site still waiting for a recovery address?
    ///
    /// Checked once at the end of a compile. A `true` here means a receiver
    /// check was elided and the slow path it should fault into was never
    /// bound — the site would run unguarded and its NPE would arrive as a
    /// SIGSEGV. The caller fails the compile.
    pub(crate) fn has_unbound_implicit_null_sites(&self) -> bool {
        !self.implicit_null_pending.is_empty()
    }

    /// Bind every pending implicit null-check site to the slow path that
    /// starts at the current buffer position, **after verifying that the
    /// instruction we declined to guard actually faults on a null receiver.**
    ///
    /// # Why the bytes are decoded rather than trusted
    ///
    /// `emit_trusted_oop_receiver_check_at` elides the check and records the
    /// offset the NEXT instruction will occupy. Which instruction that is
    /// belongs to the arm that called it, and an edit there — inserting a
    /// register move, reordering a guard — would silently move the fault onto
    /// an instruction that does not dereference the receiver, or does not
    /// fault at all. The check would then simply be gone, with nothing to say
    /// so.
    ///
    /// So the emitted bytes are decoded here and required to be
    /// `MOV r32, [RAX + disp32]` with `disp32` inside the null page. That is
    /// what the arm emits (the `GC_FLAGS` byte read at `[RAX + 15]`), and the
    /// displacement bound is the same constant the signal handler screens on,
    /// so the compiler and the handler agree by construction rather than by
    /// two people remembering the same number.
    ///
    /// # Fail-closed
    ///
    /// A mismatch calls `self.fail`, which discards the artifact and sends the
    /// method back to the interpreter. There is deliberately no path that
    /// keeps the compile and re-emits the check: the fast path is already
    /// encoded by now, and squeezing a check back in would move everything
    /// after it.
    pub(crate) fn bind_implicit_null_recovery(&mut self) {
        if self.implicit_null_pending.is_empty() {
            return;
        }
        let recover_off = self.buf.pos();
        let pending = std::mem::take(&mut self.implicit_null_pending);
        for (fault_off, bc_pc) in pending {
            let mut head = [0u8; 6];
            let ok = {
                let bytes = self.buf.as_slice();
                if fault_off + 6 <= bytes.len() {
                    head.copy_from_slice(&bytes[fault_off..fault_off + 6]);
                    true
                } else {
                    false
                }
            };
            // `MOV r32, r/m32` (0x8B), ModRM mod=10 (disp32) r/m=000 (RAX),
            // then a little-endian disp32. No REX prefix: both the destination
            // and the base are low registers in the sequence that reaches
            // here, so a REX byte means this is a different instruction.
            let shaped = ok
                && head[0] == 0x8B
                && (head[1] & 0xC7) == 0x80
                && (0..crate::implicit_null::NULL_PAGE_LIMIT as i32)
                    .contains(&i32::from_le_bytes([head[2], head[3], head[4], head[5]]));
            if !shaped {
                self.dbg_last_pc = bc_pc;
                self.fail("implicit-null-shape");
                self.implicit_null_sites.clear();
                return;
            }
            self.implicit_null_sites.push((fault_off, recover_off));
        }
    }

    /// Raise the compile-wide failure flag, naming the site that raised it.
    ///
    /// Only the FIRST call records a site: the dispatch loop deliberately keeps
    /// walking after a failure (several handlers push placeholders so
    /// downstream opcodes keep a plausible stack height), so anything raised
    /// afterwards is a consequence, not the cause.
    #[cold]
    fn fail(&mut self, site: &'static str) {
        if self.failed_site.is_none() {
            self.failed_site = Some((site, self.dbg_last_pc, self.dbg_last_op));
        }
        self.failed = true;
    }

    /// Offset for local variable `idx`: [rbp - (idx+1)*8]
    fn local_offset(&self, idx: usize) -> i32 {
        (idx as i32 + 1) * 8 // Cast: x86-64 immediate encoding
    }

    // -----------------------------------------------------------------------
    // Callee inlining
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/inlining.rs`.

    /// Emit a balanced binary search for lookupswitch.
    /// `pairs` is sorted by key (per JVM spec). Value to match is in EAX.
    /// At each node: CMP EAX, mid_key → JE target, JL left_subtree, fall to right_subtree.
    /// Leaves fall through to `default_target`.
    fn emit_binary_search_lookup(&mut self, pairs: &[(i32, usize)], default_target: usize) {
        if pairs.is_empty() {
            // Base case: no keys left → jump to default
            self.buf.emit_byte(0xE9); // JMP rel32
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        if pairs.len() == 1 {
            // Single key: CMP + JE + JMP default
            let (key, target) = pairs[0];
            self.buf.emit(&[0x3D]); // CMP EAX, imm32
            self.buf.emit(&key.to_le_bytes());
            self.buf.emit(&[0x0F, 0x84]); // JE rel32
            let patch = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((patch, target));
            // Fall through to default
            self.buf.emit_byte(0xE9);
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        if pairs.len() == 2 {
            // Two keys: CMP + JE, CMP + JE, JMP default
            for &(key, target) in pairs {
                self.buf.emit(&[0x3D]);
                self.buf.emit(&key.to_le_bytes());
                self.buf.emit(&[0x0F, 0x84]);
                let patch = self.buf.pos();
                self.buf.emit(&[0; 4]);
                self.forward_patches.push((patch, target));
            }
            self.buf.emit_byte(0xE9);
            let dp = self.buf.pos();
            self.buf.emit(&[0; 4]);
            self.forward_patches.push((dp, default_target));
            return;
        }

        // Pick the middle element
        let mid = pairs.len() / 2;
        let (mid_key, mid_target) = pairs[mid];
        let left = &pairs[..mid];
        let right = &pairs[mid + 1..];

        // CMP EAX, mid_key
        self.buf.emit(&[0x3D]);
        self.buf.emit(&mid_key.to_le_bytes());

        // JE mid_target
        self.buf.emit(&[0x0F, 0x84]);
        let je_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);
        self.forward_patches.push((je_patch, mid_target));

        // JL left_subtree (key < mid_key → search left half)
        self.buf.emit(&[0x0F, 0x8C]); // JL rel32
        let jl_patch = self.buf.pos();
        self.buf.emit(&[0; 4]);

        // Fall through: key > mid_key → search right half
        self.emit_binary_search_lookup(right, default_target);

        // Patch JL to point here (start of left subtree)
        let left_start = self.buf.pos();
        let jl_rel = left_start as i32 - (jl_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(jl_patch, jl_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // Left subtree
        self.emit_binary_search_lookup(left, default_target);
    }

    // -----------------------------------------------------------------------
    // Bytecode compilation
    // -----------------------------------------------------------------------
    //
    // `compile_bytecode` lives in `x64/bytecode_walk.rs`. It is still an
    // inherent method on this same `Compiler`, declared `pub(super)` there so
    // `compile_with_param_slots` below can call it; `Compiler` is private to
    // this module, so that is not reachable from outside the backend.

    /// Record which target `patch_branches` could not resolve, plus the
    /// highest PC at or below it that the emitter actually placed. The pair
    /// distinguishes the two ways this happens: a `nearest` strictly below the
    /// target means the walk stepped OVER it (something consumed the target's
    /// PC without emitting it), and `-1` means nothing below it was emitted at
    /// all (the target sits in a region the walk never entered).
    #[cold]
    fn note_unresolved_branch_target(&mut self, target_pc: usize) {
        if self.unresolved_branch_target.is_some() {
            return;
        }
        let nearest = (0..=target_pc.min(self.pc_to_native.len().saturating_sub(1)))
            .rev()
            .find(|&p| self.pc_to_native[p] >= 0)
            .map_or(-1i64, |p| p as i64);
        self.unresolved_branch_target = Some((target_pc, nearest));
    }

    /// Patch all forward branches and jump-table entries.
    ///
    /// Returns `false` when any recorded branch/table target has no native
    /// offset (`pc_to_native[target_pc] < 0` or out of range).
    ///
    /// **Do not read that as "malformed bytecode".** This comment used to say
    /// an unresolved target meant the method branched into the middle of an
    /// instruction — something a classfile verifier would reject, reachable
    /// only through unverified/synthetic code. That claim was wrong, and it
    /// cost real compiles: every FUSING lowering in the emitter consumes a PC
    /// without emitting it, and if that PC is a branch target the edge onto it
    /// is unresolvable here. The tail-call forms swallowed the `xreturn` at
    /// `pc + 3` and refused five ordinary Spring/bytebuddy methods this way
    /// (`ResolvableType.isAssignableFrom`, `TypeMappedAnnotations.get`, …),
    /// blaming their bytecode. See
    /// `jit-tailcall-swallows-shared-return-FIXED-20260803.md`.
    ///
    /// So when this fires, suspect a fusion before you suspect the classfile:
    /// `note_unresolved_branch_target` records the target and the nearest PC
    /// the emitter actually placed, and `CRATONVM_DBG_JITC=1` prints both.
    /// A `nearest` a few bytes below the target names the instruction whose
    /// arm over-advanced `pc`.
    ///
    /// Previously such patches were silently SKIPPED, leaving the
    /// emitted rel32 placeholder `0`: the branch fell through (or, when the
    /// branch was the last emitted instruction, execution ran off the body
    /// into the out-of-line stubs — observed as a STATUS_ACCESS_VIOLATION
    /// from a hand-written test with an off-by-one target, 2026-06-09).
    /// The caller must discard the method so it stays interpreted.
    #[must_use]
    fn patch_branches(&mut self) -> bool {
        // Patch conditional and unconditional branches
        for &(patch_offset, target_pc) in &self.forward_patches {
            let target_native = if target_pc < self.pc_to_native.len() {
                self.pc_to_native[target_pc]
            } else {
                -1
            };
            if target_native < 0 {
                self.note_unresolved_branch_target(target_pc);
                return false; // unresolved target — reject the method
            }
            // rel32 = target - (patch_offset + 4)
            let rel = target_native - (patch_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                                                                 // `try_patch_i32` already sets the sticky `overflowed` flag and
                                                                 // returns Err on an out-of-bounds offset. Honor the no-panic
                                                                 // bail contract: drop the Err and let the driver's
                                                                 // `if buf.overflowed() { return None; }` discard the method.
            if self.buf.try_patch_i32(patch_offset, rel).is_err() {
                self.buf.mark_overflowed();
            }
        }
        // Patch jump table entries: each entry is an i32 offset from table_base to target
        for &(entry_offset, table_base, target_pc) in &self.jump_table_patches {
            let target_native = if target_pc < self.pc_to_native.len() {
                self.pc_to_native[target_pc]
            } else {
                -1
            };
            if target_native < 0 {
                self.note_unresolved_branch_target(target_pc);
                return false; // unresolved table target — reject the method
            }
            let rel = target_native - table_base as i32; // Cast: x86-64 rel32 displacement
                                                         // See note above: bail via the overflowed flag, never panic.
            if self.buf.try_patch_i32(entry_offset, rel).is_err() {
                self.buf.mark_overflowed();
            }
        }
        true
    }

    fn patch_self_calls(&mut self, entry_offset: usize) {
        for &patch_offset in &self.self_call_patches {
            // rel32 = entry - (patch_offset + 4)
            let rel = entry_offset as i32 - (patch_offset as i32 + 4); // Cast: x86-64 rel32 displacement
                                                                       // See `patch_branches`: bail via the overflowed flag, never panic.
            if self.buf.try_patch_i32(patch_offset, rel).is_err() {
                self.buf.mark_overflowed();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public compilation entry points
// ---------------------------------------------------------------------------
//
// `compile`, `compile_with_param_slots`, their metadata-staging thread-locals
// and the GC-inertness proof for self-recursive bodies live in
// `x64/driver.rs`. The `pub use` above keeps `x64::compile` and
// `x64::compile_with_param_slots` resolving where they always did.

// ---------------------------------------------------------------------------
// Bytecode loop rewriter
// ---------------------------------------------------------------------------
//
// Moved to `x64/loop_rewrite.rs`: the per-thread arming switch, the native and
// bytecode unroll planners, and the side-table replication helpers.

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Flag-skew and header-offset contracts
// ---------------------------------------------------------------------------
//
// Companion doc: `arch-2026-07-26/x64-flag-skew-and-contracts.md`.
//
// These tests defend two properties that no build error would catch:
//
//   1. Codegen and the collector must derive the SAME answer for a shared
//      feature gate. Before 2026-07-26 `CRATONVM_MOVING_YOUNG` was `getenv`'d
//      independently in three crates; a divergence there is heap corruption,
//      not a wrong answer, because the JIT half (publish a complete rewritable
//      root map) and the GC half (run the moving cycle) are only sound
//      together.
//   2. The x86-64 array/field emitters bake object-header offsets into
//      instruction displacements. The planned 32→16-byte `ObjectHeader` shrink
//      must touch every one of them; the inventory counts below are the
//      tripwire that says "the doc's site list is stale".
#[cfg(test)]
mod flag_and_header_contracts;

// ---------------------------------------------------------------------------
// Loop-unroller admission and bytecode-loop-transform provenance
// ---------------------------------------------------------------------------
//
// Two unrollers exist in this backend and exactly one of them is live:
//
//   * the NATIVE byte-copy unroller in the `0xa7` arm of `compile_bytecode`,
//     admitted by `plan_native_unroll`;
//   * the BYTECODE rewriter in `x64::licm` (`plan_loop_peel` /
//     `plan_loop_unroll`), which today is consulted only as the native one's
//     admission oracle and never rewrites the code the emitter compiles.
//
// These tests pin the admission test (what the old `code[back_edge] == 0xa7`
// + body-size heuristic was missing), the mutual exclusion, and the two
// provenance contracts a future rewrite wiring must honour: deopt/oop-map
// bcis stay INTERPRETER bcis, and an OSR entry lands on the steady-state copy
// rather than a peeled prefix.
#[cfg(test)]
mod loop_unroll_admission;

/// One spliced callee's local-variable oop coverage, for the safepoints emitted
/// while its body is being walked.
///
/// `masks`/`reached` come from the SAME forward "must be oop" dataflow the
/// enclosing method uses ([`compute_local_oop_masks`]), run over the callee's
/// own bytecode and seeded with the callee's reference parameters, so a bit is
/// set only when every path reaching that callee pc stored a reference there.
#[derive(Clone, Debug)]
pub struct InlineOopScope {
    /// Frame offset of the callee's JVM local 0: local `k` lives at
    /// `[rbp - (local_base + k*8)]`, the convention `try_emit_inline_body`'s
    /// own `emit_store_local` uses when it marshals the arguments in.
    pub local_base: i32,
    /// Number of JVM local slots this splice reserved for the callee.
    pub num_locals: usize,
    /// Per-callee-pc "must be oop" masks.
    pub masks: Vec<u64>,
    /// Whether the forward dataflow reached each callee pc.
    pub reached: Vec<bool>,
    /// The callee pc currently being emitted.
    pub cur_pc: usize,
}

impl InlineOopScope {
    /// The oop-local mask at the callee pc being emitted, or `None` when the
    /// dataflow cannot answer for it -- an unsupported local count (it returns
    /// empty vectors above 64 locals) or a pc it never reached.
    ///
    /// `None` is a REFUSAL, not an empty mask. A caller that read it as "this
    /// splice holds no references" would be making exactly the claim that
    /// produced the miscompile this type exists to stop.
    pub fn mask_at_cur(&self) -> Option<u64> {
        if self.masks.is_empty() {
            return None;
        }
        if !self.reached.get(self.cur_pc).copied().unwrap_or(false) {
            return None;
        }
        self.masks.get(self.cur_pc).copied()
    }
}
