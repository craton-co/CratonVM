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
    ARRAY_LENGTH_OFFSET, FIELD_CELL_PAYLOAD32_OFFSET, FIELD_CELL_PAYLOAD64_OFFSET,
    FIELD_CELL_TAG_OFFSET, HEADER_SIZE, SLOT_SIZE,
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

/// Byte offset of `ObjectHeader::identity_hash_code`.
///
/// `types` exports a named constant for every other header field but not this
/// one, and the inline TLAB emitter needs it to zero the hash slot explicitly
/// (the lazy-mint contract). Derived from the struct via `offset_of!` rather
/// than written as a literal `8`, so the planned 32→16-byte `ObjectHeader`
/// shrink (fold `forwarding_ptr` + `identity_hash_code` into the mark word)
/// cannot silently leave this emission pointing at the wrong dword. See
/// `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §5.
const IDENTITY_HASH_CODE_OFFSET: usize =
    std::mem::offset_of!(cratonvm_types::ObjectHeader, identity_hash_code);

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
mod bytecode_walk;
mod osr;
mod deopt_stubs;
mod safepoint;
mod frames;
mod operand_stack;
mod emit;
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
    /// `[start_pc, end_pc)` ranges covered by this method's exception table.
    /// Empty when the method has no handlers. Consulted only by
    /// [`Compiler::pc_is_protected`]; see `PROTECTED_RANGES_REQUEST`.
    protected_ranges: Vec<(u32, u32)>,
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
    multianewarray_info: Vec<(usize, u8)>,
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
    /// Each entry is `(action, patch_offset)`: `action` is the JEP-358
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
    null_check_store_stubs: Vec<(u8, usize)>,
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
    /// String ldc sites: (bytecode_pc, stable UTF-8 pointer, byte length).
    /// The bytes are owned by the compiled method; code materializes the Java
    /// object through `helpers.ldc_string` instead of baking an ObjectRef.
    ldc_string_info: Vec<(usize, *const u8, usize)>,
    /// Class-`ldc` sites: `(bytecode_pc, referencing class id, CP index)`.
    /// Served by `helpers.ldc_class_cp`, which resolves the target and
    /// returns its mirror — the mirror is a heap object, so it can neither be
    /// baked as an immediate nor resolved once at compile time.
    ldc_class_info: Vec<(usize, u32, u16)>,
    /// Resolved ldc2_w constants: (bytecode_pc, i64 value).
    ldc2w_info: Vec<(usize, i64)>,
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
    /// `docs/known-issues/tomcat-08-07/testoutputbuffer-writespeed-content-length-mismatch.md`.
    /// Recording the real marks here lets the reconstruction restore them
    /// instead of guessing `false`.
    branch_target_stack_oop_marks: FxHashMap<usize, Vec<bool>>,
    /// Set to true when an internal error (e.g. stack underflow) is detected
    /// during compilation.  `compile_bytecode` checks this and bails out.
    failed: bool,
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
    /// `docs/precise-jit-stack-maps-design.md` (Stage 2).
    local_oop_masks: Vec<u64>,
    /// Stage 2 — companion to `local_oop_masks`: whether the forward local-oop
    /// dataflow reached each PC. Only `reached` PCs get precise local entries.
    local_oop_reached: Vec<bool>,
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
    /// (`docs/known-issues/tomcat-08-07/testoutputbuffer-writespeed-content-length-mismatch.md`).
    local_liveness: Vec<u64>,
    /// Parallel coverage bitmap for [`Self::local_liveness`]: `false` at a pc
    /// no basic block covers, where the liveness answer is the `0` default
    /// ("nothing live") rather than a computed result. Treating that as "every
    /// local is dead" would discard the whole frame, so the snapshot builder
    /// falls back to "everything live" there.
    local_liveness_covered: Vec<bool>,
    /// Debug-only: number of exception ranges modelled by the liveness /
    /// interference analyses for this method (CRATONVM_DBG_EXCFRAME).
    exception_ranges_dbg_len: usize,
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
        refine_ambiguous_local_kinds,
        typed_local_frame_value, LocalKind,
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

impl Compiler {
    #[allow(clippy::too_many_arguments)]
    fn new(
        method_label: String,
        buf: ExecutableBuffer,
        num_locals: usize,
        num_params: usize,
        max_stack: usize,
        needs_heap: bool,
        multianewarray_info: Vec<(usize, u8)>,
        field_info: Vec<(usize, usize, u8)>,
        typecheck_info: Vec<(usize, *const u8, usize)>,
        static_field_info: Vec<(usize, u32, usize, u8, bool)>,
        hoist_info: Vec<LoopHoist>,
        arith_hoist_info: Vec<ArithLoopHoist>,
        alloc_result: super::regalloc::RegAllocResult,
        reserve_matrix_dot_scratch: bool,
        helpers: JitRuntimeHelpers,
        num_scalar_slots: usize,
        cache_jit_thread_for_inline_new: bool,
        reserve_stack_floor: bool,
        gc_inert_selfrec: bool,
        precise_exception_frames: bool,
        protected_ranges: Vec<(u32, u32)>,
    ) -> Self {
        // Compact arrays: byte[] uses 1-byte elements, int[] uses 4-byte, ref[] uses 8-byte.
        // Each local takes 8 bytes: [rbp - 8], [rbp - 16], ...
        // If needs_heap, reserve one extra slot for the heap pointer.
        // If LICM hoisting is active, reserve extra slots for hoisted values.
        // If scalar replacement is active, reserve extra slots for replaced object fields.
        let num_hoists = hoist_info.len();
        let num_arith_hoists = arith_hoist_info.len();
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
        // `docs/known-issues/jit-no-moving-young-opt-out-unpublishes-roots.md`.
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
        // arg0 in every slot (docs/known-issues/jit-direct-call-arg1-clobbered-by-arg0.md).
        //
        // The copy needs one slot per argument, and a call site's arguments are
        // themselves on the operand stack, so `max_stack` slots of headroom is
        // always sufficient. Cap it so a deep-stack method does not double its
        // frame for a service copy that can never be that wide; a call with more
        // than `DIRECT_CALL_SERVICE_HEADROOM_SLOTS` arguments simply fails the
        // reservation and falls back, exactly as an over-wide method does today.
        const DIRECT_CALL_SERVICE_HEADROOM_SLOTS: usize = 16;
        let spill_slots = max_stack.saturating_add(max_stack.min(DIRECT_CALL_SERVICE_HEADROOM_SLOTS));
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
        let deopt_regs_size = if crate::deopt_real_enabled() || precise_exception_frames {
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
            protected_ranges,
            // Installed after construction by `compile_with_param_slots`, and
            // only when it decided to compile rewritten bytecode.
            bci_provenance: None,
            emitted_alloc_oom_check: false,
            emitted_checkcast_throw: false,
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
            branch_target_stack_depth: FxHashMap::default(),
            branch_target_stack_oop_marks: FxHashMap::default(),
            failed: false,
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
            string_layout: None,
            deopt_stubs: Vec::new(),
            stack_oop_marks: Vec::with_capacity(16),
            stack_oop_marks_exact: true,
            oop_maps: Vec::new(),
            local_oop_masks: Vec::new(),
            local_kinds: Vec::new(),
            local_kinds_refined: AmbiguousLocalKinds::default(),
            local_liveness: Vec::new(),
            local_liveness_covered: Vec::new(),
            exception_ranges_dbg_len: 0,
            uses_long_float_double: false,
            local_oop_reached: Vec::new(),
            cur_bc_pc: 0,
            slot_mirror: None,
            kernel_operand_cache: kernel_reg_homes_active,
            slot_mirror_suppressed: false,
            precise_maps,
            inline_rbp_tls_disp,
            verify_inline_frame_record,
            sp_id_slot_off,
            safepoint_reg_spill,
            safepoint_reg_spill_all,
            safepoint_reg_spill_nostore,
            flush_callee_saved_oops,
            reg_spill_base,
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

    /// Offset for local variable `idx`: [rbp - (idx+1)*8]
    fn local_offset(&self, idx: usize) -> i32 {
        (idx as i32 + 1) * 8 // Cast: x86-64 immediate encoding
    }

    // -----------------------------------------------------------------------
    // OSR exit maps
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/osr.rs`. OSR *entry* publication still lives in
    // `compile_with_param_slots` below.



    // -----------------------------------------------------------------------
    // Safepoints, shadow stack and oop maps
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/safepoint.rs`: the polls, the live-oop publication, and the
    // two elision proofs that let a poll skip publishing.




    /// Emit IEEE 754 NaN/overflow fixup after a CVTT instruction.
    ///
    /// x86 CVTTSS2SI/CVTTSD2SI returns the "indefinite integer" (0x80000000 for
    /// 32-bit, 0x8000000000000000 for 64-bit) for NaN AND overflow. The JVM
    /// spec requires: NaN→0, +overflow→MAX_VALUE, -overflow→MIN_VALUE.
    ///
    /// Call this immediately after the CVTT while XMM0 still holds the source.
    fn emit_fp_to_int_nan_fixup(&mut self, is_double: bool, is_long: bool) {
        if !is_long {
            // CMP EAX, 0x80000000
            self.buf.emit_byte(0x3D);
            self.buf.emit(&0x80000000u32.to_le_bytes());
            // JNE .done (short)
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0 — PF=1 if NaN
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign of source.
            // PXOR XMM1, XMM1
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]);
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]); // UCOMISD XMM0, XMM1
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]); // UCOMISS XMM0, XMM1
            }
            // JBE .done (negative overflow — 0x80000000 already correct)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV EAX, 0x7FFFFFFF
            self.buf.emit_byte(0xB8);
            self.buf.emit(&0x7FFFFFFFu32.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR EAX, EAX
            let nan_off = self.buf.pos();
            // Widening: usize offset -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(&mut self.buf, jp_patch, nan_off as i64 - jp_patch as i64 - 1);
            self.buf.emit(&[0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            // Widening: usize offsets -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jne_patch,
                done_off as i64 - jne_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jbe_patch,
                done_off as i64 - jbe_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jmp_patch,
                done_off as i64 - jmp_patch as i64 - 1,
            );
        } else {
            // 64-bit: CMP RAX with 0x8000000000000000
            // MOV RCX, 0x8000000000000000
            self.buf.emit(&[0x48, 0xB9]);
            self.buf.emit(&0x8000000000000000u64.to_le_bytes());
            // CMP RAX, RCX
            self.buf.emit(&[0x48, 0x39, 0xC8]);
            // JNE .done
            self.buf.emit_byte(0x75);
            let jne_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // UCOMI XMM0, XMM0
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC0]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC0]);
            }
            // JP .nan
            self.buf.emit_byte(0x7A);
            let jp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Not NaN — overflow. Check sign.
            self.buf.emit(&[0x66, 0x0F, 0xEF, 0xC9]); // PXOR XMM1, XMM1
            if is_double {
                self.buf.emit(&[0x66, 0x0F, 0x2E, 0xC1]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, 0xC1]);
            }
            // JBE .done (negative overflow)
            self.buf.emit_byte(0x76);
            let jbe_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // Positive overflow: MOV RAX, 0x7FFFFFFFFFFFFFFF
            self.buf.emit(&[0x48, 0xB8]);
            self.buf.emit(&0x7FFFFFFFFFFFFFFFu64.to_le_bytes());
            // JMP .done
            self.buf.emit_byte(0xEB);
            let jmp_patch = self.buf.pos();
            self.buf.emit_byte(0x00);

            // .nan: XOR RAX, RAX (48 31 C0)
            let nan_off = self.buf.pos();
            // Widening: usize offset -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(&mut self.buf, jp_patch, nan_off as i64 - jp_patch as i64 - 1);
            self.buf.emit(&[0x48, 0x31, 0xC0]);

            // .done:
            let done_off = self.buf.pos();
            // Widening: usize offsets -> i64 (no truncation; for displacement math)
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jne_patch,
                done_off as i64 - jne_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jbe_patch,
                done_off as i64 - jbe_patch as i64 - 1,
            );
            Self::patch_rel8_or_bail(
                &mut self.buf,
                jmp_patch,
                done_off as i64 - jmp_patch as i64 - 1,
            );
        }
    }


    /// LICM: emit a hoisted loop-invariant integer-arithmetic expression.
    ///
    /// The RPN `steps` program is evaluated using the dedicated arith-LICM
    /// scratch slot pool as a value stack; the final (single) result is left
    /// in RAX. Machine-code sequences for each binary op match the main
    /// emitter byte-for-byte (32-bit ALU op + `movsxd rax,eax` for the
    /// sign-extending ops, plain 32-bit for `iushr`), so the hoisted value is
    /// bit-identical to recomputing the expression in place.
    ///
    /// `PushLocal` reads the local via its register assignment if it has one,
    /// otherwise from its frame slot — the local is loop-invariant so its
    /// value is the same here (pre-header) as on every iteration.
    fn emit_arith_hoist_into_rax(&mut self, steps: &[ArithStep]) {
        let scratch_base = self.arith_scratch_base;
        let slot = |k: usize| scratch_base + (k as i32) * 8; // Cast: x86-64 immediate encoding
        let mut depth: usize = 0;
        for step in steps {
            match *step {
                ArithStep::PushConst(v) => {
                    self.emit_mov_imm32_sx(RAX, v);
                    let off = slot(depth);
                    self.emit_store_local(off, RAX);
                    depth += 1;
                }
                ArithStep::PushLocal(l) => {
                    if let Some(reg) = self.reg_for_local(l) {
                        self.emit_mov_reg_reg(RAX, reg);
                    } else {
                        self.emit_load_local(RAX, self.local_offset(l));
                    }
                    let off = slot(depth);
                    self.emit_store_local(off, RAX);
                    depth += 1;
                }
                ArithStep::BinOp(op) => {
                    // depth >= 2 guaranteed by `match_invariant_iarith`.
                    depth -= 1;
                    let off_b = slot(depth);
                    depth -= 1;
                    let off_a = slot(depth);
                    self.emit_load_local(RCX, off_b); // b → RCX
                    self.emit_load_local(RAX, off_a); // a → RAX
                    match op {
                        0x60 => {
                            // iadd: ADD eax,ecx ; movsxd rax,eax
                            self.buf.emit(&[0x01, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x64 => {
                            // isub: SUB eax,ecx ; movsxd
                            self.buf.emit(&[0x29, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x68 => {
                            // imul: IMUL eax,ecx ; movsxd
                            self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x78 => {
                            // ishl: SHL eax,cl ; movsxd
                            self.buf.emit(&[0xD3, 0xE0]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x7a => {
                            // ishr: SAR eax,cl ; movsxd
                            self.buf.emit(&[0xD3, 0xF8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x7c => {
                            // iushr: SHR eax,cl (32-bit zero-extends)
                            self.buf.emit(&[0xD3, 0xE8]);
                        }
                        0x7e => {
                            // iand: AND eax,ecx ; movsxd
                            self.buf.emit(&[0x21, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x80 => {
                            // ior: OR eax,ecx ; movsxd
                            self.buf.emit(&[0x09, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        0x82 => {
                            // ixor: XOR eax,ecx ; movsxd
                            self.buf.emit(&[0x31, 0xC8]);
                            self.rex_w();
                            self.buf.emit(&[0x63, 0xC0]);
                        }
                        _ => unreachable!("non-hoistable binop reached emit"),
                    }
                    let off_res = slot(depth);
                    self.emit_store_local(off_res, RAX);
                    depth += 1;
                }
            }
        }
        // Result is the single value left in scratch slot 0.
        self.emit_load_local(RAX, slot(0));
    }






    // -----------------------------------------------------------------------
    // Peephole: constant + arithmetic fusion
    // -----------------------------------------------------------------------

    /// Try to fuse a known constant with the immediately following arithmetic
    /// opcode (imul/idiv/irem). The constant is the RIGHT operand (top of stack).
    /// If the peephole fires, the following opcode is consumed and `true` is returned.
    ///
    /// `branch_targets` is the per-PC branch-target map of the enclosing
    /// `compile_bytecode` pass: fusing is only sound when `next_op_pc` is NOT
    /// a branch target. The fused sequence binds `pc_to_native[next_op_pc]`
    /// to code that hardcodes THIS path's constant and pops only the left
    /// operand; another predecessor branching to `next_op_pc` arrives with
    /// its own right operand on the canonical stack (ternary-in-step merge:
    /// `i += i == 0 ? 2 : 1` — both `iconst` arms feed one `iadd`), so it
    /// would have that operand silently dropped and the fall-through
    /// constant used instead (gap-jit-ternary-in-loop-increment).
    fn try_const_arith_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> bool {
        if next_op_pc >= code_len {
            return false;
        }
        // Never fuse across a merge point (see doc comment).
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return false;
        }
        let next_op = code[next_op_pc];
        match next_op {
            // imul: left × const_val — always optimizable
            0x68 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_imul_const(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — power-of-2 only
            0x6c if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_pow2(const_val);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — power-of-2 only
            0x70 if const_val > 0 && (const_val & (const_val - 1)) == 0 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_pow2(const_val);
                self.push_from_rax();
                true
            }
            // idiv: left / const_val — non-power-of-2 (magic number method)
            0x6c if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_idiv_magic(magic, shift);
                self.push_from_rax();
                true
            }
            // irem: left % const_val — non-power-of-2 (magic number method)
            0x70 if const_val >= 2 => {
                let (magic, shift) = self.magic_div_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_irem_magic(magic, shift, const_val);
                self.push_from_rax();
                true
            }
            // iadd: left + const_val
            0x60 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC0, const_val as u8]); // ADD EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xC0]); // ADD EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // isub: left - const_val
            0x64 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val == 0 {
                    // no-op
                } else if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE8, const_val as u8]); // SUB EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x81, 0xE8]); // SUB EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iand: left & const_val
            0x7e => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xE0, const_val as u8]); // AND EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x25]); // AND EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ior: left | const_val
            0x80 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xC8, const_val as u8]); // OR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x0D]); // OR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ixor: left ^ const_val
            0x82 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF0, const_val as u8]); // XOR EAX, imm8 // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit(&[0x35]); // XOR EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishl: left << const_val (constant shift count)
            0x78 if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE0, const_val as u8]); // SHL EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // ishr: left >> const_val (arithmetic, constant shift count)
            0x7a if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xF8, const_val as u8]); // SAR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            // iushr: left >>> const_val (logical, constant shift count)
            0x7c if (0..=31).contains(&const_val) => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.buf.emit(&[0xC1, 0xE8, const_val as u8]); // SHR EAX, imm8 // Cast: x86-64 immediate encoding
                self.rex_w();
                self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
                self.push_from_rax();
                true
            }
            _ => false,
        }
    }

    /// Try to fuse a constant with a following if_icmp* opcode.
    /// The constant is value2 (top of stack); value1 is already on the simulated stack.
    /// If the peephole fires, the if_icmp opcode is consumed and the new PC is returned.
    ///
    /// `branch_targets`: same soundness precondition as
    /// `try_const_arith_peephole` — a fused `const; if_icmp*` binds
    /// `pc_to_native[next_op_pc]` to code that compares against THIS path's
    /// constant; a predecessor branching to the if_icmp expects its own
    /// value2 on the stack. Never fuse across a merge point.
    fn try_const_compare_peephole(
        &mut self,
        const_val: i32,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> Option<usize> {
        if next_op_pc + 2 >= code_len {
            return None;
        }
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return None;
        }
        let next_op = code[next_op_pc];
        let cc = match next_op {
            0x9f => 0x84u8, // if_icmpeq → JE
            0xa0 => 0x85,   // if_icmpne → JNE
            0xa1 => 0x8C,   // if_icmplt → JL
            0xa2 => 0x8D,   // if_icmpge → JGE
            0xa3 => 0x8F,   // if_icmpgt → JG
            0xa4 => 0x8E,   // if_icmple → JLE
            _ => return None,
        };

        self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding

        let offset = i16::from_be_bytes([code[next_op_pc + 1], code[next_op_pc + 2]]) as i32; // Widening: always safe
        let target_pc = (next_op_pc as i32 + offset) as usize; // Cast: x86-64 immediate encoding

        // Values left below value1 must be flushed to canonical frame slots
        // for the taken edge (the merge-target revival reconstructs them from
        // canonical offsets; the regular if_icmp handler does the same).
        // Canonicalize BEFORE popping value1: a register-resident slot below
        // it would otherwise be stored to a canonical offset that can collide
        // with value1's own frame slot (register slots occupy a stack
        // position but no frame slot, shifting the slots above them down).
        // With value1 still on the simulated stack it is relocated above
        // every store target, so the CMP below reads the preserved value.
        if target_pc > next_op_pc && self.stack.len() > 1 {
            self.canonicalize_stack();
        }

        // Pop value1 (already on stack before the constant was pushed)
        let val1 = self.pop_stack();
        match val1 {
            StackSlot::CalleeSaved(reg) | StackSlot::Scratch(reg) => {
                // CMP reg32, imm — direct compare without loading to RAX
                if reg >= 8 {
                    self.buf.emit_byte(0x41); // REX.B
                }
                if (-128..=127).contains(&const_val) {
                    self.buf.emit_byte(0x83); // CMP r/m32, imm8
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit_byte(const_val as u8); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x81); // CMP r/m32, imm32
                    self.buf.emit_byte(0xF8 | (reg & 7));
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Frame(off) => {
                self.emit_load_local(RAX, off);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
            StackSlot::Xmm(xmm) => {
                self.emit_movq_rax_from_xmm(xmm);
                if (-128..=127).contains(&const_val) {
                    self.buf.emit(&[0x83, 0xF8, const_val as u8]); // Cast: x86-64 immediate encoding
                } else {
                    self.buf.emit_byte(0x3D); // CMP EAX, imm32
                    self.buf.emit(&const_val.to_le_bytes());
                }
            }
        }

        // Emit Jcc rel32
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(cc);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        self.forward_patches.push((patch_offset, target_pc));
        // Record the taken-edge stack depth so the merge-target revival
        // rebuilds the canonicalized slots (mirrors the regular handler).
        self.record_branch_target_depth(target_pc);
        self.reset_spills();

        Some(next_op_pc + 3)
    }

    // -----------------------------------------------------------------------
    // peephole-cmov — Round-11 HIGH-3
    //
    // Detect the user-written equivalent of `Math.min` / `Math.max`:
    //
    //     iload a            // val1
    //     iload b            // val2
    //     if_icmpXX L1       // 3 bytes
    //     iload a-or-b       // 1 byte   (INSTR_A: the "taken-false" value)
    //     goto    L2         // 3 bytes
    //   L1:
    //     iload b-or-a       // 1 byte   (INSTR_B: the "taken-true" value)
    //   L2:
    //
    // and lower it to `CMP / MOV / CMOV` instead of a branch. The
    // explicit `Math.min(a,b)` invokestatic intrinsic is handled
    // separately in the invoke dispatcher; this peephole catches the
    // inlined source-level pattern. Conservative: only the canonical
    // javac shape where INSTR_A and INSTR_B are 1-byte `iload_0..3`
    // of two *different* locals X and Y, and the immediately-
    // preceding operand loads (also `iload_0..3`) are the same two
    // locals in some order.
    // -----------------------------------------------------------------------

    /// Lookup `iload_0..3` opcode → local index. Returns `None` for
    /// any other opcode.
    fn iload_short_local(op: u8) -> Option<usize> {
        if (0x1A..=0x1D).contains(&op) {
            // Widening: u8 -> usize (opcode-relative local index, value fits)
            Some((op - 0x1A) as usize)
        } else {
            None
        }
    }

    /// Try to emit the if_icmp + iload + goto + iload min/max peephole
    /// as a branchless CMOV sequence. Called at the start of the
    /// if_icmp opcode handler at PC `pc`. If the peephole fires, all
    /// 4 source instructions (if_icmp, INSTR_A, goto, INSTR_B) are
    /// consumed; the function emits CMP+MOV+CMOV and returns
    /// `Some(new_pc)` — the PC to resume from (= the merge point L2).
    /// On `None` the caller emits the regular branch sequence.
    ///
    /// Pre-condition: `val1` and `val2` are the popped operands of
    /// the if_icmp (val1 is the deeper one). The caller MUST NOT
    /// have emitted the CMP or any output yet.
    fn try_cmov_minmax_peephole(
        &mut self,
        code: &[u8],
        pc: usize,
        op: u8,
        val1: StackSlot,
        val2: StackSlot,
    ) -> Option<usize> {
        // Only handle if_icmplt / if_icmpge.
        if !matches!(op, 0xa1 | 0xa2) {
            return None;
        }
        if pc + 3 > code.len() {
            return None;
        }
        // if_icmp branch offset
        // Cast: value to i32 (encoding immediate/displacement)
        let off1 = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let l1_pc = (pc as i32).checked_add(off1)?;
        if l1_pc < 0 {
            return None;
        }
        // Cast: non-negative index/count to usize
        let l1_pc = l1_pc as usize;

        // INSTR_A at pc+3 must be iload_0..3 (1 byte).
        let a_pc = pc + 3;
        if a_pc >= code.len() {
            return None;
        }
        let a_local = Self::iload_short_local(code[a_pc])?;

        // Next instruction must be `goto` (0xa7) at pc+4.
        let goto_pc = a_pc + 1;
        if goto_pc + 3 > code.len() || code[goto_pc] != 0xa7 {
            return None;
        }
        // Cast: value to i32 (encoding immediate/displacement)
        let goto_off = i16::from_be_bytes([code[goto_pc + 1], code[goto_pc + 2]]) as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let l2_pc = (goto_pc as i32).checked_add(goto_off)?;
        if l2_pc < 0 {
            return None;
        }
        // Cast: non-negative index/count to usize
        let l2_pc = l2_pc as usize;

        // INSTR_B at the if_icmp branch target. Must be exactly the
        // byte after the `goto` (i.e. pc+7).
        let b_pc = l1_pc;
        if b_pc != goto_pc + 3 || b_pc >= code.len() {
            return None;
        }
        let b_local = Self::iload_short_local(code[b_pc])?;

        // L2 must point at the byte right after INSTR_B (b_pc + 1).
        if l2_pc != b_pc + 1 {
            return None;
        }

        // The two inner loads must be of two *different* locals.
        if a_local == b_local {
            return None;
        }

        // The two if_icmp operands must come from `iload_0..3` of the
        // same two locals (in some order). Canonical pattern:
        //   iload_X (1 byte) ; iload_Y (1 byte) ; if_icmpXX
        if pc < 2 {
            return None;
        }
        let v1_local = Self::iload_short_local(code[pc - 2])?;
        let v2_local = Self::iload_short_local(code[pc - 1])?;
        let mut pair = [v1_local, v2_local];
        pair.sort_unstable();
        let mut inner = [a_local, b_local];
        inner.sort_unstable();
        if pair != inner {
            return None;
        }

        // ---- All checks passed. Emit branchless CMOV. ------------------
        let r1 = self.slot_to_gpr(val1, RAX);
        let r2 = self.slot_to_gpr(val2, RCX);
        self.emit_cmp_r32_r32(r1, r2);
        // Load INSTR_A's value (fall-through) into RAX and INSTR_B's value
        // (taken) into RCX. These must respect register allocation: when a
        // local is register-mapped its frame slot is never written, so a
        // raw `emit_load_local` would read uninitialized stack garbage.
        // (CMP above is already emitted, so clobbering RAX/RCX is safe; the
        // callee-saved home registers of the locals are never RAX/RCX.)
        match self.reg_for_local(a_local) {
            Some(reg) => self.emit_mov_reg_reg(RAX, reg),
            None => {
                let a_off = self.local_offset(a_local);
                self.emit_load_local(RAX, a_off);
            }
        }
        match self.reg_for_local(b_local) {
            Some(reg) => self.emit_mov_reg_reg(RCX, reg),
            None => {
                let b_off = self.local_offset(b_local);
                self.emit_load_local(RCX, b_off);
            }
        }
        // CMOVcc EAX, ECX (no REX.W; iload values are 32-bit ints):
        //   0xa1 → JL  → CMOVL  (0x4C)
        //   0xa2 → JGE → CMOVGE (0x4D)
        let cmov_cc = match op {
            0xa1 => 0x4Cu8,
            0xa2 => 0x4Du8,
            _ => return None,
        };
        // peephole-cmov: branchless lowering of user-written min/max.
        self.buf.emit(&[0x0F, cmov_cc, 0xC1]);
        // The 32-bit CMOV zero-extends into the upper 32 bits of RAX. The
        // JIT keeps `int` values sign-extended to 64 bits (see i2b/i2s/i2l),
        // so re-extend the selected value — otherwise a negative result
        // (e.g. min(-7, 4)) surfaces as a large positive. MOVSXD RAX, EAX.
        self.buf.emit(&[0x48, 0x63, 0xC0]);
        // Push RAX as the merged result.
        self.push_from_rax();

        // Map every consumed bytecode PC to the current native offset
        // so downstream PC-keyed lookups still find a valid destination.
        // Cast: buffer position/length to encoding offset (i32/u32)
        let native = self.buf.pos() as i32;
        for p in pc..=l2_pc {
            if p < self.pc_to_native.len() {
                self.pc_to_native[p] = native;
            }
        }
        // Record the merge-point stack depth so the dispatch loop's
        // merge-point canonicalization (if it kicks in at L2) sees a
        // consistent expectation. We just pushed one value.
        self.record_branch_target_depth(l2_pc);

        Some(l2_pc)
    }

    /// Emit optimized multiply by a known constant (result in EAX, sign-extended to RAX).
    fn emit_imul_const(&mut self, val: i32) {
        match val {
            0 => {
                self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            }
            1 => { /* input already in EAX */ }
            -1 => {
                self.buf.emit(&[0xF7, 0xD8]); // NEG EAX
            }
            2 => {
                self.buf.emit(&[0x01, 0xC0]); // ADD EAX, EAX
            }
            3 => {
                // LEA EAX, [RAX + RAX*2]
                self.buf.emit(&[0x8D, 0x04, 0x40]);
            }
            4 => {
                self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2
            }
            5 => {
                // LEA EAX, [RAX + RAX*4]
                self.buf.emit(&[0x8D, 0x04, 0x80]);
            }
            8 => {
                self.buf.emit(&[0xC1, 0xE0, 0x03]); // SHL EAX, 3
            }
            9 => {
                // LEA EAX, [RAX + RAX*8]
                self.buf.emit(&[0x8D, 0x04, 0xC0]);
            }
            // round-7 fix (bug 6): power-of-2 fast path for val >= 16.
            // 2/4/8 are handled above; 16/32/.../2^30 fall through to IMUL
            // imm32 (5 bytes) when they could be a 3-byte SHL EAX, imm8.
            // Negative powers of two are intentionally left to the IMUL
            // path — SHL produces an unsigned shift, and emitting
            // SHL + NEG would not be smaller than IMUL imm8/imm32.
            // Cast: bytecode/native offset to u32 (non-negative, fits)
            _ if val > 0 && (val as u32).is_power_of_two() => {
                // Cast: bytecode/native offset to u32 (non-negative, fits)
                let k = (val as u32).trailing_zeros() as u8;
                // SHL EAX, k (32-bit shift; high bits zero anyway, then
                // the MOVSXD below sign-extends, matching Java imul
                // semantics for non-negative results).
                self.buf.emit(&[0xC1, 0xE0, k]); // SHL EAX, imm8
            }
            _ if (-128..=127).contains(&val) => {
                // IMUL EAX, EAX, imm8
                self.buf.emit(&[0x6B, 0xC0, val as u8]); // Cast: x86-64 immediate encoding
            }
            _ => {
                // IMUL EAX, EAX, imm32
                self.buf.emit(&[0x69, 0xC0]);
                self.buf.emit(&val.to_le_bytes());
            }
        }
        // Sign-extend result to 64 bits (safe no-op for val==0 which zeros RAX)
        if val != 0 {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
        }
    }

    /// Emit a folded affine self-update `local = local*k + c` for an int local
    /// (the result of [`match_affine_chain`]). Reads the local into RAX,
    /// multiplies + adds the (sign-extended) constants, writes it back — a
    /// net-zero effect on the simulated operand stack (the matched bytecodes
    /// were a balanced load…store run), using RAX as the only scratch.
    fn emit_affine_fold(&mut self, local: usize, k: i32, c: i32) {
        // RAX = local
        if let Some(reg) = self.reg_for_local(local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(local));
        }
        // RAX *= k  (emit_imul_const sign-extends the result for k != 0)
        self.emit_imul_const(k);
        // RAX += c  (matches the const-arith peephole's add encoding)
        if c != 0 {
            if (-128..=127).contains(&c) {
                // Truncation: wider int -> u8 (low 8 bits, intentional)
                self.buf.emit(&[0x83, 0xC0, c as u8]); // ADD EAX, imm8
            } else {
                self.buf.emit(&[0x81, 0xC0]); // ADD EAX, imm32
                self.buf.emit(&c.to_le_bytes());
            }
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
        }
        // local = RAX
        if let Some(reg) = self.reg_for_local(local) {
            self.emit_mov_reg_reg(reg, RAX);
        } else {
            self.emit_store_local(self.local_offset(local), RAX);
        }
    }

    /// Emit optimized signed division by power-of-2 constant.
    /// Result: EAX = EAX / 2^k (rounded toward zero), sign-extended to RAX.
    /// Long (cat-2) sibling of [`Self::try_const_arith_peephole`]: fuse a
    /// resolved `ldc2_w` long constant with the immediately following
    /// `lmul`/`ldiv`/`lrem`/`ladd`/`lsub`. Same merge-point rule: never fuse
    /// when the arith op is a branch target. The JVMS ArithmeticException
    /// guard is unnecessary — the constant divisor is known non-zero — and
    /// LONG_MIN / -1 cannot arise (only positive divisors fuse).
    fn try_const_arith_peephole_long(
        &mut self,
        const_val: i64,
        next_op_pc: usize,
        code: &[u8],
        code_len: usize,
        branch_targets: &[bool],
    ) -> bool {
        if next_op_pc >= code_len {
            return false;
        }
        if branch_targets.get(next_op_pc).copied().unwrap_or(true) {
            return false;
        }
        let fits_i32 = (-0x8000_0000i64..=0x7FFF_FFFF).contains(&const_val);
        match code[next_op_pc] {
            // lmul: left * const
            0x69 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if fits_i32 {
                    self.rex_w();
                    self.buf.emit_byte(0x69); // IMUL RAX, RAX, imm32
                    self.modrm_reg(RAX, RAX);
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                } else {
                    self.emit_mov_imm64(RDX, const_val);
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
                }
                self.push_from_rax();
                true
            }
            // ldiv: left / const — power-of-2
            0x6d if const_val > 0
                && (const_val & (const_val - 1)) == 0
                && const_val - 1 <= i32::MAX as i64 =>
            {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_ldiv_pow2(const_val);
                self.push_from_rax();
                true
            }
            // lrem: left % const — power-of-2
            0x71 if const_val > 0
                && (const_val & (const_val - 1)) == 0
                && const_val - 1 <= i32::MAX as i64 =>
            {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_lrem_pow2(const_val);
                self.push_from_rax();
                true
            }
            // ldiv: left / const — non-power-of-2 (64-bit magic, mulhi form)
            0x6d if const_val >= 2 => {
                let (magic, shift) = self.magic_div64_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_ldiv_magic64(magic, shift);
                self.push_from_rax();
                true
            }
            // lrem: left % const — non-power-of-2
            0x71 if const_val >= 2 => {
                let (magic, shift) = self.magic_div64_cached(const_val);
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                self.emit_lrem_magic64(magic, shift, const_val);
                self.push_from_rax();
                true
            }
            // ladd: left + const (imm32 range only)
            0x61 if fits_i32 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val != 0 {
                    self.rex_w();
                    self.buf.emit(&[0x81, 0xC0]); // ADD RAX, imm32
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                }
                self.push_from_rax();
                true
            }
            // lsub: left - const (imm32 range only)
            0x65 if fits_i32 => {
                self.pc_to_native[next_op_pc] = self.buf.pos() as i32; // Cast: x86-64 immediate encoding
                self.pop_to_rax();
                if const_val != 0 {
                    self.rex_w();
                    self.buf.emit(&[0x81, 0xE8]); // SUB RAX, imm32
                    self.buf.emit(&(const_val as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
                }
                self.push_from_rax();
                true
            }
            _ => false,
        }
    }

    /// Signed 64-bit division by 2^k rounding toward zero (RAX in/out).
    fn emit_ldiv_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        let mask = divisor - 1; // caller guarantees fits i32
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF9, 0x3F]); // SAR RCX, 63
        self.rex_w();
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE1, mask as u8]); // AND RCX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE1]); // AND RCX, imm32
            self.buf.emit(&(mask as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x01, 0xC8]); // ADD RAX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR RAX, k // Cast: x86-64 immediate encoding
    }

    /// Signed 64-bit remainder by 2^k (RAX in/out).
    fn emit_lrem_pow2(&mut self, divisor: i64) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            self.emit_xor_reg_self(RAX); // a % 1 == 0
            return;
        }
        let mask = divisor - 1; // caller guarantees fits i32
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX (save original)
        self.rex_w();
        self.buf.emit(&[0x89, 0xC2]); // MOV RDX, RAX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xFA, 0x3F]); // SAR RDX, 63
        self.rex_w();
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE2, mask as u8]); // AND RDX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE2]); // AND RDX, imm32
            self.buf.emit(&(mask as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x01, 0xD0]); // ADD RAX, RDX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR RAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0xC1, 0xE0, k as u8]); // SHL RAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x29, 0xC1]); // SUB RCX, RAX
        self.rex_w();
        self.buf.emit(&[0x89, 0xC8]); // MOV RAX, RCX
    }

    /// Memoized [`Self::magic_signed_div64`].
    fn magic_div64_cached(&mut self, d: i64) -> (i64, u32) {
        if let Some(&pair) = self.magic_div64_memo.get(&d) {
            return pair;
        }
        let pair = Self::magic_signed_div64(d);
        self.magic_div64_memo.insert(d, pair);
        pair
    }

    /// Compute the signed 64-bit magic number for division by constant
    /// `d >= 2` (Hacker's Delight 10-4, W = 64, exact u128 arithmetic).
    /// Returns `(magic, shift)` such that with `t = mulhi_signed(magic, n)`
    /// (plus `n` when `magic < 0`):  `n / d = (t >> shift) + (n >>> 63)`.
    fn magic_signed_div64(d: i64) -> (i64, u32) {
        debug_assert!(d >= 2);
        let ad = d as u128;
        let two63: u128 = 1u128 << 63;
        let anc = two63 - 1 - two63 % ad;

        let mut p = 63u32;
        let mut q1 = two63 / anc;
        let mut r1 = two63 - q1 * anc;
        let mut q2 = two63 / ad;
        let mut r2 = two63 - q2 * ad;

        loop {
            p += 1;
            q1 *= 2;
            r1 *= 2;
            if r1 >= anc {
                q1 += 1;
                r1 -= anc;
            }
            q2 *= 2;
            r2 *= 2;
            if r2 >= ad {
                q2 += 1;
                r2 -= ad;
            }
            let delta = ad - 1 - r2;
            if q1 > delta || (q1 == delta && r1 == 0) {
                break;
            }
            if p >= 127 {
                break;
            }
        }

        let magic = (q2 + 1) as u64 as i64; // two's-complement wrap intended
        (magic, p - 64)
    }

    /// Signed 64-bit division by a non-power-of-2 constant via the mulhi
    /// magic method (RAX in/out; clobbers RCX/RDX like the 32-bit variant).
    fn emit_ldiv_magic64(&mut self, magic: i64, shift: u32) {
        self.rex_w();
        self.buf.emit(&[0x89, 0xC1]); // MOV RCX, RAX — save dividend
        self.emit_mov_imm64(RDX, magic);
        self.rex_w();
        self.buf.emit(&[0xF7, 0xEA]); // IMUL RDX — RDX:RAX = RAX * RDX (signed)
        if magic < 0 {
            // d > 0 with a wrapped (negative-as-i64) magic: t += n.
            self.rex_w();
            self.buf.emit(&[0x01, 0xCA]); // ADD RDX, RCX
        }
        if shift > 0 {
            self.rex_w();
            self.buf.emit(&[0xC1, 0xFA, shift as u8]); // SAR RDX, shift // Cast: x86-64 immediate encoding
        }
        self.rex_w();
        self.buf.emit(&[0x89, 0xD0]); // MOV RAX, RDX
        self.rex_w();
        self.buf.emit(&[0x89, 0xCA]); // MOV RDX, RCX
        self.rex_w();
        self.buf.emit(&[0xC1, 0xEA, 0x3F]); // SHR RDX, 63 — sign bit of n
        self.rex_w();
        self.buf.emit(&[0x01, 0xD0]); // ADD RAX, RDX — quotient
    }

    /// Signed 64-bit remainder by a non-power-of-2 constant (RAX in/out).
    fn emit_lrem_magic64(&mut self, magic: i64, shift: u32, divisor: i64) {
        self.emit_ldiv_magic64(magic, shift); // RAX = quotient; RCX = n
        if (-0x8000_0000i64..=0x7FFF_FFFF).contains(&divisor) {
            self.rex_w();
            self.buf.emit_byte(0x69); // IMUL RAX, RAX, imm32
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(divisor as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, divisor);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        self.rex_w();
        self.buf.emit(&[0x29, 0xC1]); // SUB RCX, RAX — n - q*d
        self.rex_w();
        self.buf.emit(&[0x89, 0xC8]); // MOV RAX, RCX
    }

    fn emit_idiv_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            return; // div by 1 = no-op
        }
        // Signed division by 2^k rounding toward zero:
        // MOV ECX, EAX;  SAR ECX, 31;  AND ECX, (2^k - 1);
        // ADD EAX, ECX;  SAR EAX, k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX
        self.buf.emit(&[0xC1, 0xF9, 0x1F]); // SAR ECX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE1, mask as u8]); // AND ECX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE1]);
            self.buf.emit(&mask.to_le_bytes()); // AND ECX, imm32
        }
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Emit optimized signed remainder by power-of-2 constant.
    /// Result: EAX = EAX % 2^k, sign-extended to RAX.
    fn emit_irem_pow2(&mut self, divisor: i32) {
        debug_assert!(divisor > 0 && (divisor & (divisor - 1)) == 0);
        let k = divisor.trailing_zeros();
        if k == 0 {
            // a % 1 == 0
            self.buf.emit(&[0x31, 0xC0]); // XOR EAX, EAX
            return;
        }
        // remainder = dividend - (dividend / 2^k) * 2^k
        self.buf.emit(&[0x89, 0xC1]); // MOV ECX, EAX (save original)
                                      // Division sequence (clobbers EAX):
        self.buf.emit(&[0x89, 0xC2]); // MOV EDX, EAX
        self.buf.emit(&[0xC1, 0xFA, 0x1F]); // SAR EDX, 31
        let mask = divisor - 1;
        if mask <= 127 {
            self.buf.emit(&[0x83, 0xE2, mask as u8]); // AND EDX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x81, 0xE2]);
            self.buf.emit(&mask.to_le_bytes()); // AND EDX, imm32
        }
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX
        self.buf.emit(&[0xC1, 0xF8, k as u8]); // SAR EAX, k // Cast: x86-64 immediate encoding
                                               // quotient * divisor:
        self.buf.emit(&[0xC1, 0xE0, k as u8]); // SHL EAX, k // Cast: x86-64 immediate encoding
                                               // remainder = original - quotient*divisor
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
    }

    /// Memoized wrapper around [`Self::magic_signed_div32`]. The magic-number
    /// derivation is a pure function of the divisor; caching it per constant
    /// avoids recomputing the Newton iteration for repeated `/ k` / `% k` in
    /// a loop body. The cached `(magic, shift)` is bit-identical to a fresh
    /// computation, so generated machine code is unchanged.
    fn magic_div_cached(&mut self, d: i32) -> (i64, u32) {
        if let Some(&pair) = self.magic_div_memo.get(&d) {
            return pair;
        }
        let pair = Self::magic_signed_div32(d);
        self.magic_div_memo.insert(d, pair);
        pair
    }

    /// Compute magic number for signed 32-bit division by constant d (d >= 2).
    /// Returns (magic, shift) such that:
    ///   n / d = ((n as i64) * magic) >> (32 + shift) + (n < 0 ? 1 : 0)
    fn magic_signed_div32(d: i32) -> (i64, u32) {
        debug_assert!(d >= 2);
        let ad = d as u64; // Cast: x86-64 immediate encoding
        let two31 = 1u64 << 31;
        let anc = two31 - 1 - two31 % ad;

        let mut p = 31u32;
        let mut q1 = two31 / anc;
        let mut r1 = two31 - q1 * anc;
        let mut q2 = two31 / ad;
        let mut r2 = two31 - q2 * ad;

        loop {
            p += 1;
            q1 *= 2;
            r1 *= 2;
            if r1 >= anc {
                q1 += 1;
                r1 -= anc;
            }
            q2 *= 2;
            r2 *= 2;
            if r2 >= ad {
                q2 += 1;
                r2 -= ad;
            }
            let delta = ad - 1 - r2;
            if q1 > delta || (q1 == delta && r1 == 0) {
                break;
            }
            if p >= 63 {
                break;
            }
        }

        let magic = (q2 + 1) as i64; // Cast: JIT ABI convention
        (magic, p - 32)
    }

    /// Emit optimized signed division by a non-power-of-2 constant using
    /// multiply-and-shift (magic number method from Hacker's Delight).
    /// Input: dividend in EAX. Output: quotient in EAX, sign-extended to RAX.
    fn emit_idiv_magic(&mut self, magic: i64, shift: u32) {
        // MOV ECX, EAX — save dividend for sign correction
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX — sign-extend to 64 bits
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69); // IMUL r64, r/m64, imm32
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]); // IMUL RAX, RDX
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // SAR RAX, imm8 // Cast: x86-64 immediate encoding
                                                         // Sign correction: SHR ECX, 31; ADD EAX, ECX
        self.buf.emit(&[0xC1, 0xE9, 0x1F]); // SHR ECX, 31
        self.buf.emit(&[0x01, 0xC8]); // ADD EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Emit optimized signed remainder by a non-power-of-2 constant.
    /// Input: dividend in EAX. Output: remainder in EAX, sign-extended to RAX.
    fn emit_irem_magic(&mut self, magic: i64, shift: u32, divisor: i32) {
        // MOV ECX, EAX — save original dividend
        self.buf.emit(&[0x89, 0xC1]);
        // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
        // IMUL RAX, RAX, magic
        if (-0x8000_0000..=0x7FFF_FFFF).contains(&magic) {
            self.rex_w();
            self.buf.emit_byte(0x69);
            self.modrm_reg(RAX, RAX);
            self.buf.emit(&(magic as i32).to_le_bytes()); // Cast: x86-64 immediate encoding
        } else {
            self.emit_mov_imm64(RDX, magic);
            self.rex_w();
            self.buf.emit(&[0x0F, 0xAF, 0xC2]);
        }
        // SAR RAX, 32 + shift
        let total_shift = 32 + shift;
        self.rex_w();
        self.buf.emit(&[0xC1, 0xF8, total_shift as u8]); // Cast: x86-64 immediate encoding
                                                         // Sign correction: MOV EDX, ECX; SHR EDX, 31; ADD EAX, EDX
        self.buf.emit(&[0x89, 0xCA]); // MOV EDX, ECX
        self.buf.emit(&[0xC1, 0xEA, 0x1F]); // SHR EDX, 31
        self.buf.emit(&[0x01, 0xD0]); // ADD EAX, EDX — quotient in EAX
                                      // Remainder = n - quotient * divisor
        if (-128..=127).contains(&divisor) {
            self.buf.emit(&[0x6B, 0xC0, divisor as u8]); // IMUL EAX, EAX, imm8 // Cast: x86-64 immediate encoding
        } else {
            self.buf.emit(&[0x69, 0xC0]); // IMUL EAX, EAX, imm32
            self.buf.emit(&divisor.to_le_bytes());
        }
        self.buf.emit(&[0x29, 0xC1]); // SUB ECX, EAX (n - q*d)
        self.buf.emit(&[0x89, 0xC8]); // MOV EAX, ECX
                                      // MOVSXD RAX, EAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }


    /// Emit a vectorized int-array sum loop.
    /// Replaces the scalar loop with AVX2 code that processes 8 int elements at a time.
    /// Assumes: RCX = array base ptr, R10D = start index, R11D = bound (exclusive count).
    /// Result: accumulator value added to long local via frame slot.
    #[allow(clippy::too_many_arguments)]
    fn emit_simd_int_array_sum(&mut self, acc_local_offset: i32, acc_is_long: bool) {
        // RCX = array base address (already loaded)
        // R10D = current index (i)
        // R11D = loop bound (n)
        // Accumulator is in frame slot at acc_local_offset

        // --- Compute number of SIMD iterations ---
        // R8D = (n - i) / 8 = number of full 8-element chunks
        // 0x44, 0x89: MOV EAX, R11D;  SUB EAX, R10D; SHR EAX, 3
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x03]); // SHR EAX, 3
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX — chunk count
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar cleanup (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- SIMD loop ---
        // VPXOR YMM0, YMM0, YMM0 — zero accumulator
        self.emit_vpxor_ymm(0, 0, 0);

        // Compute base address: RAX = RCX + R10 * 4 + HEADER_SIZE
        // (array elements start at RCX + HEADER_SIZE, each int is 4 bytes)
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10  (R10 is i)
        self.buf.emit(&[0xC1, 0xE0, 0x02]); // SHL EAX, 2  (i * 4)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX  (array base + i*4)
                                            // ADD RAX, HEADER_SIZE
        self.buf.emit(&[0x48, 0x05]); // ADD RAX, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        let simd_loop_start = self.buf.pos();
        // VPADDD YMM0, YMM0, [RAX]
        self.emit_vpaddd_ymm_mem(0, 0, RAX, 0);
        // ADD RAX, 32  (advance by 8 ints × 4 bytes)
        self.buf.emit(&[0x48, 0x83, 0xC0, 0x20]);
        // DEC R8D
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // --- Horizontal reduction: YMM0 → EAX ---
        self.emit_horizontal_sum_ymm0_to_eax();

        // VZEROUPPER
        self.emit_vzeroupper();

        // Add SIMD result to accumulator
        if acc_is_long {
            // MOVSXD RAX, EAX
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]);
            // ADD [RBP + acc_offset], RAX (64-bit add to long local)
            self.rex_w();
            self.buf.emit_byte(0x01); // ADD r/m64, r64
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            // ADD [RBP + acc_offset], EAX (32-bit add to int local)
            self.buf.emit_byte(0x01); // ADD r/m32, r32
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // Update induction variable: i += chunks_processed * 8
        // We need to know how many were processed. R10D was the start.
        // The scalar loop will pick up from the new i value.
        // Actually: the simd loop processed (original R8D) * 8 elements.
        // New i = old i + (original chunk_count * 8)
        // But R8D is now 0. Let's track differently.
        // Before SIMD loop, EAX had chunk_count. Let's save it.
        // Actually, let's just compute: new_i = old_i + (((n-old_i)/8)*8)
        // which is: new_i = n - (n - old_i) % 8
        // Simpler: after the simd loop pointer, compute i from pointer:
        //   bytes_consumed = (RAX_now - RAX_start) = chunks * 32
        //   elements_consumed = bytes_consumed / 4 = chunks * 8
        //   new_i = old_i + elements_consumed

        // Patch the skip jump target
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        let pos = simd_skip_patch;
        self.buf.try_patch_i32(pos, skip_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // Now set up for scalar cleanup:
        // R10D needs to be updated to: old_i + num_simd_elements
        // Since we already did the pointer math, recalculate:
        // We computed chunk_count = (n - i) / 8 before the loop.
        // After SIMD: new_i = old_i + chunk_count * 8
        // Recompute: EAX = (R11D - R10D) >> 3 << 3; R10D += EAX
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0x83, 0xE0, 0xF8]); // AND EAX, ~7 (round down to multiple of 8)
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // --- Scalar cleanup loop ---
        // for (i = new_i; i < n; i++) sum += arr[i]
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D, R11D
                                            // JGE end
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Load arr[i]: MOV EAX, [RCX + R10*4 + HEADER_SIZE]
        // SIB addressing: base=RCX, index=R10, scale=4
        self.buf.emit(&[0x42, 0x8B, 0x84, 0x91]); // MOV EAX, [RCX + R10*4 + disp32]
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // Add to accumulator
        if acc_is_long {
            self.rex_w();
            self.buf.emit(&[0x63, 0xC0]); // MOVSXD RAX, EAX
            self.rex_w();
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        } else {
            self.buf.emit_byte(0x01);
            self.modrm_rbp_disp(RAX, acc_local_offset);
        }

        // INC R10D
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(scalar_end_patch, end_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }

    /// Emit one checked matrix-dot element and advance R10D.
    ///
    /// The preheader keeps all Java-visible state in its original homes until
    /// the complete dot product succeeds, so a guard can safely restart the
    /// scalar bytecodes even when this element belongs to an unrolled batch.
    fn emit_matrix_dot_element(
        &mut self,
        scalar_fallbacks: &mut Vec<usize>,
        index_delta: i32,
        advance_iv: bool,
    ) {
        let b_disp = HEADER_SIZE as i32
            + index_delta * if narrow_oops_enabled() { 4 } else { 8 };
        let a_disp = HEADER_SIZE as i32 + index_delta * 4;
        // Both displacements are baked under a hard-coded `mod=01` ModRM byte
        // AND grow with the unroll index — the only computed disp8s left in
        // this file. With the batch of 8 the widest is `HEADER_SIZE + 7*8`
        // (88 today), comfortably inside the byte; a wider unroll or a larger
        // object header walks them past 127, where the previous `as u8` would
        // have silently addressed memory BEFORE the array. Require the disp8
        // form explicitly and discard the method otherwise. Neither value can
        // be zero (both are at least `HEADER_SIZE`), so `Disp::None` — which
        // would need a mod=00 ModRM byte this emitter does not write — cannot
        // arise here.
        let b_disp8 = Disp::encode(b_disp as i64).ok().and_then(Disp::as_disp8);
        let a_disp8 = Disp::encode(a_disp as i64).ok().and_then(Disp::as_disp8);
        let (Some(b_disp8), Some(a_disp8)) = (b_disp8, a_disp8) else {
            self.buf.mark_overflowed();
            return;
        };
        if narrow_oops_enabled() {
            // EAX = narrow b[k], then decode it with the loop-invariant heap
            // base already in R11. A zero encoding is null -> scalar fallback.
            self.buf.emit(&[
                0x43,
                0x8B,
                0x44,
                0x95,
                b_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
            ]); // MOV EAX,[R13+R10*4+H]
            self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX,3
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
            self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX,R11
        } else {
            self.buf.emit(&[
                0x4B,
                0x8B,
                0x44,
                0xD5,
                b_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
            ]); // MOV RAX,[R13+R10*8+H]
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        }

        // Each B row is independently mutable in Java, so its column check
        // remains per element. A failure restarts the exact scalar body, which
        // raises NPE/AIOOBE at the original bytecode.
        self.buf
            .emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD6]); // CMP R14D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x83)); // JAE

        self.buf.emit(&[
            0x43,
            0x8B,
            0x54,
            0x94,
            a_disp8 as u8, // Cast: range-checked disp8 above, reinterpreted signed
        ]); // MOV EDX,[R12+R10*4+H]
        self.buf.emit(&[
            0x42,
            0x0F,
            0xAF,
            0x54,
            0xB0,
            HEADER_SIZE as u8,
        ]); // IMUL EDX,[RAX+R14*4+H]
        self.buf.emit(&[0x41, 0x01, 0xD1]); // ADD R9D, EDX (Java int wrap)
        if advance_iv {
            self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        }
    }

    /// Emit the guarded tight loop for a [`MatrixDotLoop`].
    ///
    /// Register contract:
    /// - R12 = `a[row]`
    /// - R13 = outer `b`
    /// - R14D = column
    /// - R15D = exclusive bound
    /// - R10D = induction variable
    /// - R9D = wrapping int accumulator
    ///
    /// R12..R15 are reserved from Java-local allocation and saved by the
    /// method prologue.  RAX/RCX/RDX/R11 are ordinary emitter scratch.
    fn emit_matrix_dot_preheader(&mut self, dot: &MatrixDotLoop) {
        let mut scalar_fallbacks = Vec::new();

        // Load the carried integer state.  Frame/register homes are left
        // untouched until successful completion, so every failing guard can
        // restart the original scalar loop without reconstructing state.
        if let Some(reg) = self.reg_for_local(dot.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(dot.iv_local));
        }
        if let Some(reg) = self.reg_for_local(dot.bound_local) {
            self.emit_mov_reg_reg(R15, reg);
        } else {
            self.emit_load_local(R15, self.local_offset(dot.bound_local));
        }
        if let Some(reg) = self.reg_for_local(dot.acc_local) {
            self.emit_mov_reg_reg(R9, reg);
        } else {
            self.emit_load_local(R9, self.local_offset(dot.acc_local));
        }

        // Zero trip: do not even inspect the arrays.
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D, R15D
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x8D)); // JGE scalar header
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar header

        // Resolve a[row]. Prefer the LICM result that was emitted immediately
        // before this preheader; fall back to a guarded direct load when LICM
        // is disabled or this header was de-specialized.
        let hoisted_a = self
            .hoist_info
            .iter()
            .enumerate()
            .find(|(_, h)| {
                h.loop_header == dot.header_pc
                    && h.array_local == dot.a_outer_local
                    && h.index_local == dot.a_row_local
            })
            .map(|(idx, _)| self.hoist_offsets[idx]);
        if let Some(off) = hoisted_a {
            self.emit_load_local(RAX, off);
        } else {
            if let Some(reg) = self.reg_for_local(dot.a_outer_local) {
                self.emit_mov_reg_reg(RAX, reg);
            } else {
                self.emit_load_local(RAX, self.local_offset(dot.a_outer_local));
            }
            if let Some(reg) = self.reg_for_local(dot.a_row_local) {
                self.emit_mov_reg_reg(RCX, reg);
            } else {
                self.emit_load_local(RCX, self.local_offset(dot.a_row_local));
            }
            self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
            self.buf
                .emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
            self.buf.emit(&[0x39, 0xD1]); // CMP ECX, EDX
            scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x83)); // JAE
            self.emit_ref_aload_regs();
        }
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX (a[row])
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        self.emit_mov_reg_reg(R12, RAX);

        // Resolve the outer B array and the invariant column.
        if let Some(reg) = self.reg_for_local(dot.b_outer_local) {
            self.emit_mov_reg_reg(R13, reg);
        } else {
            self.emit_load_local(R13, self.local_offset(dot.b_outer_local));
        }
        self.buf.emit(&[0x4D, 0x85, 0xED]); // TEST R13, R13
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x84)); // JZ
        if let Some(reg) = self.reg_for_local(dot.b_column_local) {
            self.emit_mov_reg_reg(R14, reg);
        } else {
            self.emit_load_local(R14, self.local_offset(dot.b_column_local));
        }

        // A's selected row and the outer B array must both cover the complete
        // counted range.  JA (not JAE): length == bound is valid.
        self.buf.emit(&[
            0x41,
            0x8B,
            0x54,
            0x24,
            ARRAY_LENGTH_OFFSET as u8,
        ]); // MOV EDX,[R12+len]
        self.buf.emit(&[0x41, 0x39, 0xD7]); // CMP R15D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x87)); // JA
        self.buf
            .emit(&[0x41, 0x8B, 0x55, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[R13+len]
        self.buf.emit(&[0x41, 0x39, 0xD7]); // CMP R15D, EDX
        scalar_fallbacks.push(self.emit_jcc_rel32_patch(0x87)); // JA

        if narrow_oops_enabled() {
            self.emit_mov_imm64(R11, narrow_base() as i64);
        }

        // Eight-way unrolling amortizes the induction compare/branch while
        // retaining the original k-order for every multiply-add. Fixed
        // displacements let the CPU overlap independent row-pointer loads
        // without an INC dependency between elements. R8D is the
        // first k that cannot start a full batch (`bound - 7`).
        self.emit_mov_reg_reg(R8, R15);
        self.buf.emit(&[0x41, 0x83, 0xE8, 0x07]); // SUB R8D,7
        self.buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D,R8D
        let scalar_tail_patch = self.emit_jcc_rel32_patch(0x8D); // JGE scalar tail

        let batch_start = self.buf.pos();
        for index_delta in 0..8 {
            self.emit_matrix_dot_element(&mut scalar_fallbacks, index_delta, false);
        }
        self.buf.emit(&[0x41, 0x83, 0xC2, 0x08]); // ADD R10D,8
        self.buf.emit(&[0x45, 0x39, 0xC2]); // CMP R10D,R8D
        self.buf.emit(&[0x0F, 0x8C]); // JL batch_start
        let batch_patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        let batch_rel = (batch_start as i32) - (batch_patch as i32 + 4);
        self.buf.try_patch_i32(batch_patch, batch_rel).ok();

        self.patch_rel32_to_here(scalar_tail_patch);
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D,R15D
        let publish_patch = self.emit_jcc_rel32_patch(0x8D); // JGE publish

        let scalar_start = self.buf.pos();
        self.emit_matrix_dot_element(&mut scalar_fallbacks, 0, true);
        self.buf.emit(&[0x45, 0x39, 0xFA]); // CMP R10D, R15D
        self.buf.emit(&[0x0F, 0x8C]); // JL scalar_start
        let loop_patch = self.buf.pos();
        self.buf.emit(&[0u8; 4]);
        let loop_rel = (scalar_start as i32) - (loop_patch as i32 + 4);
        self.buf.try_patch_i32(loop_patch, loop_rel).ok();

        // Publish successful final state to whichever homes the scalar
        // continuation expects. MOVSXD restores the backend's signed-i32 local
        // representation after the wrapping 32-bit accumulator arithmetic.
        self.patch_rel32_to_here(publish_patch);
        self.buf.emit(&[0x4D, 0x63, 0xC9]); // MOVSXD R9,R9D
        if let Some(reg) = self.reg_for_local(dot.acc_local) {
            self.emit_mov_reg_reg(reg, R9);
        } else {
            self.emit_store_local(self.local_offset(dot.acc_local), R9);
        }
        if let Some(reg) = self.reg_for_local(dot.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(dot.iv_local), R10);
        }

        // All failed guards land after the success stores, leaving their
        // original frame/register state untouched for the scalar bytecode.
        for patch in scalar_fallbacks {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit a vectorized double-array sum loop using AVX2 VADDPD.
    /// Processes 4 doubles per iteration (256-bit YMM registers).
    /// Assumes: RCX = array base ptr, R10D = start index, R11D = bound.
    /// Result: sum added to double local via frame slot.
    fn emit_simd_fp_array_sum(&mut self, acc_local_offset: i32) {
        // --- Compute number of SIMD iterations ---
        // chunk_count = (n - i) / 4 (4 doubles per YMM register)
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0xC1, 0xE8, 0x02]); // SHR EAX, 2 (divide by 4)
        self.buf.emit(&[0x41, 0x89, 0xC0]); // MOV R8D, EAX — chunk count
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar cleanup (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- SIMD loop: accumulate 4 doubles per iteration ---
        // VPXOR YMM0, YMM0, YMM0 — zero accumulator
        self.emit_vpxor_ymm(0, 0, 0);

        // Compute base address: RAX = RCX + R10 * 8 + HEADER_SIZE
        // (double elements are 8 bytes each)
        self.buf.emit(&[0x4C, 0x89, 0xD0]); // MOV RAX, R10
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3 (i * 8)
        self.buf.emit(&[0x48, 0x01, 0xC8]); // ADD RAX, RCX
        self.buf.emit(&[0x48, 0x05]); // ADD RAX, imm32
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        let simd_loop_start = self.buf.pos();
        // VADDPD YMM0, YMM0, [RAX] — packed double add from memory
        // VEX.256.66.0F.WIG 58 /r (mod=00, r/m=RAX)
        self.emit_vex2(true, 0, true, 1); // R=1, vvvv=0 (YMM0), L=1 (256-bit), pp=01 (66)
        self.buf.emit_byte(0x58); // ADDPD
        self.buf.emit_byte(0x00); // ModRM: mod=00, reg=YMM0, rm=RAX

        // ADD RAX, 32 (advance by 4 doubles × 8 bytes)
        self.buf.emit(&[0x48, 0x83, 0xC0, 0x20]);
        // DEC R8D
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // --- Horizontal reduction: YMM0 → XMM0 scalar double ---
        // VEXTRACTF128 XMM1, YMM0, 1 — get high 128 bits
        // VEX.256.66.0F3A.W0 19 /r imm8
        self.emit_vex3(true, true, true, 0x03, false, 0, true, 1);
        self.buf.emit_byte(0x19);
        self.buf.emit_byte(0xC1); // ModRM: YMM0 → XMM1
        self.buf.emit_byte(0x01); // imm8 = 1 (high lane)

        // VADDPD XMM0, XMM0, XMM1 — add high to low (128-bit)
        // VEX.128.66.0F.WIG 58 /r
        self.emit_vex2(true, 0, false, 1); // L=0 (128-bit)
        self.buf.emit_byte(0x58);
        self.buf.emit_byte(0xC1); // ModRM: XMM0 = XMM0 + XMM1

        // VSHUFPD XMM1, XMM0, XMM0, 1 — swap the two doubles in XMM0
        // VEX.128.66.0F.WIG C6 /r imm8
        self.emit_vex2(true, 0, false, 1);
        self.buf.emit_byte(0xC6);
        self.buf.emit_byte(0xC8); // ModRM: XMM1 = shuffle(XMM0, XMM0)
        self.buf.emit_byte(0x01); // imm8 = 1

        // VADDSD XMM0, XMM0, XMM1 — final scalar add
        // VEX.LIG.F2.0F.WIG 58 /r
        self.emit_vex2(true, 0, false, 3); // pp=11 (F2)
        self.buf.emit_byte(0x58);
        self.buf.emit_byte(0xC1); // XMM0 = XMM0 + XMM1

        // VZEROUPPER
        self.emit_vzeroupper();

        // Add SIMD result to accumulator:
        // MOVQ RAX, XMM0
        self.emit_movq_rax_from_xmm(0);
        // Load current acc into XMM1 from frame
        self.emit_load_local(RCX, acc_local_offset);
        self.emit_movq_xmm_from_gpr(1, RCX);
        // MOVQ XMM0, RAX
        self.emit_movq_xmm_from_rax(0);
        // ADDSD XMM0, XMM1
        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC1]);
        // Direct MOVQ [rbp-acc_local_offset], XMM0 — keeps RAX free.
        self.emit_movq_mem_rbp_from_xmm(acc_local_offset, 0);

        // Update induction variable: i += chunks_processed * 4
        self.buf.emit(&[0x44, 0x89, 0xD8]); // MOV EAX, R11D
        self.buf.emit(&[0x44, 0x29, 0xD0]); // SUB EAX, R10D
        self.buf.emit(&[0x83, 0xE0, 0xFC]); // AND EAX, ~3 (round down to multiple of 4)
        self.buf.emit(&[0x41, 0x01, 0xC2]); // ADD R10D, EAX

        // Patch the skip jump target
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(simd_skip_patch, skip_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // --- Scalar cleanup loop ---
        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D
        self.buf.emit(&[0x45, 0x39, 0xDA]);
        // JGE end
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // Load arr[i] as double: MOVSD XMM0, [RCX + R10*8 + HEADER_SIZE]
        // Use SIB: base=RCX, index=R10, scale=8
        self.buf.emit(&[0xF2, 0x42, 0x0F, 0x10, 0x84, 0xD1]); // MOVSD XMM0, [RCX + R10*8 + disp32]
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes()); // Cast: x86-64 immediate encoding

        // ADDSD to accumulator: load acc to XMM1, add, store back
        self.emit_load_local(RAX, acc_local_offset);
        self.emit_movq_xmm_from_rax(1);
        // ADDSD XMM1, XMM0
        self.buf.emit(&[0xF2, 0x0F, 0x58, 0xC8]);
        self.emit_movq_mem_rbp_from_xmm(acc_local_offset, 1);

        // INC R10D
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(scalar_end_patch, end_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }

    // -----------------------------------------------------------------------
    // T17.Β.3 — Loop unswitch emission
    // -----------------------------------------------------------------------

    /// Emit the preheader evaluation for a loop-unswitch candidate
    /// whose header starts at `header_pc`.
    ///
    /// The output is a single `MOV/LOAD + TEST`-style probe that
    /// evaluates the invariant local and sets flags according to the
    /// branch's semantics (ifeq / ifne / iflt / ifge / ifgt / ifle).
    /// Execution never mutates any Java-visible state, so semantics
    /// are strictly additive — the code compiled with unswitch
    /// detection enabled produces the same final state as the
    /// unmodified version.
    ///
    /// # Gate
    ///
    /// Detection already enforces `body_size <= MAX_UNSWITCH_BYTECODES`;
    /// we reassert here so emission silently falls back to the
    /// scalar path if the precondition is violated.
    fn emit_loop_unswitch_preheader(&mut self, header_pc: usize) {
        // Clone to avoid aliasing self.
        let candidate = self
            .loop_unswitch_candidates
            .iter()
            .find(|c| c.header_pc == header_pc)
            .cloned();
        let Some(cand) = candidate else {
            return;
        };
        // Defensive gate — detection already checks this, but we
        // re-apply so the emission path is self-contained.
        let body_size = cand.back_edge_pc.saturating_sub(cand.header_pc);
        if body_size == 0 || body_size > MAX_UNSWITCH_BYTECODES {
            return;
        }

        // Load the invariant local into EAX.
        // Prefer a register-resident copy when the regalloc placed
        // the local in a caller-saved GPR; otherwise fall back to
        // the frame slot.
        if let Some(reg) = self.reg_for_local(cand.invariant_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(cand.invariant_local));
        }

        // Set flags for the branch. For the unary-branch opcodes in
        // scope (0x99..=0x9E), TEST EAX, EAX covers eq/ne and a CMP
        // against 0 covers lt/ge/gt/le (TEST sets SF/ZF the same
        // way, so we can use a single TEST for all six).
        //
        // TEST EAX, EAX — 85 C0
        self.buf.emit(&[0x85, 0xC0]);

        // No jump is emitted. The per-iteration branch inside the
        // body will re-evaluate the predicate and take the correct
        // side; the pre-evaluation primes the branch predictor so
        // the in-loop check is ~always correctly predicted.
        //
        // The branch_op is captured for future body-duplication
        // emission variants; consume it here to silence unused
        // warnings and document the contract.
        debug_assert!(
            matches!(cand.branch_op, 0x99..=0x9E),
            "detector rejects non-unary branches (0x99..=0x9E)"
        );
    }


    /// Emit a vectorized int-array element-wise loop:
    ///
    /// ```text
    /// for (i = R10D; i < R11D; i++) OUT[i] = A[i] OP B[i]
    /// ```
    ///
    /// Input register allocation:
    /// - RAX = base of A
    /// - RCX = base of B
    /// - RDX = base of OUT
    /// - R10D = start index (i)
    /// - R11D = bound (n, exclusive)
    ///
    /// Strategy:
    /// - Phase 1 (AVX2 8-wide batch): process 8 elements per iteration
    ///   while `(n - i) >= 8`. Uses YMM0 as `A[i..i+8]` register, then
    ///   applies `OP` with `[RCX + i*4 + H]` straight from memory, and
    ///   stores the result via VMOVDQU to `[RDX + i*4 + H]`.
    /// - Phase 2 (scalar remainder): single-element loop for the final
    ///   `(n - i) % 8` elements — never reads past the array end.
    ///
    /// # Correctness invariants
    ///
    /// - The AVX2 batch loop *stops* strictly before `n - 8`, so the
    ///   256-bit load/store never crosses the end of the array.
    /// - The scalar tail is bytecode-equivalent to the original shape
    ///   (`iaload a; iaload b; i*op; iastore out`).
    /// - `VZEROUPPER` is emitted before the scalar path so legacy SSE
    ///   isn't penalized by a dirty AVX state.
    ///
    /// # Safety
    ///
    /// Call sites already clamped `i` to a 32-bit nonneg integer and
    /// ensured `n <= len(OUT), len(A), len(B)` via the existing bounds
    /// analysis (`bounds_safe_pcs`) or a speculative BCE guard.
    fn emit_simd_int_array_element_wise(&mut self, op: ElementWiseOp) {
        // --- Compute chunk_count = (n - i) >> 3 into R8D ---
        //
        // This must NOT route through EAX: the caller leaves array A's base
        // pointer in RAX (B in RCX, OUT in RDX), and that base is read by
        // the preheader's `ADD R9, RAX` below and by the scalar-remainder
        // `MOV EAX, [RAX + R10*4 + H]`. Using EAX as scratch here would
        // overwrite A's base with the chunk count, so `&A[i]` degenerates to
        // `i*4 + chunk_count + H` and the first VMOVDQU faults.
        self.buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D
        self.buf.emit(&[0x45, 0x29, 0xD0]); // SUB R8D, R10D
        self.buf.emit(&[0x41, 0xC1, 0xE8, 0x03]); // SHR R8D, 3
        self.buf.emit(&[0x45, 0x85, 0xC0]); // TEST R8D, R8D
                                            // JZ to scalar remainder (patch later)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x84);
        let simd_skip_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // --- AVX2 batch loop ---
        // Compute byte offset into arrays: EAX = i*4, stays in RDI so
        // the SIB-style `[base + offset]` addressing is a plain disp32
        // (we recompute pointer-adjusted base regs instead).
        //
        // Strategy: materialize pointers once, advance them by 32 per
        // iteration so the inner loop body stays small.
        //
        //   R9  = &A[i]    (= RAX + R10*4 + H)
        //   R12 = &B[i]    (= RCX + R10*4 + H)
        //   R13 = &OUT[i]  (= RDX + R10*4 + H)
        //
        // R12/R13 are callee-saved; they were recorded as saved in the
        // prologue via `force_callee_saved_live` so regalloc doesn't
        // reuse them. But element-wise emission runs as a pre-header
        // before the scalar loop body, so we need to preserve them.
        //
        // To keep this self-contained we use R9 and scratch via push/pop
        // of R12/R13.

        // Save R12, R13 on the stack (callee-saved — must be restored).
        // PUSH R12 (41 54), PUSH R13 (41 55)
        self.buf.emit(&[0x41, 0x54]);
        self.buf.emit(&[0x41, 0x55]);

        // &A[i]: R9 = RAX + R10*4 + H
        // MOV R9, R10 (4D 89 D1)
        self.buf.emit(&[0x4D, 0x89, 0xD1]);
        // SHL R9, 2  (49 C1 E1 02) — R9 = i * 4
        self.buf.emit(&[0x49, 0xC1, 0xE1, 0x02]);
        // ADD R9, RAX  (49 01 C1) — R9 = RAX + i*4
        self.buf.emit(&[0x49, 0x01, 0xC1]);
        // ADD R9, HEADER_SIZE  (49 81 C1 imm32)
        self.buf.emit(&[0x49, 0x81, 0xC1]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &B[i]: R12 = RCX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD4]); // MOV R12, R10
        self.buf.emit(&[0x49, 0xC1, 0xE4, 0x02]); // SHL R12, 2
        self.buf.emit(&[0x49, 0x01, 0xCC]); // ADD R12, RCX
        self.buf.emit(&[0x49, 0x81, 0xC4]); // ADD R12, imm32
                                            // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // &OUT[i]: R13 = RDX + R10*4 + H
        self.buf.emit(&[0x4D, 0x89, 0xD5]); // MOV R13, R10
        self.buf.emit(&[0x49, 0xC1, 0xE5, 0x02]); // SHL R13, 2
        self.buf.emit(&[0x49, 0x01, 0xD5]); // ADD R13, RDX
        self.buf.emit(&[0x49, 0x81, 0xC5]); // ADD R13, imm32
                                            // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        let simd_loop_start = self.buf.pos();
        // YMM0 = A[i..i+8]:  VMOVDQU YMM0, [R9]
        self.emit_vmovdqu_load_ymm(0, 9, 0);
        // YMM0 = YMM0 OP [R12]
        self.emit_ewise_ymm_mem(op, 0, 0, 12, 0);
        // [R13] = YMM0:  VMOVDQU [R13], YMM0
        self.emit_vmovdqu_store_ymm(13, 0, 0);

        // Advance all 3 pointers by 32 bytes (8 ints × 4 bytes).
        // ADD R9, 32 — 49 83 C1 20
        self.buf.emit(&[0x49, 0x83, 0xC1, 0x20]);
        // ADD R12, 32 — 49 83 C4 20
        self.buf.emit(&[0x49, 0x83, 0xC4, 0x20]);
        // ADD R13, 32 — 49 83 C5 20
        self.buf.emit(&[0x49, 0x83, 0xC5, 0x20]);

        // DEC R8D — 41 FF C8
        self.buf.emit(&[0x41, 0xFF, 0xC8]);
        // JNZ simd_loop_start
        let rel = (simd_loop_start as i32) - (self.buf.pos() as i32 + 6); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x85);
        self.buf.emit(&rel.to_le_bytes());

        // VZEROUPPER — safe legacy-SSE transition before the scalar tail.
        self.emit_vzeroupper();

        // Advance R10D by chunks_consumed * 8 = ((n - i) & ~7).
        // Scratch via R8D (chunk counter, decremented to zero by the loop
        // above and now dead) — RAX still holds array A's base, which the
        // scalar remainder below dereferences.
        self.buf.emit(&[0x45, 0x89, 0xD8]); // MOV R8D, R11D
        self.buf.emit(&[0x45, 0x29, 0xD0]); // SUB R8D, R10D
        self.buf.emit(&[0x41, 0x83, 0xE0, 0xF8]); // AND R8D, ~7
        self.buf.emit(&[0x45, 0x01, 0xC2]); // ADD R10D, R8D

        // Restore R13, R12 before falling into scalar cleanup.
        // POP R13 (41 5D), POP R12 (41 5C)
        self.buf.emit(&[0x41, 0x5D]);
        self.buf.emit(&[0x41, 0x5C]);

        // Patch skip-to-scalar target — when R8D == 0, jump here.
        let after_simd = self.buf.pos();
        let skip_rel = (after_simd as i32) - (simd_skip_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(simd_skip_patch, skip_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails

        // --- Scalar remainder ---
        //
        // for (; i < n; i++) OUT[i] = A[i] OP B[i]
        //
        // The A and B base pointers arrive in RAX/RCX, but the loop body's
        // `MOV EAX, [base+...]` / `MOV ECX, [base+...]` loads overwrite
        // EAX/ECX — the low halves of the very RAX/RCX registers used as
        // the base. After the first iteration the base is destroyed and
        // the next load faults (any tail length >= 2 segfaults). Stash the
        // bases into R8/R9, which are both dead here (R8 was the chunk
        // counter, R9 the SIMD A-pointer), and address off those. RDX (OUT
        // base) is only ever a store base, never clobbered, so it stays.
        // This runs on both the SIMD-taken and SIMD-skipped paths since it
        // is emitted at the `after_simd` join point.
        self.buf.emit(&[0x49, 0x89, 0xC0]); // MOV R8, RAX  (A base)
        self.buf.emit(&[0x49, 0x89, 0xC9]); // MOV R9, RCX  (B base)

        let scalar_loop_start = self.buf.pos();
        // CMP R10D, R11D — 45 39 DA
        self.buf.emit(&[0x45, 0x39, 0xDA]);
        // JGE end (patched)
        self.buf.emit_byte(0x0F);
        self.buf.emit_byte(0x8D);
        let scalar_end_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);

        // EAX = A[i] = [R8 + R10*4 + H]
        //   43 8B 84 90 <disp32> — REX.B selects R8 as the SIB base
        self.buf.emit(&[0x43, 0x8B, 0x84, 0x90]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // ECX = B[i] = [R9 + R10*4 + H]
        //   43 8B 8C 91 <disp32> — REX.B selects R9 as the SIB base
        self.buf.emit(&[0x43, 0x8B, 0x8C, 0x91]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // EAX = EAX OP ECX
        self.emit_ewise_scalar_eax_ecx(op);

        // [RDX + R10*4 + H] = EAX
        //   42 89 84 92 <disp32>  — MOV [RDX + R10*4 + disp32], EAX
        self.buf.emit(&[0x42, 0x89, 0x84, 0x92]);
        // Cast: fixed struct/layout offset to i32 instruction displacement
        self.buf.emit(&(HEADER_SIZE as i32).to_le_bytes());

        // INC R10D — 41 FF C2
        self.buf.emit(&[0x41, 0xFF, 0xC2]);
        // JMP scalar_loop_start
        let rel2 = (scalar_loop_start as i32) - (self.buf.pos() as i32 + 5); // Cast: x86-64 rel32 displacement
        self.buf.emit_byte(0xE9);
        self.buf.emit(&rel2.to_le_bytes());

        // Patch scalar end.
        let scalar_end = self.buf.pos();
        let end_rel = (scalar_end as i32) - (scalar_end_patch as i32 + 4); // Cast: x86-64 rel32 displacement
        self.buf.try_patch_i32(scalar_end_patch, end_rel).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
    }



    fn inline_card_mark_available(&self) -> bool {
        // Keep every old-receiver write on the helper-owned barrier path.
        //
        // The inline byte-store sequence is locally sound, but WildFly's real
        // JIT boot audit observed an old `org/jboss/modules/Module` reference
        // to a young child on a clean card. A clean card is a correctness
        // failure: the next minor collection can reclaim that reachable child.
        // Until the direct emitter has end-to-end coverage for every compiled
        // store form and card-table lifecycle, `jit_putfield_object` remains
        // the single source of truth for old-to-young post barriers. Young
        // receivers still retain their barrier-free direct stores.
        //
        // Keep the published metadata in `JitRuntimeHelpers` for a future
        // verified implementation; merely exposing it must not select the
        // unsafe fast path.
        false
    }

    /// Emit the generational post-write barrier using `source_reg` and
    /// `target_reg`, immediately after the reference-slot store.
    ///
    /// Protocol: slot store -> release dirty-byte store. x86-64 TSO preserves
    /// store-store order, so a plain byte store is the release implementation
    /// and needs no `SFENCE`; the STW consumer acquire-scans the atomic card
    /// bytes before following the old-to-young edge. G1/ZGC never expose this
    /// metadata and retain their helper-owned remembered-set barriers.
    fn emit_inline_card_mark_regs(&mut self, source_reg: u8, target_reg: u8) {
        debug_assert!(self.inline_card_mark_available());
        debug_assert!(!matches!(source_reg, RCX | R10 | R11));
        debug_assert!(!matches!(target_reg, RCX | R10 | R11));

        let mut done = Vec::new();
        self.emit_test_r64_r64(target_reg);
        done.push(self.emit_jcc_rel32_patch(0x84)); // null target

        self.emit_mov_imm64_full(R10, self.helpers.jit_card_old_base as i64);
        self.emit_cmp_r64_r64(source_reg, R10);
        done.push(self.emit_jcc_rel32_patch(0x82)); // source below old
        self.emit_mov_imm64_full(R11, self.helpers.jit_card_old_end as i64);
        self.emit_cmp_r64_r64(source_reg, R11);
        done.push(self.emit_jcc_rel32_patch(0x83)); // source at/above old end

        self.emit_cmp_r64_r64(target_reg, R10);
        let target_below_old = self.emit_jcc_rel32_patch(0x82);
        self.emit_cmp_r64_r64(target_reg, R11);
        done.push(self.emit_jcc_rel32_patch(0x82)); // old -> old
        self.patch_rel32_to_here(target_below_old);

        self.emit_mov_r64_r64(RCX, source_reg);
        self.emit_sub_r64_r64(RCX, R10);
        self.emit_shr_r64_imm8(RCX, 9); // CARD_SIZE = 512
        self.emit_mov_imm64_full(R11, self.helpers.jit_card_table_addr as i64);
        self.emit_mov_mem8_indexed_imm8(R11, RCX, 1); // CARD_DIRTY

        for patch in done {
            self.patch_rel32_to_here(patch);
        }
    }




    /// Emit an inline "decode the String character at `idx`" sequence for
    /// the STRING_SEARCH `compareTo` / `indexOf` intrinsics.
    ///
    /// Reads the code unit at element index `idx_reg` of the backing
    /// `byte[]` whose payload starts at `val_reg + HEADER_SIZE`, branching
    /// on `coder_reg` (0 = LATIN1, one byte/char zero-extended; non-zero =
    /// UTF16, two little-endian bytes/char). The zero-extended `u16` result
    /// lands in the low 16 bits of `dst` (upper bits cleared). The four
    /// register operands are distinct 0..=15 GPR numbers; `idx_reg` is the
    /// SIB index and so must not be RSP (4) — and an index field of 100
    /// means "no index", so it must not be R12 (12) either. `val_reg` is
    /// the SIB base and may be any register (R12/RSP as a base is legal
    /// with the disp8 ModRM used here). No CALL; no memory beyond the array
    /// payload is touched.
    fn emit_string_decode_char(&mut self, dst: u8, val_reg: u8, idx_reg: u8, coder_reg: u8) {
        // TEST coder_reg, coder_reg ; JNZ utf16
        let mut rex = 0x48u8;
        if coder_reg >= 8 {
            rex |= 0x05;
        }
        self.buf.emit_byte(rex);
        self.buf.emit_byte(0x85);
        self.buf
            .emit_byte(0xC0 | ((coder_reg & 7) << 3) | (coder_reg & 7));
        let utf16 = self.emit_jcc_rel32_patch(0x85); // JNZ

        // LATIN1: MOVZX dst32, BYTE [val_reg + idx_reg*1 + HEADER_SIZE].
        // 0F B6 /r with a SIB byte (scale=00 → *1).
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB6]);
        // ModRM: mod=01 (disp8), reg=dst, r/m=100 (SIB follows).
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=00, index=idx_reg, base=val_reg.
        self.buf.emit_byte(((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        let done = self.emit_jmp_rel32_patch();

        // UTF16: MOVZX dst32, WORD [val_reg + idx_reg*2 + HEADER_SIZE].
        self.patch_rel32_to_here(utf16);
        let mut rex = 0x40u8;
        if dst >= 8 {
            rex |= 0x04;
        }
        if idx_reg >= 8 {
            rex |= 0x02;
        }
        if val_reg >= 8 {
            rex |= 0x01;
        }
        if rex != 0x40 {
            self.buf.emit_byte(rex);
        }
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x40 | ((dst & 7) << 3) | 0x04);
        // SIB: scale=01 (*2), index=idx_reg, base=val_reg.
        self.buf
            .emit_byte(0x40 | ((idx_reg & 7) << 3) | (val_reg & 7));
        // Truncation: usize -> u8 (small fixed struct offset, fits in an instr disp byte)
        self.buf.emit_byte(HEADER_SIZE as u8);
        self.patch_rel32_to_here(done);
    }


    /// Load a String receiver's `value` field (the backing `byte[]`/`char[]`
    /// ref) from `base` into `dst`, correctly handling BOTH object layouts
    /// that can coexist for `java/lang/String` at runtime:
    ///
    ///   * compact-ref-field layout — the address is
    ///     `StringFieldLayout::value_compact_offset`;
    ///   * a LEGACY-laid-out instance of the same class — per the getfield
    ///     (opcode 0xb4) inline path's own comment, "a class with a
    ///     registered compact layout may still have LEGACY-laid-out
    ///     instances" (e.g. an allocation whose field count didn't match the
    ///     registered `CompactLayout` at alloc time). Its address is
    ///     `StringFieldLayout::value_legacy_offset`.
    ///
    /// Both offsets are exact payload addresses computed independently by
    /// `StringFieldLayout::new` (see BUG-STRING-CODER-COMPACT-20260726 there
    /// for why neither may be derived from the other). Dispatches per-object
    /// via the `GC_FLAG_COMPACT` header bit, exactly mirroring the getfield
    /// 0xb4 inline path. No scratch register needed.
    fn emit_load_string_value_ptr(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84); // JZ (flag clear => legacy)
        self.emit_mov_r64_mem_disp32(dst, base, compact_offset);
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_mov_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }

    /// Load `String.coder` / `String.hash` into `dst` with the same
    /// per-object compact/legacy dispatch as
    /// [`Self::emit_load_string_value_ptr`].
    ///
    /// `compact_is_byte` selects a zero-extending BYTE load for the compact
    /// arm: `CompactLayout` stores `coder` at its natural one-byte Java
    /// width, and the bytes that follow it are the class's padding — a
    /// 4-byte load there would fold that padding into the value. The legacy
    /// arm is always a sign-extended 4-byte `Value` payload load. `coder`
    /// and `hash` are non-negative in practice, so sign- vs zero-extension
    /// is behaviourally identical for the widths that do overlap.
    fn emit_load_string_i32_field(
        &mut self,
        dst: u8,
        base: u8,
        compact_offset: i32,
        compact_is_byte: bool,
        legacy_offset: i32,
    ) {
        self.emit_test_mem8_imm8(
            base,
            cratonvm_types::GC_FLAGS_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        let legacy = self.emit_jcc_rel32_patch(0x84);
        if compact_is_byte {
            // MOVZX dst64, BYTE [base + compact_offset]
            self.emit_movx_r64_mem_disp32(dst, base, compact_offset, 8, false);
        } else {
            self.emit_movsxd_r64_mem_disp32(dst, base, compact_offset);
        }
        let done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(legacy);
        self.emit_movsxd_r64_mem_disp32(dst, base, legacy_offset);
        self.patch_rel32_to_here(done);
    }


    /// Emit a guarded `REP STOSB` preheader for a canonical zero-fill loop.
    ///
    /// Failed guards reach the scalar header without changing Java state. This
    /// retains partial writes before an eventual AIOOBE when the upper bound is
    /// beyond the array.
    fn emit_bulk_zero_byte_fill_preheader(&mut self, fill: &BulkZeroByteFillLoop) {
        // array -> RAX, iv -> R10D, inclusive bound -> R11D.
        if let Some(reg) = self.reg_for_local(fill.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(fill.array_local));
        }
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(fill.iv_local));
        }
        if let Some(reg) = self.reg_for_local(fill.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(fill.bound_local));
        }

        let mut scalar_patches = Vec::with_capacity(5);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar

        // ECX = bound - iv + 1 (the REP counter).
        self.buf.emit(&[0x44, 0x89, 0xD9]); // MOV ECX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD1]); // SUB ECX,R10D
        self.buf.emit(&[0xFF, 0xC1]); // INC ECX

        // RDI is a Java-local home on Windows. No call or safepoint occurs
        // while RSP is transiently adjusted.
        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit(&[0x48, 0x89, 0xC7]); // MOV RDI,RAX
        self.buf.emit(&[0x4C, 0x01, 0xD7]); // ADD RDI,R10
        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI,HEADER_SIZE
        self.buf.emit(&[0x31, 0xC0]); // XOR EAX,EAX
        self.buf.emit(&[0xF3, 0xAA]); // REP STOSB
        self.buf.emit_byte(0x5F); // POP RDI

        // Make the original inclusive condition false and fall through to it.
        self.buf.emit(&[0x45, 0x8D, 0x53, 0x01]); // LEA R10D,[R11D+1]
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(fill.iv_local), R10);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit a guarded register-only loop for a canonical strided byte store.
    ///
    /// All guards precede the first store. A failed guard therefore reaches
    /// the original scalar loop with untouched Java state, retaining null,
    /// bounds, negative-step, and signed-overflow behavior.
    fn emit_bulk_set_byte_stride_preheader(&mut self, fill: &BulkSetByteStrideLoop) {
        // array -> RAX, iv -> R10D, inclusive bound -> R11D, step -> R9D.
        if let Some(reg) = self.reg_for_local(fill.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(fill.array_local));
        }
        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(fill.iv_local));
        }
        if let Some(reg) = self.reg_for_local(fill.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(fill.bound_local));
        }
        if let Some(reg) = self.reg_for_local(fill.step_local) {
            self.emit_mov_reg_reg(R9, reg);
        } else {
            self.emit_load_local(R9, self.local_offset(fill.step_local));
        }

        let mut scalar_patches = Vec::with_capacity(7);
        self.buf.emit(&[0x45, 0x85, 0xD2]); // TEST R10D,R10D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x88)); // JS scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar
        self.buf.emit(&[0x45, 0x85, 0xC9]); // TEST R9D,R9D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8E)); // JLE scalar
        self.buf.emit(&[0xBA, 0xFF, 0xFF, 0xFF, 0x7F]); // MOV EDX,INT_MAX
        self.buf.emit(&[0x44, 0x29, 0xDA]); // SUB EDX,R11D
        self.buf.emit(&[0x41, 0x39, 0xD1]); // CMP R9D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar

        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit(&[0x48, 0x89, 0xC7]); // MOV RDI,RAX
        self.buf.emit(&[0x4C, 0x01, 0xD7]); // ADD RDI,R10
        self.buf.emit(&[0x48, 0x83, 0xC7, HEADER_SIZE as u8]); // ADD RDI,HEADER_SIZE
        let store_start = self.buf.pos();
        self.buf.emit(&[0xC6, 0x07, 0x01]); // MOV byte ptr [RDI],1
        self.buf.emit(&[0x4C, 0x01, 0xCF]); // ADD RDI,R9
        self.buf.emit(&[0x45, 0x01, 0xCA]); // ADD R10D,R9D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE store_start
        let rel = store_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(rel as i32).to_le_bytes());
        self.buf.emit_byte(0x5F); // POP RDI

        if let Some(reg) = self.reg_for_local(fill.iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(fill.iv_local), R10);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }

    /// Emit the guarded remainder of a canonical byte-array Sieve loop nest.
    ///
    /// The range, object, overflow, and safepoint-span guards all precede the
    /// first write. A rejected shape therefore reaches the original bytecode
    /// with every local and array element untouched.
    fn emit_byte_sieve_preheader(&mut self, sieve: &ByteSieveLoop) {
        if let Some(reg) = self.reg_for_local(sieve.array_local) {
            self.emit_mov_reg_reg(RAX, reg);
        } else {
            self.emit_load_local(RAX, self.local_offset(sieve.array_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.outer_iv_local) {
            self.emit_mov_reg_reg(R10, reg);
        } else {
            self.emit_load_local(R10, self.local_offset(sieve.outer_iv_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.bound_local) {
            self.emit_mov_reg_reg(R11, reg);
        } else {
            self.emit_load_local(R11, self.local_offset(sieve.bound_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.count_local) {
            self.emit_mov_reg_reg(R8, reg);
        } else {
            self.emit_load_local(R8, self.local_offset(sieve.count_local));
        }
        if let Some(reg) = self.reg_for_local(sieve.inner_iv_local) {
            self.emit_mov_reg_reg(RCX, reg);
        } else {
            self.emit_load_local(RCX, self.local_offset(sieve.inner_iv_local));
        }

        let mut scalar_patches = Vec::with_capacity(6);
        self.buf.emit(&[0x41, 0x83, 0xFA, 0x02]); // CMP R10D,2
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8C)); // JL scalar
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x44, 0x29, 0xD2]); // SUB EDX,R10D
        self.buf.emit(&[0x81, 0xFA]); // CMP EDX,imm32
        self.buf.emit(&MAX_BULK_BYTE_LOOP_SPAN.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x87)); // JA scalar
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x84)); // JE scalar
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // MOV EDX,[RAX+len]
        self.buf.emit(&[0x41, 0x39, 0xD3]); // CMP R11D,EDX
        scalar_patches.push(self.emit_jcc_rel32_patch(0x83)); // JAE scalar
        self.buf.emit(&[0x41, 0x81, 0xFB]); // CMP R11D,imm32
        self.buf.emit(&0x3FFF_FFFFi32.to_le_bytes());
        scalar_patches.push(self.emit_jcc_rel32_patch(0x8F)); // JG scalar

        // Hold the word-at-a-time zero-byte constants in callee-saved
        // registers. Their Java-local home values are restored before the
        // optimized loop publishes any final local state.
        self.buf.emit_byte(0x57); // PUSH RDI
        self.buf.emit_byte(0x56); // PUSH RSI
        self.emit_mov_imm64_full(RDI, 0x0101_0101_0101_0101);
        self.emit_mov_imm64_full(RSI, 0x8080_8080_8080_8080u64 as i64);
        self.buf.emit(&[0x48, 0x83, 0xC0, HEADER_SIZE as u8]); // ADD RAX,HEADER_SIZE
        let outer_start = self.buf.pos();
        // If at least eight bounded elements remain, detect an all-nonzero
        // qword and skip it in one step:
        //   has_zero = (word - 0x01..) & ~word & 0x80..
        self.buf.emit(&[0x44, 0x89, 0xDA]); // MOV EDX,R11D
        self.buf.emit(&[0x83, 0xEA, 0x07]); // SUB EDX,7
        self.buf.emit(&[0x41, 0x39, 0xD2]); // CMP R10D,EDX
        let scalar_outer = self.emit_jcc_rel32_patch(0x8F); // JG scalar_outer
        self.buf.emit(&[0x4A, 0x8B, 0x14, 0x10]); // MOV RDX,[RAX+R10]
        self.buf.emit(&[0x49, 0x89, 0xD1]); // MOV R9,RDX
        self.buf.emit(&[0x49, 0x29, 0xF9]); // SUB R9,RDI
        self.buf.emit(&[0x48, 0xF7, 0xD2]); // NOT RDX
        self.buf.emit(&[0x49, 0x21, 0xD1]); // AND R9,RDX
        self.buf.emit(&[0x49, 0x85, 0xF1]); // TEST R9,RSI
        let scalar_has_zero = self.emit_jcc_rel32_patch(0x85); // JNE scalar_outer
        self.buf.emit(&[0x41, 0x83, 0xC2, 0x08]); // ADD R10D,8
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE outer_start
        let outer_word_rel = outer_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(outer_word_rel as i32).to_le_bytes());
        let word_scan_done = self.emit_jmp_rel32_patch();
        self.patch_rel32_to_here(scalar_outer);
        self.patch_rel32_to_here(scalar_has_zero);
        self.buf.emit(&[0x42, 0x80, 0x3C, 0x10, 0x00]); // CMP byte [RAX+R10],0
        let composite = self.emit_jcc_rel32_patch(0x85); // JNE outer_increment
        self.buf.emit(&[0x41, 0xFF, 0xC0]); // INC R8D (prime count)
        self.buf.emit(&[0x43, 0x8D, 0x0C, 0x12]); // LEA ECX,[R10+R10]
        self.buf.emit(&[0x44, 0x39, 0xD9]); // CMP ECX,R11D
        let inner_done = self.emit_jcc_rel32_patch(0x8F); // JG inner_done
        let inner_start = self.buf.pos();
        self.buf.emit(&[0xC6, 0x04, 0x08, 0x01]); // MOV byte [RAX+RCX],1
        self.buf.emit(&[0x44, 0x01, 0xD1]); // ADD ECX,R10D
        self.buf.emit(&[0x44, 0x39, 0xD9]); // CMP ECX,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE inner_start
        let inner_rel = inner_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(inner_rel as i32).to_le_bytes());
        self.patch_rel32_to_here(inner_done);
        self.patch_rel32_to_here(composite);
        self.buf.emit(&[0x41, 0xFF, 0xC2]); // INC R10D
        self.buf.emit(&[0x45, 0x39, 0xDA]); // CMP R10D,R11D
        self.buf.emit(&[0x0F, 0x8E]); // JLE outer_start
        let outer_rel = outer_start as i64 - (self.buf.pos() + 4) as i64;
        self.buf.emit(&(outer_rel as i32).to_le_bytes());
        self.patch_rel32_to_here(word_scan_done);
        self.buf.emit_byte(0x5E); // POP RSI
        self.buf.emit_byte(0x5F); // POP RDI

        if let Some(reg) = self.reg_for_local(sieve.outer_iv_local) {
            self.emit_mov_reg_reg(reg, R10);
        } else {
            self.emit_store_local(self.local_offset(sieve.outer_iv_local), R10);
        }
        if let Some(reg) = self.reg_for_local(sieve.count_local) {
            self.emit_mov_reg_reg(reg, R8);
        } else {
            self.emit_store_local(self.local_offset(sieve.count_local), R8);
        }
        if let Some(reg) = self.reg_for_local(sieve.inner_iv_local) {
            self.emit_mov_reg_reg(reg, RCX);
        } else {
            self.emit_store_local(self.local_offset(sieve.inner_iv_local), RCX);
        }

        for patch in scalar_patches {
            self.patch_rel32_to_here(patch);
        }
    }


    fn emit_guarded_getfield_receiver_check(&mut self, bounds_addr: usize) -> Vec<usize> {
        let mut slow: Vec<usize> = Vec::new();
        // 1. null → slow (helper throws the NPE).
        self.emit_test_r64_r64(RAX);
        slow.push(self.emit_jcc_rel32_patch(0x84)); // JZ
                                                    // 2. alignment: low 3 bits must be clear.
        self.emit_mov_r64_r64(RCX, RAX);
        self.emit_and_r64_imm8(RCX, 7);
        slow.push(self.emit_jcc_rel32_patch(0x85)); // JNZ
                                                    // 3. region containment. RDX = &JIT_REGION_BOUNDS (six usize words:
                                                    //    [b0, e0, b1, e1, b2, e2]).
        self.emit_mov_imm64(RDX, bounds_addr as i64);
        // region 0: RAX >= b0 && RAX < e0 → ok
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 0);
        let below_b0 = self.emit_jcc_rel32_patch(0x82); // JB → try region 1
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 8);
        let ok0 = self.emit_jcc_rel32_patch(0x82); // JB → in region 0
        self.patch_rel32_to_here(below_b0);
        // region 1
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 16);
        let below_b1 = self.emit_jcc_rel32_patch(0x82); // JB → try region 2
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 24);
        let ok1 = self.emit_jcc_rel32_patch(0x82); // JB → in region 1
        self.patch_rel32_to_here(below_b1);
        // region 2 — last chance: outside → slow.
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 32);
        slow.push(self.emit_jcc_rel32_patch(0x82)); // JB → slow
        self.emit_cmp_r64_mem_disp32(RAX, RDX, 40);
        slow.push(self.emit_jcc_rel32_patch(0x83)); // JAE → slow
                                                    // fall-through / ok: receiver is inside a published live region.
        self.patch_rel32_to_here(ok0);
        self.patch_rel32_to_here(ok1);
        slow
    }

    /// Cheaper receiver guard for a value whose operand-stack type is already
    /// proven to be an oop by the bytecode/type tracker. Such a value cannot be
    /// an unaligned integer or an arbitrary out-of-heap address without an
    /// earlier JIT/GC correctness failure, so repeating the six arena-bound
    /// comparisons at every field access is redundant. Null remains a real
    /// Java exceptional case and is routed to the existing checked helper.
    fn emit_trusted_oop_receiver_check(&mut self) -> Vec<usize> {
        self.emit_test_r64_r64(RAX);
        vec![self.emit_jcc_rel32_patch(0x84)] // JZ -> checked helper
    }

    /// The full-barrier route every inline reference-`putfield` arm falls back
    /// to: `jit_putfield_object(heap, obj, field_index, value)`, which performs
    /// the SATB pre-barrier and the collector's OWN post-write barrier — G1's
    /// `post_write_barrier_rset` included, which is the remembered-set edge a
    /// JNI-pinned (CSet-excluded) young region is reachable only through.
    ///
    /// Factored out for G1-2 so the "bounds are not live ⇒ take the helper"
    /// short-circuit is literally the same instruction sequence as the bail
    /// target the fast paths already patch to.
    fn emit_ref_putfield_helper_call(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
    ) {
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
        self.load_slot_to_reg(ARG_REGS[3], val_slot);
        self.emit_call_absolute(self.helpers.putfield_object);
    }

    /// Emit a compact reference-field store with a barrier-free fast path and
    /// the validated helper as its slow path.
    ///
    /// Small callees such as constructors are emitted by
    /// `try_emit_inline_body`, not the top-level bytecode loop. Keeping this
    /// emitter shared inside `Compiler` makes their field stores follow the
    /// same safety contract as top-level compact `putfield`: only a mapped,
    /// genuinely compact, young receiver whose old field is null is written
    /// directly. Every case requiring SATB/card barriers goes through
    /// `jit_putfield_object`.
    fn emit_inline_body_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
    ) {
        let cell_off = (HEADER_SIZE + compact_body_offset as usize) as i32;

        // G1-2: no published bounds ⇒ no generational card metadata ⇒ the
        // "young receiver needs no post barrier" premise does not hold (G1's
        // RSet edge into a JNI-pinned, CSet-excluded region would be lost).
        // The containment guard below would reject every receiver anyway with
        // an all-zero table, and with an unwired table it would bake a
        // `MOV RDX,0` + `CMP RAX,[RDX]` that faults — so take the helper
        // outright instead of emitting an inline path that can never run.
        if !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();

        self.load_slot_to_reg(RAX, obj_slot);
        bail.extend(self.emit_guarded_getfield_receiver_check(self.helpers.region_bounds_addr));

        // A registered compact class may still have legacy instances when a
        // synthetic/native allocation used a mismatched slot count.
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper

        // Without direct generational card metadata, old receivers retain the
        // collector-specific helper. Otherwise the post-store mark is inline.
        if !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        // A non-null old value needs the SATB pre-barrier.
        self.emit_mov_r64_mem_disp32(RCX, RAX, cell_off);
        self.emit_test_r64_r64(RCX);
        bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ non-null -> helper

        // Match the interpreter/helper's silent out-of-bounds drop.
        self.emit_mov_r32_mem_disp32(RCX, RAX, cratonvm_types::NUM_SLOTS_OFFSET as i32);
        self.emit_mov_imm64(RDX, field_index as i64);
        self.emit_cmp_r32_r32(RDX, RCX);
        let oob = self.emit_jcc_rel32_patch(0x83); // JAE -> drop

        // Compact reference fields are bare 8-byte pointers.
        self.load_slot_to_reg(RDX, val_slot);
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        if self.inline_card_mark_available() {
            self.emit_inline_card_mark_regs(RAX, RDX);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(oob);
        self.patch_rel32_to_here(done);
    }

    /// Constructor-only specialization for the first syntactic write to a
    /// compact reference field.
    ///
    /// JVM verification only permits `<init>` on a non-null uninitialized
    /// object produced by `new`. The inline resolver additionally admits only
    /// empty super-constructor chains and forward control flow. Therefore the
    /// first write to a given field starts from null and its resolved slot is
    /// in bounds. A young compact receiver needs no barrier; the only runtime
    /// checks retained are the per-object compact flag (synthetic allocations
    /// can still use legacy cells) and old-generation bit (allocation spill).
    ///
    /// G1-2 (`docs/gc/g1-audit.md` §8.1): "a young compact receiver needs no
    /// barrier" is a GENERATIONAL claim. This emitter used to state it with no
    /// receiver guard whatsoever — not even the null test its two sibling
    /// emitters have — so on a backend that publishes no region bounds it wrote
    /// the reference inline and lost the collector's post-write barrier. Under
    /// G1 that is the JNI-pinned-young-region remembered-set edge (a pinned
    /// region is excluded from the CSet, so its rset is the ONLY way in), i.e. a
    /// use-after-free. It now takes the helper outright when
    /// [`region_bounds_are_live`] is false, and when it is true it emits the
    /// same null test the trusted-oop arms emit.
    fn emit_inline_fresh_ctor_compact_ref_putfield(
        &mut self,
        obj_slot: StackSlot,
        val_slot: StackSlot,
        field_index: usize,
        compact_body_offset: u32,
    ) {
        let cell_off = (HEADER_SIZE + compact_body_offset as usize) as i32;

        // G1-2: bounds not live ⇒ not the generational backend ⇒ every
        // reference store must run the collector's own post-write barrier.
        if !region_bounds_are_live(self.helpers.region_bounds_addr) {
            self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);
            return;
        }

        let mut bail: Vec<usize> = Vec::new();

        self.load_slot_to_reg(RAX, obj_slot);
        // G1-2: receiver guard, consistent with the other two emitters. The
        // full containment check is deliberately NOT repeated here — with
        // bounds live the backend is Generational, and this receiver is the
        // `new`-produced uninitialized object the JVM verifier requires for
        // `<init>` (see the precondition above), so the remaining exceptional
        // case is null. It is unreachable in practice (the caller already
        // emitted `emit_precise_null_check_field_store`) and therefore costs a
        // perfectly-predicted not-taken branch; without it a null receiver
        // faulted on the `gc_flags` header read below instead of reaching the
        // helper's defined no-op semantics.
        bail.extend(self.emit_trusted_oop_receiver_check());
        self.emit_test_mem8_imm8(
            RAX,
            cratonvm_types::GC_FLAGS_OFFSET as i32,
            cratonvm_types::GC_FLAG_COMPACT,
        );
        bail.push(self.emit_jcc_rel32_patch(0x84)); // JZ legacy -> helper
        if !self.inline_card_mark_available() {
            self.emit_test_mem8_imm8(
                RAX,
                cratonvm_types::GC_FLAGS_OFFSET as i32,
                cratonvm_types::GC_FLAG_OLD_GEN,
            );
            bail.push(self.emit_jcc_rel32_patch(0x85)); // JNZ old -> helper
        }

        self.load_slot_to_reg(RDX, val_slot);
        self.emit_mov_mem_disp32_r64(RAX, RDX, cell_off);
        if self.inline_card_mark_available() {
            self.emit_inline_card_mark_regs(RAX, RDX);
        }
        let done = self.emit_jmp_rel32_patch();

        for b in bail {
            self.patch_rel32_to_here(b);
        }
        self.emit_ref_putfield_helper_call(obj_slot, val_slot, field_index);

        self.patch_rel32_to_here(done);
    }

    fn emit_inline_tlab_new(
        &mut self,
        class_id_raw: u32,
        num_fields: usize,
        // CRIT-2 — when both `has_nonzero_tag_primitive_init` and
        // `has_finalizer` are statically known false at the call site, the post-init
        // helper has nothing meaningful to do beyond writing the
        // identity-hash and num_slots header words. We can emit those
        // inline and skip the helper call (which otherwise costs a
        // class_manager.read() and a finalizer-queue lock). When
        // unknown (the conservative default in `try_compile`), we
        // still issue the helper call.
        skip_post_init_helper: bool,
    ) {
        // A raw compiled bump updates `Tlab::cursor` without going through
        // the allocator's publication protocol. In concurrent Elasticsearch
        // merge churn that left a malformed young-space span before the next
        // collection could obtain an exact object map. Route through the
        // checked runtime helper until the raw JIT path can share the same
        // atomic publication contract as `Tlab::alloc_initialized`.
        //
        // The helper retains TLAB allocation (and its fast path); it merely
        // removes the unsynchronised machine-code cursor writer.
        if !inline_tlab_new_enabled() {
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32);
            self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32);
            self.emit_call_absolute(self.helpers.new_object);
            return;
        }

        // Compact reference-field layout: when a per-class layout is registered
        // for exactly this field count, allocate the packed body size and mark
        // the object compact (array_length = body bytes, GC_FLAG_COMPACT) inline
        // — no helper call, no per-alloc layout lookup. `class_layout` here runs
        // once at JIT-compile time, not per allocation.
        // GROOVY-CLUSTER-20260717 → GUARDED RESTORE (perf/halfgap-20260717):
        // class_layout(class_id_raw) is snapshotted ONCE at JIT-compile time
        // and its body_size gets baked as immediate constants below
        // (bump-allocation size, array_length header write). When a class's
        // registered compact layout is later REPLACED (class_manager.rs's
        // recompute_subclass_layouts — the synthetic-stub→real-bytecode
        // upgrade, the exact shape of ANTLR/Groovy-generated parser
        // classes), an already-compiled site would keep allocating at the
        // OLD size while field access (correctly, per-object) uses the
        // CURRENT layout — confirmed heap corruption; the interim fix
        // disabled this path entirely (compact_body = None).
        //
        // The restore bakes the ADDRESS + compile-time VALUE of the class's
        // layout-REPLACE counter (`types::field_layout::layout_replace_guard`
        // — fixed-capacity table, addresses stable for process life; new
        // class REGISTRATIONS don't bump it, only replacements do) and
        // emits a 3-instruction guard at the top of the inline path:
        //     mov r11, imm64(count_addr)
        //     mov eax, [r11]
        //     cmp eax, imm32(count_at_compile_time)  ;  jne slow_path
        // A replaced layout therefore permanently routes this site to the
        // always-correct `new_object` helper — for classes that never get
        // replaced (every benchmark and the overwhelming majority of real
        // classes), the full compact inline path is back.
        let compact_snapshot: Option<(usize, *const u32, u32)> =
            if cratonvm_types::compact_ref_fields_enabled() {
                cratonvm_types::class_layout(class_id_raw)
                    .filter(|l| l.field_count() == num_fields)
                    .map(|l| {
                        let (addr, expected) = cratonvm_types::layout_replace_guard(class_id_raw);
                        (l.body_size as usize, addr, expected)
                    })
            } else {
                None
            };
        let compact_body: Option<usize> = compact_snapshot.map(|(body, _, _)| body);
        // Object total size (header + body). Computed at compile time.
        let total_size = HEADER_SIZE + compact_body.unwrap_or(num_fields * SLOT_SIZE);
        // Cast: value to i32 (encoding immediate/displacement)
        let cursor_off = self.helpers.tlab_cursor_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let end_off = self.helpers.tlab_end_offset_in_thread as i32;
        // Cast: value to i32 (encoding immediate/displacement)
        let class_id_off = self.helpers.class_id_offset_in_obj as i32;

        // Step 0: layout-replace guard (see the GUARDED RESTORE note above).
        // Runs before anything else so a stale-layout site diverts to the
        // helper with zero state to unwind. R11/RAX are scratch here.
        let layout_guard_patch = compact_snapshot.map(|(_, count_addr, expected)| {
            self.emit_mov_imm64_full(R11, count_addr as i64);
            self.emit_mov_r32_mem_disp32(RAX, R11, 0);
            // CMP EAX, imm32 (EAX-only short form 0x3D).
            self.buf.emit_byte(0x3D);
            self.buf.emit(&(expected as i32).to_le_bytes());
            self.emit_jcc_rel32_patch(0x85) // JNE slow_path
        });

        // Step 1: fetch the JvmThread*. Allocation-heavy methods cache it in
        // the prologue/OSR trampoline; otherwise use the small TLS helper.
        //
        // HIGH-2 / Fix 2 — direct `MOV reg, FS:[off]` TLS load is the
        // ideal sequence (saves ~5 ns per `new`). It is NOT applied
        // here in this round because it requires runtime cooperation
        // we do not yet have:
        //
        //   * Rust's `thread_local!` macro hides the TLS slot offset
        //     entirely — there is no portable API to extract the
        //     FS/GS-relative offset of `JIT_THREAD` at JIT-compile
        //     time. A `#[thread_local]` static (unstable on stable
        //     Rust) would still need a startup probe (inline asm
        //     `mov rax, fs:[OFFSET]` against a known sentinel) to
        //     recover the loader-assigned displacement.
        //   * On Windows the slot lives at GS:[0x58 + slot*8] where
        //     `slot` is allocated dynamically by `TlsAlloc`; the same
        //     probe machinery applies but with a different segment
        //     prefix and one extra indirection. Per task scope, this
        //     arm is intentionally left on the helper.
        //   * The current `JitRuntimeHelpers` table exposes only the
        //     helper function pointer; wiring an `Option<(SegPrefix,
        //     u32)>` field plus a startup probe in the VM is a
        //     cross-crate change outside the scope of this fix
        //     round.
        //
        // Until that plumbing lands, the helper call stays — see the
        // task notes for the planned approach.
        // Common case: one prologue/OSR helper call per invocation, not per `new`.
        if self.jit_thread_slot_off != 0 {
            self.emit_load_local(RAX, self.jit_thread_slot_off);
            self.emit_test_r64_r64(RAX);
            let have_cached_thread = self.emit_jcc_rel32_patch(0x85); // JNE have_thread
            self.emit_call_absolute(self.helpers.get_current_thread);
            self.emit_store_local(self.jit_thread_slot_off, RAX);
            self.patch_rel32_to_here(have_cached_thread);
        } else {
            self.emit_call_absolute(self.helpers.get_current_thread);
        }
        self.emit_test_r64_r64(RAX);
        let null_thread_patch = self.emit_jcc_rel32_patch(0x84); // JE slow_path

        // R10 = thread; R11 = cursor.
        self.emit_mov_r64_r64(R10, RAX);
        self.emit_mov_r64_mem_disp32(R11, R10, cursor_off);

        // Align cursor up to 8 bytes (matches `Tlab::alloc(_, 8)`'s
        // behaviour). Without this, an interleaved array allocation that
        // left the cursor misaligned would force this `new` object onto a
        // non-8-aligned address — the GC walker assumes 8-aligned object
        // headers and would mis-decode the layout. Total cost: 2
        // instructions (8 bytes encoded) — negligible vs the cache miss
        // the slow path would incur.
        self.emit_add_r64_imm8(R11, 7);
        self.emit_and_r64_imm8(R11, -8);

        // RAX = R11 + total_size (new cursor).
        self.emit_lea_r64_mem_disp32(RAX, R11, total_size as i32); // Cast: x86-64 immediate encoding

        // CMP RAX, [R10 + end_off]; JA slow_path (TLAB exhausted).
        self.emit_cmp_r64_mem_disp32(RAX, R10, end_off);
        let tlab_full_patch = self.emit_jcc_rel32_patch(0x87); // JA slow_path

        // JVM default initialization and TLAB-reuse safety. All refill
        // backends return zeroed TLAB ranges, including cells reused by a
        // non-moving sweep, so the default path does not repeat those stores
        // per object. The opt-out retains the older defensive clear. Both
        // layouts are qword-sized here (legacy fields are 16 bytes; compact
        // fields are 8 or 16 bytes).
        //
        // This also makes the all-zero-tag primitive family (int, boolean,
        // byte, char, short) fully initialized inline as `Value::Int(0)`.
        // Only long/float/double need the post-init helper to install a non-zero
        // Value discriminant; reference fields in the compact layout are null
        // bare pointers after this clear.
        let zero_elision = inline_tlab_zero_elision_enabled();
        if !zero_elision {
            debug_assert_eq!((total_size - HEADER_SIZE) % 8, 0);
            self.emit_mov_imm32_sx(RDX, 0);
            for body_off in (HEADER_SIZE..total_size).step_by(8) {
                self.emit_mov_mem_disp32_r64(R11, RDX, body_off as i32);
            }
        }

        // BinTrees-18 heap-corruption fix (jit/gc audit, 2026-06):
        // *** Write the full object header BEFORE committing the TLAB
        // cursor. ***
        //
        // The previous order committed the bump (published the object's
        // address into `thread.tlab.cursor`) and only THEN wrote the
        // header fields. That left a window in which the object region was
        // already part of the "used" portion of the TLAB / young arena but
        // its header was still the TLAB-zeroed pattern (class_id=0,
        // kind=Object, num_slots=0). Any heap walk that observed the object
        // during that window — the non-moving young sweep that runs while
        // JIT frames are active (`gc_quiescence`), the Cheney to-space
        // scan, or a background-thread STW collection that parks this
        // mutator at a poll inside the in-between helper — computed
        // `size = HEADER_SIZE + 0*SLOT_SIZE = HEADER_SIZE` and stepped 40
        // bytes into the object's own field region. There it decoded the
        // first `Value` field cell (discriminant word = 4 = `Object`) as a
        // bogus header: `class_id=4`, `array_length=1` (upper half of the
        // 8-byte object-pointer payload), `num_slots=384` (the next cell's
        // discriminant region) — exactly the
        // "kind=Object but array_length=1 (num_slots=384, class_id=4)"
        // inconsistency reported by `gen_object_total_size`, after which
        // the walker desynced / looped (rc=124 timeout on `bintrees18`).
        //
        // Writing the header first means the object is fully walker-coherent
        // at the instant its address becomes reachable via the committed
        // cursor: the store to `cursor` below is the single linearization
        // point, and on x86-64 it is not reordered ahead of the header
        // stores (TSO: stores are not reordered with older stores). So no
        // walker can ever see a committed-but-unheadered object.
        //
        //   class_id  → identifies the object's class (offset 0)
        //   off 4     → kind=Object(0) / elem=Reference(0) / padding(0)
        //   off 12    → array_length=0 (Object kind never sets this)
        //   num_slots → walker's stride: size = HEADER_SIZE + n*SLOT_SIZE
        //
        // identity_hash_code (offset 8) stays 0 (TLAB-zeroed); the lazy-
        // mint contract in `System.identityHashCode()` handles it on
        // demand. The `jit_post_tlab_init` helper below still runs for the
        // primitive-init / finalizer paths, but the header is already
        // walker-coherent before the object is ever published.
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            class_id_off,
            class_id_raw as i32, // Cast: ClassId immediate fits in 32 bits
        );
        // Defensively zero offset 4 (kind=Object=0, elem=Reference=0, pad=0)
        // and offset 12 (array_length=0). The historical assumption "TLAB
        // refill zeroes the region" was empirically violated on long runs
        // (BinTrees-18, ECJ HashtableOfInt /by-zero #23): the GC-ARRAY-GUARD
        // observed `kind=Object && array_length=0x01010101` on freshly-
        // bumped slots. The defensive walker in `bcd70d0` catches that
        // pattern as corruption, but the right place to enforce the
        // invariant is at the *allocator* — write the four header bytes
        // (and the four array_length bytes) explicitly. Two extra dwords
        // per `new` is negligible vs. the safety guarantee.
        // `OBJECT_KIND_OFFSET` (4) names the dword that packs
        // kind/element_type/gc_age/gc_flags; `IDENTITY_HASH_CODE_OFFSET` (8)
        // names the identity-hash dword. Both were bare literals until the
        // 2026-07-26 header-offset audit — see
        // `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md` §5.
        self.emit_mov_dword_mem_disp32_imm32(R11, cratonvm_types::OBJECT_KIND_OFFSET as i32, 0);
        if !zero_elision {
            // identity_hash_code = 0 (lazy-mint contract).
            self.emit_mov_dword_mem_disp32_imm32(R11, IDENTITY_HASH_CODE_OFFSET as i32, 0);
        }
        // offset 12: the full 32-bit field count for Object kind.
        let shape = num_fields as u32;
        self.emit_mov_dword_mem_disp32_imm32(
            R11,
            cratonvm_types::NUM_SLOTS_OFFSET as i32,
            shape as i32,
        );
        // Compact object: set GC_FLAG_COMPACT (bit 2) in the gc_flags byte.
        if let Some(body) = compact_body {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_COMPACT_INLINE").is_some() {
                eprintln!(
                    "[compact-inline] new class_id={class_id_raw} body={body} total={total_size}"
                );
            }
            // `GC_FLAGS_OFFSET` (7) is byte 3 of the dword at
            // `OBJECT_KIND_OFFSET` (4), hence the `<< 24`. The shift is only
            // correct while `GC_FLAGS_OFFSET - OBJECT_KIND_OFFSET == 3`;
            // `header_offset_contract_gc_flags_is_byte3_of_kind_dword` pins it.
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::OBJECT_KIND_OFFSET as i32,
                (cratonvm_types::GC_FLAG_COMPACT as i32)
                    << (8 * (cratonvm_types::GC_FLAGS_OFFSET - cratonvm_types::OBJECT_KIND_OFFSET)),
            );
        }
        // The opt-out also retains the older defensive forwarding_ptr and
        // mark_word stores. The default path gets their required zero values
        // from the refill invariant; neither field is subsequently published
        // with a non-zero initialization value.
        if !zero_elision {
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::FORWARDING_PTR_OFFSET as i32,
                0,
            );
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::FORWARDING_PTR_OFFSET as i32 + 4,
                0,
            );
            self.emit_mov_dword_mem_disp32_imm32(R11, cratonvm_types::MARK_WORD_OFFSET as i32, 0);
            self.emit_mov_dword_mem_disp32_imm32(
                R11,
                cratonvm_types::MARK_WORD_OFFSET as i32 + 4,
                0,
            );
        }

        // Commit the bump LAST: [R10 + cursor_off] = RAX. This publishes the
        // object's end as the new cursor (and, transitively, the object's
        // address as a live allocation). x86-64 TSO preserves the required
        // header/body-before-cursor store order; the STW handshake provides
        // the acquire side. Do not add an SFENCE here: it is unnecessary on
        // this backend and would tax every fast-path allocation.
        self.emit_mov_mem_disp32_r64(R10, RAX, cursor_off);

        if skip_post_init_helper {
            // CRIT-2 fast path — no primitive defaults to apply and no
            // finalizer to register. With class_id + num_slots already
            // written inline above, the header is complete enough for
            // both the GC walker and the runtime; no helper call needed.
            //
            // Class, kind/flags, and shape are explicitly published above.
            // Body defaults, forwarding_ptr=null, and
            // mark_word=MARK_NEUTRAL come from the refill zeroing invariant
            // unless the conservative opt-out repeats those stores inline.
            //
            // RAX = obj_ptr — both arms converge with RAX holding the
            // freshly-allocated object pointer.
            self.emit_mov_r64_r64(RAX, R11);
        } else {
            // Hand off to post-init: tlab_post_init(vm_ptr, obj_ptr, cid, nf).
            // The helper now only does the cold work (identity-hash mint,
            // primitive-typed default values, finalizer registration); the
            // walker-coherent header bits (class_id + num_slots) are
            // already in place from the inline writes above.
            self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
            self.emit_mov_r64_r64(ARG_REGS[1], R11);
            self.emit_mov_imm32_sx(ARG_REGS[2], class_id_raw as i32); // Cast: ClassId fits in 32 bits
            self.emit_mov_imm32_sx(ARG_REGS[3], num_fields as i32); // Cast: x86-64 immediate encoding
            self.emit_call_absolute(self.helpers.tlab_post_init);
        }

        // Jump over the slow path; both arms converge with RAX = obj_ptr.
        let done_patch = self.emit_jmp_rel32_patch();

        // ----- slow_path -----
        self.patch_rel32_to_here(null_thread_patch);
        self.patch_rel32_to_here(tlab_full_patch);
        if let Some(patch) = layout_guard_patch {
            // Layout-replace guard mismatch: the baked compact size is stale;
            // the helper allocates per the CURRENT layout.
            self.patch_rel32_to_here(patch);
        }
        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: ClassId fits in 32 bits
        self.emit_mov_imm32_sx(ARG_REGS[2], num_fields as i32); // Cast: x86-64 immediate encoding
        self.emit_call_absolute(self.helpers.new_object);

        // ----- done -----
        self.patch_rel32_to_here(done_patch);
    }


    /// Emit inlined callee bytecode at the given caller PC.
    ///
    /// Returns `true` if inlining succeeded, `false` to fall back to a normal call.
    /// The callee's locals are allocated in the caller's spill area so no new frame
    /// is needed. Forward branches within the callee are tracked and patched after
    /// emission. On return, the callee's result (if any) is on the caller's operand
    /// stack.
    /// Speculatively inline the callee at `pc`. On any mid-body bail this
    /// rolls back ALL speculative state — emitted machine code, the
    /// simulated operand stack, its oop-mark vector, and the spill
    /// cursor — so the caller can cleanly fall back to a normal call.
    ///
    /// Historically the `return false` bail points inside the inline
    /// interpreter reset only `next_spill_offset`; the partially-emitted
    /// callee body (and any operand-stack pops) were left in place. The
    /// caller then emitted a *second*, full dispatch for the same call
    /// site — duplicate/garbage code that miscompiled the method (boxed
    /// values came back as 0). Snapshotting + rollback here makes a bail
    /// fully transparent.
    fn try_emit_inline(&mut self, pc: usize) -> bool {
        let buf_checkpoint = self.buf.pos();
        let stack_checkpoint = self.stack.clone();
        let oop_marks_checkpoint = self.stack_oop_marks.clone();
        let spill_checkpoint = self.next_spill_offset;
        // groovyjarjarasm-asm-handler-getexceptiontablesize-sigsegv-20260713:
        // the buffer/stack/oop-marks/spill rollback above is NOT the full set
        // of speculative side effects `try_emit_inline_body` can produce. Any
        // bytecode instruction it simulates (e.g. an inlined `invoke*` via
        // `emit_post_invoke_exception_check`) can also push a **patch-site
        // offset** -- a raw `usize` into `self.buf` -- onto one of these
        // deferred patch-list fields. Those offsets are only meaningful while
        // they point at the placeholder bytes (`0F 84 00 00 00 00` etc.) that
        // were live when they were recorded. A bail rewinds `self.buf` past
        // them (via `rewind_to` above) and the fall-through normal-call path
        // then emits *different* code over that same buffer range -- but
        // without this snapshot/truncate, the stale offset(s) from the
        // abandoned attempt survive in the Vec and get blindly patched later
        // (`emit_exception_check_stub` / `emit_deopt_stubs` / `patch_branches`
        // / `patch_self_calls`, all of which run once at the very end of
        // `compile_bytecode` over the FINAL, already-reused buffer), corrupting
        // whatever real instruction now lives at that stale offset. Root-caused
        // via a live trace on `groovyjarjarasm.asm.Handler.getExceptionTableSize`
        // (pulled in by Groovy's ASM-based class generation under
        // `GroovyScriptFactoryTests`): a stale `exception_check_stubs` entry
        // from a rewound inline attempt got patched into the middle of the
        // *kept* method's precise-maps safepoint-id store, scribbling a bogus
        // immediate byte and a corrupt REX prefix into otherwise-valid JIT
        // code -- an immediate SIGSEGV the instant the (very hot, called
        // thousands of times) method next ran, well before any test
        // discovery. Snapshot every such deferred patch-list field here and
        // truncate back on bail, mirroring the buffer/stack rollback above.
        let exception_check_stubs_checkpoint = self.exception_check_stubs.len();
        let deopt_stubs_checkpoint = self.deopt_stubs.len();
        let forward_patches_checkpoint = self.forward_patches.len();
        let jump_table_patches_checkpoint = self.jump_table_patches.len();
        let self_call_patches_checkpoint = self.self_call_patches.len();
        let bounds_check_stubs_checkpoint = self.bounds_check_stubs.len();
        let null_check_store_stubs_checkpoint = self.null_check_store_stubs.len();
        // Reload-elision mirror: the inline mini-emitter replays CALLEE
        // bytecode whose internal joins the position rule cannot see (the
        // main loop's branch-target invalidation covers only OUTER-method
        // pcs). Suppress the mechanism for the duration and drop any live
        // mirror on both entry and exit; a bail additionally rewinds the
        // buffer, which would otherwise let a stale recorded position
        // "validate" against different, re-emitted code.
        let mirror_suppressed_checkpoint = self.slot_mirror_suppressed;
        self.slot_mirror = None;
        self.slot_mirror_suppressed = true;
        let inline_ok = self.try_emit_inline_body(pc);
        self.slot_mirror_suppressed = mirror_suppressed_checkpoint;
        self.slot_mirror = None;
        if inline_ok {
            true
        } else {
            // Discard every speculative side effect of the abandoned
            // inline attempt so the fall-through normal-call path starts
            // from exactly the pre-inline machine state.
            self.buf.rewind_to(buf_checkpoint);
            self.stack = stack_checkpoint;
            self.stack_oop_marks = oop_marks_checkpoint;
            self.next_spill_offset = spill_checkpoint;
            self.exception_check_stubs
                .truncate(exception_check_stubs_checkpoint);
            self.deopt_stubs.truncate(deopt_stubs_checkpoint);
            self.forward_patches.truncate(forward_patches_checkpoint);
            self.jump_table_patches
                .truncate(jump_table_patches_checkpoint);
            self.self_call_patches
                .truncate(self_call_patches_checkpoint);
            self.bounds_check_stubs
                .truncate(bounds_check_stubs_checkpoint);
            self.null_check_store_stubs
                .truncate(null_check_store_stubs_checkpoint);
            false
        }
    }

    /// Inline-emission body. MUST only be called via [`Self::try_emit_inline`],
    /// which snapshots and restores compiler state around it. A `false`
    /// return from anywhere inside is safe precisely because of that
    /// wrapper — the bail sites here therefore no longer need to unwind
    /// `next_spill_offset` by hand.
    fn try_emit_inline_body(&mut self, pc: usize) -> bool {
        let site = match self.inline_sites.get(&pc) {
            Some(s) => s.clone(),
            None => return false,
        };

        // An inlined callee emits arbitrary code that uses the caller-saved
        // scratch GPRs (R8/R9) and FP temporaries (XMM0-7) — exactly the
        // registers the deferred-spill operand model (`StackSlot::Scratch` /
        // `StackSlot::Xmm`) parks live values in. Inlining is a call boundary,
        // so flush every caller-live Scratch/Xmm operand to its frame slot
        // BEFORE emitting the callee body — otherwise the callee clobbers a
        // value still live on the caller's operand stack (e.g. a computed
        // double argument or a result held across the call), silently
        // corrupting it. Without this, `leaf(x) + leaf(x*0.5)` and the whole
        // commons-math FastMath.sin family miscompiled under JIT. `try_emit_inline`
        // snapshots+restores all state, so a later mid-body bail rolls this back.
        self.flush_scratch_registers();

        let callee_code = &site.callee_code;
        let callee_len = site.callee_code_len;
        let callee_num_args = site.callee_num_args;
        let callee_max_locals = site.callee_max_locals;
        let _return_type = site.return_type;
        let (callee_param_jvm_slots, callee_param_slot_span) =
            crate::compute_param_jvm_slots(&site.descriptor, site.callee_is_static);
        if callee_param_jvm_slots.len() != callee_num_args {
            return false;
        }

        let callee_locals_size = callee_max_locals.max(callee_param_slot_span);
        // Allocate callee locals in caller's spill area.
        let Some(callee_local_base) = self.reserve_spill_slots(callee_locals_size) else {
            return false;
        };

        // Pop arguments from caller stack and store into callee locals.
        // Args are pushed left-to-right, so stack top = last arg.
        // For instance methods, arg0 = objectref ('this').
        let stack_len = self.stack.len();
        if stack_len < callee_num_args {
            self.next_spill_offset = callee_local_base;
            return false;
        }

        // Store args into the callee's JVM local slots. Category-2 parameters
        // consume two JVM slots while the JIT operand stack carries one i64
        // value, so the descriptor-derived slot map must mirror the normal
        // prologue layout (`(JJI)J` -> slots 0, 2, 4).
        for i in (0..callee_num_args).rev() {
            let slot = self.pop_stack();
            let local_idx = callee_param_jvm_slots[i];
            let local_off = callee_local_base + (local_idx as i32) * 8; // Cast: x86-64 immediate encoding
            self.load_slot_to_reg(RAX, slot);
            self.emit_store_local(local_off, RAX);
        }

        // Zero-init every non-parameter callee local. Do not start at
        // `callee_num_args`: that compact arg count is not a JVM local index
        // when category-2 parameters are present.
        for i in 0..callee_locals_size {
            if callee_param_jvm_slots.contains(&i) {
                continue;
            }
            let local_off = callee_local_base + (i as i32) * 8; // Cast: x86-64 immediate encoding
            self.emit_xor_reg_self(RAX);
            self.emit_store_local(local_off, RAX);
        }

        // Track forward branches within the inlined code: (patch_offset, target_callee_pc)
        let mut branch_patches: Vec<(usize, usize)> = Vec::new();
        // Map callee PC → native offset for branch targets
        let mut callee_pc_to_native: Vec<i64> = vec![-1; callee_len + 1];

        let mut cpc: usize = 0;
        let save_spill = self.next_spill_offset;
        // Operand-stack depth belonging to the CALLER; the callee's operands
        // sit above it. The callee operand stack is "empty" exactly when
        // `self.stack.len() == caller_base_depth`.
        let caller_base_depth = self.stack.len();
        // Callee branch targets (merge points). Branchy callees inline only
        // when every merge has an EMPTY callee operand stack (enforced by the
        // merge-point reset below + the per-branch checks). 419a6f5 blanket-
        // bailed ALL branches to stop a value-merge slot desync (the
        // `iconst_1; goto L; iconst_0; L: ireturn` diamond); this restores the
        // provably-safe subset (e.g. `x>=0?x:-x`) while still bailing diamonds.
        let callee_branch_targets = compute_branch_targets(callee_code, callee_len);
        // True after an instruction that does NOT fall through (goto/return/
        // athrow) so the merge-point check can distinguish a dead fall-through
        // (stale slots — safe to reset) from a live value-merge (must bail).
        let mut prev_was_terminator = false;

        while cpc < callee_len {
            callee_pc_to_native[cpc] = self.buf.pos() as i64; // Cast: address arithmetic
            let op = callee_code[cpc];

            // Merge-point handling: at a branch target the callee operand
            // stack must be the canonical empty state (caller_base_depth). A
            // live path arriving with a value is a value-producing merge the
            // spill-slot model can't represent soundly -> bail. A dead fall-
            // through (previous instr was a terminator) only left stale slots;
            // reset them so the target starts from the empty state every
            // branch into it also guarantees (branches require empty stack).
            if callee_branch_targets.get(cpc).copied().unwrap_or(false) {
                if !prev_was_terminator && self.stack.len() != caller_base_depth {
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
                self.stack.truncate(caller_base_depth);
                self.stack_oop_marks.truncate(caller_base_depth);
                self.next_spill_offset = save_spill;
            }

            match op {
                // nop
                0x00 => {
                    cpc += 1;
                }

                // aconst_null
                0x01 => {
                    self.emit_xor_reg_self(RAX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iconst_m1..iconst_5
                0x02..=0x08 => {
                    let val = (op as i32) - 3; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lconst_0, lconst_1
                0x09 | 0x0a => {
                    let val = (op as i64) - 9; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // fconst_0, fconst_1, fconst_2
                0x0b..=0x0d => {
                    let fval: f32 = (op - 0x0b) as f32; // Cast: JIT ABI convention
                    let bits = fval.to_bits() as i64; // Cast: JIT ABI convention
                    self.emit_mov_imm32_sx(RAX, bits as i32); // Cast: x86-64 immediate encoding
                    self.push_from_rax();
                    cpc += 1;
                }

                // dconst_0, dconst_1
                0x0e | 0x0f => {
                    let dval: f64 = (op - 0x0e) as f64; // Cast: JIT ABI convention
                    let bits = dval.to_bits() as i64; // Cast: JIT ABI convention
                    if bits == 0 {
                        self.emit_xor_reg_self(RAX);
                    } else {
                        self.emit_mov_imm64(RAX, bits);
                    }
                    self.push_from_rax();
                    cpc += 1;
                }

                // bipush
                0x10 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val = callee_code[cpc + 1] as i8 as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 2;
                }

                // sipush
                0x11 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let val =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    self.emit_mov_imm32_sx(RAX, val);
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc
                //
                // `site.ldc_info` / `site.ldc2w_info` are keyed by the
                // callee bytecode PC (see the inline resolver in
                // `try_jit_compile_callee`), NOT the CP index. Match on
                // `cpc` — the prior CP-index lookup silently missed and
                // pushed 0 for the constant.
                0x12 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) {
                        self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 2;
                }

                // ldc_w
                0x13 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if let Some((_, val)) = site.ldc_info.iter().find(|(p, _)| *p == cpc) {
                        self.emit_mov_imm32_sx(RAX, *val as i32); // Cast: x86-64 immediate encoding
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 3;
                }

                // ldc2_w
                0x14 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if let Some((_, val)) = site.ldc2w_info.iter().find(|(p, _)| *p == cpc) {
                        self.emit_mov_imm64(RAX, *val);
                    } else {
                        self.emit_xor_reg_self(RAX);
                    }
                    self.push_from_rax();
                    cpc += 3;
                }

                // iload, lload, fload, dload, aload
                0x15 | 0x16 | 0x17 | 0x18 | 0x19 => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 2;
                }

                // iload_0..iload_3
                0x1a..=0x1d => {
                    let idx = (op - 0x1a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lload_0..lload_3
                0x1e..=0x21 => {
                    let idx = (op - 0x1e) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // fload_0..fload_3
                0x22..=0x25 => {
                    let idx = (op - 0x22) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // dload_0..dload_3
                0x26..=0x29 => {
                    let idx = (op - 0x26) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // aload_0..aload_3
                0x2a..=0x2d => {
                    let idx = (op - 0x2a) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    self.push_from_rax();
                    cpc += 1;
                }

                // istore, lstore, fstore, dstore, astore
                0x36 | 0x37 | 0x38 | 0x39 | 0x3a => {
                    if cpc + 1 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 2;
                }

                // istore_0..istore_3
                0x3b..=0x3e => {
                    let idx = (op - 0x3b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // lstore_0..lstore_3
                0x3f..=0x42 => {
                    let idx = (op - 0x3f) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // fstore_0..fstore_3
                0x43..=0x46 => {
                    let idx = (op - 0x43) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // dstore_0..dstore_3
                0x47..=0x4a => {
                    let idx = (op - 0x47) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // astore_0..astore_3
                0x4b..=0x4e => {
                    let idx = (op - 0x4b) as usize; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.pop_to_rax();
                    self.emit_store_local(local_off, RAX);
                    cpc += 1;
                }

                // pop
                0x57 => {
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // pop2
                0x58 => {
                    let _ = self.pop_stack();
                    let _ = self.pop_stack();
                    cpc += 1;
                }

                // dup
                0x59 => {
                    let slot = self.pop_stack();
                    self.load_slot_to_reg(RAX, slot);
                    self.push_from_rax();
                    self.push_from_rax();
                    cpc += 1;
                }

                // swap
                0x5f => {
                    let a = self.pop_stack();
                    let b = self.pop_stack();
                    // Read BOTH operands before the first push: push_from_rax
                    // reuses the just-reclaimed lower spill slot, which is
                    // exactly `b`'s frame slot when both operands are
                    // frame-resident — storing into it before reading `b`
                    // duplicated value1 into both result slots.
                    self.load_slot_to_reg(RAX, a);
                    self.load_slot_to_reg(RCX, b);
                    self.push_from_rax();
                    self.emit_mov_reg_reg(RAX, RCX);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iadd
                0x60 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // ADD RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ladd
                0x61 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x01, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // isub
                0x64 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // SUB RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lsub
                0x65 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x29, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // imul
                0x68 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    // IMUL RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lmul
                0x69 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xAF, 0xC1]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // idiv — JVMS-compliant: guards divide-by-zero (→ deopt to
                // throw ArithmeticException) and INT_MIN / -1 (→ INT_MIN,
                // matches dividend) before issuing CDQ; IDIV ECX.
                0x6c => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ldiv — JVMS-compliant guards; emits CQO; IDIV RCX with the
                // LONG_MIN / -1 overflow special-case materialised inline.
                0x6d => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ false);
                    self.push_from_rax();
                    cpc += 1;
                }

                // irem — JVMS-compliant guards; INT_MIN % -1 yields 0.
                0x70 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ false, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lrem — JVMS-compliant guards; LONG_MIN % -1 yields 0.
                0x71 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    self.emit_safe_idiv(cpc, /*is_64bit*/ true, /*is_rem*/ true);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ineg
                0x74 => {
                    self.pop_to_rax();
                    // NEG EAX
                    self.buf.emit(&[0xF7, 0xD8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lneg
                0x75 => {
                    self.pop_to_rax();
                    // NEG RAX
                    self.rex_w();
                    self.buf.emit(&[0xF7, 0xD8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishl
                0x78 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL EAX, CL
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshl
                0x79 => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHL RAX, CL
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ishr
                0x7a => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SAR EAX, CL
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lshr
                0x7b => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xF8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iushr
                0x7c => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    // SHR EAX, CL
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lushr
                0x7d => {
                    let shift = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, shift);
                    self.rex_w();
                    self.buf.emit(&[0xD3, 0xE8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iand
                0x7e => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]); // AND RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // land
                0x7f => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x21, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ior
                0x80 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]); // OR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lor
                0x81 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x09, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ixor
                0x82 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]); // XOR RAX, RCX
                    self.push_from_rax();
                    cpc += 1;
                }

                // lxor
                0x83 => {
                    self.pop_to_rax();
                    let slot2 = self.pop_stack();
                    self.load_slot_to_reg(RCX, slot2);
                    self.rex_w();
                    self.buf.emit(&[0x31, 0xC8]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // iinc
                0x84 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let idx = callee_code[cpc + 1] as usize; // Widening: always safe
                    let inc = callee_code[cpc + 2] as i8 as i32; // Widening: always safe
                    let local_off = callee_local_base + (idx as i32) * 8; // Cast: x86-64 immediate encoding
                    self.emit_load_local(RAX, local_off);
                    // ADD RAX, imm32
                    self.rex_w();
                    self.buf.emit_byte(0x05);
                    self.buf.emit(&inc.to_le_bytes());
                    self.emit_store_local(local_off, RAX);
                    cpc += 3;
                }

                // i2l — identity in our i64 representation
                0x85 => {
                    cpc += 1;
                }

                // l2i — truncate to 32-bit, sign-extend back
                0x88 => {
                    self.pop_to_rax();
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2b
                0x91 => {
                    self.pop_to_rax();
                    // MOVSX RAX, AL
                    self.rex_w();
                    self.buf.emit(&[0x0F, 0xBE, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2c
                0x92 => {
                    self.pop_to_rax();
                    // MOVZX EAX, AX (zero-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xB7, 0xC0]);
                    // Upper 32 bits auto-zeroed
                    self.push_from_rax();
                    cpc += 1;
                }

                // i2s
                0x93 => {
                    self.pop_to_rax();
                    // MOVSX EAX, AX (sign-extend 16-bit)
                    self.buf.emit(&[0x0F, 0xBF, 0xC0]);
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // lcmp
                0x94 => {
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    self.load_slot_to_reg(RCX, top);
                    // CMP RAX, RCX
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]);
                    // SETG AL (1 if >)
                    self.buf.emit(&[0x0F, 0x9F, 0xC0]);
                    // MOVZX EAX, AL
                    self.buf.emit(&[0x0F, 0xB6, 0xC0]);
                    // SETL CL
                    self.buf.emit(&[0x0F, 0x9C, 0xC1]);
                    // MOVZX ECX, CL
                    self.buf.emit(&[0x0F, 0xB6, 0xC9]);
                    // SUB EAX, ECX
                    self.buf.emit(&[0x29, 0xC8]);
                    // MOVSXD RAX, EAX
                    self.rex_w();
                    self.buf.emit(&[0x63, 0xC0]);
                    self.push_from_rax();
                    cpc += 1;
                }

                // ifeq..ifle (0x99..0x9e) — conditional branch on <cmp> 0.
                //
                // Restored (419a6f5 blanket-bailed all branches). Inlining a
                // branchy callee is sound in this single-linear-pass emitter
                // ONLY when no operand is live across a merge: operand-stack
                // slots are handed out by a growing `next_spill_offset`, so two
                // paths reaching a merge with a value would hold it in
                // different frame slots (the `iconst_1; goto L; iconst_0; L:
                // ireturn` diamond — the bug 419a6f5 fixed). We therefore
                // require the callee operand stack to be EMPTY after the branch
                // pops its operands (here) AND at every target (the merge-point
                // reset at the loop top); any value-merge bails. Forward
                // branches only — backward edges (loops) re-enter an already-
                // emitted target whose slot layout we can't re-canonicalise.
                0x99..=0x9e => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit(&[0x85, 0xC0]); // TEST EAX, EAX
                    let cc = match op {
                        0x99 => 0x84u8, // JE
                        0x9a => 0x85,   // JNE
                        0x9b => 0x8C,   // JL
                        0x9c => 0x8D,   // JGE
                        0x9d => 0x8F,   // JG
                        0x9e => 0x8E,   // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // if_icmpeq..if_icmple (0x9f..0xa4) — int compare branch. Same
                // empty-stack-at-merge safety as 0x99..0x9e above.
                0x9f..=0xa4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.buf.emit(&[0x39, 0xC8]); // CMP EAX, ECX
                    let cc = match op {
                        0x9f => 0x84u8, // JE
                        0xa0 => 0x85,   // JNE
                        0xa1 => 0x8C,   // JL
                        0xa2 => 0x8D,   // JGE
                        0xa3 => 0x8F,   // JG
                        0xa4 => 0x8E,   // JLE
                        _ => unreachable!(),
                    };
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // goto (0xa7) — unconditional forward branch. Same safety.
                0xa7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.buf.emit_byte(0xE9); // JMP rel32
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ireturn, lreturn, areturn, freturn, dreturn
                0xac | 0xad | 0xb0 | 0xae | 0xaf => {
                    // `areturn` returns an object reference, and the popped
                    // entry's own mark says the same thing. The pop/push pair
                    // below moves the value to a new slot, and `push_from_rax`
                    // always marks its push `false` — so without carrying the
                    // mark across, inlining a reference-returning callee erases
                    // the oop tag of its result. Under moving-young that entry
                    // is then neither published nor rewritable.
                    let ret_is_oop =
                        op == 0xb0 || self.stack_oop_marks.last().copied().unwrap_or(false);
                    // Pop callee's return value → push onto caller stack
                    self.pop_to_rax();
                    // Reclaim callee locals
                    self.next_spill_offset = save_spill;
                    self.push_from_rax();
                    if ret_is_oop {
                        self.mark_top_as_oop();
                    }
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    // Use callee_len as the "after inline" target
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // return (void)
                0xb1 => {
                    self.next_spill_offset = save_spill;
                    // Jump past the rest of the inlined code
                    self.buf.emit_byte(0xE9);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, callee_len));
                    cpc += 1;
                }

                // getfield (0xb4) — use callee's field_info
                0xb4 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // `site.field_info` is keyed by the callee bytecode PC
                    // (`fpc` in `try_jit_compile_callee` / the inline
                    // resolver), NOT by the constant-pool index. Using the
                    // CP index here silently mismatched — a multi-field
                    // callee could pick another field op's `field_index`
                    // and read/write the wrong slot. Look up by `cpc`.
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        let obj_slot = self.pop_stack();
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.emit_call_absolute(self.helpers.getfield);
                        // The checked helper returns the `i64::MIN` deopt/NPE
                        // sentinel on a bad (stale/corrupt) receiver instead of a
                        // real field value — without this guard the sentinel is
                        // pushed and treated as legitimate data, turning what used
                        // to be an immediate SIGSEGV into silent corruption/hangs
                        // downstream. Mirrors the invoke-site guard below.
                        self.emit_post_invoke_exception_check(type_tag);
                        self.push_from_rax();
                        // Mirrors the top-level `getfield` arms and the inlined
                        // `getstatic` arm just below: a reference field's value
                        // is a live oop and must be tagged, or it is invisible
                        // to both the precise oop map and the shadow stack.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                    } else {
                        // Cannot resolve field — bail out
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putfield (0xb5) — use callee's field_info
                0xb5 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — see the `getfield`
                    // note above. Mismatching CP index vs PC here is the
                    // bug that dropped constructor field writes (boxed
                    // ints / `String.value` came back as 0).
                    if let Some((_, field_index, type_tag)) =
                        site.field_info.iter().find(|(p, _, _)| *p == cpc).copied()
                    {
                        let val_slot = self.pop_stack();
                        let obj_slot = self.pop_stack();
                        // Make a null receiver a real Java NPE before either
                        // the inline store or a legacy helper can turn it into
                        // a silent no-op. Protected sites retain their precise
                        // exceptional frame for javac monitor cleanup.
                        self.load_slot_to_reg(RAX, obj_slot);
                        self.emit_precise_null_check_field_store();
                        if type_tag == b'L' || type_tag == b'[' {
                            let compact_offset = site
                                .compact_field_info
                                .iter()
                                .find(|(p, _, is_ref)| *p == cpc && *is_ref)
                                .map(|(_, offset, _)| *offset);
                            let fresh_ctor_first_store =
                                inline_site_is_fresh_ctor_first_store(&site, cpc, field_index);
                            if inline_putfield_enabled()
                                && !narrow_oops_block_inline_fields()
                                && cratonvm_types::compact_ref_fields_enabled()
                                && self.helpers.region_bounds_addr != 0
                            {
                                if let Some(offset) = compact_offset {
                                    if fresh_ctor_first_store {
                                        self.emit_inline_fresh_ctor_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    } else {
                                        self.emit_inline_body_compact_ref_putfield(
                                            obj_slot,
                                            val_slot,
                                            field_index,
                                            offset,
                                        );
                                    }
                                } else {
                                    self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                    self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                    self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32);
                                    self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                    self.emit_call_absolute(self.helpers.putfield_object);
                                }
                            } else {
                                self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                                self.load_slot_to_reg(ARG_REGS[1], obj_slot);
                                self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                                self.load_slot_to_reg(ARG_REGS[3], val_slot);
                                self.emit_call_absolute(self.helpers.putfield_object);
                            }
                        } else {
                            self.load_slot_to_reg(ARG_REGS[0], obj_slot);
                            self.emit_mov_imm32_sx(ARG_REGS[1], field_index as i32); // Cast: x86-64 immediate encoding
                            self.load_slot_to_reg(ARG_REGS[2], val_slot);
                            let helper = match type_tag {
                                b'J' => self.helpers.putfield_long,
                                b'F' => self.helpers.putfield_float,
                                b'D' => self.helpers.putfield_double,
                                _ => self.helpers.putfield_int,
                            };
                            self.emit_call_absolute(helper);
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // getstatic (0xb2) — use callee's static_field_info
                //
                // MED-2 bail (round-2 JIT review): same gap as the top-level
                // 0xb2 handler at line ~9620 — see the long comment there
                // for the full unblocking plan. Briefly: `SharedVm.classes.statics`
                // slot addresses aren't stable (Vec resize, lazy entry),
                // so we can't bake them as `imm64` and emit `MOV reg,
                // [imm64]`. Stay on the helper-call path.
                0xb2 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // `static_field_info` is keyed by callee bytecode PC,
                    // not CP index — match on `cpc` (see the `getfield`
                    // note above).
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.emit_call_absolute(self.helpers.getstatic);
                        // jit-linewrapper-flushtype-npe fix (2026-07-17):
                        // see the matching fix + comment at the top-level
                        // 0xb2 arm -- same helper, same missing
                        // post-invoke exception check for a `<clinit>`
                        // failure surfaced via the deopt sentinel.
                        self.emit_post_invoke_exception_check(type_tag);
                        // Volatile static: emit MFENCE after read (SeqCst acquire)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.push_from_rax();
                        // T1.1.a (fix, 2026-07-07) — see the matching fix at
                        // the top-level 0xb2 arm: a reference-typed static
                        // field must not keep push_from_rax's default
                        // non-oop mark, or it decodes wrong in a precise
                        // GC/deopt oop map while live.
                        if type_tag == b'L' || type_tag == b'[' {
                            self.mark_top_as_oop();
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // putstatic (0xb3) — use callee's static_field_info
                //
                // MED-2 bail (round-2 JIT review): same gap as the top-level
                // 0xb3 handler — slot pointer not stable, no VM-crate access
                // from `jit`. See the comment on the top-level 0xb2 handler
                // for the full unblocking plan.
                0xb3 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.flush_scratch_registers();
                    // Keyed by callee bytecode PC — match on `cpc`.
                    if let Some((_, class_id_raw, field_index, type_tag, is_volatile)) = site
                        .static_field_info
                        .iter()
                        .find(|(p, _, _, _, _)| *p == cpc)
                        .copied()
                    {
                        let val_slot = self.pop_stack();
                        let helper_fn: usize = match type_tag {
                            b'J' => self.helpers.putstatic_long,
                            b'F' => self.helpers.putstatic_float,
                            b'D' => self.helpers.putstatic_double,
                            b'L' | b'[' => self.helpers.putstatic_object,
                            _ => self.helpers.putstatic_int,
                        };
                        // Volatile static: emit MFENCE before write (SeqCst release)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                        self.emit_load_local(ARG_REGS[0], self.heap_local_offset);
                        self.emit_mov_imm32_sx(ARG_REGS[1], class_id_raw as i32); // Cast: x86-64 immediate encoding
                        self.emit_mov_imm32_sx(ARG_REGS[2], field_index as i32); // Cast: x86-64 immediate encoding
                        self.load_slot_to_reg(ARG_REGS[3], val_slot);
                        self.emit_call_absolute(helper_fn);
                        // jit-putstatic-clinit-gap fix (2026-07-17): the
                        // helper now runs `<clinit>` on first touch before
                        // writing and, on failure, returns the `i64::MIN`
                        // deopt sentinel instead of `0` (mirrors
                        // `jit_getstatic`'s sentinel; see the fix comment on
                        // `jit_putstatic_class_init_guard` in
                        // `vm/src/jit/helpers.rs`). `putstatic` is
                        // void-returning, so route it through the shared
                        // void-helper exception-check convention (same one
                        // the `invokestatic` arraycopy dispatch call uses)
                        // rather than pushing a value.
                        self.emit_post_invoke_exception_check(b'V');
                        // Volatile static: emit MFENCE after write (SeqCst store-load barrier)
                        if is_volatile {
                            self.buf.emit(&[0x0F, 0xAE, 0xF0]); // MFENCE
                        }
                    } else {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    cpc += 3;
                }

                // if_acmpeq/if_acmpne (0xa5/0xa6) — reference compare branch.
                // Same empty-stack-at-merge safety as 0x99..0x9e.
                0xa5 | 0xa6 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let top = self.pop_stack();
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.load_slot_to_reg(RCX, top);
                    self.rex_w();
                    self.buf.emit(&[0x39, 0xC8]); // CMP RAX, RCX
                    let cc = if op == 0xa5 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // ifnull/ifnonnull (0xc6/0xc7) — null-compare branch. Same safety.
                0xc6 | 0xc7 => {
                    if cpc + 2 >= callee_len {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let offset =
                        i16::from_be_bytes([callee_code[cpc + 1], callee_code[cpc + 2]]) as i32; // Widening: always safe
                    let target = (cpc as i32 + offset) as usize; // Cast: x86-64 immediate encoding
                    if target <= cpc {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.pop_to_rax();
                    if self.stack.len() != caller_base_depth {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    self.rex_w();
                    self.buf.emit(&[0x85, 0xC0]); // TEST RAX, RAX
                    let cc = if op == 0xc6 { 0x84u8 } else { 0x85 }; // JE / JNE
                    self.buf.emit(&[0x0F, cc]);
                    let patch_off = self.buf.pos();
                    self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
                    branch_patches.push((patch_off, target));
                    cpc += 3;
                }

                // invokespecial (0xb7) — ONLY resolver-proven no-op super
                // constructor calls (`site.elided_invoke_pcs`): the target is
                // `java/lang/Object.<init>()V` (or an elidable trivial chain
                // to it), so the call has no observable effect. Pop the
                // receiver the preceding `aload_0` pushed — a compile-time
                // stack-model adjustment, no machine code — and continue.
                // This is what admits CONSTRUCTOR bodies to inlining. Any
                // other invokespecial bails to the dispatch fallback.
                0xb7 => {
                    if cpc + 2 >= callee_len || !site.elided_invoke_pcs.contains(&cpc) {
                        self.next_spill_offset = callee_local_base;
                        return false;
                    }
                    let _ = self.pop_stack();
                    cpc += 3;
                }

                // Unsupported opcode in inline context — bail out
                _ => {
                    // Restore spill offset and return false to fall back to a call
                    self.next_spill_offset = callee_local_base;
                    return false;
                }
            }

            // For the next iteration's merge-point check: goto (0xa7) and the
            // returns (0xac..=0xb1) jump away, so the following PC is reachable
            // only as a branch target (a dead fall-through whose stale slots
            // the merge-point reset may clear). Everything else falls through.
            prev_was_terminator = matches!(op, 0xa7 | 0xac..=0xb1);
        }

        // Mark the "after inline" position for return-jumps
        callee_pc_to_native[callee_len] = self.buf.pos() as i64; // Cast: address arithmetic

        // Patch all forward branches
        for (patch_off, target_cpc) in &branch_patches {
            let target_native = if *target_cpc < callee_pc_to_native.len() {
                callee_pc_to_native[*target_cpc]
            } else {
                self.buf.pos() as i64 // Cast: address arithmetic
            };
            if target_native < 0 {
                // Target not yet emitted (shouldn't happen for forward branches after full emission)
                // Fall back: point to current position
                let rel32 = (self.buf.pos() as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            } else {
                let rel32 = (target_native as i32) - (*patch_off as i32 + 4); // Cast: x86-64 rel32 displacement
                self.buf.try_patch_i32(*patch_off, rel32).ok(); // on Err try_patch_i32 set buf.overflowed; compile bails
            }
        }

        // Reclaim callee local spill slots. The `ireturn` handler pushed any
        // return value AT `save_spill` (it set next_spill=save_spill then
        // push_from_rax, leaving next_spill=save_spill+8). Resetting next_spill
        // back to `save_spill` here would FREE that return-value slot, so the
        // next push (e.g. a sibling call's argument) reused it and clobbered the
        // value — `leaf(a) + leafBig(a)` miscompiled because `leaf(a)`'s result
        // was overwritten by `iload a` for leafBig's argument. Keep next_spill
        // above the live operand-stack top so the return value is preserved.
        let next_spill = if self.stack.len() > caller_base_depth {
            // A return value occupies one slot at `save_spill`.
            let Some(end) = self.checked_spill_range_end(save_spill, 1) else {
                return false;
            };
            end
        } else {
            // Void callee: nothing pushed, callee operand stack fully reclaimed.
            save_spill
        };
        self.next_spill_offset = next_spill;

        true
    }

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
    // Inline array access emitters
    // -----------------------------------------------------------------------

    /// Inline int element load (compact: 4 bytes/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 64-bit).
    fn emit_int_aload_regs(&mut self) {
        // MOVSXD RAX, DWORD [RAX + RCX*4 + HEADER_SIZE]
        // Encoding: REX.W + 0x63 + ModRM(mod=01, reg=RAX, r/m=100) + SIB(scale=2, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x63);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element load (compact: 1 byte/element). RAX=array, RCX=index.
    /// Result in RAX (sign-extended to 32-bit, then to 64-bit).
    fn emit_byte_aload_regs(&mut self) {
        // MOVSX EAX, BYTE [RAX + RCX*1 + HEADER_SIZE]
        // Encoding: 0x0F 0xBE + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=0, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBE]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
                                               // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline int element store (compact: 4 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_int_astore_regs(&mut self) {
        // MOV DWORD [RAX + RCX*4 + HEADER_SIZE], EDX
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=EDX(010), r/m=SIB(100)
        self.buf.emit_byte(0x88); // SIB: scale=2(10=*4), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline byte element store (compact: 1 byte/element). RAX=array, RCX=index, RDX=value.
    fn emit_byte_astore_regs(&mut self) {
        // MOV BYTE [RAX + RCX*1 + HEADER_SIZE], DL
        self.buf.emit_byte(0x88);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DL(010), r/m=SIB(100)
        self.buf.emit_byte(0x08); // SIB: scale=0(00=*1), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // disp8 // Cast: x86-64 immediate encoding
    }

    /// Inline ref element load from Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index. Result in RAX (raw pointer, 0 for null).
    ///
    /// Emits: MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
    fn emit_ref_aload_regs(&mut self) {
        if narrow_oops_enabled() {
            self.emit_narrow_ref_aload_regs();
            return;
        }
        // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
        // REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B); // MOV r64, r/m64
        self.buf.emit_byte(0x44); // ModRM: mod=01(disp8), reg=000(RAX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Compressed-oops reference element load. RAX=array, RCX=index; result in
    /// RAX as a full 64-bit pointer, so every consumer downstream is unchanged.
    ///
    /// The element is a 4-byte `(addr - base) >> 3`, with 0 meaning null, so the
    /// decode is `base + (narrow << 3)` — except for null, which must stay 0
    /// rather than becoming `base`. `SHL` sets ZF from its result (the count is
    /// a non-zero literal), so the null test is free: branch over the rebase
    /// when the shifted value is zero.
    ///
    /// R11 is the scratch: it is neither an `ARG_REGS` nor a `SCRATCH_REGS`
    /// member, so the operand-stack register cache never parks a value there.
    fn emit_narrow_ref_aload_regs(&mut self) {
        // MOV EAX, DWORD [RAX + RCX*4 + HEADER_SIZE]   (32-bit dst zero-extends)
        self.buf.emit_byte(0x8B); // MOV r32, r/m32
        self.buf.emit_byte(0x44); // ModRM: mod=01(disp8), reg=000(EAX), r/m=100(SIB)
        self.buf.emit_byte(0x88); // SIB: scale=10(*4), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
        self.buf.emit(&[0x48, 0xC1, 0xE0, 0x03]); // SHL RAX, 3
        self.buf.emit(&[0x74, 0x0D]); // JZ +13 (past the rebase: null stays 0)
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().to_le_bytes()); // ... = heap base
        self.buf.emit(&[0x4C, 0x01, 0xD8]); // ADD RAX, R11
    }

    /// Inline ref element store to Object[] array (compact 8-byte pointers).
    /// RAX=array, RCX=index, RDX=value (raw pointer, 0 for null).
    ///
    /// Emits: MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
    ///
    /// Wired into the `aastore` opcode arm; the GC write-barrier is emitted
    /// separately as a call to `self.helpers.write_barrier` after the store.
    fn emit_ref_astore_regs(&mut self) {
        if narrow_oops_enabled() {
            self.emit_narrow_ref_astore_regs();
            return;
        }
        // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
        // REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89); // MOV r/m64, r64
        self.buf.emit_byte(0x54); // ModRM: mod=01(disp8), reg=010(RDX), r/m=100(SIB)
        self.buf.emit_byte(0xC8); // SIB: scale=11(*8), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Compressed-oops reference element store. RAX=array, RCX=index,
    /// RDX=value (raw 64-bit pointer, 0 for null).
    ///
    /// Encodes to `(addr - base) >> 3` in R11 and stores 4 bytes; a null value
    /// stores 0. **RDX is preserved** — the `aastore` arm hands it to the write
    /// barrier after this store — so the subtraction is done as
    /// `R11 = (-base) + RDX` rather than in place.
    fn emit_narrow_ref_astore_regs(&mut self) {
        self.buf.emit(&[0x4D, 0x31, 0xDB]); // XOR R11, R11 (the null encoding)
        self.buf.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        self.buf.emit(&[0x74, 0x11]); // JZ +17 (store the zero already in R11)
        self.buf.emit(&[0x49, 0xBB]); // MOV R11, imm64
        self.buf.emit(&narrow_base().wrapping_neg().to_le_bytes()); // ... = -base
        self.buf.emit(&[0x49, 0x01, 0xD3]); // ADD R11, RDX -> addr - base
        self.buf.emit(&[0x49, 0xC1, 0xEB, 0x03]); // SHR R11, 3
        self.buf.emit_byte(0x44); // MOV DWORD [..], R11D: REX.R (R11 as reg field)
        self.buf.emit_byte(0x89); // MOV r/m32, r32
        self.buf.emit_byte(0x5C); // ModRM: mod=01(disp8), reg=011(R11), r/m=100(SIB)
        self.buf.emit_byte(0x88); // SIB: scale=10(*4), index=001(RCX), base=000(RAX)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline short/char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// For saload: sign-extends to 32-bit then to 64-bit.
    fn emit_short_aload_regs(&mut self) {
        // MOVSX EAX, WORD [RAX + RCX*2 + HEADER_SIZE]
        // Encoding: 0x0F 0xBF + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xBF]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                                               // Sign-extend EAX to RAX
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]);
    }

    /// Inline char element load (compact: 2 bytes/element). RAX=array, RCX=index.
    /// Zero-extends to 32-bit then sign-extends to 64-bit.
    fn emit_char_aload_regs(&mut self) {
        // MOVZX EAX, WORD [RAX + RCX*2 + HEADER_SIZE]
        // Encoding: 0x0F 0xB7 + ModRM(mod=01, reg=EAX, r/m=SIB) + SIB(scale=1, idx=RCX, base=RAX) + disp8
        self.buf.emit(&[0x0F, 0xB7]);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=EAX(000), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
                                               // MOVZX already zero-extends to EAX, upper 32 bits of RAX auto-zeroed
    }

    /// Inline short/char element store (compact: 2 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_short_astore_regs(&mut self) {
        // MOV WORD [RAX + RCX*2 + HEADER_SIZE], DX
        // Encoding: 0x66 prefix + 0x89 + ModRM + SIB + disp8
        self.buf.emit_byte(0x66); // operand size prefix (16-bit)
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=DX(010), r/m=SIB(100)
        self.buf.emit_byte(0x48); // SIB: scale=1(01=*2), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element load (compact: 8 bytes/element). RAX=array, RCX=index.
    /// Result in RAX.
    fn emit_long_aload_regs(&mut self) {
        // MOV RAX, QWORD [RAX + RCX*8 + HEADER_SIZE]
        // Encoding: REX.W + 0x8B + ModRM(mod=01, reg=RAX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x8B);
        self.buf.emit_byte(0x44); // ModRM: mod=01, reg=RAX(000), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline long/double element store (compact: 8 bytes/element). RAX=array, RCX=index, RDX=value.
    fn emit_long_astore_regs(&mut self) {
        // MOV QWORD [RAX + RCX*8 + HEADER_SIZE], RDX
        // Encoding: REX.W + 0x89 + ModRM(mod=01, reg=RDX, r/m=SIB) + SIB(scale=3, idx=RCX, base=RAX) + disp8
        self.rex_w();
        self.buf.emit_byte(0x89);
        self.buf.emit_byte(0x54); // ModRM: mod=01, reg=RDX(010), r/m=SIB(100)
        self.buf.emit_byte(0xC8); // SIB: scale=3(11=*8), index=RCX(001), base=RAX(000)
        self.buf.emit_byte(HEADER_SIZE as u8); // Cast: x86-64 immediate encoding
    }

    /// Inline arraylength. Assumes RAX=array ptr. Result in RAX.
    fn emit_arraylength_regs(&mut self) {
        // Assumes RAX = array ptr. MOV EAX, DWORD [RAX + ARRAY_LENGTH_OFFSET]
        self.buf.emit(&[0x8B, 0x40, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding
    }

    /// Round-8 CRIT fix: emit an inline null check on the array receiver
    /// (assumed already in RAX) for an inline array-store opcode. On null,
    /// branches to the shared `null_check_store_stub` (emitted at method
    /// end by [`emit_null_check_store_stubs`]). On non-null, falls through
    /// to the caller's bounds check + inline store.
    ///
    /// Mirrors the structure of [`emit_bounds_check`]. Without this guard,
    /// the immediately-following `MOV R10D, [RAX + ARRAY_LENGTH_OFFSET]`
    /// in `emit_bounds_check` would dereference NULL and SIGSEGV — the
    /// signal handler at `vm/src/runtime/crash_handler.rs` only dumps an
    /// hs_err then re-raises, killing the VM instead of throwing NPE.
    /// The previous `process::abort()` in `vm/src/jit/helpers.rs`
    /// jit_iastore/bastore/aastore was a comment-level "fail loudly"
    /// theater because the helpers were never reached on the inline path.
    fn emit_null_check_array_store(&mut self, action: u8) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 → null-store stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs.push((action, patch_offset));
    }

    /// Null check for `putfield` inside a protected range whose handler needs
    /// a precise frame. Unlike the generic array stub this records the frame
    /// at the trapping bytecode, then routes the NPE through that frame so a
    /// javac monitor-cleanup handler retains its synthetic monitor local.
    fn emit_precise_null_check_field_store(&mut self) {
        let bci = self.dbg_last_pc;
        if !self.precise_exception_frames || !self.pc_is_protected(bci) {
            self.emit_null_check_array_store(npe_action::NONE);
            return;
        }
        if !self.exc_frame_box_ptr_by_bci.contains_key(&bci) {
            let box_ptr = self.build_and_record_deopt_point(
                bci,
                crate::deopt::DeoptReason::PendingException,
            );
            self.exc_frame_box_ptr_by_bci.insert(bci, box_ptr);
        }
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX,RAX
        self.buf.emit(&[0x0F, 0x84]); // JZ rel32 -> precise NPE stub
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        // Reason 10 is a locally-detected pending NPE. The common reason-9
        // path already has an exception from a helper; this one creates it in
        // the out-of-line frame-deopt stub before the router consumes it.
        self.deopt_stubs.push((patch_offset, bci, 10));
    }

    /// Round-11 HIGH-2: variant of `emit_null_check_array_store` that
    /// elides the inline TEST/JZ entirely when the null-check
    /// elimination dataflow proves the array receiver came from a
    /// local that is known non-null at this bytecode PC. The decision
    /// is made by walking the bytecode 1-4 bytes back from `bc_pc` to
    /// find the `aload N; <index>; <arraystore>` pattern via
    /// [`array_receiver_local`]; if found AND the local is proven
    /// non-null, the entire TEST/JZ pair is skipped (5 bytes saved
    /// per occurrence + branch-predictor pressure reduction).
    ///
    /// Safe to call instead of `emit_null_check_array_store` at every
    /// inline array-store site; the conservative path is identical.
    fn emit_null_check_array_store_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        // JEP 358: the trapping opcode IS at `code[bc_pc]` (the array-store
        // arm passes its own pc), so derive the per-element-type action.
        self.emit_null_check_array_store(array_opcode_npe_action(code, bc_pc));
    }

    /// Round-9 HIGH fix (asymmetric coverage): emit an inline null check
    /// on the array receiver (assumed already in RAX) for an inline
    /// array-LOAD opcode (iaload / aaload / baload / caload / saload /
    /// laload / faload / daload). The round-8 stub covered only stores;
    /// loads still relied on the page-fault path that
    /// `emit_null_check_array_store`'s doc rightly calls out as broken
    /// (the signal handler at `vm/src/runtime/crash_handler.rs` re-raises
    /// rather than throwing NPE, killing the VM).
    ///
    /// Loads have identical pre-state to stores (`RAX = array_ptr` at
    /// the bounds-check site) and the desired failure outcome is the
    /// same — set `JIT_PENDING_NPE`, deopt out with `RAX = i64::MIN`,
    /// run the epilogue. We therefore reuse the SAME shared stub by
    /// pushing the JZ patch offset into the same `null_check_store_stubs`
    /// vector; both loads and stores branch to it.
    fn emit_null_check_array_load(&mut self, action: u8) {
        // TEST RAX, RAX  (48 85 C0)
        self.buf.emit(&[0x48, 0x85, 0xC0]);
        // JZ rel32 → shared null-check stub (patched later)
        self.buf.emit(&[0x0F, 0x84]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.null_check_store_stubs.push((action, patch_offset));
    }

    /// Round-11 HIGH-2 (mirrors `emit_null_check_array_store_at`):
    /// elide the inline TEST/JZ null check on array loads when the
    /// receiver is proven non-null at `bc_pc` by the dataflow.
    fn emit_null_check_array_load_at(&mut self, code: &[u8], bc_pc: usize) {
        if let Some(local) = array_receiver_local(code, bc_pc) {
            if self.is_local_nonnull(bc_pc, local) {
                // peephole-null-elim: dataflow proves non-null; skip
                // the 8-byte TEST/JZ sequence entirely.
                return;
            }
        }
        // JEP 358: derive the per-element-type action from the trapping opcode.
        self.emit_null_check_array_load(array_opcode_npe_action(code, bc_pc));
    }

    /// Emit an inline null check on the `arraylength` receiver (assumed
    /// already in RAX). `arraylength` previously emitted a raw
    /// `MOV EAX, [RAX + ARRAY_LENGTH_OFFSET]` with no guard — a null
    /// receiver dereferenced low memory and SIGSEGV'd the VM (the crash
    /// handler re-raises rather than throwing NPE). This reuses the
    /// shared null-check stub (sets `JIT_PENDING_NPE`, deopts out) that
    /// array loads/stores already branch to.
    ///
    /// Unlike the load/store `_at` helpers, the dataflow elision keys on
    /// the directly-preceding `aload`/`aload_<n>` of the array receiver:
    /// for `arraylength` there is no index push between the `aload` and
    /// the opcode, so the load/store `array_receiver_local` parser (which
    /// expects an index push) would mis-decode the bytecode. We instead
    /// inspect the single instruction before `bc_pc`.
    fn emit_null_check_arraylength(&mut self, code: &[u8], bc_pc: usize) {
        if bc_pc >= 1 {
            let prev = code[bc_pc - 1];
            // aload_0..aload_3 (0x2A..0x2D) — single-byte, receiver local n.
            if (0x2A..=0x2D).contains(&prev) {
                // Widening: u8 -> usize (opcode-relative local index, value fits)
                let local = (prev - 0x2A) as usize;
                if self.is_local_nonnull(bc_pc, local) {
                    return;
                }
            } else if bc_pc >= 2 && code[bc_pc - 2] == 0x19 {
                // aload <u8> — two-byte, receiver local code[bc_pc-1].
                // Widening: u8 -> wider int (bytecode operand byte, value fits)
                let local = code[bc_pc - 1] as usize;
                if self.is_local_nonnull(bc_pc, local) {
                    return;
                }
            }
        }
        // Reuse the shared null-check stub machinery; the action is the
        // `arraylength` JEP-358 code ("Cannot read the array length").
        self.emit_null_check_array_load(npe_action::ARRAY_LENGTH);
    }

    /// Emit an array bounds check. RAX=array ptr, RCX=index (as i64).
    ///
    /// Loads array length from header offset 12, compares index (unsigned) against length.
    /// If index >= length (unsigned comparison catches negatives too), jumps to an
    /// out-of-line stub that calls `jit_throw_aioobe`.
    ///
    /// The stub is emitted later by `emit_bounds_check_stubs()` after the main code.
    fn emit_bounds_check(&mut self, bc_pc: usize) {
        // Skip if loop analysis proved this access is safe.
        //
        // SECURITY INVARIANT (V16, per-array 2026-07-11): `bounds_safe_pcs`
        // only contains a PC when `analyze_bounds_elimination` established,
        // for the enclosing counted loop (exclusive comparator, IV stepped
        // only +1, array/bound locals loop-invariant), ONE of:
        //   * STATIC proof: the bound provably IS this access's array's own
        //     length (`find_bound_arraylength_provenance` — a bound taken from
        //     a DIFFERENT array's length proves nothing for this one, see
        //     docs/known-issues/jit-bce-multi-array-oob-store-20260711.md) and
        //     the IV provably starts non-negative (`find_iv_nonneg_start`); or
        //   * SPECULATIVE guard: a `SpeculativeBCEGuard` for exactly this
        //     access's array, emitted at the loop header (`iv >= 0` and
        //     `array.length >= bound` or deopt). If that guard is later
        //     dropped by per-bci de-spec, the guard's `covered_pcs` are
        //     removed from `bounds_safe_pcs` so this check comes back.
        // Do not add a PC to `bounds_safe_pcs` from any path that does not
        // establish one of the two.
        if self.bounds_safe_pcs.contains(&bc_pc) {
            return;
        }

        // MOV R10D, DWORD [RAX + ARRAY_LENGTH_OFFSET]  — load array_length from ObjectHeader
        // Encoding: 44 8B 50 xx (REX.R + MOV r32, r/m32 + ModRM(01, R10, RAX) + disp8)
        self.buf
            .emit(&[0x44, 0x8B, 0x50, ARRAY_LENGTH_OFFSET as u8]); // Cast: x86-64 register encoding

        // CMP ECX, R10D  — unsigned compare index vs length
        // If index >= length (unsigned), JAE to failure stub
        // Encoding: 41 3B CA (REX.B + CMP r32, r/m32 + ModRM(11, ECX, R10))
        self.buf.emit(&[0x41, 0x3B, 0xCA]);

        // JAE rel32 — jump if above-or-equal (unsigned >= means out of bounds)
        // The rel32 will be patched to point to the out-of-line stub
        self.buf.emit(&[0x0F, 0x83]);
        let patch_offset = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]); // placeholder rel32
        self.bounds_check_stubs.push((patch_offset, bc_pc));
    }

    /// Emit the CRC-32 (reflected) inner fold of ONE byte for the
    /// `java.util.zip.CRC32` intrinsic — IEEE 802.3, which the hardware
    /// `CRC32` instruction (Castagnoli) cannot compute.
    ///
    /// Contract:
    ///   * `ECX` holds the running (uncomplemented) CRC state — read and
    ///     overwritten with the folded result.
    ///   * `EDX` holds the input byte; the caller MUST have zero-extended it
    ///     (a `MOVZX r32, m8`), so `EDX[31:8] == 0`. `EDX` is consumed.
    ///   * `EAX` is used as scratch and clobbered.
    ///
    /// No other register is touched — in particular the `update([BII)V`
    /// loop's index (`R9`), end (`R11`) and array base (`R8`) are preserved.
    /// No memory is accessed and no `CALL` is emitted (roadmap §8).
    ///
    /// The folded value is bit-identical to `native-builtins/src/zip_real.rs`
    /// `crc32_step` / the branchless reflected-CRC step: for each of the 8
    /// bits, `mask = -(crc & 1)` then `crc = (crc >> 1) ^ (poly & mask)`.
    /// `poly` is the reflected IEEE polynomial `0xEDB88320`.
    fn emit_crc32_ieee_fold_byte(&mut self, reversed_poly: u32) {
        // crc ^= byte:  XOR ECX, EDX  (31 D1).
        self.buf.emit(&[0x31, 0xD1]);
        // Eight identical reflected-CRC bit steps. Unrolled (a fixed count)
        // so the loop body has no branch and no loop counter register.
        for _ in 0..8 {
            // EAX = ECX:        MOV EAX, ECX        (89 C8)
            self.buf.emit(&[0x89, 0xC8]);
            // EAX &= 1:         AND EAX, 1          (83 E0 01)
            self.buf.emit(&[0x83, 0xE0, 0x01]);
            // EAX = -EAX:       NEG EAX             (F7 D8)
            //   → mask is 0xFFFFFFFF iff the low bit was set, else 0.
            self.buf.emit(&[0xF7, 0xD8]);
            // ECX >>= 1 (logical):  SHR ECX, 1      (D1 E9)
            self.buf.emit(&[0xD1, 0xE9]);
            // EAX &= reversed_poly:  AND EAX, imm32 (25 imm32)
            self.buf.emit_byte(0x25);
            self.buf.emit(&reversed_poly.to_le_bytes());
            // ECX ^= EAX:       XOR ECX, EAX        (31 C1)
            self.buf.emit(&[0x31, 0xC1]);
        }
    }

    /// Emit a JVMS-compliant signed integer division or remainder.
    ///
    /// Assumes the dividend is in RAX and the divisor in RCX. Leaves the
    /// result in RAX (sign-extended to 64 bits for the 32-bit forms so that
    /// the value is safe to push as a long-width stack slot).
    ///
    /// Guards required by JVMS §6.5.{idiv,irem,ldiv,lrem}:
    ///   * divisor == 0 → throw `ArithmeticException` (routed through the
    ///     uncommon-trap deopt stub with `DEOPT_REASON_DIV_BY_ZERO = 3`; the
    ///     interpreter materialises the exception from the i64::MIN sentinel).
    ///   * `INT_MIN / -1` (or `LONG_MIN / -1`) — the raw x86 IDIV faults with
    ///     #DE on this overflow. The Java spec says no exception is raised:
    ///     `idiv`/`ldiv` must return the dividend unchanged, and `irem`/`lrem`
    ///     must return 0. We special-case this with a CMP/CMP/branch pair and
    ///     synthesise the result without executing IDIV.
    ///
    /// `bci` is the bytecode pc used for the deopt-stub bookkeeping.
    fn emit_safe_idiv(&mut self, bci: usize, is_64bit: bool, is_rem: bool) {
        // -------- Guard 1: divide-by-zero --------
        if is_64bit {
            // TEST RCX, RCX  (48 85 C9)
            self.buf.emit(&[0x48, 0x85, 0xC9]);
        } else {
            // TEST ECX, ECX  (85 C9)
            self.buf.emit(&[0x85, 0xC9]);
        }
        // JZ rel32 → deopt stub (DEOPT_REASON_DIV_BY_ZERO = 3)
        self.buf.emit(&[0x0F, 0x84]);
        let dz_patch = self.buf.pos();
        self.buf.emit(&[0x00, 0x00, 0x00, 0x00]);
        self.deopt_stubs.push((dz_patch, bci, 3));

        // -------- Guard 2: INT_MIN / -1  (or LONG_MIN / -1) --------
        // If dividend == MIN and divisor == -1, IDIV would raise #DE.
        // Materialise the JVMS-mandated result and skip the IDIV.
        //
        //     CMP   dividend, MIN
        //     JNE   :do_div
        //     CMP   divisor, -1
        //     JNE   :do_div
        //     <materialise result>      ; idiv → dividend (RAX already holds MIN)
        //                               ; irem → 0
        //     JMP   :after_div
        //   :do_div
        //     CDQ / CQO
        //     IDIV  ECX / RCX
        //     <move result into RAX, sign-extending for 32-bit>
        //   :after_div

        // CMP dividend, MIN
        if is_64bit {
            // CMP RAX, imm32 sign-extended — we need full i64::MIN which doesn't
            // fit in imm32. Load i64::MIN into R10 and CMP RAX, R10.
            // MOV R10, i64::MIN  (49 BA <imm64>)
            self.buf.emit(&[0x49, 0xBA]);
            // Cast: non-negative value to u64
            self.buf.emit(&(i64::MIN as u64).to_le_bytes());
            // CMP RAX, R10  (4C 39 D0)
            self.buf.emit(&[0x4C, 0x39, 0xD0]);
        } else {
            // CMP EAX, imm32  (3D <imm32>)
            self.buf.emit_byte(0x3D);
            // Cast: bytecode/native offset to u32 (non-negative, fits)
            self.buf.emit(&(i32::MIN as u32).to_le_bytes());
        }
        // JNE rel8 → :do_div (we'll patch after we know the size)
        self.buf.emit(&[0x75, 0x00]); // placeholder rel8
        let jne1_patch = self.buf.pos() - 1;

        // CMP divisor, -1
        if is_64bit {
            // CMP RCX, -1  (48 83 F9 FF)  — imm8 sign-extended to 64
            self.buf.emit(&[0x48, 0x83, 0xF9, 0xFF]);
        } else {
            // CMP ECX, -1  (83 F9 FF)     — imm8 sign-extended to 32
            self.buf.emit(&[0x83, 0xF9, 0xFF]);
        }
        // JNE rel8 → :do_div
        self.buf.emit(&[0x75, 0x00]);
        let jne2_patch = self.buf.pos() - 1;

        // Materialise the overflow result.
        if is_rem {
            // result = 0
            if is_64bit {
                // XOR EAX, EAX (zeros full RAX)
                self.buf.emit(&[0x31, 0xC0]);
            } else {
                self.buf.emit(&[0x31, 0xC0]);
            }
        } else {
            // result = dividend (RAX/EAX already holds MIN). For 32-bit, ensure
            // RAX is sign-extended like the IDIV path does.
            if !is_64bit {
                // MOVSXD RAX, EAX  (48 63 C0)
                self.buf.emit(&[0x48, 0x63, 0xC0]);
            }
        }
        // JMP rel8 → :after_div
        self.buf.emit(&[0xEB, 0x00]);
        let jmp_after_patch = self.buf.pos() - 1;

        // :do_div — patch JNE targets to here
        let do_div_off = self.buf.pos();
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel1 = (do_div_off as i64) - (jne1_patch as i64 + 1);
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel2 = (do_div_off as i64) - (jne2_patch as i64 + 1);
        // A rel8 displacement that does not fit in an i8 would silently
        // miscompile in release builds. `emit_safe_idiv` cannot signal a failure
        // (it returns `()`), so honor the no-panic contract: mark the buffer
        // overflowed (the driver's `if buf.overflowed() { return None; }`
        // discards the half-emitted method and falls back to the interpreter)
        // instead of asserting. The intervening block is fixed-size and small,
        // so this can only fire on a genuine codegen bug.
        if !(-128..=127).contains(&rel1) || !(-128..=127).contains(&rel2) {
            self.buf.mark_overflowed();
            return;
        }
        // Truncation: i64 -> u8 (short (rel8) branch displacement, range-checked)
        self.buf.try_patch_byte(jne1_patch, rel1 as u8).ok(); // on Err try_patch_byte set buf.overflowed; compile bails
                                                              // Truncation: i64 -> u8 (short (rel8) branch displacement, range-checked)
        self.buf.try_patch_byte(jne2_patch, rel2 as u8).ok(); // on Err try_patch_byte set buf.overflowed; compile bails

        // Sign-extend RAX → RDX:RAX (or EAX → EDX:EAX), then IDIV.
        if is_64bit {
            // CQO  (48 99)
            self.buf.emit(&[0x48, 0x99]);
            // IDIV RCX  (48 F7 F9)
            self.buf.emit(&[0x48, 0xF7, 0xF9]);
        } else {
            // CDQ  (99)
            self.buf.emit_byte(0x99);
            // IDIV ECX  (F7 F9)
            self.buf.emit(&[0xF7, 0xF9]);
        }

        // Move the result (quotient in RAX/EAX, remainder in RDX/EDX) into RAX,
        // sign-extending 32-bit results so callers can treat RAX as i64.
        if is_rem {
            if is_64bit {
                // MOV RAX, RDX  (48 89 D0)
                self.buf.emit(&[0x48, 0x89, 0xD0]);
            } else {
                // MOVSXD RAX, EDX  (48 63 C2)
                self.buf.emit(&[0x48, 0x63, 0xC2]);
            }
        } else if !is_64bit {
            // MOVSXD RAX, EAX  (48 63 C0)
            self.buf.emit(&[0x48, 0x63, 0xC0]);
        }

        // :after_div — patch the JMP from the overflow path.
        let after_off = self.buf.pos();
        // Widening: usize/u32 offset -> i64 (no truncation; for rel/displacement math)
        let rel_jmp = (after_off as i64) - (jmp_after_patch as i64 + 1);
        // No-panic bail (see JNE patch checks above): a rel8 that does not fit in
        // an i8 would silently miscompile, so mark the buffer overflowed and let
        // the driver discard the method instead of asserting.
        if !(-128..=127).contains(&rel_jmp) {
            self.buf.mark_overflowed();
            return;
        }
        // Truncation: i64 -> u8 (short (rel8) branch displacement, range-checked)
        self.buf.try_patch_byte(jmp_after_patch, rel_jmp as u8).ok(); // on Err try_patch_byte set buf.overflowed; compile bails
    }


    /// Emit an `ldc <Class>` site: call `helpers.ldc_class_cp` and push the
    /// returned mirror as an oop. Returns `false` when this pc is not a
    /// class-`ldc` **or** the site cannot be served (the helper is unwired, or
    /// this artifact has no VM context to pass it), leaving the caller to fall
    /// through to the immediate/string arms or refuse the method.
    ///
    /// The mirror is re-fetched on every execution rather than baked, exactly
    /// as `helpers.ldc_string` re-interns its String: both are heap objects a
    /// relocating collector may move between two runs of this body.
    fn emit_ldc_class(&mut self, pc: usize) -> bool {
        let Some(&idx) = self.ldc_class_info_idx.get(&pc) else {
            return false;
        };
        if self.helpers.ldc_class_cp == 0 || !self.needs_heap {
            return false;
        }
        let (_, holder_class_id, cp_idx) = self.ldc_class_info[idx];
        self.emit_pre_safepoint_spill();
        crate::runtime_lowering::emit_ldc_class_cp_stub(
            &mut self.buf,
            self.heap_local_offset,
            self.helpers.ldc_class_cp,
            holder_class_id,
            cp_idx,
            self.helpers.frame_record,
        );
        // Resolution can load a class — arbitrary Java, hence a GC point — so
        // this is a real safepoint, and its `0` return is a published pending
        // exception (`NoClassDefFoundError` and friends), not a value.
        self.emit_oop_map_for_safepoint();
        self.emit_post_alloc_oom_check();
        self.push_from_rax();
        self.mark_top_as_oop();
        true
    }


    // -----------------------------------------------------------------------
    // SSE float/double helpers
    // -----------------------------------------------------------------------

    /// SSE float binary op: pop two f32 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from fload of XMM locals
    /// or prior float arithmetic), avoids the GPR→XMM round-trip. Mirrors
    /// emit_double_binop's XMM chaining for float values.
    fn emit_float_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        // See emit_double_binop: relocate any live XMM0 operand still on the
        // remaining stack before this op clobbers XMM0/XMM1 as scratch.
        self.flush_xmm0_slots();

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                                                  // F3 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF3, 0x0F, sse_op, 0xC1]);
        // MOVD EAX, XMM0
        self.buf.emit(&[0x66, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// SSE double binary op: pop two f64 values, apply SSE scalar op, push result.
    /// `sse_op`: 0x58=ADD, 0x59=MUL, 0x5C=SUB, 0x5E=DIV
    ///
    /// Optimized: if operands are already in XMM registers (from dload of XMM locals),
    /// avoids the GPR→XMM round-trip. When slot1 is Xmm(0) and slot2 is a high XMM
    /// (8-15), emits the SSE op directly against that register, skipping XMM1 entirely.
    fn emit_double_binop(&mut self, sse_op: u8) {
        let slot2 = self.pop_stack(); // value2 (top)
        let slot1 = self.pop_stack(); // value1 (deeper)

        // This op clobbers XMM0 (and XMM1) as scratch. A value still live DEEPER
        // on the operand stack that is parked in XMM0 (the deferred-FP cache, e.g.
        // a prior call result or `push_from_rax_as_xmm0`) would be destroyed by the
        // `load slot1 -> XMM0` below before it is ever consumed. Relocate any such
        // live XMM0 operand to a scratch XMM / frame first. slot1/slot2 are already
        // popped, so this only touches the *remaining* stack. (emit_fcmp already
        // does this; emit_double/float_binop did not — that gap silently corrupted
        // `f(x) + f(g(x))`-shaped code and the whole commons-math FastMath.sin family
        // under JIT, where a call result sat in XMM0 across the next arg's FP math.)
        self.flush_xmm0_slots();

        self.load_slot_to_reg(RCX, slot2);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
        self.load_slot_to_reg(RAX, slot1);
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                                                        // F2 0F <sse_op> C1 — XMM0 = XMM0 op XMM1
        self.buf.emit(&[0xF2, 0x0F, sse_op, 0xC1]);
        // MOVQ RAX, XMM0
        self.buf.emit(&[0x66, 0x48, 0x0F, 0x7E, 0xC0]);
        self.push_from_rax();
    }

    /// Float/double compare: pop two values, produce -1/0/1.
    /// `is_double`: true for dcmp*, false for fcmp*.
    /// `nan_positive`: true for *cmpg (NaN→1), false for *cmpl (NaN→-1).
    fn emit_fcmp(&mut self, is_double: bool, nan_positive: bool) {
        let slot2 = self.pop_stack();
        let slot1 = self.pop_stack();
        self.flush_xmm0_slots();

        // Load slot2 into XMM1 (or use directly for UCOMISD XMM0, XMMn)
        let cmp_xmm2: u8; // register holding value2 for the UCOMI instruction
        match slot2 {
            StackSlot::Xmm(xmm) if xmm >= 2 => {
                // Can use directly in UCOMISD/UCOMISS
                cmp_xmm2 = xmm;
            }
            StackSlot::Xmm(xmm) => {
                if xmm != 1 {
                    if is_double {
                        self.emit_movsd_xmm_xmm(1, xmm);
                    } else {
                        self.emit_movss_xmm_xmm(1, xmm);
                    }
                }
                cmp_xmm2 = 1;
            }
            _ => {
                self.load_slot_to_reg(RCX, slot2);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC9]); // MOVQ XMM1, RCX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC9]); // MOVD XMM1, ECX
                }
                cmp_xmm2 = 1;
            }
        }

        // Load slot1 into XMM0
        match slot1 {
            StackSlot::Xmm(xmm) if xmm != 0 => {
                if is_double {
                    self.emit_movsd_xmm_xmm(0, xmm);
                } else {
                    self.emit_movss_xmm_xmm(0, xmm);
                }
            }
            StackSlot::Xmm(0) => {} // already in XMM0
            _ => {
                self.load_slot_to_reg(RAX, slot1);
                if is_double {
                    self.buf.emit(&[0x66, 0x48, 0x0F, 0x6E, 0xC0]); // MOVQ XMM0, RAX
                } else {
                    self.buf.emit(&[0x66, 0x0F, 0x6E, 0xC0]); // MOVD XMM0, EAX
                }
            }
        }

        // UCOMISD/UCOMISS XMM0, XMMn
        if is_double {
            // 66 [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x66, 0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x66, 0x0F, 0x2E, modrm]);
            }
        } else {
            // [REX.B] 0F 2E modrm
            let modrm = 0xC0 | (cmp_xmm2 & 7);
            if cmp_xmm2 >= 8 {
                self.buf.emit(&[0x41, 0x0F, 0x2E, modrm]);
            } else {
                self.buf.emit(&[0x0F, 0x2E, modrm]);
            }
        }

        if nan_positive {
            // *cmpg: NaN → 1
            // Extract all flags before any ALU ops (which clobber CF/ZF/PF)
            // SETA AL (above = value1 > value2)
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // SETB CL (below = value1 < value2 or NaN)
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // SETP DL (parity = NaN)
            self.buf.emit(&[0x0F, 0x9A, 0xC2]);
            // Now safe to use ALU ops
            // OR AL, DL — positive = above OR NaN
            self.buf.emit(&[0x08, 0xD0]); // OR AL, DL
                                          // XOR DL, 1 — !NaN
            self.buf.emit(&[0x80, 0xF2, 0x01]);
            // AND CL, DL — below AND !NaN
            self.buf.emit(&[0x20, 0xD1]); // AND CL, DL
                                          // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        } else {
            // *cmpl: NaN → -1 (SETB naturally includes NaN)
            // SETA AL
            self.buf.emit(&[0x0F, 0x97, 0xC0]);
            // MOVZX EAX, AL
            self.buf.emit(&[0x0F, 0xB6, 0xC0]);
            // SETB CL
            self.buf.emit(&[0x0F, 0x92, 0xC1]);
            // MOVZX ECX, CL
            self.buf.emit(&[0x0F, 0xB6, 0xC9]);
            // SUB EAX, ECX
            self.buf.emit(&[0x29, 0xC8]);
        }

        // Sign-extend EAX to RAX (for -1)
        self.rex_w();
        self.buf.emit(&[0x63, 0xC0]); // movsxd rax, eax
        self.push_from_rax();
    }

    // -----------------------------------------------------------------------
    // Bytecode compilation
    // -----------------------------------------------------------------------
    //
    // `compile_bytecode` lives in `x64/bytecode_walk.rs`. It is still an
    // inherent method on this same `Compiler`, declared `pub(super)` there so
    // `compile_with_param_slots` below can call it; `Compiler` is private to
    // this module, so that is not reachable from outside the backend.


    /// Patch all forward branches and jump-table entries.
    ///
    /// Returns `false` when any recorded branch/table target has no native
    /// offset (`pc_to_native[target_pc] < 0` or out of range). Every target
    /// the scan pass collects is revived and emitted by the dead-code walk,
    /// so an unresolved target means the bytecode branches to a PC that is
    /// not an instruction boundary (e.g. into the middle of a `goto`'s
    /// operand bytes) — malformed bytecode that a classfile verifier would
    /// reject, but which CratonVM can still meet via unverified/synthetic
    /// code. Previously such patches were silently SKIPPED, leaving the
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
// Public compilation entry point
// ---------------------------------------------------------------------------

thread_local! {
    /// Compact reference-field layout: per-pc `(byte_offset, is_ref)` for the
    /// next `compile()` call, set by the interpreter's execute / OSR compile
    /// paths (which use the `compile` wrapper, not `compile_with_param_slots`
    /// directly) so their getfield/putfield get inline compact codegen. Taken
    /// (cleared) by the wrapper. Empty for every other caller (tests, AOT) →
    /// legacy/helper field path. Same-thread, synchronous compile, no nesting.
    static PENDING_COMPACT_FIELD_INFO: std::cell::RefCell<Vec<(usize, u32, bool)>> =
        const { std::cell::RefCell::new(Vec::new()) };
    static PENDING_VERIFIED_MAX_STACK: std::cell::RefCell<Option<usize>> =
        const { std::cell::RefCell::new(None) };
    /// `(start_pc, end_pc, handler_pc)` of this method's exception table,
    /// staged for the next `compile_with_param_slots` on this thread and
    /// consumed (taken) at its entry, so a compile that bails out cannot leak
    /// them into the next method compiled on this worker. Empty for every
    /// caller that does not stage them (tests, AOT, the legacy `compile`
    /// wrapper, OSR artifacts) and for every handler-free method — byte
    /// identical codegen there. See `find_bypassable_loop_headers`.
    static PENDING_EXCEPTION_RANGES: std::cell::RefCell<Vec<(usize, usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Stage the compact-field-info for the next [`compile`] call on this thread.
/// Call immediately before `compile`; the wrapper takes (clears) it.
pub fn set_pending_compact_field_info(info: Vec<(usize, u32, bool)>) {
    PENDING_COMPACT_FIELD_INFO.with(|c| *c.borrow_mut() = info);
}

/// Stage the reader/verifier max_stack for the next x64 compile on this thread.
/// Synthetic callers that do not stage it keep using the local estimator.
pub(crate) fn set_pending_verified_max_stack(max_stack: usize) {
    PENDING_VERIFIED_MAX_STACK.with(|c| *c.borrow_mut() = Some(max_stack));
}

/// Stage this method's exception table as `(start_pc, end_pc, handler_pc)` for
/// the next x64 compile on this thread. Call immediately before
/// `compile_with_param_slots`; it takes (clears) them. Consumed only by
/// `find_bypassable_loop_headers`, to treat a handler that can be entered from
/// outside a loop as an external entry into that loop's header.
pub(crate) fn set_pending_exception_ranges(ranges: Vec<(usize, usize, usize)>) {
    PENDING_EXCEPTION_RANGES.with(|c| *c.borrow_mut() = ranges);
}

/// Compile a JVM bytecode method to x86-64 machine code.
///
/// When `needs_heap` is true, the compiled code expects a heap pointer as the
/// hidden first C argument, and Java parameters follow. This enables
/// JIT-compiled array allocation and element access via helper call-outs.
///
/// Legacy entry point: assumes `arg index == JVM slot`, which is correct only
/// for methods whose parameters are all category-1 (no long/double). Test call
/// sites use this; the production path (`jit/src/lib.rs::try_compile`) calls
/// [`compile_with_param_slots`] with the real parameter layout so long/double
/// parameters land in the slots their body reads.
///
/// Returns `Some(CompiledMethod)` on success, `None` if compilation fails.
#[allow(clippy::too_many_arguments)]
pub fn compile(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    multianewarray_info: Vec<(usize, u8)>,
    field_info: Vec<(usize, usize, u8)>,
    typecheck_info: Vec<(usize, *const u8, usize)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    anewarray_info: Vec<(usize, u32)>,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    direct_calls: Vec<(usize, super::JitDirectCall)>,
    mic_slots: Vec<(usize, *const super::JitMICSlot)>,
    pic_slots: Vec<(usize, *const super::JitPICSlot)>,
    ldc_info: Vec<(usize, i64)>,
    ldc2w_info: Vec<(usize, i64)>,
    branch_hints: HashMap<usize, bool>,
    loop_unroll_hints: HashMap<usize, usize>,
    helpers: &JitRuntimeHelpers,
    non_escaping_new: std::collections::HashSet<usize>,
    inline_sites: HashMap<usize, crate::InlineSite>,
    string_layout: Option<crate::StringFieldLayout>,
) -> Option<CompiledMethod> {
    compile_with_param_slots(
        code,
        code_len,
        num_params,
        max_locals,
        needs_heap,
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        new_info,
        // Deferred (not-yet-loaded) `new`/`anewarray` sites: the legacy/test
        // wrapper has no constant pool to defer against, so never any.
        Vec::new(),
        anewarray_info,
        Vec::new(),
        invoke_info,
        direct_calls,
        mic_slots,
        pic_slots,
        ldc_info,
        // ldc_string_info / ldc_class_info: the legacy/test wrapper has no
        // constant pool to resolve either against, so never any.
        Vec::new(),
        Vec::new(),
        ldc2w_info,
        branch_hints,
        loop_unroll_hints,
        helpers,
        non_escaping_new,
        inline_sites,
        string_layout,
        &[],
        0,
        0, // param_oop_mask: legacy/test path seeds no oop params (conservative)
        // Compact field info staged by the caller (interpreter execute/OSR);
        // empty for tests/AOT → legacy/helper field path.
        PENDING_COMPACT_FIELD_INFO.with(|c| std::mem::take(&mut *c.borrow_mut())),
        "",         // method_key: legacy/test wrapper disables the per-bci de-spec consult
        Vec::new(), // indy_info: legacy/test wrapper passes no invokedynamic sites
    )
}

/// Compile a method to native code with an explicit parameter→JVM-slot map.
///
/// `param_jvm_slots[i]` is the JVM local slot of the i-th incoming JIT
/// argument (`this` first for instance methods, then declared params), and
/// `param_slot_span` is the total JVM slots the parameters occupy (category-2
/// counted as 2). These let the prologue place long/double parameters in the
/// slots the body actually reads. Pass `&[]` / `0` for the legacy
/// "arg index == slot" behavior (see the [`compile`] wrapper).
#[allow(clippy::too_many_arguments)]
/// Prove that every hot recursive edge in this body is GC-inert.
///
/// A raw `invokestatic` (no invoke/direct-call metadata) is the x64 backend's
/// representation of a self call; `try_compile` only leaves a site raw after
/// resolving it to the current method. The strict opcode whitelist excludes
/// allocation, arbitrary helpers, monitors, exception creation, arrays, and
/// loops. Forward branches and resolved inline `getfield` are harmless.
fn gc_inert_selfrec_candidate(
    code: &[u8],
    code_len: usize,
    field_info: &[(usize, usize, u8)],
    new_info: &[(usize, u32, usize, bool, bool)],
    new_deferred_info: &[(usize, u32, u16)],
    anewarray_info: &[(usize, u32)],
    anewarray_deferred_info: &[(usize, u32, u16)],
    invoke_info: &[(usize, *const JitInvokeInfo)],
    direct_calls: &[(usize, super::JitDirectCall)],
    mic_slots: &[(usize, *const super::JitMICSlot)],
    pic_slots: &[(usize, *const super::JitPICSlot)],
    indy_info: &[(usize, usize, u8, Vec<u8>, usize)],
) -> bool {
    if !gc_inert_selfrec_enabled()
        || !new_info.is_empty()
        // A deferred `new`/`anewarray` allocates too — the opcode whitelist
        // below already excludes 0xbb/0xbd, but keep the metadata gate
        // symmetric with the resolved lists so a future whitelist change
        // cannot silently admit an allocating body here.
        || !new_deferred_info.is_empty()
        || !anewarray_info.is_empty()
        || !anewarray_deferred_info.is_empty()
        || !invoke_info.is_empty()
        || !direct_calls.is_empty()
        || !mic_slots.is_empty()
        || !pic_slots.is_empty()
        || !indy_info.is_empty()
    {
        return false;
    }

    let mut pc = 0usize;
    let mut self_calls = 0usize;
    let mut saw_return = false;
    while pc < code_len {
        let op = code[pc];
        let allowed = match op {
            0x00..=0x11
            | 0x15..=0x2d
            | 0x36..=0x4e
            | 0x57..=0x6b
            | 0x74..=0x98 => true,
            // Conditional branches and forward goto only. A backward edge
            // would need cooperative polling and is therefore not GC-inert.
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                if pc + 2 >= code_len {
                    return false;
                }
                let rel = i16::from_be_bytes([code[pc + 1], code[pc + 2]]) as isize;
                rel > 0 && pc.checked_add_signed(rel).is_some_and(|t| t < code_len)
            }
            0xac..=0xb1 => {
                saw_return = true;
                true
            }
            0xb4 => field_info.iter().any(|(field_pc, _, _)| *field_pc == pc),
            0xb8 => {
                self_calls += 1;
                true
            }
            _ => false,
        };
        if !allowed {
            return false;
        }
        let len = bytecode_len_at(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return false;
        }
        pc += len;
    }
    pc == code_len && self_calls != 0 && saw_return
}

thread_local! {
    /// Opt-in for the bytecode loop rewriter, per compiler thread.
    ///
    /// **Off by default, process-wide.** Nothing in the VM arms it; the only
    /// way in is [`set_bytecode_loop_rewriter_armed`]. That makes the default
    /// compile path byte-identical to before this wiring landed (one
    /// thread-local `Cell<bool>` load per compile), and it keeps the opt-in
    /// out of the declared-flag table, which lives in a crate this backend
    /// does not own.
    ///
    /// Thread-local rather than a process-wide `AtomicBool` for two reasons:
    /// the JIT compiles on worker threads, so arming is a per-worker decision
    /// a validation harness can make one thread at a time; and the unit tests
    /// in this file run concurrently in one process, where a global switch
    /// would leak one test's transform into another's compile.
    static BYTECODE_LOOP_REWRITER_ARMED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

/// Arm (or disarm) the bytecode loop rewriter for every subsequent compile on
/// THIS thread, returning the previous setting.
///
/// See [`bytecode_loop_xform_rewrites_bytecode`] for what it switches on and
/// `docs/jit/loop-rewriter-wiring.md` for what is and is not validated.
/// Callers that arm it must disarm it again (the tests below use a guard), or
/// every later compile on the same thread inherits it.
pub fn set_bytecode_loop_rewriter_armed(on: bool) -> bool {
    BYTECODE_LOOP_REWRITER_ARMED.with(|c| c.replace(on))
}

/// Does the bytecode-level loop transform (`plan_loop_peel` /
/// `plan_loop_unroll` in `x64::licm`) rewrite the bytecode the emitter
/// compiles?
///
/// `false` unless [`set_bytecode_loop_rewriter_armed`] armed this thread.
///
/// When it is `false`, `compile_with_param_slots` compiles the caller's
/// original bytecode verbatim, so every pc the emitter handles is an
/// interpreter bci, every caller-supplied side table (`field_info`,
/// `invoke_info`, `mic_slots`, `inline_sites`, …) is still keyed correctly,
/// and every deopt bci, oop-map `bytecode_pc`, `pc_to_native` index and
/// `osr_entry_native` index is already in interpreter-bci space with nothing
/// to translate.
///
/// When it is `true`, `compile_with_param_slots` asks
/// `plan_bytecode_loop_xform` for a transform. If it gets one it compiles the
/// REWRITTEN bytes, replicates every pc-keyed side table into output-PC space
/// through `LoopXform::replicate_pc_keyed`, and translates the three
/// bci-valued immediates the emitter bakes into machine code back through
/// `Compiler::orig_bci`. If it does not get one — the common case, because
/// the admission test is deliberately narrow — the compile is byte-identical
/// to the unarmed one except that the native unroller is off.
///
/// This is deliberately the same switch that turns the native byte-copy
/// unroller off (see `native_unroller_enabled`): the two must never both fire
/// on one loop, or `k+1` bytecode copies get machine-code-duplicated `k+1`
/// more times behind one back edge, giving `(k+1)^2` bodies per poll — a
/// time-to-safepoint neither unroller's budget check ever saw.
fn bytecode_loop_xform_rewrites_bytecode() -> bool {
    BYTECODE_LOOP_REWRITER_ARMED.with(|c| c.get())
}

/// Is the native byte-copy unroller (the `0xa7` arm of `compile_bytecode`)
/// the current owner of loop unrolling?
///
/// Mutually exclusive with `bytecode_loop_xform_rewrites_bytecode` by
/// construction — see that function for why the exclusion is a correctness
/// requirement and not a tidiness one.
fn native_unroller_enabled() -> bool {
    !bytecode_loop_xform_rewrites_bytecode()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_UNROLL").is_none()
}

/// Structural admission test for the native byte-copy loop unroller.
///
/// The unroller duplicates emitted MACHINE CODE between `pc_to_native[header]`
/// and the back edge, shifting eight patch vectors, re-resolving helper
/// `rel32`s and minting fresh IC slots per copy. Whether that is legal is a
/// question about the loop's CONTROL FLOW, and the test that used to guard it
/// — `code[back_edge] == 0xa7` plus a body-byte-size band — asked no
/// control-flow question at all. In particular it never established that:
///
/// * **the loop is reducible** (the header dominates its own region). In an
///   irreducible loop "the bytes from the header to the back edge" are not one
///   iteration of anything, so duplicating them duplicates the wrong region.
/// * **the region has a single entry.** A branch from outside that lands
///   *below* the header enters copy 0 mid-body; the `k` copies that follow it
///   are then whole extra bodies the original would not have run before its
///   next exit test.
/// * **no cycle strictly inside the body is irreducible.** The duplicator
///   resolves an internal forward patch to `pc_to_native[target] + shift`,
///   which is only the copy's own image of the target when the inner cycle is
///   reducible and wholly contained in the body span.
/// * **nothing branches to the back-edge instruction itself.** That
///   instruction exists in the last copy only, so such an edge would target
///   code that is no longer where the branch thinks it is.
/// * **no exception handler lands inside the duplicated region, and no
///   protected range only partially overlaps it.** The bytecode→native handler
///   ranges are derived from `pc_to_native`, which covers copy 0 only, so a
///   throw from copy `1..k` is not covered by the range that protects the
///   loop — the exception escapes a `try` that lexically encloses it.
/// * **the header is not in `bypassable_headers`.** Every other speculating
///   transform in this pipeline — the aaload LICM hoists, the arith LICM
///   hoists, the FP hoists, matrix-dot, the bulk-byte loops — filters on that
///   set, and the unroller sits on the same pre-header: `pc_to_native[header]`
///   points PAST it, so `body_start` excludes it and each copy runs without
///   it, while an external edge into the header bypasses it entirely.
/// * **the poll-free span stays bounded.** `k+1` bodies now sit behind one
///   back-edge poll.
///
/// Rather than re-derive those facts here, this asks the bytecode-level
/// rewriter [`plan_loop_unroll`], whose admission test is exactly that list,
/// proved with real dominators over an instruction-granularity CFG
/// (`MethodCfg`) instead of the "any backward branch is a loop" heuristic used
/// elsewhere in this backend, and re-checked on its own output. The rewritten
/// bytes are discarded — only the verdict is used, so the emitter still sees
/// the caller's bytecode and nothing needs re-keying. See
/// `docs/jit/loop-transforms.md`.
///
/// Fail-closed: every refusal, including a malformed-input `BadShape`, means
/// the loop is compiled without unrolling.
fn plan_native_unroll(
    code: &[u8],
    code_len: usize,
    header: usize,
    back_edge: usize,
    extra_copies: usize,
    exception_ranges: &[(usize, usize, usize)],
    bypassable_headers: &FxHashSet<usize>,
) -> Option<(usize, usize, usize)> {
    let dbg = cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some();
    if bypassable_headers.contains(&header) {
        if dbg {
            eprintln!(
                "[JIT_GEN] unroll refused: header={header} back_edge={back_edge} \
                 reason=BypassableHeader"
            );
        }
        return None;
    }
    match plan_loop_unroll(
        code,
        code_len,
        header,
        back_edge,
        extra_copies,
        exception_ranges,
    ) {
        Ok(_) => Some((header, back_edge, extra_copies)),
        Err(refusal) => {
            if dbg {
                eprintln!(
                    "[JIT_GEN] unroll refused: header={header} back_edge={back_edge} \
                     copies={extra_copies} reason={refusal:?}"
                );
            }
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Bytecode loop rewriter: planning and side-table replication
// ---------------------------------------------------------------------------

/// Why `compile_with_param_slots` did not compile rewritten bytecode.
///
/// Every variant means "compile the caller's original bytecode", which is the
/// pre-existing behaviour and always correct — the transform is an
/// optimisation, so refusing it costs speed and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopRewriteRefusal {
    /// The default. This thread never called
    /// [`set_bytecode_loop_rewriter_armed`].
    NotArmed,
    /// `CRATONVM_DEOPT_REAL`. The precise-deopt snapshots record
    /// `DeoptimizationPoint::bci`, which the VM *resumes at*, and they are
    /// built deep inside the emitter from the emitter's own pc. Translating
    /// them is a separate piece of work; until it lands, refuse.
    DeoptRealEnabled,
    /// Precise exceptional frames. Same reason: `emit_post_invoke_exception_check`
    /// and `emit_precise_null_check_field_store` record a resume-bearing
    /// snapshot keyed on the emitter pc.
    PreciseExceptionFrames,
    /// The method has an `invokedynamic`. Its lowering is an unconditional
    /// trap that records an `UnreachedCode` snapshot through
    /// `emit_osr_exit_map_at_reason` — and unlike the two above, that path is
    /// NOT gated on `deopt_real_enabled()`, so it would fire in production.
    InvokedynamicPresent,
    /// The method has inline sites. An inlined callee contributes its own bci
    /// space (`docs/jit/deopt-inline-scopes.md`) that this wiring's
    /// caller-only provenance map does not describe.
    InlineSitesPresent,
    /// No loop in the method passed the profitability band and the structural
    /// admission test.
    NoCandidateLoop,
    /// The planner produced a transform whose provenance is not total. Cannot
    /// happen (`rewrite_loop_copies` builds `bci_of` byte by byte and checks
    /// its length), and is re-checked here because `Compiler::orig_bci`'s
    /// soundness rests on it.
    ProvenanceNotTotal,
    /// The rewriter itself refused the last candidate loop.
    Planner(LoopXformRefusal),
}

/// The properties of a pending compile that decide whether the bytecode loop
/// rewriter may run at all, independent of any particular loop.
pub(crate) struct LoopRewriteShape {
    pub(crate) deopt_real: bool,
    pub(crate) precise_exception_frames: bool,
    pub(crate) has_indy: bool,
    pub(crate) has_inline_sites: bool,
}

/// Choose ONE loop to unroll at the bytecode level and rewrite the method.
///
/// The profitability band is deliberately character-for-character the native
/// unroller's (see the `unroll_loops` construction in
/// `compile_with_param_slots`), so arming the rewriter changes *which
/// machinery* unrolls a loop, not *which loops* are considered. The
/// legality question is `plan_loop_unroll`'s, exactly as it already is for
/// the native unroller via `plan_native_unroll`.
///
/// Exactly one loop, because a `LoopXform` describes one rewrite: composing
/// two would mean composing their provenance maps, which the primitives in
/// `x64::licm` do not do. The first admitted loop in `detect_loops` order
/// wins, and that order is innermost-first for a nest, which is the
/// profitable choice. Every other loop in the method is left alone — the
/// rewriter shifts it correctly, it just does not duplicate it.
fn plan_bytecode_loop_xform(
    code: &[u8],
    code_len: usize,
    exception_ranges: &[(usize, usize, usize)],
    loop_unroll_hints: &HashMap<usize, usize>,
    shape: LoopRewriteShape,
) -> Result<LoopXform, LoopRewriteRefusal> {
    use LoopRewriteRefusal as R;

    if !bytecode_loop_xform_rewrites_bytecode() {
        return Err(R::NotArmed);
    }
    // Whole-compile refusals, cheapest first. Each names a construct that
    // publishes an emitter pc to the VM as a resume bci through a path this
    // wiring does not translate; see the variant docs.
    if shape.deopt_real {
        return Err(R::DeoptRealEnabled);
    }
    if shape.precise_exception_frames {
        return Err(R::PreciseExceptionFrames);
    }
    if shape.has_indy {
        return Err(R::InvokedynamicPresent);
    }
    if shape.has_inline_sites {
        return Err(R::InlineSitesPresent);
    }

    let loops = detect_loops(code, code_len);
    let bypassable = find_bypassable_loop_headers(code, code_len, &loops, exception_ranges);
    let mut last_refusal: Option<LoopXformRefusal> = None;
    for &(header, back_edge) in &loops {
        if back_edge >= code_len || code[back_edge] != 0xa7 {
            continue;
        }
        // Same pre-header placement contract the native unroller, the LICM
        // hoists, matrix-dot and the bulk-byte loops all apply.
        if bypassable.contains(&header) {
            continue;
        }
        let body_size = back_edge - header;
        if body_size < 5 {
            continue;
        }
        // PGO path: a profiled trip count extends eligibility to larger
        // bodies. A refusal here does NOT fall through to the static
        // heuristic — same as the native unroller's `return`.
        if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
            if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                return match plan_loop_unroll(
                    code,
                    code_len,
                    header,
                    back_edge,
                    pgo_factor.saturating_sub(1),
                    exception_ranges,
                ) {
                    Ok(x) if !x.provenance_is_total() => Err(R::ProvenanceNotTotal),
                    Ok(x) => Ok(x),
                    Err(e) => Err(R::Planner(e)),
                };
            }
        }
        let extra_copies = if body_size <= 20 {
            3 // 4x unroll
        } else if body_size <= 50 {
            1 // 2x unroll
        } else {
            continue;
        };
        match plan_loop_unroll(
            code,
            code_len,
            header,
            back_edge,
            extra_copies,
            exception_ranges,
        ) {
            Ok(x) if !x.provenance_is_total() => return Err(R::ProvenanceNotTotal),
            Ok(x) => return Ok(x),
            Err(e) => last_refusal = Some(e),
        }
    }
    Err(last_refusal.map(R::Planner).unwrap_or(R::NoCandidateLoop))
}

/// [`LoopXform::replicate_pc_keyed`] for a table whose entry is a 3-tuple
/// `(pc, a, b)`.
///
/// The primitive is defined over `(usize, T)`; these two adapters pack the
/// payload into a tuple and unpack it again so that EVERY pc-keyed table goes
/// through the one replication function. That is the point: replicating some
/// tables and not others compiles fine and silently produces a loop copy
/// missing a field resolution or an inline cache.
fn replicate_pc3<A: Clone, B: Clone>(x: &LoopXform, t: Vec<(usize, A, B)>) -> Vec<(usize, A, B)> {
    let packed: Vec<(usize, (A, B))> = t.into_iter().map(|(pc, a, b)| (pc, (a, b))).collect();
    x.replicate_pc_keyed(&packed)
        .into_iter()
        .map(|(pc, (a, b))| (pc, a, b))
        .collect()
}

/// [`replicate_pc3`] for a 5-tuple `(pc, a, b, c, d)`.
fn replicate_pc5<A: Clone, B: Clone, C: Clone, D: Clone>(
    x: &LoopXform,
    t: Vec<(usize, A, B, C, D)>,
) -> Vec<(usize, A, B, C, D)> {
    let packed: Vec<(usize, (A, B, C, D))> = t
        .into_iter()
        .map(|(pc, a, b, c, d)| (pc, (a, b, c, d)))
        .collect();
    x.replicate_pc_keyed(&packed)
        .into_iter()
        .map(|(pc, (a, b, c, d))| (pc, a, b, c, d))
        .collect()
}

#[allow(clippy::too_many_arguments)]
pub fn compile_with_param_slots(
    code: &[u8],
    code_len: usize,
    num_params: usize,
    max_locals: usize,
    needs_heap: bool,
    multianewarray_info: Vec<(usize, u8)>,
    field_info: Vec<(usize, usize, u8)>,
    typecheck_info: Vec<(usize, *const u8, usize)>,
    static_field_info: Vec<(usize, u32, usize, u8, bool)>,
    // CRIT-2 — see `new_info` field doc on the compiler struct.
    new_info: Vec<(usize, u32, usize, bool, bool)>,
    // Cold-`new` fix — see `new_deferred_info` on the compiler struct. Sites
    // whose target class was not loaded at compile time; served by the
    // CP-indexed `new_object_cp` helper. Disjoint from `new_info`.
    new_deferred_info: Vec<(usize, u32, u16)>,
    anewarray_info: Vec<(usize, u32)>,
    // `anewarray` sibling of `new_deferred_info`.
    anewarray_deferred_info: Vec<(usize, u32, u16)>,
    invoke_info: Vec<(usize, *const JitInvokeInfo)>,
    direct_calls: Vec<(usize, super::JitDirectCall)>,
    mic_slots: Vec<(usize, *const super::JitMICSlot)>,
    // HIGH-7 — Inline 4-way PIC slots passed alongside MIC slots.
    //
    // Each entry is `(bytecode_pc, &JitPICSlot as *const _)`. When a
    // PIC slot is present at a given pc, the codegen in
    // `Compiler::compile_op_invokevirtual` emits the 4-way inline
    // cascade in place of the MIC probe (PIC supersedes MIC — it is
    // a 4-entry superset). The slot itself is allocated and owned by
    // the caller (`jit/src/lib.rs::try_compile`); it must outlive the
    // compiled method, which is ensured by attaching the boxed slot
    // to `CompiledMethod._jit_pic_slots`.
    //
    // Callers that don't yet allocate PIC slots (e.g. legacy test
    // call sites that build short bytecode snippets) pass
    // `Vec::new()` and the cascade is simply not emitted at any pc.
    pic_slots: Vec<(usize, *const super::JitPICSlot)>,
    ldc_info: Vec<(usize, i64)>,
    ldc_string_info: Vec<(usize, *const u8, usize)>,
    // Class-`ldc` sites — see `ldc_class_info` on the compiler struct. Served
    // by the CP-indexed `ldc_class_cp` helper; disjoint from `ldc_info` and
    // `ldc_string_info`.
    ldc_class_info: Vec<(usize, u32, u16)>,
    ldc2w_info: Vec<(usize, i64)>,
    branch_hints: HashMap<usize, bool>,
    loop_unroll_hints: HashMap<usize, usize>,
    helpers: &JitRuntimeHelpers,
    non_escaping_new: std::collections::HashSet<usize>,
    inline_sites: HashMap<usize, crate::InlineSite>,
    // Compile-time resolved `java/lang/String` field layout for the String
    // call-site intrinsics (length/charAt/hashCode/…). `None` means "String
    // layout unavailable" — String-intrinsic codegen (added by a later
    // wave) treats it as a bail-to-dispatch. See `crate::StringFieldLayout`.
    string_layout: Option<crate::StringFieldLayout>,
    param_jvm_slots: &[usize],
    param_slot_span: usize,
    // Stage A.4 (precise oop maps) — bitmask of JVM local slots holding a
    // reference parameter on entry (bit `k` ⇒ slot `k` is an oop). Seeds the
    // "must be oop" local dataflow so oop params live at an early safepoint are
    // precisely covered. `0` on the default path → byte-identical codegen.
    param_oop_mask: u64,
    // Compact reference-field layout: per-getfield/putfield `(pc, byte_offset,
    // is_ref)` so the codegen can emit an inline compact field access (no helper
    // call, no runtime layout lookup). Empty when the flag is off → the inline
    // emitters fall back to the legacy 16-byte cell / helper path.
    compact_field_info: Vec<(usize, u32, bool)>,
    // deopt-osr Step 9 follow-up (c) — this method's
    // `"<class>.<method>:<descriptor>"` key, used to consult the per-bci de-spec
    // registry (`crate::deopt::despec_contains`) and suppress a loop-header
    // speculative-BCE guard that has repeatedly deopted. `""` (the legacy/test
    // `compile()` wrapper) disables the consult; the registry is empty in
    // production, so a non-empty key is still byte-identical there.
    method_key: &str,
    // Resolved `invokedynamic` (0xba) call-site info — see the `indy_info`
    // field doc on the `Compiler` struct. Empty from the legacy `compile()`
    // test wrapper (which also passes no `indy_ops` to `jit_scan` callers, so
    // this is always consistent with an invokedynamic-free method there).
    indy_info: Vec<(usize, usize, u8, Vec<u8>, usize)>,
) -> Option<CompiledMethod> {
    // A class-`ldc` calls a helper that takes the VM context as its first
    // argument, exactly like a string-`ldc`, so it forces the context form of
    // the artifact too.
    let needs_heap = needs_heap || !ldc_string_info.is_empty() || !ldc_class_info.is_empty();
    let gc_inert_selfrec = gc_inert_selfrec_candidate(
        code,
        code_len,
        &field_info,
        &new_info,
        &new_deferred_info,
        &anewarray_info,
        &anewarray_deferred_info,
        &invoke_info,
        &direct_calls,
        &mic_slots,
        &pic_slots,
        &indy_info,
    );
    let verified_max_stack = PENDING_VERIFIED_MAX_STACK.with(|c| c.borrow_mut().take());
    // One-shot like every other staged request below: take it here so an early
    // bail cannot leak this method's handler ranges into an unrelated later
    // compile on this worker thread.
    let exception_ranges: Vec<(usize, usize, usize)> =
        PENDING_EXCEPTION_RANGES.with(|c| std::mem::take(&mut *c.borrow_mut()));
    // Consume the pure-kernel GPR local-homes request FIRST so an early bail
    // below can never leak it into an unrelated later compile on this thread.
    let kernel_reg_homes_requested = KERNEL_REG_HOMES_REQUEST.with(|c| c.take());
    // A handler-local request is one-shot too, so a compile bailout cannot
    // accidentally arm the next unrelated method on this worker thread.
    let precise_exception_frames = PRECISE_EXCEPTION_FRAME_REQUEST.with(|c| c.take());
    // Same one-shot discipline as the flag above.
    let protected_ranges = PROTECTED_RANGES_REQUEST
        .with(|c| c.take())
        .unwrap_or_default();
    // OSR-tier request (perf/halfgap-20260717): same purity conditions below,
    // but the published artifact KEEPS its OSR entries — the trampoline's
    // register-seeded entry contract is exactly what the assignments
    // describe. See `set_kernel_reg_homes_osr_request`.
    let kernel_reg_homes_osr_requested =
        KERNEL_REG_HOMES_OSR_REQUEST.with(|c| c.take()) && kernel_reg_osr_enabled();

    // ── Bytecode loop rewriter (opt-in; see `set_bytecode_loop_rewriter_armed`)
    //
    // This is the whole interception. Below this point `code`/`code_len` are
    // the REWRITTEN method and every pc-keyed side table has been lifted into
    // its PC space, so the ~40 analyses and the emitter are unmodified: they
    // simply see a different method. Three things make that sound, and all
    // three are here rather than scattered:
    //
    //   (i)   the rewrite itself, refused for anything it cannot describe;
    //   (ii)  ONE replication of ALL pc-keyed tables, in one expression, so a
    //         table cannot be forgotten silently;
    //   (iii) the coordinate change back to interpreter-bci space, which is
    //         `Compiler::orig_bci` at the three sites that BAKE a bci into
    //         machine code plus the `osr_pc_to_native` / `osr_dead_mask`
    //         rebuild at the end of this function.
    //
    // `None` on every unarmed compile, at the cost of one thread-local
    // `Cell<bool>` load, and the whole path below is then the identity.
    let loop_xform: Option<LoopXform> = match plan_bytecode_loop_xform(
        code,
        code_len,
        &exception_ranges,
        &loop_unroll_hints,
        LoopRewriteShape {
            deopt_real: crate::deopt_real_enabled(),
            precise_exception_frames,
            has_indy: !indy_info.is_empty(),
            has_inline_sites: !inline_sites.is_empty(),
        },
    ) {
        Ok(x) => {
            if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some() {
                eprintln!(
                    "[JIT_GEN] bytecode loop rewrite: header={} body_len={} copies={} \
                     code_len {}->{} poll_free={}",
                    x.header, x.body_len, x.copies, code_len, x.code_len, x.poll_free_bytes
                );
            }
            Some(x)
        }
        Err(refusal) => {
            if !matches!(refusal, LoopRewriteRefusal::NotArmed)
                && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some()
            {
                eprintln!("[JIT_GEN] bytecode loop rewrite refused: {refusal:?}");
            }
            None
        }
    };
    // Kept for the coordinate change at the end of this function; `code_len`
    // is about to become the rewritten length.
    let orig_code_len = code_len;
    // A rewritten method has a backward branch, and `gc_inert_selfrec_candidate`
    // (computed above, on the ORIGINAL bytes and tables) rejects every backward
    // branch — so this conjunction is already true. Written out anyway so the
    // one predicate computed before the rewrite cannot silently start
    // describing a method that no longer exists.
    let gc_inert_selfrec = gc_inert_selfrec && loop_xform.is_none();
    let (code, code_len): (&[u8], usize) = match &loop_xform {
        Some(x) => (&x.code[..], x.code_len),
        None => (code, code_len),
    };
    // Handler ranges in output coordinates. A range enclosing the loop is
    // WIDENED over the copies by the rewriter, which is what keeps a `try`
    // that lexically encloses the loop covering every copy.
    let exception_ranges: Vec<(usize, usize, usize)> = match &loop_xform {
        Some(x) => x.exception_ranges.clone(),
        None => exception_ranges,
    };
    // The per-bci de-spec registry (`crate::deopt::despec_contains`) is keyed
    // by INTERPRETER bci, but every loop header below is an output pc. Consult
    // it through the provenance map. Identity when unarmed.
    let despec_bci = |pc: usize| -> u32 {
        // Cast: bci fits u32 (checked against u32::MAX by the rewriter)
        loop_xform.as_ref().and_then(|x| x.bci_at(pc)).unwrap_or(pc) as u32
    };

    // ── Side-table replication — ATOMIC BY CONSTRUCTION ────────────────
    //
    // All 21 pc-keyed tables `compile_with_param_slots` receives, rebound in
    // ONE expression. Adding a 22nd parameter and forgetting it here is a
    // compile error at the destructuring, not a silent miscompile: a copy that
    // lost its `field_info` entry takes the helper-call fallback instead of the
    // inline access and nothing fails.
    //
    // Payloads are CLONED, so the pointer-carrying tables (`typecheck_info`,
    // `invoke_info`, `mic_slots`, `pic_slots`, `ldc_string_info`) share one
    // target across the copies. For the read-only ones (a class name, a
    // resolved-invoke descriptor, an interned string) that is trivially sound.
    // For the two MUTABLE ones (`mic_slots`, `pic_slots`) it is sound because
    // an inline cache keyed on a call site sees the same receiver distribution
    // in every copy — exactly what happens today when a non-unrolled loop runs
    // many times. It is also what makes them safe to share at all: the caller
    // (`jit/src/lib.rs::try_compile`) owns and outlives these slots, and
    // minting fresh ones per copy would need code outside this crate's file.
    let (
        multianewarray_info, field_info, typecheck_info, static_field_info,
        new_info, new_deferred_info, anewarray_info, anewarray_deferred_info,
        invoke_info, direct_calls, mic_slots, pic_slots,
        ldc_info, ldc_string_info, ldc_class_info, ldc2w_info, branch_hints,
        loop_unroll_hints, non_escaping_new, inline_sites, compact_field_info,
        indy_info,
    ) = match &loop_xform {
        None => (
            multianewarray_info, field_info, typecheck_info, static_field_info,
            new_info, new_deferred_info, anewarray_info, anewarray_deferred_info,
            invoke_info, direct_calls, mic_slots, pic_slots,
            ldc_info, ldc_string_info, ldc_class_info, ldc2w_info, branch_hints,
            loop_unroll_hints, non_escaping_new, inline_sites, compact_field_info,
            indy_info,
        ),
        Some(x) => (
            x.replicate_pc_keyed(&multianewarray_info),
            replicate_pc3(x, field_info),
            replicate_pc3(x, typecheck_info),
            replicate_pc5(x, static_field_info),
            replicate_pc5(x, new_info),
            replicate_pc3(x, new_deferred_info),
            x.replicate_pc_keyed(&anewarray_info),
            replicate_pc3(x, anewarray_deferred_info),
            x.replicate_pc_keyed(&invoke_info),
            // `JitDirectCall` is not `Clone` (it lives in `jit/src/lib.rs`,
            // which this agent does not own), so its payload is packed into a
            // tuple of `Copy` fields, replicated by the same primitive, and
            // rebuilt. Adding `#[derive(Clone)]` there would let this use
            // `replicate_pc_keyed` directly.
            {
                let packed: Vec<(usize, (usize, bool, usize, u8, u32))> = direct_calls
                    .into_iter()
                    .map(|(pc, d)| {
                        (
                            pc,
                            (
                                d.entry,
                                d.needs_context,
                                d.num_params,
                                d.return_type,
                                d.guard_class_id,
                            ),
                        )
                    })
                    .collect();
                x.replicate_pc_keyed(&packed)
                    .into_iter()
                    .map(
                        |(pc, (entry, needs_context, num_params, return_type, guard_class_id))| {
                            (
                                pc,
                                super::JitDirectCall {
                                    entry,
                                    needs_context,
                                    num_params,
                                    return_type,
                                    guard_class_id,
                                },
                            )
                        },
                    )
                    .collect::<Vec<(usize, super::JitDirectCall)>>()
            },
            x.replicate_pc_keyed(&mic_slots),
            x.replicate_pc_keyed(&pic_slots),
            x.replicate_pc_keyed(&ldc_info),
            replicate_pc3(x, ldc_string_info),
            replicate_pc3(x, ldc_class_info),
            x.replicate_pc_keyed(&ldc2w_info),
            x.replicate_pc_keyed(&branch_hints.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, bool>>(),
            // Keyed by BACK-EDGE pc. Under `Unroll` an original back-edge bci
            // has exactly one image (the last copy carries the only back edge),
            // so this stays single-valued.
            x.replicate_pc_keyed(&loop_unroll_hints.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, usize>>(),
            x.replicate_pc_keyed(
                &non_escaping_new
                    .into_iter()
                    .map(|pc| (pc, ()))
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|(pc, ())| pc)
            .collect::<std::collections::HashSet<usize>>(),
            // Always empty here — `InlineSitesPresent` refuses the transform —
            // but routed through the same primitive so the census has no
            // "handled elsewhere" entry.
            x.replicate_pc_keyed(&inline_sites.into_iter().collect::<Vec<_>>())
                .into_iter()
                .collect::<HashMap<usize, crate::InlineSite>>(),
            replicate_pc3(x, compact_field_info),
            // Always empty here — `InvokedynamicPresent` refuses the transform.
            replicate_pc5(x, indy_info),
        ),
    };

    // Estimate buffer size. A bytecode invoke is not the old ~40-byte helper
    // call: the current lowering can emit a context bridge, exception/deopt
    // edge, and MIC/PIC dispatch machinery. Hibernate's concurrent query path
    // demonstrated that the former 96-byte invoke allowance repeatedly
    // exhausted otherwise modest 10 KiB buffers, leaving hot methods in the
    // interpreter. Keep enough headroom for those sites; the code-cache cap
    // remains the global bound on retained executable memory.
    //
    // 512 -> 1024 (2026-08-01). Widening the PIC's inter-slot branch from
    // `rel8` to `rel32` grew every inline-cache site, and 512 stopped covering
    // them. Measured on one Spring Boot suite class: TEN methods overflowed per
    // run, and in every one of them `inline_extra` was 0 and the whole shortfall
    // sat in this term. Solving each for the per-invoke cost the body actually
    // needed — `(wanted - code_len * 96 - 8192) / invokes`, which OVER-attributes
    // (the `code_len * 96` term also pays for the invoke bytecodes) — gives:
    //
    //     MapperListener.containerEvent           68 invokes   957 B/invoke
    //     AbstractBeanDefinition.<init>           83 invokes   824
    //     OnBeanCondition.getMatchingBeans        34 invokes   728
    //     ObjectCreateRule.begin                  22 invokes   674
    //     ResolvableType.getNested                 6 invokes   642
    //     ClassFileAnnotationMetadata.resolveTypeName 7 invokes 640
    //     StringUtils.collectionToDelimitedString 16 invokes   578
    //     AbstractAutowireCapableBeanFactory.populateBean 34   547
    //     DateTimeFormatterBuilder$NumberPrinterParser.format 36 531
    //     jdk.internal.classfile.impl.ClassImpl.forEach 21     515
    //
    // 1024 covers the worst of them with margin. Unlike the optimizing tier —
    // which now measures the shortfall and re-runs the lowering at that size
    // (`ir_lower::lower_inner`) — this backend cannot retry: it consumes six
    // one-shot thread-local staging requests before the buffer is allocated,
    // and re-entering it would find them gone. The estimate has to be right the
    // first time here, so it errs high.
    let inline_extra: usize = inline_sites
        .values()
        .map(|s| s.callee_code_len.saturating_mul(64))
        .sum();
    let estimated_size = code_len
        .saturating_mul(96)
        .saturating_add(8192)
        .saturating_add(invoke_info.len().saturating_mul(1024))
        .saturating_add(inline_extra);
    let mut buf = ExecutableBuffer::new(estimated_size.max(4096))?;
    buf.set_tag("x64-single-pass");

    // Size operand-stack spills from the reader/verifier max_stack when the
    // production path supplies it. Keep the local estimator as a defensive floor
    // for legacy tests and future synthetic call sites.
    let estimated_max_stack = estimate_max_stack(code, code_len);
    let max_stack = verified_max_stack
        .map(|verified| verified.max(estimated_max_stack))
        .unwrap_or(estimated_max_stack);
    // Bug-4 frame sizing, part B: the invoke-dispatch sites carve their
    // outgoing args buffer at the CURRENT spill watermark and extend it by
    // n*8 bytes for the call's duration. At worst (operand stack at
    // max_stack depth when the deepest-arity call is emitted) the buffer
    // tops out n slots past the spill region — overlapping the callee-saved
    // save area, or, past `frame_size`, the callee's own stack (where the
    // next CALL's return-address push zeroes it). Reserve the worst-case
    // arity on top of the estimate so the buffer always stays inside the
    // reserved frame.
    let max_invoke_args: usize = invoke_info
        .iter()
        // SAFETY: invoke_info pointers are kept alive by the caller for the
        // duration of compilation (same contract as the emission sites).
        .map(|(_, p)| unsafe { (**p).num_jit_args })
        .max()
        .unwrap_or(0);
    // Inlining allocates extra spill slots for each inlined callee's locals
    // and operand stack ON TOP of the caller's `max_stack` (and, since the
    // inline epilogue keeps the return value rather than reclaiming the callee
    // locals, sequential inlines accumulate). `spill_size` is derived purely
    // from `max_stack`, so without this reserve the inlined code writes past
    // the spill region into the callee-saved / shadow area — corrupting live
    // values (observed as a `ClassCastException: …$TaskOption not an enum` when
    // a clobbered slot fed an enum-typed field). Reserve, per site,
    // `callee_max_locals + callee_code_len` (the latter bounds the callee's own
    // operand depth); the total is bounded by `MAX_INLINE_BUDGET`.
    let inline_stack_reserve: usize = inline_sites
        .values()
        .map(|s| {
            let (_, param_span) = crate::compute_param_jvm_slots(&s.descriptor, s.callee_is_static);
            s.callee_max_locals
                .max(param_span)
                .saturating_add(s.callee_code_len)
        })
        .sum();
    let max_stack = max_stack
        .saturating_add(max_invoke_args)
        .saturating_add(inline_stack_reserve);

    // LICM: detect loops and find invariant aaload sequences to hoist
    let loops = detect_loops(code, code_len);
    // Pre-header placement soundness: a loop header that can be entered by a
    // branch from outside the loop would run the loop body with an
    // uninitialised hoist slot / an unrun speculative guard, because
    // `pc_to_native[header]` deliberately points PAST the pre-header. Drop
    // every speculating transform for such headers — see
    // `find_bypassable_loop_headers` for the full derivation and the
    // `AttributesImpl.ensureCapacity` witness.
    let bypassable_headers =
        find_bypassable_loop_headers(code, code_len, &loops, &exception_ranges);
    let hoist_info =
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_AALOAD_LICM").is_some() {
            Vec::new()
        } else {
            find_loop_hoists(code, code_len, &loops)
        };
    // Per-bci de-spec (same registry the speculative-BCE guards use): the
    // hoisted aaload's null+bounds preheader guard deopts at the loop-header
    // bci; once a header crosses the de-spec threshold, drop its hoists so
    // the recompile emits the in-loop aaload with its normal checks instead
    // of re-making the failed speculation. Must run BEFORE `Compiler::new`
    // pairs `hoist_offsets` with `hoist_info` by index.
    let hoist_info: Vec<LoopHoist> = hoist_info
        .into_iter()
        .filter(|h| {
            let despec = crate::deopt::despec_contains(method_key, despec_bci(h.loop_header));
            if despec && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                eprintln!(
                    "[cratonvm-deopt] de-spec: dropping aaload LICM hoist at loop_header \
                     bci={} for {} (recompile with in-loop checked access)",
                    h.loop_header, method_key
                );
            }
            !despec
        })
        .collect();
    let hoist_info: Vec<LoopHoist> = hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();

    // LICM: find loop-invariant integer-arithmetic runs to hoist into the
    // loop pre-header. These are pure, non-faulting ALU expressions on
    // loop-invariant locals/constants — see `find_arith_loop_hoists`.
    let arith_hoist_info =
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_ARITH_LICM").is_some() {
            Vec::new()
        } else {
            find_arith_loop_hoists(code, code_len, &loops)
        };
    let arith_hoist_info: Vec<ArithLoopHoist> = arith_hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();
    if !arith_hoist_info.is_empty()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some()
    {
        eprintln!(
            "[JIT_GEN] arith-LICM hoists={} runs={:?}",
            arith_hoist_info.len(),
            arith_hoist_info
                .iter()
                .map(|h| (h.seq_start, h.seq_end, h.steps.len()))
                .collect::<Vec<_>>(),
        );
    }

    // Round-8 wave-3 HIGH fix (Fix 3): generic LICM scaffold for
    // getfield/getstatic loads. The analysis is invoked here so the
    // pipeline links against the new `loop_analysis` module and
    // pattern surfaces during compilation; the result is currently
    // discarded because hoisting itself requires safepoint /
    // oop-map / regalloc participation that is intentionally
    // deferred to a later round (see `loop_analysis.rs` module doc).
    //
    // TODO(round-12+): wire the returned `InvariantLoad` records
    // into the emitter as a pre-header hoist consumer alongside the
    // existing `LoopHoist` (aaload) and `FpLoopHoist` (dload)
    // mechanisms.
    {
        let licm_loops = crate::loop_analysis::detect_loops(code, code_len);
        let mut total = 0usize;
        for li in &licm_loops {
            let v = crate::loop_analysis::find_invariant_loads(li, code);
            total += v.len();
        }
        // Suppress dead_code warnings on the analysis output without
        // changing emission behavior.
        let _ = total;
    }

    // T5.2.1 — SCEV induction variable analysis.
    //
    // Produces an `InductionVar` entry per detected counted loop. The
    // result is stored on the Compiler so downstream passes (unrolling,
    // vectorization, range-check elimination) can query stride, bound,
    // and trip count without re-walking the bytecode.
    let induction_vars = crate::scev::analyze_induction_variables(code, code_len, &loops);

    // T5.2.14 — Null-check elimination dataflow.
    //
    // Walks the bytecode once and produces a per-PC bitmask of locals
    // proven non-null. Future null-check emission paths consult this
    // via `Compiler::is_local_nonnull(pc, local)` to skip redundant
    // `TEST reg, reg; JZ throw_npe` sequences.
    let null_check_info = crate::null_check_elim::analyze(code, code_len);

    // BCE: analyze loops for bounds check elimination
    // DBG (env-gated): CRATONVM_JIT_NO_BCE disables bounds-check elimination
    // (and SIMD, which also elides per-element checks) so every array access is
    // bounds-checked — to test whether an elided check causes the out-of-bounds
    // array-store heap corruption.
    let no_bce = cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_NO_BCE").is_some();
    let (bounds_safe_pcs, speculative_bce_guards) = if no_bce {
        (FxHashSet::default(), Vec::new())
    } else {
        // The handler table is REQUIRED by the guard-dominated range reason: a
        // flow-sensitive fact is a claim about every way control reaches a
        // point, and an exception edge is one of those. Without the table that
        // reason refuses outright. `exception_ranges` here is the shadowed
        // output-coordinate copy, which is the space BCE works in.
        analyze_bounds_elimination_with_handlers(code, code_len, &loops, Some(&exception_ranges))
    };

    // Guarded matrix dot-product lowering.  This is a pre-header replacement
    // like the SIMD reductions below, but it remains useful for Java's
    // array-of-row `int[][]` layout where the right-hand column is not
    // contiguous and therefore cannot use ordinary packed loads.  The kill
    // switch restores the generic scalar emitter for diagnostics.
    let matrix_dot_enabled =
        cratonvm_types::flags::runtime_var("CRATONVM_JIT_MATRIX_DOT").map_or(true, |v| {
            let value = v.trim();
            value != "0"
                && !value.eq_ignore_ascii_case("false")
                && !value.eq_ignore_ascii_case("off")
        });
    let matrix_dot_loops: Vec<MatrixDotLoop> = if matrix_dot_enabled && !no_bce {
        loops
            .iter()
            .filter(|(header, _)| !bypassable_headers.contains(header))
            .filter_map(|&(header, back_edge)| {
                let loop_end = back_edge + bytecode_len_at(code, back_edge);
                let iv = find_induction_variable(code, header, loop_end)?;
                detect_matrix_dot_loop(code, header, back_edge, iv)
            })
            .collect()
    } else {
        Vec::new()
    };
    if !matrix_dot_loops.is_empty()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some()
    {
        eprintln!(
            "[JIT_GEN] matrix-dot headers={:?}",
            matrix_dot_loops
                .iter()
                .map(|dot| dot.header_pc)
                .collect::<Vec<_>>()
        );
    }

    // SIMD: detect vectorizable int-array-sum loops (requires AVX2)
    let simd_loops = if has_avx2() && !no_bce {
        let mut simd = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_sum(code, header, back_edge, iv) {
                    simd.push(info);
                }
            }
        }
        simd
    } else {
        Vec::new()
    };

    // SIMD FP: detect vectorizable double-array-sum loops (requires AVX2)
    let simd_fp_loops = if has_avx2() {
        let mut simd = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_fp_array_sum(code, header, back_edge, iv) {
                    simd.push(info);
                }
            }
        }
        simd
    } else {
        Vec::new()
    };

    // T5.2.15 — Int-array element-wise SIMD detection.
    //
    // Unlike reduction, detection here is *unconditional on AVX2* so
    // the information is available to any downstream pass (e.g.
    // cost-based vectorization, auto-tuning). Emission code checks
    // `has_avx2()` before issuing AVX2-only encodings.
    let simd_element_wise_loops = {
        let mut ewise = Vec::new();
        for &(header, back_edge) in &loops {
            let back_edge_end = back_edge + bytecode_len_at(code, back_edge);
            if let Some(iv) = find_induction_variable(code, header, back_edge_end) {
                if let Some(info) = detect_int_array_element_wise(code, header, back_edge, iv) {
                    ewise.push(info);
                }
            }
        }
        ewise
    };
    let bulk_zero_byte_fill_loops: Vec<BulkZeroByteFillLoop> = if bulk_byte_loops_enabled() {
        loops
            .iter()
            .filter_map(|&(header, back_edge)| {
                detect_bulk_zero_byte_fill_loop(code, code_len, header, back_edge)
            })
            .filter(|fill| !bypassable_headers.contains(&fill.header_pc))
            .collect()
    } else {
        Vec::new()
    };
    let bulk_set_byte_stride_loops: Vec<BulkSetByteStrideLoop> = if bulk_byte_loops_enabled() {
        loops
            .iter()
            .filter_map(|&(header, back_edge)| {
                detect_bulk_set_byte_stride_loop(code, code_len, header, back_edge)
            })
            .filter(|fill| !bypassable_headers.contains(&fill.header_pc))
            .collect()
    } else {
        Vec::new()
    };
    let byte_sieve_loops: Vec<ByteSieveLoop> = if bulk_byte_loops_enabled() {
        loops
            .iter()
            .filter_map(|&(header, back_edge)| {
                detect_byte_sieve_loop(code, code_len, header, back_edge)
            })
            .filter(|sieve| !bypassable_headers.contains(&sieve.header_pc))
            .collect()
    } else {
        Vec::new()
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some()
        && !(bulk_zero_byte_fill_loops.is_empty()
            && bulk_set_byte_stride_loops.is_empty()
            && byte_sieve_loops.is_empty())
    {
        eprintln!(
            "[JIT_GEN] bulk-byte headers: zero-fill={:?} set-stride={:?} sieve={:?}",
            bulk_zero_byte_fill_loops
                .iter()
                .map(|f| f.header_pc)
                .collect::<Vec<_>>(),
            bulk_set_byte_stride_loops
                .iter()
                .map(|f| f.header_pc)
                .collect::<Vec<_>>(),
            byte_sieve_loops
                .iter()
                .map(|s| s.header_pc)
                .collect::<Vec<_>>(),
        );
    }

    // Loop unrolling: detect small loops suitable for unrolling
    // PGO: use profiled trip counts to guide unroll factor when available.
    // Static heuristic fallback:
    //   Body ≤20 bytecodes  → 4x unroll (3 extra copies)
    //   Body 20-50 bytecodes → 2x unroll (1 extra copy)
    //   Body > 50            → no unroll (unless PGO says otherwise, up to 100 bytes)
    //
    // The size band is a PROFITABILITY heuristic and nothing else. Every
    // LEGALITY question — reducibility, single entry, inner-cycle
    // reducibility, branches to the back edge, handler containment,
    // pre-header bypass, time-to-safepoint — is answered by
    // `plan_native_unroll`, which gates every entry that reaches
    // `compiler.unroll_loops`. It must stay the only producer of this vector:
    // the emitter's `0xa7` arm treats membership as proof that duplicating
    // the body's machine code is sound. See its doc comment for what the old
    // `code[back_edge] == 0xa7` + body-size test was missing.
    //
    // `loop_xform.is_some()` is redundant with `!native_unroller_enabled()`
    // (arming the rewriter is what turns the native unroller off, and a
    // transform can only exist when armed), and is written anyway: this
    // vector is the emitter's proof that duplicating machine code is sound,
    // and "the bytecode was already duplicated" must be visible AT the vector
    // rather than two functions away.
    let unroll_loops: Vec<(usize, usize, usize)> = if loop_xform.is_some()
        || !native_unroller_enabled()
    {
        Vec::new()
    } else {
        loops
            .iter()
            .filter_map(|&(header, back_edge)| {
                // Only unroll loops with goto back-edge (not conditional)
                if back_edge >= code_len || code[back_edge] != 0xa7 {
                    return None;
                }
                let body_size = back_edge - header;
                if body_size < 5 {
                    return None;
                }

                // PGO path: use profiled trip count if available for this back-edge
                if let Some(&pgo_factor) = loop_unroll_hints.get(&back_edge) {
                    // `saturating_sub`: the old `pgo_factor - 1` underflowed on
                    // a 0 hint. A 0/1 factor now means "no extra copies", which
                    // `plan_native_unroll` refuses as `TooManyCopies`.
                    let extra_copies = pgo_factor.saturating_sub(1);
                    // PGO extends unrolling eligibility to larger loops (up to 100 bytes)
                    if body_size <= 50 || (body_size <= 100 && pgo_factor <= 2) {
                        return plan_native_unroll(
                            code,
                            code_len,
                            header,
                            back_edge,
                            extra_copies,
                            &exception_ranges,
                            &bypassable_headers,
                        );
                    }
                }

                // Static heuristic fallback
                let extra_copies = if body_size <= 20 {
                    3 // 4x unroll
                } else if body_size <= 50 {
                    1 // 2x unroll (covers FP-heavy loops like N-Body advance)
                } else {
                    return None;
                };
                plan_native_unroll(
                    code,
                    code_len,
                    header,
                    back_edge,
                    extra_copies,
                    &exception_ranges,
                    &bypassable_headers,
                )
            })
            .collect()
    };
    if !unroll_loops.is_empty()
        && cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some()
    {
        eprintln!("[JIT_GEN] unroll admitted={unroll_loops:?}");
    }

    // FP LICM: detect loop-invariant FP loads to hoist
    let fp_hoist_info = find_fp_loop_hoists(code, code_len, &loops);
    let fp_hoist_info: Vec<FpLoopHoist> = fp_hoist_info
        .into_iter()
        .filter(|h| !bypassable_headers.contains(&h.loop_header))
        .collect();

    // FP strength reduction: detect dmul-by-2.0 → dadd-self inside loops
    let fp_strength_reduction_pcs =
        find_fp_strength_reductions(code, code_len, &loops, &ldc2w_info);

    // Register allocation: graph-coloring allocator for locals.
    //
    // A method compiled with precise exceptional frames has its handler frame
    // rebuilt from REGISTER homes, so the interference graph must know that
    // protected code can branch to the handler — otherwise a local only the
    // catch block reads is dead throughout the try and shares its register with
    // something else. Every other compile passes no handlers and is unchanged.
    let ra_handlers: &[(usize, usize, usize)] = if precise_exception_frames {
        &exception_ranges
    } else {
        &[]
    };
    // `param_jvm_slots` (not just `num_params`): a category-2 parameter spans
    // two JVM slots, so the allocator must know which slots actually hold an
    // incoming argument. See `regalloc::param_live_in_mask`.
    let alloc_result = super::regalloc::allocate_registers_with_handlers(
        code,
        code_len,
        max_locals,
        num_params,
        param_jvm_slots,
        &loops,
        ra_handlers,
    );

    // Pure-kernel GPR local homes (see `kernel_reg_locals_enabled` for the
    // full safety argument). Consume the per-compile request (set only by the
    // method-entry compile path) so it can never leak into a later compile,
    // then engage only for the pure-kernel shape: no calls of any kind, no
    // field/static ops, no allocation, no typechecks, no inline sites, and no
    // speculative BCE guards (those deopt with frame-stashed state). Reference
    // locals are masked back to frame homes, so GC visibility is unchanged.
    let pure_kernel = (kernel_reg_homes_requested || kernel_reg_homes_osr_requested)
        && kernel_reg_locals_enabled()
        && invoke_info.is_empty()
        && direct_calls.is_empty()
        && mic_slots.is_empty()
        && pic_slots.is_empty()
        && indy_info.is_empty()
        && field_info.is_empty()
        && static_field_info.is_empty()
        && new_info.is_empty()
        && new_deferred_info.is_empty()
        && anewarray_info.is_empty()
        && anewarray_deferred_info.is_empty()
        && multianewarray_info.is_empty()
        && typecheck_info.is_empty()
        && compact_field_info.is_empty()
        && inline_sites.is_empty()
        && speculative_bce_guards.is_empty();
    // When precise-map general register homes are already enabled, this pure
    // kernel does not need the narrow allocator to turn homes on again. It is
    // still a pure kernel, however, and therefore remains eligible for the
    // call-free deferred operand cache captured by `Compiler::new`.
    let kernel_reg_homes = pure_kernel && !callee_saved_gpr_local_homes_enabled();
    let mut alloc_result = if kernel_reg_homes {
        let mut ar = alloc_result;
        let ref_mask =
            super::regalloc::find_reference_locals(code, code_len, max_locals) | param_oop_mask;
        for (i, assignment) in ar.assignments.iter_mut().enumerate() {
            if i >= 64 || (ref_mask >> i) & 1 == 1 {
                *assignment = None;
            }
        }
        // Recompute the save/restore set from the surviving assignments so
        // the prologue/epilogue and frame sizing stay consistent.
        let mut used: Vec<u8> = ar.assignments.iter().flatten().copied().collect();
        used.sort_unstable();
        used.dedup();
        ar.used_callee_saved = used;
        ar
    } else {
        alloc_result
    };
    if !matrix_dot_loops.is_empty() {
        // R12..R15 are private scratch homes for the tight pre-header.  Do not
        // let graph coloring simultaneously assign a Java local to one of
        // them; locals displaced here simply retain their canonical frame
        // homes.  Compiler::new still saves all four registers for ABI
        // correctness, independently of the local-home diagnostic gates.
        for assignment in &mut alloc_result.assignments {
            if matches!(*assignment, Some(R12 | R13 | R14 | R15)) {
                *assignment = None;
            }
        }
    }

    // Precise escape re-analysis. `jit_scan` produced `non_escaping_new`
    // with a conservative empty shape map (it has no CP resolver). Now
    // that `invoke_info` carries every invokespecial's resolved
    // descriptor, rebuild the shape map and re-run `analyze_escapes`.
    // This lets a trivial `new; dup; invokespecial <init>()V` keep its
    // scalar-replacement eligibility while an arg-bearing constructor
    // (`<init>(I)V`, …) correctly escapes its receiver — the latter
    // initializes fields in a separate, un-inlined method body that the
    // JIT frame cannot reproduce. Skipping this re-analysis (or running
    // it without descriptors) caused boxed values to come back as 0
    // (the `Integer.valueOf` / `String.toLowerCase` archetype).
    //
    // SR-reachability fix (real-frame-deopt x64 backport, Phase B prerequisite):
    // the precise re-analysis here is the AUTHORITATIVE escape analysis — it
    // recomputes from scratch with the resolved invokespecial shapes and does not
    // use `non_escaping_new` as a seed. Gate it on whether the method has any
    // `new` allocation (`new_info`), NOT on whether `jit_scan`'s conservative
    // pre-pass found a non-escaping object. `jit_scan` runs `analyze_escapes` with
    // an EMPTY shape map, whose `None` arm `escape_all!`s every invokespecial
    // receiver — so it returns an empty `non_escaping_new` for the ubiquitous
    // `new X(); <init>()V` pattern, and the old `if non_escaping_new.is_empty()`
    // gate then skipped the precise pass that WOULD recognize it. Net effect of
    // that bug: single-pass scalar replacement never fired for ordinary
    // allocations at runtime. Gating on `new_info` instead lets it fire (and is
    // what makes the Phase B `VirtualObject` deopt path reachable).
    let non_escaping_new: std::collections::HashSet<usize> = if new_info.is_empty() {
        non_escaping_new
    } else {
        let mut invokespecial_shapes: FxHashMap<usize, InvokeSpecialShape> = FxHashMap::default();
        for &(ipc, info_ptr) in &invoke_info {
            // SAFETY: `info_ptr` comes from `invoke_info`, whose entries
            // are kept live by the caller for the whole compilation.
            let info = unsafe { &*info_ptr };
            if info.invoke_kind == 1 {
                invokespecial_shapes.insert(
                    ipc,
                    InvokeSpecialShape {
                        arg_slots: info.num_jit_args,
                        is_trivial_void_init: info.method_name == "<init>"
                            && info.descriptor == "()V",
                    },
                );
            }
        }
        analyze_escapes(code, code_len, &invokespecial_shapes)
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some()
        && !new_info.is_empty()
    {
        eprintln!(
            "[DBG_SCALAR_DEOPT] x64::compile escape re-analysis: new_info={} non_escaping_new={:?} invoke_info={}",
            new_info.len(),
            { let mut v: Vec<usize> = non_escaping_new.iter().copied().collect(); v.sort(); v },
            invoke_info.len(),
        );
    }

    // Scalar replacement: plan frame-local storage for non-escaping object fields
    let num_hoists = hoist_info.len();
    let scalar_base = max_locals + (if needs_heap { 1 } else { 0 }) + num_hoists;
    let empty_non_escaping = std::collections::HashSet::new();
    let non_escaping_for_sr =
        if precise_exception_frames
            || cratonvm_types::flags::runtime_var_os("CRATONVM_DISABLE_SCALAR_REPLACEMENT").is_some()
        {
            &empty_non_escaping
        } else {
            &non_escaping_new
        };
    let sr_plan = plan_scalar_replacement(
        code,
        code_len,
        non_escaping_for_sr,
        &new_info,
        &invoke_info,
        scalar_base,
    );
    let num_scalar_slots = sr_plan.total_slots;
    let force_inline_new =
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_ENABLE_INLINE_NEW").is_some();
    let cache_jit_thread_for_inline_new = needs_heap
        && helpers.get_current_thread != 0
        && helpers.tlab_post_init != 0
        && helpers.new_object != 0
        && cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_DISABLE_INLINE_NEW").is_none()
        && new_info
            .iter()
            .any(|(_, _, num_fields, has_prim_init, has_finalizer)| {
                HEADER_SIZE + num_fields.saturating_mul(SLOT_SIZE) <= 256
                    && ((!*has_prim_init && !*has_finalizer) || force_inline_new)
            });
    // Inline self-recursion stack check: a raw self-call site is an
    // `invokestatic` pc with neither an invoke-info entry nor a direct-call
    // plan (the exact condition the 0xb8 arm's "Self-recursive call"
    // else-branch keys on -- `try_compile` deliberately skips creating invoke
    // metadata for them). When one exists, reserve the floor frame slot so
    // each such site can do the two-instruction `CMP RSP, [rbp - floor]`
    // instead of a `self_call_stack_guard` helper CALL per recursion level.
    // The walk uses `bytecode_len_at`; a desync past a variable-length switch
    // can at worst set the flag spuriously, which only reserves an unused
    // slot + one prologue helper call (never unsound).
    let reserve_stack_floor = needs_heap
        && helpers.self_call_stack_guard != 0
        && helpers.native_stack_floor_fn != 0
        && inline_self_guard_enabled()
        && {
            let mut found = false;
            let mut pc = 0usize;
            while pc < code_len {
                if code[pc] == 0xb8
                    && !invoke_info.iter().any(|(p, _)| *p == pc)
                    && !direct_calls.iter().any(|(p, _)| *p == pc)
                {
                    found = true;
                    break;
                }
                pc += bytecode_len_at(code, pc);
            }
            found
        };

    KERNEL_REG_HOMES_ACTIVE.with(|c| c.set(pure_kernel));
    let mut compiler = Compiler::new(
        method_key.to_string(),
        buf,
        max_locals,
        num_params,
        max_stack,
        needs_heap,
        multianewarray_info,
        field_info,
        typecheck_info,
        static_field_info,
        hoist_info,
        arith_hoist_info,
        alloc_result,
        !matrix_dot_loops.is_empty(),
        *helpers,
        num_scalar_slots,
        cache_jit_thread_for_inline_new,
        reserve_stack_floor,
        gc_inert_selfrec && reserve_stack_floor,
        precise_exception_frames,
        protected_ranges,
    );
    KERNEL_REG_HOMES_ACTIVE.with(|c| c.set(false));
    // Safepoint publication plan (arch-2026-07-26 R1). Built here rather than
    // inside `Compiler::new` because it needs `code` and `param_oop_mask`,
    // neither of which that constructor receives. `compiler.local_assignments`
    // is final at this point — `Compiler::new` moved the (possibly
    // kernel-masked, possibly all-`None` when GPR local homes are disabled)
    // assignment vector into the struct and nothing mutates it afterwards — so
    // the plan describes exactly the register homes this compile will emit.
    //
    // `param_oop_mask` is unioned in for the case `find_reference_locals`
    // cannot see: a reference PARAMETER that the method never `aload`s. Without
    // it such a local would look primitive and could be left unpublished while
    // genuinely holding an oop.
    //
    // COST GATE. `plan_safepoint_publication` runs `live_locals_per_pc_with_
    // coverage`, a second whole-method liveness pass on top of the one
    // `allocate_registers` just did. R1 consumes only `no_reference_in_registers()`
    // and never touches the liveness-narrowed `publish_at` vector, so paying for
    // it on every compile would be a JIT-compile-time regression inside a change
    // whose entire purpose is a speedup — and would confound measuring it.
    //
    // Skip it whenever no local has a register home at all: there the plan
    // provably cannot change the answer (`register_homed_reference_locals` would
    // be `0` ⇒ `no_reference_in_registers()` ⇒ `false`, which is exactly what
    // the `None` fallback's `any(Option::is_some)` also yields), so leaving the
    // plan absent is behaviour-identical at zero cost. The methods that DO have
    // register homes are precisely the population R1 exists to speed up.
    //
    // FOLLOW-UP: R2 needs `publish_at`, so it will need the pass unconditionally.
    // Before landing R2, `regalloc` should grow a `publish_always`-only
    // constructor that skips the liveness walk, or thread `allocate_registers`'
    // existing liveness result through instead of recomputing it.
    if compiler.local_assignments.iter().any(Option::is_some) {
        // Bound separately: `compiler.a = f(&compiler.b)` borrows and assigns
        // the same struct in one statement, which is needlessly close to the edge.
        let safepoint_publish = super::regalloc::plan_safepoint_publication(
            code,
            code_len,
            max_locals,
            num_params,
            param_jvm_slots,
            &compiler.local_assignments,
            param_oop_mask,
            ra_handlers,
        );
        compiler.safepoint_publish = Some(safepoint_publish);
    }
    compiler.param_jvm_slots = param_jvm_slots.to_vec();
    compiler.param_slot_span = param_slot_span;
    compiler.method_key = method_key.to_string();
    // Coordinate change, emitter half: the three sites that BAKE a bci as an
    // immediate into machine code consult this. It must be installed before
    // `compile_bytecode` runs — those stubs are emitted at the end of that
    // call, so no post-pass over the finished `CompiledMethod` could reach
    // them. `None` (the identity) on every unarmed compile.
    compiler.bci_provenance = loop_xform.as_ref().map(|x| x.bci_of.clone());
    // deopt-osr Step 9 follow-up (c): per-bci de-spec. Drop any speculative-BCE
    // guard whose loop header was recorded in the de-spec registry (a guard that
    // repeatedly deopted past the per-bci give-up threshold). Those headers fall
    // back to per-access bounds checks instead of the speculative elide, so the
    // method stays compiled (no whole-method blacklist) but no longer re-makes
    // the failed speculation. Dropping a guard MUST also drop the elisions it
    // justified: each guard's `covered_pcs` are removed from `bounds_safe_pcs`
    // so those accesses get their per-element checks back — a dropped guard
    // with the elisions left in place would be an UNGUARDED speculative elide
    // (silent out-of-bounds access on exactly the input that kept deopting).
    // Inert in production / on the `compile()` wrapper: `despec_contains`
    // returns `false` for an empty key or empty registry, so both sets are
    // unchanged ⇒ byte-identical codegen.
    let mut bounds_safe_pcs = bounds_safe_pcs;
    let speculative_bce_guards: Vec<SpeculativeBCEGuard> = speculative_bce_guards
        .into_iter()
        .filter(|g| {
            let despec = crate::deopt::despec_contains(method_key, despec_bci(g.loop_header));
            if despec {
                for covered_pc in &g.covered_pcs {
                    bounds_safe_pcs.remove(covered_pc);
                }
                if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_DEOPT").is_some() {
                    eprintln!(
                        "[cratonvm-deopt] de-spec: suppressing speculative-BCE guard at \
                         loop_header bci={} for {} (recompile without it; per-element \
                         checks restored at {:?})",
                        g.loop_header, method_key, g.covered_pcs
                    );
                }
            }
            !despec
        })
        .collect();
    // Same obligation as the de-spec drop above, for a header whose pre-header
    // an outside-the-loop branch can skip: the guard would not have run, so the
    // elisions it justified must go back to per-access checks (an elide with no
    // guard is a silent out-of-bounds access).
    let speculative_bce_guards: Vec<SpeculativeBCEGuard> = speculative_bce_guards
        .into_iter()
        .filter(|g| {
            let bypassable = bypassable_headers.contains(&g.loop_header);
            if bypassable {
                for covered_pc in &g.covered_pcs {
                    bounds_safe_pcs.remove(covered_pc);
                }
            }
            !bypassable
        })
        .collect();
    compiler.bounds_safe_pcs = bounds_safe_pcs;

    // SIMD gating: a SIMD loop transform replaces the per-element accesses of
    // `arr[i]` for `i` in `[entry_iv, bound)` with an UNCHECKED batch loop, so
    // it carries the same proof obligation as a BCE elision — for EVERY array
    // it touches, `bound <= arr.length` (plus a non-negative start index) must
    // be established either statically (the bound provably IS that array's
    // length and the IV provably starts >= 0) or by a speculative loop-header
    // guard that survived de-spec (the guard also tests `iv >= 0`, and is
    // emitted before the SIMD preheader). An uncovered array — e.g. the OUT
    // store of `out[i] = a[i] + b[i]` in a loop bounded by `a.length` when
    // `out` is shorter — would batch-store past the array end with no
    // exception (docs/known-issues/jit-bce-multi-array-oob-store-20260711.md;
    // before this gate, `detect_int_array_element_wise` candidates vectorized
    // with no coupling to the bounds analysis at all).
    // (`no_bce` also lands here: CRATONVM_JIT_NO_BCE always claimed to disable
    // "BCE and SIMD", but only the int-sum detection was actually gated on it —
    // the FP-sum and element-wise transforms kept vectorizing with elided
    // checks. Routing every SIMD candidate through this coverage check makes
    // the debug gate true to its documentation.)
    let simd_covered = |header: usize, arr_local: usize, bound_local: usize, iv_local: usize| {
        !no_bce
            && ((find_bound_arraylength_provenance(code, code_len, bound_local) == Some(arr_local)
                && find_iv_nonneg_start(code, code_len, iv_local))
                || speculative_bce_guards.iter().any(|g| {
                    g.loop_header == header
                        && g.array_local == arr_local
                        && g.bound_local == bound_local
                }))
    };
    let simd_loops: Vec<SimdIntArraySum> = simd_loops
        .into_iter()
        .filter(|s| simd_covered(s.header_pc, s.array_local, s.bound_local, s.iv_local))
        .collect();
    let simd_fp_loops: Vec<SimdFpArraySum> = simd_fp_loops
        .into_iter()
        .filter(|s| simd_covered(s.header_pc, s.array_local, s.bound_local, s.iv_local))
        .collect();
    let simd_element_wise_loops: Vec<SimdArrayElementWise> = simd_element_wise_loops
        .into_iter()
        .filter(|e| {
            simd_covered(e.header_pc, e.out_local, e.bound_local, e.iv_local)
                && simd_covered(e.header_pc, e.a_local, e.bound_local, e.iv_local)
                && simd_covered(e.header_pc, e.b_local, e.bound_local, e.iv_local)
        })
        .collect();
    // A SIMD batch pre-header is emitted under the same placement contract as
    // the LICM hoists, so a bypassable header must not carry one either.
    let simd_loops: Vec<SimdIntArraySum> = simd_loops
        .into_iter()
        .filter(|s| !bypassable_headers.contains(&s.header_pc))
        .collect();
    let simd_fp_loops: Vec<SimdFpArraySum> = simd_fp_loops
        .into_iter()
        .filter(|s| !bypassable_headers.contains(&s.header_pc))
        .collect();
    let simd_element_wise_loops: Vec<SimdArrayElementWise> = simd_element_wise_loops
        .into_iter()
        .filter(|e| !bypassable_headers.contains(&e.header_pc))
        .collect();
    // Index the speculative guards by loop-header PC once, so the per-header
    // emit loop does an O(1) map lookup instead of an O(guards) filtered scan
    // at every loop header.
    {
        let mut by_header: FxHashMap<usize, Vec<SpeculativeBCEGuard>> = FxHashMap::default();
        for g in &speculative_bce_guards {
            by_header.entry(g.loop_header).or_default().push(g.clone());
        }
        compiler.speculative_bce_guards_by_header = by_header;
    }
    compiler.speculative_bce_guards = speculative_bce_guards;
    compiler.compact_field_off = compact_field_info
        .into_iter()
        .map(|(pc, off, is_ref)| (pc, (off, is_ref)))
        .collect();
    compiler.new_info = new_info;
    compiler.new_deferred_info = new_deferred_info;
    compiler.anewarray_info = anewarray_info;
    compiler.anewarray_deferred_info = anewarray_deferred_info;
    compiler.invoke_info = invoke_info;
    compiler.indy_info = indy_info;
    compiler.direct_calls = direct_calls;
    // deopt-osr Step 8 (test trigger): under CRATONVM_OSR_EXIT_TEST + CRATONVM_DEOPT_REAL,
    // pick the first (lowest-pc) detected loop header as the synthetic OSR-exit
    // branch site. `None` in production (either gate off) ⇒ no trigger emitted ⇒
    // byte-identical code. `detect_loops` returns (header, end) pairs.
    //
    // P4 (Step 8 follow-up): `CRATONVM_OSR_EXIT_AFTER=N` also arms the trigger at the
    // same loop header, but counter-gated (`osr_exit_after_count = Some(N)`) so the
    // JIT advances ~N iterations before the exit — exercising the true OSR-exit
    // transfer with genuinely advanced state. Either gate (both require DEOPT_REAL)
    // selects the bci; AFTER takes precedence over TEST for the emitted form.
    compiler.osr_exit_after_count = if crate::deopt_real_enabled() {
        crate::osr_exit_after()
    } else {
        None
    };
    compiler.osr_exit_test_trigger_bci = if crate::deopt_real_enabled()
        && (crate::osr_exit_test_enabled() || compiler.osr_exit_after_count.is_some())
    {
        loops.iter().map(|&(h, _)| h).min()
    } else {
        None
    };
    // deopt-osr: the through-JIT deopt-EXIT differential trigger. Under
    // CRATONVM_DEOPT_EAGER + DEOPT_REAL, force a reason-2 deopt-EXIT at the first
    // (lowest-pc) loop header. `None` (either gate off / no loop) ⇒ byte-identical.
    compiler.deopt_eager_bci = if crate::deopt_real_enabled() && crate::deopt_eager_enabled() {
        loops.iter().map(|&(h, _)| h).min()
    } else if crate::deopt_real_enabled() {
        // Phase B e2e: `CRATONVM_DEOPT_EAGER_BCI=<n>` points the eager deopt-EXIT
        // at a specific straight-line bci (where a scalar object is live in a
        // local), the only way to drive `VirtualObject` resume through the JIT.
        // Fire ONLY in a method that actually has a scalar object live at that
        // bci — so the global env doesn't perturb unrelated methods (e.g. the
        // driver `main`, which has no scalar replacement).
        crate::deopt_eager_bci_override().filter(|n| compiler.sr_local_prov_at.contains_key(n))
    } else {
        None
    };
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SCALAR_DEOPT").is_some()
        && !compiler.scalar_replaced.is_empty()
    {
        let mut keys: Vec<usize> = compiler.sr_local_prov_at.keys().copied().collect();
        keys.sort();
        eprintln!(
            "[DBG_SCALAR_DEOPT] x64 single-pass compile: scalar_replaced={} sr_local_prov_at_pcs={:?} eager_bci={:?} elided_monitor={}",
            compiler.scalar_replaced.len(),
            keys,
            compiler.deopt_eager_bci,
            compiler.has_elided_monitor,
        );
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some() {
        eprintln!(
            "[JIT_GEN_INSTALL] mic_slots count={} pcs={:?}",
            mic_slots.len(),
            mic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.mic_slots = mic_slots;
    // HIGH-7 — Inline 4-way PIC fast-path wiring (now active).
    //
    // The codegen in `Compiler::compile_op_invokevirtual` (search
    // `pic_inline`) keys off `compiler.pic_slots`. With the
    // `pic_slots` parameter now threaded through, callers that
    // eagerly allocate a `Box<JitPICSlot>` per polymorphic call
    // site (see `jit/src/lib.rs::try_compile`) activate the inline
    // cascade. Slots start empty (class_id == 0 at all 4 entries),
    // so the CMP cascade falls straight through to the helper on
    // first invocation; once the runtime helper populates a slot,
    // subsequent dispatches take the inline fast path.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JIT_GEN").is_some() {
        eprintln!(
            "[JIT_GEN_INSTALL] pic_slots count={} pcs={:?}",
            pic_slots.len(),
            pic_slots.iter().map(|(pc, _)| pc).collect::<Vec<_>>(),
        );
    }
    compiler.pic_slots = pic_slots;
    compiler.unroll_loops = unroll_loops;
    compiler.simd_loops = simd_loops;
    compiler.matrix_dot_loops = matrix_dot_loops;
    // The pure-kernel deferred cache owns R8/R9 across bytecodes, while the
    // matrix-dot preheader uses those registers for its batch limit and
    // wrapping accumulator. Keep upstream's pure-kernel local homes, but
    // disable only the conflicting operand cache for this exact lowering.
    if !compiler.matrix_dot_loops.is_empty() {
        compiler.kernel_operand_cache = false;
    }
    compiler.branch_hints = branch_hints.into_iter().collect();
    compiler.loop_unroll_hints = loop_unroll_hints.into_iter().collect();
    compiler.ldc_info = ldc_info;
    compiler.ldc_string_info = ldc_string_info;
    compiler.ldc_class_info = ldc_class_info;
    compiler.ldc2w_info = ldc2w_info;
    compiler.fp_hoist_info = fp_hoist_info;
    compiler.fp_strength_reduction_pcs = fp_strength_reduction_pcs;
    compiler.simd_fp_loops = simd_fp_loops;
    // Phase B (real-frame-deopt x64 backport): build the per-field type map for
    // scalar-replaced objects by joining the per-access-site `field_info`
    // (`(pc, field_index, type_tag)`) with the plan's `field_ops` (`pc → new_pc`).
    // Each accessed field of a scalar object thus learns its JVM type tag, which
    // drives the `VirtualObject` field `FrameValue` width/ref-ness on deopt. Built
    // before `field_ops` is moved into `scalar_field_ops` below.
    {
        let mut sr_field_types: FxHashMap<(usize, usize), u8> = FxHashMap::default();
        for &(pc, field_index, type_tag) in &compiler.field_info {
            if let Some(&new_pc) = sr_plan.field_ops.get(&pc) {
                sr_field_types.insert((new_pc, field_index), type_tag);
            }
        }
        compiler.sr_field_types = sr_field_types;
    }
    compiler.sr_local_prov_at = sr_plan.local_prov_at;
    compiler.sr_monitor_at = sr_plan.monitor_at;
    compiler.sr_monitor_scalar_ops = sr_plan.monitor_scalar_ops;
    compiler.scalar_replaced = sr_plan.objects;
    compiler.scalar_field_ops = sr_plan.field_ops;
    compiler.scalar_init_skips = sr_plan.init_skips;
    compiler.inline_sites = inline_sites.into_iter().collect();
    // String call-site intrinsics: hand the resolved String field layout to
    // the compiler so intrinsic codegen can emit inline field loads.
    compiler.string_layout = string_layout;
    // T5.2.1 + T5.2.14 — transfer the pre-computed analyses.
    compiler.induction_vars = induction_vars;
    compiler.null_check_info = null_check_info;
    // T5.2.15 — element-wise SIMD detections.
    compiler.simd_element_wise_loops = simd_element_wise_loops;
    compiler.bulk_zero_byte_fill_loops = bulk_zero_byte_fill_loops;
    compiler.bulk_set_byte_stride_loops = bulk_set_byte_stride_loops;
    compiler.byte_sieve_loops = byte_sieve_loops;
    // Same R8/R9 ownership conflict the matrix-dot lowering has above: the
    // sieve preheader keeps the prime count in R8 and its word-scan temporary
    // in R9, and the strided-store preheader keeps the step in R9. Those are
    // exactly the pure-kernel deferred operand cache's two scratch registers.
    if !compiler.byte_sieve_loops.is_empty() || !compiler.bulk_set_byte_stride_loops.is_empty() {
        compiler.kernel_operand_cache = false;
    }
    // T5.2.17 — loop unswitching candidates.
    compiler.loop_unswitch_candidates = detect_loop_unswitch_candidates(code, code_len, &loops);

    // MED-4 / Fix 3 — pre-build pc-indexed lookup maps for the hot
    // codegen sites (getfield/putfield/invoke*/new/anewarray/ldc/…)
    // so each query is O(1) rather than scanning the Vec.
    compiler.build_pc_indices();

    // Stage 2 (precise oop maps) — forward "must be oop" local-variable
    // dataflow. Lets `emit_oop_map_for_safepoint` record the canonical frame
    // slots of register/memory locals that hold object references at each
    // safepoint (in addition to the operand-stack slots), so a moving GC has
    // precise, updatable coverage of every live oop. Behaviour-neutral on the
    // default (non-moving) path: `conservative_roots::scan_one_frame_precise`
    // already sweeps the whole frame region, so the extra precise entries are
    // redundant there and re-validated via `heap.is_object_address`.
    let (lo_masks, lo_reached) =
        compute_local_oop_masks(code, code_len, max_locals, param_oop_mask);
    compiler.local_oop_masks = lo_masks;
    compiler.local_oop_reached = lo_reached;

    // deopt-osr P2 — per-local width/type source for the deopt snapshot. Only the
    // (gated) snapshot consumes it, so skip the scan entirely in production.
    if crate::deopt_real_enabled() || precise_exception_frames {
        compiler.local_kinds = classify_local_kinds(code, code_len, max_locals);
        // Resolve the `Ambiguous` votes per bci where control flow allows it.
        compiler.local_kinds_refined = refine_ambiguous_local_kinds(
            code,
            code_len,
            &compiler.local_kinds,
            &exception_ranges,
        );
        // FU2 — method-level cat-2/FP gate for the operand-stack snapshot.
        compiler.uses_long_float_double = code_uses_long_float_double(code, code_len);
        // deopt-osr OSR-exit dead-local fix — see `local_liveness`'s doc comment.
        // The exception table MUST be modelled: these snapshots are taken at
        // pcs inside protected ranges, and a local only the handler reads is
        // otherwise computed dead exactly there.
        let (liveness, covered) = super::regalloc::live_locals_per_pc_with_handlers(
            code,
            code_len,
            num_params,
            param_jvm_slots,
            &exception_ranges,
        );
        compiler.local_liveness = liveness;
        compiler.local_liveness_covered = covered;
        compiler.exception_ranges_dbg_len = exception_ranges.len();
    }

    // Emit prologue
    compiler.emit_prologue();
    if compiler.failed {
        return None;
    }
    let entry_offset = 0; // prologue starts at offset 0
    compiler.body_entry_offset = compiler.buf.pos(); // offset right after prologue

    // Compile bytecode
    if !compiler.compile_bytecode(code, code_len) {
        // Publish the refusing bytecode to the caller's bail-site record too:
        // this line names the opcode but not the method, and `try_compile`'s
        // `compile-bail` line names the method but not the opcode. Neither is
        // a diagnosis on its own, and they are not even both printed on the
        // same run for an OSR/callee compile.
        crate::note_jit_bail_site_at(
            "singlepass-codegen",
            compiler.dbg_last_pc,
            compiler.dbg_last_op,
        );
        if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_JITC").is_some() {
            eprintln!(
                "[cratonvm-jitc] codegen-bail pc={} op=0x{:02x}",
                compiler.dbg_last_pc, compiler.dbg_last_op
            );
        }
        return None;
    }

    // Patch branches (both forward and backward are handled). A `false`
    // return means some branch targeted a PC that was never emitted as an
    // instruction boundary (malformed/unverified bytecode) — reject the
    // method rather than leave an unpatched jump in executable code.
    if !compiler.patch_branches() {
        crate::note_jit_bail_site("branch-target-not-an-instruction-boundary");
        return None;
    }

    // Patch self-recursive calls to point to entry
    compiler.patch_self_calls(entry_offset);

    // Lazy-prologue perf lever: now that the body is fully compiled,
    // `shadow_pushed_any` is final — NOP out the prologue thread-fetch if no
    // register-resident oop was ever published (no-op when shadow is off).
    compiler.maybe_nop_out_shadow_fetch();

    // `estimated_size` is a heuristic; a pathological method can emit past it.
    // The emit hot path records the overflow instead of panicking — bail to
    // the interpreter here rather than returning a truncated, unsafe method.
    if compiler.buf.overflowed() {
        // Name the method and the shortfall. A silent bail here is
        // indistinguishable from "the JIT chose not to compile this", which is
        // how a whole class of invoke-heavy methods came to stop being compiled
        // unnoticed (`docs/internal/resolvabletype-equals-jit-...`): the only
        // visible symptom was a flood of anonymous `try_patch_*: offset out of
        // bounds` warnings with no method attached to any of them.
        tracing::warn!(
            method = method_key,
            code_len = code_len,
            capacity = compiler.buf.capacity(),
            wanted = compiler.buf.wanted(),
            "JIT compile bailed: code buffer estimate too small; method stays interpreted"
        );
        crate::note_jit_bail_site("code-buffer-estimate-too-small");
        return None;
    }

    // Fail-closed backstop for the bytecode rewriter. `DeoptimizationPoint::bci`
    // is what the VM RESUMES AT, and it is recorded from the emitter's own pc
    // deep inside the emitter. `plan_bytecode_loop_xform` refuses every
    // construct that records one (`DeoptRealEnabled`, `PreciseExceptionFrames`,
    // `InvokedynamicPresent`), so this vector is provably empty here — but if a
    // future emit path records one anyway, discard the method rather than
    // publish an output PC as an interpreter resume point.
    if loop_xform.is_some() && !compiler.deopt_points.is_empty() {
        tracing::warn!(
            method = method_key,
            deopt_points = compiler.deopt_points.len(),
            "JIT compile bailed: bytecode loop rewrite recorded a deopt point whose \
             bci is an output PC; method stays interpreted"
        );
        return None;
    }

    // Build the CompiledMethod with OSR metadata.
    //
    // `has_dispatch` gates the interpreter's compiled-entry fast path
    // (`interpreter.rs`: `if !compiled.has_dispatch { ... }`), which skips
    // `set_jit_thread`. That TLS pointer is what the dispatch helpers
    // (`jit_invoke_dispatch` / `jit_invoke_virtual_mic`) read via
    // `jit_thread_mut()`; when it is null they short-circuit and return 0.
    //
    // A method with `direct_calls` makes machine-level `CALL`s into other
    // compiled callees WITHOUT crossing a Rust boundary that could set the
    // TLS. Those callees inherit whatever `JIT_THREAD` the caller was
    // entered with, and may themselves dispatch (or transitively call a
    // method that does). So a method whose only inter-method calls are
    // direct calls must STILL be entered through the slow path that sets
    // `JIT_THREAD` — otherwise the callee's dispatch helper sees a null
    // thread and silently returns 0.
    //
    // This was the `Character.getType(char)` miscompile: its sole call,
    // `invokestatic Character.getType(int)`, resolved to a `direct_call`,
    // leaving `invoke_info` empty. The fast path skipped `set_jit_thread`,
    // so the directly-called `getType(int)` ran with a null thread and its
    // `CharacterData.of(...)` / `getType(...)` dispatches returned 0 →
    // `Character.getType` returned UNASSIGNED for every Latin-1 letter once
    // JIT-compiled.
    let has_dispatch = !compiler.invoke_info.is_empty()
        || !compiler.direct_calls.is_empty()
        || !compiler.bounds_check_stubs.is_empty()
        || !compiler.null_check_store_stubs.is_empty()
        // RBC.6 — an athrow stashes a pending JIT exception; the
        // `!has_dispatch` fast entry paths return the raw value WITHOUT
        // draining it, which would leak the exception (and mis-read the
        // sentinel as a return value). Force the dispatch-aware route.
        || compiler.emitted_athrow
        // Live monitor helpers call `jit_thread_mut()` to identify the owner
        // and to enter GC-blocked parking on contention. A monitor-only method
        // otherwise looks call-free and would take the TLS-free fast entry.
        || compiler.emitted_monitor_call
        // A fallible `newarray` OOM bail needs the per-thread TLS set so the
        // helper can GC + construct the OOME (same rationale as direct_calls).
        || compiler.emitted_alloc_oom_check
        // Residual-6 companion fix — a failed checkcast stashes a CCE through
        // the JIT_THREAD TLS and bails with the i64::MIN sentinel; the entry
        // path must set the TLS and drain the pending exception.
        || compiler.emitted_checkcast_throw
        // BUG-1 companion — a direct (non-dispatch) self-recursive CALL site:
        // its stack guard stashes a catchable StackOverflowError near native
        // exhaustion and returns the i64::MIN sentinel, so the method MUST be
        // entered through the dispatch-aware path that sets `JIT_THREAD` (the
        // guard constructs the SOE through it) and drains
        // `JIT_PENDING_EXCEPTION` on return. Without this, the register-only
        // fast entry mis-read the sentinel as a return value (int-truncated
        // to 0) and leaked the pending SOE (observed: DeepRec printed
        // "no-overflow r=0" instead of catching the error).
        || !compiler.self_call_patches.is_empty()
        // jit-clinit-gap-has-dispatch fix (2026-07-17): `jit_getstatic`,
        // `jit_putstatic_*`, and `jit_new_object` all now run
        // `ensure_class_initialized_shared` (see the matching fix comments
        // on each in `vm/src/jit/helpers.rs`), which needs `jit_thread_mut()`
        // to resolve a live `&mut JvmThread` -- exactly the same
        // `JIT_THREAD` TLS this whole `has_dispatch` flag exists to
        // guarantee (see the doc comment above: "otherwise the callee's
        // dispatch helper sees a null thread and silently returns 0").
        // Before this line, a method whose ONLY JIT-relevant content was
        // getstatic/putstatic/new sites -- no invoke, no direct call, no
        // bounds check, nothing else on this list -- compiled with
        // `has_dispatch=false` and was entered through `execute_jit_call`'s
        // fast arm (`vm/src/runtime/interpreter.rs`, `if !compiled.
        // has_dispatch`), which skips `set_jit_thread` entirely. Any of
        // those three helpers then saw `jit_thread_mut() == None` and
        // silently skipped the class-init check altogether (the same
        // "silently returns 0"-shaped failure the `Character.getType`
        // fix above this comment already fixed for `direct_calls`) --
        // confirmed via a minimal repro (`static void touch(boolean w) {
        // if (w) { Init.VALUE = v; } else { dummy++; } }`, no other
        // dispatch-needing construct) whose compiled artifact had
        // `has_dispatch=false`, `static_field_info.len()=3`, and observably
        // wrote the static WITHOUT running `<clinit>` first. `new_info` gets
        // the same treatment for the identical reason on the `jit_new_object`
        // side.
        || !compiler.static_field_info.is_empty()
        || !compiler.new_info.is_empty()
        // `jit_new_object_cp` / `jit_anewarray_object_cp` need `jit_thread_mut()`
        // for even more than the resolved helpers do: class RESOLUTION itself
        // (a possible user `ClassLoader.loadClass`) runs on that thread, not
        // just `<clinit>`. A null thread there would leave the site unable to
        // resolve at all.
        || !compiler.new_deferred_info.is_empty()
        || !compiler.anewarray_deferred_info.is_empty()
        // `jit_ldc_class_cp` needs `jit_thread_mut()` for the same reason the
        // two above do: the resolution it performs may run a user
        // `ClassLoader.loadClass`, and a failure has to publish a pending
        // exception on this thread.
        || !compiler.ldc_class_info.is_empty();
    // Snapshot the frame partition and the label BEFORE `compiler.buf` is moved
    // into the artifact (which partially moves `compiler`).
    let frame_layout = compiler.frame_layout();
    let method_label = compiler.method_label.clone();
    let mut cm = if needs_heap {
        CompiledMethod::new_with_context(compiler.buf)
    } else {
        CompiledMethod::new(compiler.buf)
    };
    cm.has_dispatch = has_dispatch;

    // RBC.5 — record the declaring classes of every getstatic/putstatic
    // site (already resolved into `static_field_info` by the caller) so the
    // interpreter's compiled-entry fast path can ensure-initialize them
    // once per artifact instead of re-resolving the constant pool on every
    // call (see `static_init_classes` on `CompiledMethod`).
    cm.static_init_classes = {
        let mut ids: Vec<u32> = compiler
            .static_field_info
            .iter()
            .map(|&(_, class_id_raw, ..)| class_id_raw)
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids
    };

    // Store OSR metadata for On-Stack Replacement entry.
    //
    // OSR uses `osr_entry_native` rather than the branch-patch `pc_to_native`:
    // for loop headers carrying LICM-hoisted preheader code the two differ —
    // `pc_to_native[header]` points *after* the preheader (so in-loop
    // back-edges skip it) while `osr_entry_native[header]` points *before* it
    // (so a cold OSR entry runs the hoist initialisation). For every other PC
    // the two are identical.
    //
    // Pure-kernel GPR local homes: publish NO OSR entries for such a body.
    // Its non-reference locals live exclusively in callee-saved registers,
    // and the OSR trampoline's frame-seeded entry contract is exactly the
    // "OSR transition" the kernel-homes safety argument excludes. The OSR
    // pipeline compiles its own separate, memory-homed artifact
    // (`compile_osr_artifact` never requests kernel homes), so loop-hot
    // methods still get OSR service.
    //
    // Coordinate change, artifact half. `osr_entry_native` is written by the
    // emitter at the pc it is emitting, so under a bytecode rewrite it is
    // indexed by OUTPUT pc while the runtime indexes the published vector by
    // INTERPRETER bci. It cannot merely be translated on read: a bci inside a
    // transformed region has SEVERAL native offsets and picking the wrong one
    // re-runs iterations. `LoopXform::rebuild_pc_to_native` applies
    // `osr_entry_pc`'s steady-state choice pointwise and leaves the `-1`
    // sentinel wherever there is no valid image (the unrolled back-edge gap),
    // where entering compiled code is not valid at all and `can_osr_enter`
    // must refuse. The identity — the same vector, moved — when unarmed.
    let osr_entry_native = compiler.osr_entry_native;
    let osr_entry_native = match &loop_xform {
        Some(x) => {
            let mut v = x.rebuild_pc_to_native(&osr_entry_native, orig_code_len);
            // Enforce the refusal independently of who filled the vector.
            //
            // `rebuild_pc_to_native` leaves the sentinel wherever `osr_entry_pc`
            // answers `None`, so on the path where it is the sole producer this
            // loop is a no-op. It is here because it was NOT the sole producer:
            // the emitter also writes `osr_entry_native` while emitting, once
            // per copy, and the back-edge bci is written by the LAST copy. That
            // left a live entry at a bci with no steady-state image — entering
            // there resumes a "back edge next" frame at the top of a fresh body
            // and runs one extra iteration. Caught by
            // `a_rewritten_compile_publishes_osr_metadata_in_interpreter_bci_space`
            // the first time the suite was run against the wired rewriter.
            //
            // Stated as an invariant rather than a repair: after this, no bci
            // that `osr_entry_pc` refuses carries an offset, whatever produced
            // the vector.
            for (bci, slot) in v.iter_mut().enumerate() {
                if x.osr_entry_pc(bci).is_none() {
                    *slot = -1;
                }
            }
            v
        }
        None => osr_entry_native,
    };
    cm.osr_pc_to_native = if kernel_reg_homes && !kernel_reg_homes_osr_requested {
        // Method-entry kernel homes: same length, every entry -1 —
        // `can_osr_enter` refuses every pc (the method-entry body was never
        // built for trampoline entry).
        Some(vec![-1; osr_entry_native.len()])
    } else {
        // Ordinary bodies AND OSR-tier kernel-homed bodies publish real
        // entries: the OSR trampoline seeds every local into its
        // `osr_local_assignments` register (or frame slot for `None`/ref
        // locals), which for a kernel-homed body is exactly its homes.
        Some(osr_entry_native)
    };
    cm.osr_num_locals = compiler.num_locals;
    cm.osr_num_reg_locals = compiler.num_reg_locals;

    // --- OSR soundness: drop register assignments for long/double high-half
    // slots ---------------------------------------------------------------
    // A `long`/`double` JVM local at index N reserves index N+1 as its dead
    // "high half". The JIT models 64-bit values as a single register, so it
    // never reads index N+1 — but the graph-colouring allocator still hands
    // that dead slot a physical register, and freely reuses one register for
    // *several* dead high-halves AND a live local (they never interfere, so
    // colouring is legal for the running code).
    //
    // The OSR trampoline, however, copies every `jit_locals[i]` into
    // `local_assignments[i]`'s register in ascending index order. When a dead
    // high-half index shares a register with a live local at a *lower* index,
    // the trampoline's write of the high-half's garbage value (the interpreter
    // supplies 0 for the unused slot) clobbers the live local that was already
    // loaded. For a `long` loop counter this reset the counter to 0 mid-loop,
    // producing a wrong result; for a pointer-typed local it corrupts a heap
    // reference and segfaults.
    //
    // Fix: null out the OSR register assignment for every high-half slot.
    // The high-half carries no live value, so the trampoline simply spills its
    // garbage to a frame slot nobody reads — and the live local keeps its
    // register. This only touches the OSR metadata copy; the running code's
    // `reg_for_local` (which never asks for a high-half) is unaffected.
    let mut osr_local_assignments = compiler.local_assignments.clone();
    // ES-tdigest OSR fix: the high-half nulling and the per-PC dead mask below
    // must cover XMM-resident (float/double) locals exactly like GPR-resident
    // ones. DualPivotQuicksort.sort coalesces several disjoint-live-range
    // double locals (pivots, run temporaries) onto one XMM register; an OSR
    // entry at a PC where one of them is dead loaded the dead local's garbage
    // over the live owner's XMM value (the GPR-only mask said "safe"), so
    // Arrays.sort(double[]) silently mis-sorted / threw garbage-index AIOOBE
    // once the sort loop OSR-entered.
    let mut osr_xmm_assignments = compiler.xmm_assignments.clone();
    {
        let high_halves = wide_local_high_halves(code, code_len);
        for &hh in &high_halves {
            if hh < osr_local_assignments.len() {
                osr_local_assignments[hh] = None;
            }
            if hh < osr_xmm_assignments.len() {
                osr_xmm_assignments[hh] = None;
            }
        }
    }
    // Per-OSR-entry-PC "dead local" mask. The OSR trampoline loads locals into
    // their (graph-colouring-coalesced) registers in index order; a local that
    // is DEAD at the entry PC but shares a register with a LIVE local would
    // clobber the live one when loaded (e.g. an `int[]` arg and a later-loop
    // accumulator colour to the same callee-saved register because their live
    // ranges don't overlap — entering the first loop then reading the array
    // gets the accumulator's value, a null/garbage pointer → spurious NPE →
    // OSR deopt → back-off → the loops never sustain JIT). The previously-fixed
    // category-2 high-half clobber is one instance; this generalises it to any
    // pair of real locals. For each basic-block start PC (OSR entries are
    // loop-header block starts), mark the register-resident locals NOT live
    // there so the trampoline skips loading them, leaving each shared register
    // to its live owner.
    let reg_resident: u64 = osr_local_assignments
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_some())
        .fold(0u64, |m, (i, _)| if i < 64 { m | (1u64 << i) } else { m });
    // XMM-resident locals participate in the same graph-colouring coalescing
    // as GPR-resident ones, so they need the same dead-at-entry protection
    // (liveness tracks d/f locals at their base index via dload/dstore).
    let xmm_resident: u64 = osr_xmm_assignments
        .iter()
        .enumerate()
        .filter(|(_, a)| a.is_some())
        .fold(0u64, |m, (i, _)| if i < 64 { m | (1u64 << i) } else { m });
    // The mask must name the dead locals that are actually *hazardous*, not
    // every dead local. `osr_enter` declines any entry whose mask is non-zero
    // (a deliberate 2026-07-04 conservatism: the trampoline's skip-the-load
    // avoided clobbering the live owner, but the resulting coalesced state
    // transition was not proven safe -- see
    // docs/internal/fixed-suite-bugs/jit-osr-linux-regression-triad.md). The
    // hazard that argument rests on is *sharing*: a dead local whose register
    // is also some live local's home. A dead local that owns its register
    // outright has no coalesced state to reconstruct -- nothing reads it before
    // the loop redefines it -- so flagging it only costs OSR entries.
    //
    // The blanket form cost a lot of them. `org/h2/compress/CompressLZF.
    // compress(Ljava/nio/ByteBuffer;I[BI)I` -- the single hottest method in
    // H2's `TestFileSystem` `nioMemLZF:` case -- was refused at its main loop
    // header (`entry_pc=220`, mask `0x201`: `this` and one temporary, neither
    // sharing a register with anything live) and so never ran compiled at all
    // (2026-07-27).
    //
    // Set CRATONVM_JIT_OSR_DEAD_MASK_BLANKET=1 to restore the old
    // flag-every-dead-local behaviour.
    let blanket_dead_mask =
        cratonvm_types::flags::runtime_var_os("CRATONVM_JIT_OSR_DEAD_MASK_BLANKET").is_some();
    let resident = reg_resident | xmm_resident;
    // `local <-> home register` lookup, GPR and XMM kept apart: they are
    // different register files and can never alias each other.
    let gpr_home = |i: usize| osr_local_assignments.get(i).copied().flatten();
    let xmm_home = |i: usize| osr_xmm_assignments.get(i).copied().flatten();
    let mut osr_dead_mask = vec![0u64; code_len + 1];
    for &(pc, live_in) in &compiler.osr_block_live_in {
        if pc >= osr_dead_mask.len() {
            continue;
        }
        let dead = resident & !live_in;
        if blanket_dead_mask || dead == 0 {
            osr_dead_mask[pc] = dead;
            continue;
        }
        let live_resident = resident & live_in;
        let mut hazardous = 0u64;
        for i in 0..64 {
            if (dead >> i) & 1 == 0 {
                continue;
            }
            let (dg, dx) = (gpr_home(i), xmm_home(i));
            for j in 0..64 {
                if (live_resident >> j) & 1 == 0 {
                    continue;
                }
                let shares = (dg.is_some() && dg == gpr_home(j))
                    || (dx.is_some() && dx == xmm_home(j));
                if shares {
                    hazardous |= 1u64 << i;
                    break;
                }
            }
        }
        osr_dead_mask[pc] = hazardous;
    }
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_OSR_META").is_some() {
        // Report the mask that is actually published, alongside the blanket
        // "every dead register-resident local" set it is refined from, so the
        // two can be compared directly. Printing a separately recomputed
        // blanket value made this diagnostic silently disagree with the real
        // metadata once the refinement landed.
        let mut blanket: Vec<(usize, u64)> = Vec::new();
        let mut published: Vec<(usize, u64)> = Vec::new();
        for &(pc, live_in) in &compiler.osr_block_live_in {
            let b = (reg_resident | xmm_resident) & !live_in;
            if b != 0 {
                blanket.push((pc, b));
            }
            let p = osr_dead_mask.get(pc).copied().unwrap_or(0);
            if p != 0 {
                published.push((pc, p));
            }
        }
        if !blanket.is_empty() || !published.is_empty() {
            eprintln!(
                "[osr-meta] gpr_resident={reg_resident:#x} xmm_resident={xmm_resident:#x} \
                 blanket_entries={blanket:x?} published_entries={published:x?} \
                 unblocked={}",
                blanket.len() - published.len()
            );
        }
    }
    // Indexed by the same interpreter bci as `osr_pc_to_native` above, so it
    // needs the same coordinate change and the same image choice — a mask
    // read for one copy while the entry jumps into another would skip loading
    // a local that IS live at the entry it actually takes. A bci with no
    // steady-state image keeps a zero mask, which is never read: the entry is
    // already refused by the `-1` in `osr_pc_to_native`.
    let osr_dead_mask = match &loop_xform {
        Some(x) => {
            let mut rebuilt = vec![0u64; orig_code_len + 1];
            for (bci, slot) in rebuilt.iter_mut().enumerate() {
                if let Some(image) = x.osr_entry_pc(bci) {
                    *slot = osr_dead_mask.get(image).copied().unwrap_or(0);
                }
            }
            rebuilt
        }
        None => osr_dead_mask,
    };
    // ── The OSR entry-metadata contract ──────────────────────────────
    //
    // Everything above built four vectors in THREE different coordinate spaces
    // (interpreter bci, output pc, local index) and moved two of them between
    // spaces. `osr_contract::check_at_publication` is the one place that says,
    // in code rather than in a comment, that the results agree.
    //
    // Fail closed, per `docs/feature-designs/jit-osr-entry-metadata.md`: on a
    // violation publish NO OSR metadata at all rather than a set whose pieces
    // disagree. `can_osr_enter` then answers false everywhere and the method
    // runs to completion in the interpreter, which is always valid. Over-
    // refusal costs an optimisation; under-refusal re-runs loop iterations or
    // resumes with the wrong locals.
    //
    // The check that matters is the length of the two bci-indexed vectors:
    // `can_osr_enter_with` reads the dead mask through `.unwrap_or(0)`, so a
    // short mask reads as "no dead locals" for every bci in the tail and admits
    // entries that must be refused. Nothing downstream can notice.
    let osr_metadata_agrees = crate::osr_contract::check_at_publication(
        cm.osr_pc_to_native.as_deref().unwrap_or(&[]),
        &osr_dead_mask,
        &osr_local_assignments,
        &osr_xmm_assignments,
        compiler.num_locals,
        &method_label,
    );
    //
    // Clearing the fields rather than returning early: everything below this
    // point publishes NON-OSR state (oop maps, deopt points, frame layout, the
    // epoch guard). An early return would drop all of it and turn a metadata
    // disagreement into a much larger regression than the one it prevents.
    if osr_metadata_agrees {
        cm.osr_dead_mask = Some(osr_dead_mask);
        cm.osr_local_assignments = Some(osr_local_assignments);
        cm.osr_xmm_assignments = Some(osr_xmm_assignments);
    } else {
        cm.osr_pc_to_native = None;
        cm.osr_dead_mask = None;
        cm.osr_local_assignments = None;
        cm.osr_xmm_assignments = None;
    }
    cm.osr_frame_size = compiler.frame_size;
    cm.osr_callee_saved_base = compiler.callee_saved_base;
    // HIB-CV-20 OSR caller-corruption fix: hand the trampoline the EXACT
    // callee-saved sets the epilogue restores (GPR + XMM), so it spills the
    // caller's value for every one of them at the matching slot index. The
    // prologue/epilogue spill/restore `alloc_used_regs` / `alloc_used_xmms` (the
    // full allocator-used set, which can include callee-saved regs used for
    // operand-stack temporaries — NOT just locals); the old trampoline only
    // spilled a local_assignments-derived subset, so any non-local callee-saved
    // register was restored from the wrong (or an uninitialised) slot, silently
    // corrupting the OSR caller's live registers after return.
    cm.osr_callee_saved_regs = Some(compiler.alloc_used_regs.clone());
    cm.osr_callee_saved_xmms = Some(compiler.alloc_used_xmms.clone());
    cm.osr_xmm_saved_base = compiler.xmm_saved_base;
    cm.method_label = method_label;
    cm.shadow_savebase_slot_off = compiler.shadow_savebase_slot_off;
    cm.frame_layout = frame_layout;
    cm.osr_heap_local_offset = compiler.heap_local_offset;
    cm.jit_thread_slot_off = compiler.jit_thread_slot_off;
    cm.stack_floor_slot_off = compiler.stack_floor_slot_off;
    cm.osr_frame_record = compiler.helpers.frame_record;

    // T1.1.a — transfer precise oop maps collected during codegen.
    // The GC root walker's `JitEntryGuard::enter_with_compiled` path
    // checks `CompiledMethod::has_precise_oop_maps()` to decide
    // whether to use them for this frame; when empty, it falls back
    // to the conservative stack scan for that frame — always a
    // correct super-set of the precise coverage.
    cm.oop_maps = compiler.oop_maps;
    // deopt-osr Step 1: transfer precise deopt-exit snapshots collected at
    // eligible guards (currently the speculative-BCE loop-header guard). No live
    // path consumes these yet — emit-and-discard until the in-stub trampoline +
    // resume land (real-frame-deopt-x64-backport Steps 2-4) — so this is inert
    // (find_deopt_point has no live caller; the i64::MIN re-run is unchanged).
    cm.deopt_points = compiler.deopt_points;
    cm._deopt_point_boxes = compiler.deopt_boxes;
    // deopt-osr Step 9 follow-up (a): hand the retained epoch guard (baked as the
    // 4th arg into every frame-deopt stub) to the artifact so the VM can stamp it
    // (creation epoch + live-epoch cell) at install. Null on production artifacts
    // (no frame-deopt stub emitted unless `deopt_real_enabled()`).
    cm.deopt_epoch_guard = compiler.deopt_epoch_guard;
    // deopt-osr x64-backport Step 5 — finalize the per-method deopt-resume
    // coverage gate (mirrors `fully_oop_covered` / `can_osr_exit`). A method may
    // resume a real-frame deopt only when:
    //   1. it emitted at least one deopt-exit snapshot (`deopt_points`), and
    //   2. it scalar-replaced NO objects (`scalar_replaced` empty).
    // (2) is load-bearing: this backend records a scalar-replaced slot by its
    // machine provenance (Register/StackSlot), NOT as a `VirtualObject`, so its
    // snapshot cannot be re-materialized — and lock elision over such an object
    // makes mid-method resume unsound (the elided-monitor hazard). Until the x64
    // emitter writes `VirtualObject` deopt slots + an elided-monitor flag, a
    // scalar-replacing method stays on the safe re-run path. Empty/false unless
    // `deopt_real_enabled()` (the snapshot emit site is gated), so production
    // artifacts are unchanged. Consumed at the interpreter deopt sink, which
    // attempts `resume_real_ir_deopt` only when `compiled.can_deopt_resume`.
    //
    // P2.1/P2.2 (cat-2 + FP resume): the snapshot now has a per-slot WIDTH source
    // (`classify_local_kinds`) and emits typed `RegisterLong`/`StackSlotLong`
    // (long), `XmmFloat`/`XmmDouble`/`StackSlotFloat`/`StackSlotDouble` (FP) values
    // that the resume mapper reconstructs as full-width `Value::Long`/`Float`/
    // `Double`; the deopt stub spills XMM0..15 (under the gate) so the XMM-resident
    // FP forms resolve. P2.0's blanket wide-local exclusion is fully lifted. Any
    // slot the classifier can't type (Ambiguous / a register-resident ref /
    // contradiction) emits `Unsupported`, and the mapper re-runs the whole method
    // for it — the per-slot fine-grained safety net behind this coarse gate.
    //
    // Phase B (real-frame-deopt x64 backport): the scalar-replacement exclusion is
    // RELAXED. The snapshot builder now emits `FrameValue::VirtualObject` /
    // `VirtualObjectRef` for a scalar-replaced object live in a local at a deopt
    // point (its fields read from their frame slots, typed by `sr_field_types`),
    // which the VM materializer (`deopt_materialize::materialize_virtual_objects`)
    // rebuilds on resume. So a scalar-replacing method MAY resume — UNLESS it
    // elided a `monitorenter`/`monitorexit` over a scalar object (`has_elided_monitor`):
    // an elided lock leaves no `monitors` trace, so the resume would skip the
    // re-lock (the elided-monitor hazard, deferred to Phase C). `ACC_SYNCHRONIZED`
    // is independently caught by the VM resume sink's `is_synchronized` bail.
    cm.can_deopt_resume = !cm.deopt_points.is_empty() && !compiler.has_elided_monitor;
    // deopt-osr Step 7 — transfer the OSR-exit loop-boundary bci set and set the
    // per-method gate. Both are empty/false unless `deopt_real_enabled()` was on
    // (the emit site is gated), so production artifacts are unchanged. Step 8
    // consults `can_osr_exit` + `osr_exit_points` (under `CRATONVM_DEOPT_REAL`)
    // to route a mid-loop bail through the deopt trampoline.
    // Bcis, consumed by the VM's OSR-exit route. Provably empty under a
    // bytecode rewrite (`emit_osr_exit_map_at` fires only when
    // `deopt_real_enabled()`, and the indy site is refused), so the
    // translation arm is a backstop rather than a live path. `filter_map`
    // drops a pc with no provenance instead of publishing it raw.
    cm.osr_exit_points = match &loop_xform {
        Some(x) => compiler
            .osr_exit_points
            .iter()
            .filter_map(|&pc| x.bci_at(pc))
            .collect(),
        None => compiler.osr_exit_points,
    };
    // FIX: mirror `can_deopt_resume`'s elided-monitor exclusion above — an
    // OSR-exit transfer materializes the same kind of reconstructed frame, so
    // a scalar-replaced object held under an elided `synchronized` block is
    // the same unsound-resume hazard here as it is for `can_deopt_resume`.
    cm.can_osr_exit = !cm.osr_exit_points.is_empty() && !compiler.has_elided_monitor;
    // jit-invokedynamic-groovy-regression fix: a compiled 0xba site is an
    // UNCONDITIONAL trap (the instruction is never JIT-executed), so any
    // execution of this artifact that reaches it deopts. Publishing this
    // artifact's entry where MACHINE CODE calls it directly (a baked
    // JIT→JIT direct call, a MIC/PIC inline-cache entry) would let the
    // sentinel + stashed frame bail through a compiled CALLER's epilogue,
    // where no consumer can resume the callee precisely (the caller's
    // continuation is already lost). The publication gates (see
    // `callee_compiler` in vm/src/runtime/interpreter.rs and the MIC/PIC
    // install sites in vm/src/jit/helpers.rs) consult this flag so every
    // call to such a method stays on a dispatch helper, whose
    // `try_resume_trapped_callee` resolves the trap precisely in place.
    // Only sites that actually lower to a trap count. A fully bridged
    // method (every indy is a StringConcatFactory call) carries no trap, so it
    // must not be forced onto the dispatch-helper path for its callers.
    cm.has_indy_trap = {
        let concat_entry = crate::INDY_STRING_CONCAT_FN.load(std::sync::atomic::Ordering::Relaxed);
        compiler
            .indy_info
            .iter()
            .any(|(_pc, _arg_slots, ret_type, _tags, concat_site)| {
                !(*concat_site != 0 && concat_entry != 0 && matches!(*ret_type, b'L' | b'['))
            })
    };
    // Stage 3 — the frame offset where this method stores the active
    // safepoint's bytecode PC (0 when the precise gate was off at compile).
    cm.sp_id_slot_off = compiler.sp_id_slot_off;
    // Stage A.2 (precise oop maps, B-K fix) — a method is "fully precisely
    // covered" only when EVERY GC-capable safepoint that flushed its
    // register-locals (`safepoint_pcs`) also recorded a precise oop map
    // (`mapped_safepoint_pcs`), the precise gate is on (so the sp-id slot
    // exists and the per-safepoint id is stored), and there is no construct the
    // current mapping cannot describe (inlined-callee safepoints). OSR entry
    // used to be a coverage breaker because it bypassed the prologue's shadow
    // and exact-RBP setup; the OSR trampoline now mirrors both before jumping to
    // the loop body, so OSR artifacts use the same completeness predicate.
    // Stage B consults this to decide whether the GC may skip the conservative
    // backstop for this frame and treat its precise oops as movable. It is a
    // NECESSARY codegen precondition; the runtime `CRATONVM_DBG_VERIFY_OOP_MAPS`
    // oracle (Stage G0) is the SUFFICIENT proof that must gate the actual
    // backstop suppression before the moving path relies on it. Always `false`
    // on the default path (`sp_id_slot_off == 0`), so it is inert until the gate
    // is on AND Stage B lands.
    cm.fully_oop_covered = compiler.precise_maps
        && compiler.sp_id_slot_off != 0
        && compiler.inline_sites.is_empty()
        && compiler
            .safepoint_pcs
            .is_subset(&compiler.mapped_safepoint_pcs);
    // Shadow-stack — frame offsets + thread-struct offset, so the OSR trampoline
    // can replicate the prologue's shadow setup (cache the thread ptr + snapshot
    // the `top` watermark) for OSR-entered frames. All 0 when shadow-stack
    // support was off at compile.
    cm.shadow_thread_slot_off = compiler.shadow_thread_slot_off;
    cm.shadow_savetop_slot_off = compiler.shadow_savetop_slot_off;
    cm.shadow_off_in_thread = compiler.shadow_off_in_thread;

    // Task #60 — attach unroll-cloned MIC/PIC slots to the
    // CompiledMethod so they outlive the compiled code. The imm64
    // baked into duplicated `MOV R10, imm64` instructions is a raw
    // pointer to one of these boxes; without keeping them alive on the
    // CompiledMethod, the first GC of the Box would invalidate the
    // pointer and the next invokevirtual on an unrolled copy would
    // dereference freed memory.
    //
    // The caller-supplied slots (from `lib.rs::try_compile`) remain
    // owned by the caller and are attached to `_jit_mic_slots` /
    // `_jit_pic_slots` separately on the lib.rs side. This `extend`
    // is purely additive — both vectors retain their previous
    // contents.
    cm._jit_mic_slots.extend(compiler.cloned_mic_slots);
    cm._jit_pic_slots.extend(compiler.cloned_pic_slots);

    Some(cm)
}

/// Estimate the maximum operand stack depth for the method.
/// Simple conservative estimate: count push-like opcodes.
fn estimate_max_stack(code: &[u8], code_len: usize) -> usize {
    let mut max_depth = 0usize;
    let mut depth = 0usize;
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            // Push operations (aconst_null, load const/local/ref, bipush, sipush → +1)
            0x01..=0x14 | 0x15..=0x19 | 0x1a..=0x2d => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Pop operations (binary ops pop 2, push 1 → net -1)
            0x60..=0x71 | 0x78..=0x83 | 0x94..=0x98 => {
                depth = depth.saturating_sub(1);
            }
            // Store ops pop 1 (i/l/f/d/astore, i/l/f/d/astore_N)
            0x36..=0x4e => {
                depth = depth.saturating_sub(1);
            }
            // Array load: pop 2 (array, index), push 1 → net -1
            0x2e..=0x35 => {
                depth = depth.saturating_sub(1);
            }
            // Array store: pop 3 (array, index, value) → net -3
            0x4f..=0x56 => {
                depth = depth.saturating_sub(3);
            }
            // getstatic: push 1 (value) → net +1
            0xb2 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // putstatic: pop 1 (value) → net -1
            0xb3 => {
                depth = depth.saturating_sub(1);
            }
            // getfield: pop 1 (objectref), push 1 (value) → net 0
            0xb4 => {}
            // putfield: pop 2 (objectref, value) → net -2
            0xb5 => {
                depth = depth.saturating_sub(2);
            }
            // newarray: pop 1 (count), push 1 (ref) → net 0
            0xbc => {}
            // new: push 1 object ref (no pop) → +1
            0xbb => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // anewarray: pop 1 (count), push 1 (ref) → net 0
            0xbd => {}
            // arraylength: pop 1 (ref), push 1 (int) → net 0
            0xbe => {}
            // Return pops 1 (ireturn, lreturn, freturn, dreturn, areturn)
            0xac..=0xb0 => {
                depth = 0;
            }
            // void return
            0xb1 => {
                depth = 0;
            }
            // Unary ops (neg, conversions): pop 1, push 1 → net 0
            0x74..=0x77 | 0x85..=0x93 => {}
            // Branch pops (ifXX pop 1, if_icmpXX pop 2, if_acmpXX pop 2)
            0x99..=0x9e => {
                depth = depth.saturating_sub(1);
            }
            0x9f..=0xa6 => {
                depth = depth.saturating_sub(2);
            }
            // ifnull/ifnonnull pop 1
            0xc6 | 0xc7 => {
                depth = depth.saturating_sub(1);
            }
            // checkcast: pop 1, push 1 → net 0
            0xc0 => {}
            // instanceof: pop 1, push 1 → net 0
            0xc1 => {}
            // Pop
            0x57 => {
                depth = depth.saturating_sub(1);
            }
            // Pop2
            0x58 => {
                depth = depth.saturating_sub(2);
            }
            // Dup: +1
            0x59 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // dup_x1 / dup_x2 add one copy of the top operand.
            0x5a | 0x5b => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // dup2* can add two category-1 slots; category-2 values are still
            // one slot in this x64 operand-stack model.
            0x5c..=0x5e => {
                depth += 2;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // Swap: 0
            0x5f => {}
            // iinc: 0
            0x84 => {}
            // goto: 0
            0xa7 => {
                depth = 0;
            }
            // jsr pushes a returnAddress in legacy bytecode.
            0xa8 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // invokestatic/invokevirtual/invokespecial/invokeinterface: conservatively
            // assume they push 1 result (pops are hard to estimate without descriptors)
            0xb6..=0xb9 => {
                depth += 1;
                if depth > max_depth {
                    max_depth = depth;
                }
            }
            // multianewarray: pops ndims, pushes 1 → net -(ndims-1)
            0xc5 => {
                let ndims = code.get(pc + 3).copied().unwrap_or(2) as usize; // Cast: address arithmetic
                depth = depth.saturating_sub(ndims.saturating_sub(1));
            }
            _ => {}
        }
        // Advance PC via the canonical length table. The ad-hoc copy this
        // replaces was missing `ldc` (0x12) and treated tableswitch/
        // lookupswitch as 1-byte, so the walk stepped through operand bytes
        // (incl. switch pad/offset tables) as phantom opcodes; a phantom
        // return zeroed `depth` and could UNDER-estimate the frame's operand
        // stack (the CM-FASTMATH length-table desync family).
        let len = bytecode_len_at(code, pc);
        if len == 0 {
            break;
        }
        pc += len;
    }
    // Add safety margin (conservative for invoke stack effects not tracked above)
    max_depth + 4
}

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Flag-skew and header-offset contracts
// ---------------------------------------------------------------------------
//
// Companion doc: `docs/internal/arch-2026-07-26/x64-flag-skew-and-contracts.md`.
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
