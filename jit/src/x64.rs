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
mod objects;
mod arrays;
mod simd;
mod arith;
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
    // Object headers, fields, allocation, string layout
    // -----------------------------------------------------------------------
    //
    // Moved to `x64/objects.rs`.



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
