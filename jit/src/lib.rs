// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT compiler for hot bytecode methods.
//!
//! Compiles self-contained JVM bytecode methods to native x86-64 machine code.
//! Methods are classified as "pure" (int/long only) or "context" (VM-interacting).
//! Pure methods are called directly; context methods receive a SharedVm pointer as a
//! hidden first argument, enabling JIT-compiled array allocation, field access, type
//! checks, and method dispatch.
//!
//! ## Architecture
//!
//! - `JitCache`: maps method identity (class_name, method_name, descriptor) to compiled code
//! - `CompiledMethod`: holds executable memory, entry point, and `needs_context` flag
//! - `compile_method`: analyzes bytecode, emits x64 code, patches self-calls
//! - `ExecutableBuffer`: platform-specific executable memory (VirtualAlloc on Windows)
//!
//! ## Internal JIT calling convention (NOT the platform ABI)
//!
//! JIT-compiled methods do **not** follow the SysV (System V AMD64) or
//! Win64 ABI for argument passing. They use a private convention that is
//! only valid between two CratonVM-JIT-compiled functions:
//!
//! - **All** Java arguments — including `float` and `double` — are passed
//!   through the general-purpose argument registers (`ARG_REGS`). Floating
//!   point arguments are transmitted as their raw 64-bit IEEE-754 bit
//!   pattern in a GPR, *not* in an XMM register as the platform ABI
//!   requires. The callee prologue moves each FP parameter from its GPR
//!   into an XMM register via `movq` (see `x64.rs`, `emit_movq_xmm_from_rax`
//!   in the prologue). Stack-passed arguments (when the register file is
//!   exhausted) are likewise raw 64-bit slots.
//! - "context" methods additionally receive a hidden `SharedVm` pointer in
//!   `ARG_REGS[0]`, shifting all Java args by one register.
//!
//! WARNING: because of this, a function pointer obtained from any source
//! other than CratonVM's own JIT (a libc symbol, a C-compiled callback, a
//! function produced by another compiler) **must never** be entered with
//! this convention, and conversely a CratonVM JIT entry point must never
//! be handed to external code expecting the platform ABI. The argument
//! registers, FP-in-GPR encoding, and stack layout are all incompatible.
//! Cross-ABI calls must go through an explicit thunk that re-marshals
//! arguments.
//!
//! ## SAFETY INVARIANT: GC must conservatively re-sweep every JIT frame
//!
//! The JIT register allocator's callee-saved GPR local homes are default-**ON**
//! whenever precise JIT maps (or moving-young) are active, which is the default
//! — see `x64::callee_saved_gpr_local_homes_enabled`, whose env override
//! `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0` is now the *opt-out*. (This
//! paragraph said "default-off" until 2026-07-27; the default flipped back with
//! `precise_jit_maps_enabled` on 2026-07-07 and the prose was not updated.
//! `skip_list.rs` carried a second, private copy of this switch that kept its
//! own default of `false`, which is how a ~950-line ban list ended up inert for
//! weeks — see is-known-miscompile-block-retired-20260727.md.)
//! With that allocator active, a Java local
//! (including an object reference) may live **exclusively in a callee-saved GPR**
//! between bytecode aload/astore opcodes — the value need not be present in the
//! frame's local slot at any given native PC. The precise oop map (`OopMapEntry`)
//! describes only *frame-slot* oops; it has **no register-oop bitmap**. Therefore
//! a register-resident oop is invisible to any GC scan that walks frame memory
//! alone.
//!
//! Correctness of the legacy opt-in path depends on two cooperating mechanisms,
//! and **both must be kept**:
//!
//! 1. Before every safepoint-causing call, the code generator spills every
//!    register-resident local back to its canonical frame slot
//!    `[rbp - (idx+1)*8]` (see `x64.rs::emit_pre_safepoint_spill`).
//! 2. `conservative_roots::scan_one_frame_precise` performs a *full
//!    conservative sweep* of the entire JIT frame region (validating each
//!    qword via `heap.is_object_address`), in addition to consulting the
//!    precise oop map.
//!
//! The precise oop map is a pure optimization layered on top of the
//! conservative sweep. **Do not** remove or weaken the conservative
//! frame sweep, and **do not** remove the pre-safepoint spill, unless a
//! genuine register-oop map is added to `OopMapEntry` *and* consumed by
//! the GC scanner. Removing either one in isolation would let
//! register-resident oops escape GC root scanning, causing live objects
//! to be reclaimed and the heap to corrupt.

// Every `unsafe` block states why it is sound (jit review 2026-09-12). The
// workspace allows the lint; this crate generates and enters machine code,
// so it opts in.
#![deny(clippy::undocumented_unsafe_blocks)]

pub use cratonvm_jit_aarch64 as aarch64;
pub mod aarch64_backend;
pub mod backend_parity;

/// The `cratonvm.JitCompileDecision` JFR producer, split out of this file.
mod jfr_compile_decision;
pub use jfr_compile_decision::*;
/// Validated OSR entry and the OSR trampoline, split out of this file.
mod osr_entry;
pub use osr_entry::*;
/// The polymorphic inline cache slot, split out of this file.
mod inline_cache_pic;
pub use inline_cache_pic::*;
/// The IR ↔ escape-analysis bridge and the scalar-replacement rewrite,
/// split out of this file.
mod ea_ir_bridge;
pub use ea_ir_bridge::*;
/// Executable memory: the `ExecutableBuffer` that owns every W^X mapping in
/// this crate, and the `Drop` that is the only code allowed to unmap one.
/// Split out of this file.
mod exec_memory;
pub use exec_memory::*;
/// Not-entrant compiled bodies: the patchable entry pad, the stub a patched
/// entry jumps to, and the redefinition's patch pass (interpreter round i1
/// wave 22, lane L6). See the module header.
mod not_entrant;
pub use not_entrant::*;
/// One cache's superseded bodies, for the superseded half of an
/// invalidation (j27 wave 4, lane GLOBALS; R15B-23). See the module header.
mod superseded_bodies;
/// Loop-shaped single-pass intrinsic sites the optimizing tier keeps as a
/// call under an opt-in switch (j27 wave 8, lane W8REV). See the module header.
mod ir_loop_intrinsic_call;
/// Per-method invocation counting from inside compiled code — the live count
/// the tiered manager's own counter stops being once a body is published.
/// Default-ON since round 9 wave 3 (priced in `NOTES-w3-tiering3.md`);
/// `CRATONVM_JIT_ENTRY_COUNTER=0` is the kill switch. See the module header.
pub mod entry_counter;

/// The object-layout-constant emission tripwire, split out of this file.
mod layout_const_inventory;
pub use layout_const_inventory::*;
/// Method-descriptor and opcode classification — the pure spec decoders the
/// tier gates ask "what shape is this method?". Split out of this file.
mod descriptor_shapes;
pub use descriptor_shapes::*;
/// The call-site intrinsic catalogue and its `usize::MAX - N` sentinel
/// encoding, including the frozen `MATH_*_INTRINSIC` aliases that encoding
/// must keep honouring. Split out of this file.
mod intrinsic_catalogue;
pub use intrinsic_catalogue::*;
pub mod bailout;
// The one bytecode decoder both tiers share: instruction lengths, branch and
// switch targets, reachability, and an instruction CFG with dominators.
pub mod bytecode_analysis;
// The IR splice planner's visiting order (`CRATONVM_JIT_IR_SPLICE_TAIL_EXITS_LAST`).
mod ir_splice_order;
// j27 wave 7 (lane T1STATICFWD, STATICFWD21-1): a spliced loop's blind
// primitive `putstatic` that a replay re-runs harmlessly.
pub(crate) mod ir_splice_static_replay;
// j27 wave 8 (lane W8T1, STATICFWD21-1 reference half): a spliced loop's
// committed `putstatic` left to the chain fences.
pub(crate) mod ir_splice_static_chain;
// j27 wave 3 (lane SMALLCALL): the `osr-splice-guarded-refused` line's hint.
mod osr_guarded_refusal_hint;
/// j27 wave 5 (lane STRPIN): frame-named pure values scheduled before the
/// deopt point that publishes the frame (`ir_schedule` step 5).
mod sched_named_before_anchor;
// j27 wave 4 (lane OSRSUPER): a bound `super.m()` inside a guarded OSR body.
pub mod osr_guarded_super_bind;
// Every input of one method-entry compilation, passed by reference.
pub mod compile_request;
pub use compile_request::{CompileRealm, CompileRequest, StaticVarHandleField};
// The VM's thin direct-call helper addresses for one compile.
/// JIT round 19 proposal wave 4, lane strgap4 (STRGAP19-3): the compile-time
/// plan of an inline `StringConcatFactory` concatenation.
pub mod concat_inline_plan;
/// JIT round 21 wave 20, lane sbfuse (STRINTR21-4): an explicit
/// `new StringBuilder().append(..)...toString()` chain fused into one concat.
pub mod sb_chain_fuse;
// j27 lane HANGHUNT: when the inline-cache profile seed asks and spends its recompile.
pub mod ic_seed_claim;
pub mod direct_helpers;
// j27 wave 6 lane T1DISP20 (DISP20-8): the `Unsafe` reference-array element arm.
pub mod unsafe_ref_array_arm;
pub mod ffm_element_inline;
pub use direct_helpers::{ReflectInvokeCache, REFLECT_RET_BOOL, REFLECT_RET_INT, REFLECT_RET_OBJ, REFLECT_RET_VOID, 
    is_scoped_memory_op, is_unsafe_accessor_op, DirectHelperTable, ScopedMemoryOp, UnsafeAccessorOp,
};
// The one door every backend entry point must pass through. There are THREE
// doors (method entry, the eager first-call compile, OSR), and only the first
// ever asked the admission questions; the other two grew hand-copied subsets
// of them. See `docs/feature-designs/jit-osr-entry-metadata.md` step 3.
// Publication/retirement events for profilers and debuggers, and the three
// sinks behind them (perf map, jitdump, GDB JIT interface).
pub mod code_events;
pub mod compile_gate;
pub mod deopt;
pub mod escape_analysis;
pub mod frozen_facts;
pub mod gdb_jit;
pub mod gpu_barrier;
pub mod instrument_census;
pub mod ir;
pub mod ir_check_elim;
pub mod ir_evidence;
// j27 wave 7, lane T1ACCEPT (ACCEPT21-1): the measuring gate's two-body window
// decision; fed by TIERMGR21-4's timing hook.
pub mod accept_measure;
// JIT round 21, lane vhcae (VHCAE21-1): the compiled G1 post barrier's edge filter.
pub mod ir_g1_edge_filter;
pub mod ir_lower;
pub mod ir_optimize;
pub mod ir_schedule;
pub(crate) mod ir_vector_loop;
pub mod jitdump;
// j27 wave 4, lane SPLICECAT: concat bridge sites as splice indy rows.
pub mod splice_concat_bridge;
pub mod perf_map;
// The OSR entry-metadata contract, as executable checks — see
// `docs/feature-designs/jit-osr-entry-metadata.md`. Kept out of this file
// deliberately: it must be testable against synthetic vectors, and a check
// that can only be handed a real `CompiledMethod` cannot be.
pub mod osr_contract;
// The OSR *exit* side of the same argument — where an exit landed relative to
// the recorded loop-boundary maps, and whether its resume bci names one image
// or several. See `docs/feature-designs/jit-osr-exit-and-recompile.md`. Kept
// out of this file for the same reason as `osr_contract`: it is tested against
// synthetic point lists, not against whatever a real compile produced.
pub mod osr_exit;
// The two OSR pc spaces, kept apart by the type system. `osr_contract` catches
// the consequence (vectors that disagree); this catches the cause (an integer
// in the wrong space).
pub mod implicit_null;
// t1 wave 8, lane SAFEPOLL (`CRATONVM_JIT_PAGE_POLL`): the page form of the poll.
pub mod poll_page;
pub mod ir_verify;
pub mod jdk_only_sink;
pub mod lambda_adapter;
pub mod loop_analysis;
pub mod metrics;
// JIT round 21 wave 4 (lane monitors): block-monitor balance, read by the
// single-pass admission gate and the interpreter's structured locking.
pub mod monitor_balance;
pub mod null_check_elim;
pub mod offload_hook;
pub mod osr_coords;
pub mod platform;
pub mod profile;
pub mod profile_store;
pub mod range_analysis;
pub mod regalloc;
pub mod runtime_lowering;
pub mod scev;
// JIT round 19 (lane selflock19): SYNCDOOR618-2 / SDB17-2, per-VM.
pub mod self_lock_census;
// j27 wave 1 (lane TYPECHK): the `ThreadLocal` thin binds' override decline.
pub mod thread_local_thin_bind;
// j27 wave 3 (lane CLONE): the virtual `clone()` thin bind.
pub mod clone_thin_bind;
// j27 wave 4 (lane ARGFIX2): owned arguments at a spliced call (two-level replay).
mod owned_args_splice_replay;
// j27 wave 8 (lane W8OSR): the OSR term of dev's service-copy window (opt-in).
pub mod osr_service_copy_window;
// j27 wave 6 (lane T1LAM2): a bound STATIC constant handle's compiled call.
mod ir_mh_bound_static;
// j27 wave 7 (lane T1LAM3, LAMBDA321-3): why a lambda site got no impl-call plan.
mod lambda_impl_site_skip;
// j27 wave 8 (lane W8LOOP): branch-row fill at compile time (race B census).
mod never_taken_row_census;
// j27 wave 1 (lane TYPECHK): the referent-read pins.
#[cfg(test)]
mod j27_typechk_tests;
// j27 wave 5 (lane REFS): referent reads on a heap whose ZGC gate may arm.
#[cfg(test)]
mod j27_refs_tests;
// j27 wave 4 (lane LATCHES): compile-time switch latches read per call.
#[cfg(test)]
mod j27_latches_tests;
// j27 wave 4 (lane MEMOSLOT): per-op compile switches memoised in a `MemoSlot`.
// Public since j27 wave 7 (lane T1IC): the VM's helper switches use it too.
pub mod memo_flag;
#[cfg(test)]
mod j27_memoslot_tests;
// j27 wave 5 (lane DROPGAP): a frameless hand-off's target is owned through the queue.
#[cfg(test)]
mod j27_dropgap_tests;
// JIT round 21, lane str21b (STRINGS2-21-4): the Latin-1 regionMatchesCI prefix's naming and switch.
#[cfg(test)]
mod str21b_tests;
pub mod tiered;
// JIT round 21, lane dispatch: the per-VM type-check rows (`CRATONVM_JIT_TYPECHECK_ROWS`).
pub mod typecheck_rows;
// JIT tier2 wave 4 (page agent bar21): GC barrier contracts of compiled reference stores.
#[cfg(test)]
mod bar21_tests;
pub mod x64;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use std::hash::Hasher;
pub use cratonvm_jit_api::{count_param_slots, CachedBytecodeMethod, JitRuntimeHelpers};
#[allow(unused_imports)]
use cratonvm_types::{
    ObjectRef, Value, ARRAY_LENGTH_OFFSET, HEADER_SIZE, REF_ELEMENT_SIZE, SLOT_SIZE,
};

// ---------------------------------------------------------------------------
// Code-cache allocation-failure ledger
// ---------------------------------------------------------------------------
//
// WHY THE COUNTERS LIVE HERE AND NOT IN `vm::jit::code_cache_lifecycle`.
//
// That module owns the report an operator reads, and it already had three
// `record_*` entry points for these gauges — but nothing in the workspace
// could ever call them, and the reason is the crate graph, not an oversight:
// `cratonvm-vm` depends on `cratonvm-jit` (see `vm/Cargo.toml`), and there is
// no edge back. Every site that can actually refuse a code-cache allocation is
// inside `cratonvm-jit` (`platform::alloc_executable`, the arena's
// `ExecutableBuffer::new_in`, the cap gate above, the code-buffer-estimate
// discard in `try_compile`), so a push from the allocation site to the report
// is not expressible. Adding a `cratonvm-jit -> cratonvm-vm` dependency to
// make it expressible would make the workspace's two largest crates mutually
// dependent, which Cargo forbids outright.
//
// So the direction is inverted, exactly as §5 of `code_cache_lifecycle`'s own
// module header already did for installs, withdrawals, queued/reclaimed bytes
// and drain counts: the JIT keeps the numbers, and the report PULLS them
// through [`jit_code_alloc_failure_stats`]. That is the same shape as
// [`jit_code_reclamation_stats`] and it costs the allocation paths one relaxed
// `fetch_add` on the failure branch only.
//
// The reason codes below are a deliberate mirror of
// `vm::jit::code_cache_lifecycle::alloc_failure`. The duplication is forced by
// the same missing crate edge; it is made safe by a `const` assertion on the
// VM side that the two numberings agree, which turns any future drift into a
// compile error rather than into mislabelled buckets.

/// Why a code-cache allocation was refused.
///
/// Producer-side mirror of `vm::jit::code_cache_lifecycle::alloc_failure`,
/// which owns the human-readable labels and the report. Numbering is stable
/// and is checked against that module at compile time; append new codes and
/// bump `COUNT` in BOTH places.
pub mod code_alloc_failure {
    /// The OS refused the mapping (`mmap`/`VirtualAlloc` returned null).
    pub const OS_REFUSED: usize = 0;
    /// The configured code-cache cap would have been exceeded.
    pub const CAP_EXCEEDED: usize = 1;
    /// Enough total free space, but no single extent large enough.
    ///
    /// **Never recorded by this crate as it stands, and that is a property of
    /// the allocator rather than a gap in the instrument.** The shared code
    /// arena does not refuse for want of an extent: when its free lists and
    /// every region's bump room have all missed, `JitCodeArena::alloc` maps a
    /// fresh region and, failing that, falls back to a standalone mapping — so
    /// its `None` is always an OS refusal, which is what its own doc comment
    /// says. Fragmentation shows up here as extra regions and as
    /// `external_fragmentation` climbing in the census, not as a refusal. The
    /// code is kept (rather than deleted) because the numbering is shared with
    /// the VM-side report and must stay stable, and because an arena that
    /// grows a bounded reservation would produce it. See
    /// `r10-gauges-no-extent-large-enough-is-unreachable-20260921-RESOLVED-20260922.md`.
    ///
    /// **This is a RESERVED code, not an orphaned instrument, and the
    /// difference is what keeps it here.** An orphaned instrument is dangerous
    /// because it reads zero where a reader can see it, and zero is
    /// indistinguishable from "this never happened". This bucket is never
    /// shown at all: `CodeCacheLifecycleReport::allocation_failure_breakdown`
    /// is `.filter(|&(_, n)| n != 0)` before it maps a code to a label, so
    /// `no-free-extent-large-enough` has never appeared in a report and cannot
    /// while nothing produces it. There is no operator to mislead. Deleting it
    /// would renumber `SIZE_ESTIMATE_OVERRUN` from 3 to 2 in two crates at
    /// once and silently mislabel every histogram captured from an older
    /// build — a real cost paid to remove a label nobody has ever seen.
    ///
    /// **If you are about to make this reachable, read this first.** "Bound the
    /// arena's reservation" is NOT sufficient, and doing only that would
    /// reintroduce exactly the misdiagnosis these reason codes exist to
    /// prevent. An arena that stops mapping new regions at the code-cache cap
    /// and then refuses is refusing *for want of the cap*, which is
    /// [`CAP_EXCEEDED`] — the same condition `note_jit_code_cache_cap_refusal`
    /// already files there, just detected one layer lower and after codegen has
    /// run rather than before it. Labelling that `NO_EXTENT_LARGE_ENOUGH` would
    /// send an operator to `external_fragmentation` for a workload whose real
    /// answer is `CRATONVM_JIT_CODE_CACHE_MAX_MB`.
    ///
    /// The condition this code actually names is narrower: a FIXED reservation
    /// (not merely a capped one) in which `free_bytes >= rounded` across the
    /// regions but no single run of free pages is `rounded` long — i.e. the
    /// splitter has produced fragmentation and there is no coalescing to undo
    /// it. That state is reachable only once the arena stops growing on demand,
    /// AND the refusal path can distinguish "no contiguous extent" from "no
    /// space at all". Both halves are required; a bounded arena with only the
    /// first gives you a `CAP_EXCEEDED` wearing a fragmentation label.
    /// `REVIEW-NOTE 3` in `jit/src/platform.rs` asks for `blocks_split` against
    /// `served_from_free_list` and `bytes_free_listed / free_blocks` over time
    /// precisely because that evidence is what would justify building the
    /// coalescing whose absence this code would report.
    pub const NO_EXTENT_LARGE_ENOUGH: usize = 2;
    /// Codegen overran its size estimate and the emitted body was discarded
    /// (`ExecutableBuffer::overflowed`, surfaced as the
    /// `CODE_BUFFER_TOO_SMALL_SITE` bail site).
    pub const SIZE_ESTIMATE_OVERRUN: usize = 3;

    /// One past the highest defined code.
    pub const COUNT: usize = 4;
}

/// Why a published body was withdrawn from a [`JitCache`].
///
/// Producer-side mirror of `vm::jit::code_cache_lifecycle::retire_reason`,
/// which owns the human-readable labels and the report, on exactly the terms
/// [`code_alloc_failure`] above is a mirror of that module's `alloc_failure`:
/// the crate graph forbids `cratonvm-jit` from naming `cratonvm-vm`, the
/// withdrawal sites are all in this crate, so the numbering is duplicated and
/// pinned together by a `const` assertion on the VM side. The histogram is
/// copied POSITIONALLY by `code_cache_lifecycle_raw`, so drift would not fail
/// to compile on its own — it would print `class-unloaded` over a count of
/// supersedes. Append new codes and bump `COUNT` in BOTH places.
///
/// # Every code's site in this crate, and the two that have none
///
/// Filled in round 10 wave 8 (lane `producers`), closing the
/// `retirements_by_reason` half of
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// The mapping was taken from each withdrawal ENTRY POINT's own production
/// callers rather than from the shape of `invalidate_matching`, and two of the
/// four guesses in `docs/feature-designs/jit-ir-optimizations.md` were
/// wrong when checked that way — which is recorded here because a histogram
/// bucket with the wrong name on it is worse than a suppressed field:
///
/// * [`SUPERSEDED`] — `JitCache::put` / `put_osr` displacing an earlier body
///   of the same key.
/// * [`INVALIDATED`] — `invalidate_for_class_change`, `invalidate_for_class`,
///   and `clear_all`. **`clear_all` is NOT [`SHUTDOWN`]**, which is what the
///   proposal assumed: its two production callers are
///   `vm/src/vm/vm_exec.rs`'s `redefineClass` handler and
///   `vm/src/vm/vm_init.rs`'s `jit_invalidate_adapter` (a synthetic-stub
///   layout change). Both are redefinition-class invalidations of a running
///   VM, and labelling them `shutdown` would have put "the VM is going away"
///   on the busiest bucket a redefinition-heavy agent workload produces.
/// * [`DEOPTIMIZED`] — `JitCache::remove`. Its single production caller in the
///   workspace is the deopt eviction in `vm/src/jit/helpers.rs`, behind
///   `tiered::deopt_evicts_method_body(reason, action)`. **That is a property
///   of the caller, not of `remove`**, whose own doc says only "for
///   invalidation"; `remove` is `pub` and a second caller withdrawing for
///   another reason would silently land in this bucket. The remedy if one
///   appears is to give `remove` a `reason` parameter — it cannot be done
///   pre-emptively here, because that signature is called from a file outside
///   this crate.
/// * [`CACHE_PRESSURE`] — `JitCache::sweep_cold_bodies`.
/// * [`CLASS_UNLOADED`] — `invalidate_unloaded_class`. Reachable and distinct,
///   which the proposal doubted: it worried that `invalidate_matching` "serves
///   both assumption invalidation and class unloading" and that folding them
///   would defeat the one reading the report exists for. It does not have to
///   fold, because the distinction lives one level UP, in which named entry
///   point was called. `invalidate_matching` itself never decides.
/// * [`SHUTDOWN`] — **no site, deliberately.** Nothing in the workspace tears
///   a `JitCache` down at process exit through a path that withdraws: a
///   `JitCache` dropped at exit drops its bodies without going through
///   `retire_withdrawn_body`, and `clear_all` (the only full flush) is
///   redefinition. Kept as a RESERVED code for the same reason
///   [`code_alloc_failure::NO_EXTENT_LARGE_ENOUGH`] is: deleting it would
///   renumber nothing today but would renumber every later code the moment one
///   is appended, in two crates at once. It is never SHOWN — the report's
///   `retire_reason_breakdown` filters zero buckets before it labels them — so
///   there is no operator to mislead, which is the difference between a
///   reserved code and an orphaned instrument.
///
/// # The transitive closure carries the TRIGGERING reason, on purpose
///
/// `invalidate_matching_collecting` withdraws more than its predicate matches:
/// it takes the transitive reverse closure over baked direct calls and lambda
/// adapters, so a caller is withdrawn because its callee was. The proposal
/// suggested that might be "arguably a seventh code". It is not, because these
/// codes name the CAUSE and not the mechanism: a caller withdrawn because a
/// class it inlined from was unloaded was withdrawn *by the class unloading*,
/// and an operator reading `class-unloaded=N` wants N to include it. A
/// separate `closure` code would answer "how did this body get on the list",
/// which no line of the report asks.
pub mod retire_reason {
    /// A newer compilation of the same method replaced it.
    pub const SUPERSEDED: usize = 0;
    /// A speculative assumption failed — class hierarchy change, redefinition,
    /// a broken final/stable-field dependency.
    pub const INVALIDATED: usize = 1;
    /// The body deoptimized and its frames were materialised into the
    /// interpreter.
    pub const DEOPTIMIZED: usize = 2;
    /// Evicted to make room: the cache is at its cap.
    pub const CACHE_PRESSURE: usize = 3;
    /// The owning class (or class loader) was unloaded.
    pub const CLASS_UNLOADED: usize = 4;
    /// VM shutdown / cache teardown. Reserved; see the module doc.
    pub const SHUTDOWN: usize = 5;

    /// One past the highest defined code.
    pub const COUNT: usize = 6;
}



/// Round 14 wave 1 (lane codecache): the scoped reclamation authorisation.
#[cfg(test)]
mod r14w1_codecache_cascade_proof_tests;
#[cfg(test)]
mod r23_w2c_typecheck_miss_memo_tests;
/// Round-9 runtime-lane tests for the code-free ring, the constructor pair and
/// the value-shape admission term. Each is written to survive the rest of the
/// binary running concurrently: nothing asserts an absolute value of a
/// process-global counter.
#[cfg(test)]
mod r9_runtime_tests;

/// Round 9 wave 2, lane `runtime2`: executed tests for the pages this lane
/// resolved. Like `r9_runtime_tests`, nothing here asserts an absolute value
/// of a process-global counter another test could move.
#[cfg(test)]
mod r9w2_runtime2_tests;
#[cfg(test)]
mod gch_h4jm_sp_stack_prim_fold_tests;
#[cfg(test)]
mod frame_liveness_table_tests;
#[cfg(test)]
mod local_handlers_armed_scope_tests;
#[cfg(test)]
mod t1_argfix_spliced_body_codes_tests;


#[cfg(test)]
mod r11w16_replay_splice_call_planner_tests;

/// Round 12 wave 5 (lane iropt4): the planner half of the relaxed replay
/// fence, on the JDK 25 `HashMap.get` / `put` / `hash` bodies and their
/// must-refuse twins. The builder half is `ir::r12w5_iropt4_fence_tests`.
#[cfg(test)]
mod r12w5_iropt4_planner_fence_tests;
#[cfg(test)]
mod r23w9_l6_unresolved_ldc_row_tests;
#[cfg(test)]
mod nio21w6_unbindable_nested_tests;
#[cfg(test)]
mod r13w8_iropt7_cold_nested_tests;



/// The for-each residual round (2026-10-06): the factory trap's profile rule.
#[cfg(test)]
mod fer_factory_trap_profile_tests;

mod same_target_splice;


#[cfg(test)]
mod nio21w7_scalar_row_body_tests;
#[cfg(test)]
mod nio21w1_spliced_scalar_row_tests;


#[cfg(test)]
mod r16_callbind_box_thin_tests;


#[cfg(test)]
mod inline_selection_tests;
/// t1gate (page `t1-SAFEPOLL-proposal-poll-on-backward-handler-edges`): which
/// handler edges of a local-handler site may be declined while a pause is
/// pending.
#[cfg(test)]
mod t1gate_backward_handler_edge_tests;
/// JIT r20 lane rev12: the getClass fold names only a site that RESOLVES to
/// `Object.getClass` -- a class's own private `getClass()Ljava/lang/Class;`
/// (legal: the final-override check skips private methods) keeps its call.
#[cfg(test)]
mod rev12_getclass_fold_declaring_tests;
#[cfg(test)]
mod r23w4_ffm_element_site_word_tests;
#[cfg(test)]
mod r15w6_vh8_judged_site;
#[cfg(test)]
mod nio21w12_vh_const_admission_tests;

#[cfg(test)]
mod varhandle_ir_read_admission;

#[cfg(test)]
mod r15w4_vh6_ir_refusal_admission;

#[cfg(test)]
mod r15w4_vh6_superseded_thin_bind_retire;

#[cfg(test)]
mod r15w5_vh7_mint_forces_exits;
#[cfg(test)]
mod varhandle_cas_slot_tests;

#[cfg(test)]
mod varhandle_write_slot_tests;
#[cfg(test)]
mod object_clone_helper_tests;
#[cfg(test)]
mod r23_w4_string_regex_bind_tests;

#[cfg(test)]
mod r23_w4_reflect_new_array_bind_tests;
#[cfg(test)]
mod r23_w13_l7_direct_static_spliced_tests;
#[cfg(test)]
mod r23_w2d_ffm_element_call_tests;
/// Matcher for the BOX_UNBOX region — `Long.longValue()J` and
/// `Integer.intValue()I`, the unbox half of autoboxing.
///
/// Same contract as [`try_resolve_atomic_long_intrinsic`]: returns the
/// intrinsic entry, the parameter count excluding the receiver, the return-type
/// tag, and the receiver class id to guard on.
///
/// # Why the guard is emitted for a FINAL class
///
/// `java.lang.Long` and `java.lang.Integer` are final, so unlike the `Atomic*`
/// families there is no override to protect against and the guard can never
/// legitimately miss. It is emitted anyway because its miss is a DEOPT, not a
/// wrong answer: if the constant-pool class ever resolves to something else,
/// the site falls back to dispatch instead of loading slot 0 of an unrelated
/// object. One compare is a cheap price for failing closed.
///
/// The field-slot premise is the same one the `Atomic*` regions rest on and is
/// checked the same way: `AtomicLongFieldLayout::new(0, ..)` /
/// `AtomicIntFieldLayout::new(0, ..)` return `None` unless slot 0's compact
/// storage is exactly 8 / 4 bytes wide, so a layout this load could not address
/// never reaches codegen.
#[cfg(test)]
thread_local! {
    /// `Some(true)` = force the box/unbox family ON for this thread's test,
    /// `Some(false)` = force it OFF, `None` = ask the flags.
    static BOX_UNBOX_FORCE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn box_unbox_forced() -> Option<bool> {
    BOX_UNBOX_FORCE.with(|c| c.get())
}

/// RAII: force the box/unbox family ON for the current thread.
#[cfg(test)]
struct BoxUnboxForceOn;

#[cfg(test)]
impl BoxUnboxForceOn {
    fn new() -> Self {
        BOX_UNBOX_FORCE.with(|c| c.set(Some(true)));
        Self
    }
}

#[cfg(test)]
impl Drop for BoxUnboxForceOn {
    fn drop(&mut self) {
        BOX_UNBOX_FORCE.with(|c| c.set(None));
    }
}

#[cfg(test)]
mod string_builder_intrinsic_registration;
#[cfg(test)]
mod r13_sync_direct_instance_guard_tests;
#[cfg(test)]
mod r23w9_cold_instance_self_call_tests;
#[cfg(test)]
mod t1w8_accept_osr_route_memo_tests;
#[cfg(test)]
mod j27_spliceffm2_replan_tests;
#[cfg(test)]
mod r13w12_osrdoor_splice_plan_tests;
#[cfg(test)]
mod dependency_bucket_tests;
#[cfg(test)]
mod r19w5_throws5_service_args_word_tests;
#[cfg(test)]
mod gcd_d9d_callee_owned_args_tests;
#[cfg(test)]
mod retire_queue_lock_order_tests;
#[cfg(test)]
mod pump_pace_tests;
#[cfg(test)]
mod t1v5_installed_body_record_tests;
/// Interpreter round i4 wave 88, lane L7: a new subtype withdraws only the
/// bodies that depend on an overridable member of its supertype.
#[cfg(test)]
mod i88_l7_new_subtype_invalidation_tests;
#[cfg(test)]
mod callee_publish_stamp_tests;
/// gci i12/ul F3: an inlinee name matches by name unless the namesake
/// predicate proves the body sees a live class of that name.
#[cfg(test)]
mod gci_i12_ul_unloaded_set_tests;
// The free verdict functions below read and write `process_jit_verdicts`, the
// registry of compiles with no VM in scope. They are `#[cfg(test)]` (interpreter
// round i1 wave 12): a VM records into and asks its own registry
// (`TieredCompilationManager::verdicts`), and a production caller of the free
// spelling would read a registry no VM ever writes.

/// [`JitVerdictRegistry::forget_for_class`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn forget_jit_verdicts_for_class(class_id: cratonvm_types::ClassId, class_name: &str) -> usize {
    process_jit_verdicts().forget_for_class(class_id, class_name)
}

/// [`JitVerdictRegistry::is_bail_listed`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn is_jit_bail_listed(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    process_jit_verdicts().is_bail_listed(class_id, class_name, method_name, descriptor)
}

/// [`JitVerdictRegistry::mark_bail_listed`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn mark_jit_bail_listed(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) {
    process_jit_verdicts().mark_bail_listed(class_id, class_name, method_name, descriptor);
}

/// [`JitVerdictRegistry::record_compile_refusal`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn record_compile_refusal(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    why: &'static str,
) {
    process_jit_verdicts().record_compile_refusal(
        class_id,
        class_name,
        method_name,
        descriptor,
        why,
    );
}

/// [`JitVerdictRegistry::bail_reason_for`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn jit_bail_reason_for(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> Option<String> {
    process_jit_verdicts().bail_reason_for(class_id, class_name, method_name, descriptor)
}

// ---------------------------------------------------------------------------
// OSR entry-point reject memo
// ---------------------------------------------------------------------------
//
// An OSR compile is requested for one specific back-edge PC, but the artifact
// it produces decides for itself which PCs it will accept: `can_osr_enter`
// refuses any PC with no published native offset (the codegen writes -1 for a
// pc strictly inside a LICM-hoisted loop body) and — under
// `CRATONVM_JIT_OSR_DEAD_LOCALS=0` only — any PC whose `osr_dead_mask` is
// non-zero. Those two decisions are made by different parts of the pipeline,
// and they can disagree: the compile succeeds and the resulting body then
// refuses the very entry it was compiled for.
//
// Nothing memoised that disagreement, so the next trip over the same back-edge
// ran the FULL x64 pipeline again, for the same PC, to the same conclusion —
// forever. Measured on `org/h2/compress/CompressLZF.compress(Ljava/nio/
// ByteBuffer;I[BI)I`: 256 compiles of `entry_pc=220` in ten operations of H2's
// `TestFileSystem` `nioMemLZF:` case, zero OSR entries, zero compiled code
// executed (2026-07-27). Same shape as the two waste loops this module already
// memoises — RBC.2's 2,610 recompiles of `SecP521R1Curve$1.lookup` and RBC.4's
// 35,923 re-run pipelines on `Nat.inc`.
//
// The rejection is a function of the compile, which is deterministic for a
// given method and compile state. It is keyed per (method, entry_pc) rather
// than per method: a method's other back-edges are usually fine, and banning
// all of them would cost real throughput. It lives in the VM's
// `JitVerdictRegistry` beside the method's other verdicts, so it expires with the bytecode it was measured on
// and, for the refusals that depend on compile-time state, with that state.

/// [`JitVerdictRegistry::is_osr_entry_rejected`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn is_osr_entry_rejected(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    entry_pc: usize,
) -> bool {
    process_jit_verdicts().is_osr_entry_rejected(
        class_id,
        class_name,
        method_name,
        descriptor,
        entry_pc,
    )
}

/// [`JitVerdictRegistry::mark_osr_entry_rejected`] on [`process_jit_verdicts`].
#[cfg(test)]
pub fn mark_osr_entry_rejected(
    class_id: cratonvm_types::ClassId,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    entry_pc: usize,
) {
    process_jit_verdicts().mark_osr_entry_rejected(
        class_id,
        class_name,
        method_name,
        descriptor,
        entry_pc,
    );
}

#[cfg(test)]
mod r10_drv_force_interpret_tests;
/// [`JitVerdictRegistry::direct_call_requires_dispatch`] on
/// [`process_jit_verdicts`]: the answer for compiles with no VM in scope. A VM
/// asks its own registry (`TieredCompilationManager::verdicts`), the one its
/// compiles record cycles into, so this is test-only (interpreter round i1
/// wave 12). The cycle tests drive a registry of their own since wave 17;
/// this spelling is left for the assertion that the process registry stays
/// untouched by them.
#[cfg(test)]
pub fn jit_direct_call_requires_dispatch(
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    process_jit_verdicts().direct_call_requires_dispatch(class_name, method_name, descriptor)
}

#[cfg(test)]
mod zs7vl_single_pass_only_tests;
/// Empty this thread's nested-compile stack, for the cycle tests that push
/// entries on it by hand.
///
/// Until interpreter round i1 wave 17 this also emptied the recursion-cycle
/// sets of the no-VM registry ([`process_jit_verdicts`]), which every cycle
/// test shared, and a test lock serialised the tests that did so (a clear
/// landing inside another test's window was measured at 3 flakes in 25 loaded
/// runs). Each of those tests now drives a `JitVerdictRegistry` of its own,
/// which `CompileRequest::new` requires, so there is nothing shared to clear
/// or lock. The stack is thread-local.
#[cfg(test)]
fn clear_jit_compile_stack_for_test() {
    JIT_COMPILE_STACK.with(|stack| stack.borrow_mut().clear());
}

#[cfg(test)]
mod sync_direct_lookup_memo_tests;
// Test-only telemetry for wire-tiered-manager Step 3: how many times the
// optimizing IR-lowering pipeline produced the final compiled body **on the
// current thread**. The per-call-toggle test reads this to prove
// `optimize=false` (the C1 tier) skips the IR path while `optimize=true` (C2)
// takes it.
//
// A `thread_local!` rather than a global atomic so the count is isolated
// per-test: `try_compile` runs the whole pipeline synchronously on the caller's
// thread, and the cargo harness runs each `#[test]` on its own thread, so a
// concurrently-running compile test cannot perturb this thread's count.
// `#[cfg(test)]` so production codegen carries no extra work on the hot path.
#[cfg(test)]
thread_local! {
    pub(crate) static IR_LOWER_COMPILES: std::cell::Cell<u64> = std::cell::Cell::new(0);
}

#[cfg(test)]
mod r13w8_irexc2_osr_arm_mask_tests;
#[cfg(test)]
mod r23w8_in_place_trap_frame_check_tests;

/// Round 11 wave 10 (lane irexc): the post-lowering half of the
/// unresumable-trap waiver.
#[cfg(test)]
mod r11w10_irexc_trap_post_check_tests;
#[cfg(test)]
mod i115_l2_fall_through_cause_tests;
#[cfg(test)]
mod nio20w4_nested_trap_charge_tests;

#[cfg(test)]
mod r14_chain_splice_despec_tests;
#[cfg(test)]
mod r15w3_chain3_field_decl_rows_tests;
#[cfg(test)]
mod r16w17_mh_constant_plan_tests;

#[cfg(test)]
mod r16w10_indyir_plan_tests;

#[cfg(test)]
mod r20w11_pindy_mutable_plan_tests;
#[cfg(test)]
mod r15w6_sweepa_indy_table_refusal_tests;
#[cfg(test)]
mod r13_sync2_self_lock_admission_tests;

#[cfg(test)]
mod r13w13_syncres_static_array_loop_tests;
#[cfg(test)]
mod r15w3_sync2_static_ir_self_lock_candidate_tests;




#[cfg(test)]
mod r15w4_sync3_static_self_lock_recompile_tests;
#[cfg(test)]
mod r13_sync4_self_lock_refusal_memo_tests;
#[cfg(test)]
mod r23_w5_sp_intrinsic_trap_despec_tests;
// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod speculation_feed_tests;

#[cfg(test)]
mod code_buffer_retry_tests;

#[cfg(test)]
mod long_box_direct_bind_tests;

#[cfg(test)]
mod direct_exc_table_publish_policy;

#[cfg(test)]
mod varhandle_read_direct_bind_tests;

// ---------------------------------------------------------------------------
// Retirement-queue quiescence — RT-3
// ---------------------------------------------------------------------------

/// The retirement queue's release predicate, tested as a predicate.
///
/// `ThreadQuiescenceEvidence`, `BlockedStackSummary` and
/// `classify_thread_quiescence` are private to this file, so none of this can
/// be an integration test; that is why it lives here rather than in
/// `jit/tests/`.
///
/// # The plan this replaces, and why it was wrong
///
/// `NOTES-runtime.md` RT-3 asked for boundary cases "`entry` itself,
/// `entry + len - 1`, `entry + len` (must permit), `entry - 1` (must permit)".
/// There is no `len` in this decision. `BlockedStackSummary::buffers` holds
/// the sorted, deduplicated STARTS of the buffers a stack word points into —
/// the range containment happened in `collect_code_buffers_in_band`, which
/// mapped each word to its owning buffer's start. `permits_stamp` does an
/// exact-match `binary_search` on those starts and never sees a length. A test
/// built on `entry + len - 1` would have asserted a property of a different
/// design, and would have passed for the wrong reason.
///
/// The boundaries below are the ones `binary_search` actually has: the first
/// element, an interior element, the last element, a value below the first,
/// between two, and above the last, and an empty list. One of them —
/// `0x1001`, one byte past a present start — is deliberately the shape the old
/// plan would have written, and it must PERMIT.
#[cfg(test)]
mod retirement_quiescence_tests;

/// Round 14 wave 7 (lane mega): [`jit_ic_grace_catch_up`] re-stamps the
/// refusing thread before it collects the evidence
/// (`CRATONVM_JIT_IC_CATCH_UP_SELF_STAMP`). Each test runs on its own fresh
/// thread, so its record starts unstamped (`ic_quiescent_gen == 0`) and its
/// catch-up countdown at zero (the first call proves). Only this thread's own
/// record is read; the generations are process-wide and only compared with
/// `>=` against a value read before the call (they never go down).
#[cfg(test)]
mod r14w7_mega_catch_up_self_stamp_tests;

// ---------------------------------------------------------------------------
// Code-cache cold-body sweep — RT-8
// ---------------------------------------------------------------------------

/// The sweep POLICY, over synthetic censuses.
///
/// Everything around the policy needs real compiled artifacts: a
/// `CompiledMethod` cannot be built without running a compile, and an
/// end-to-end test of an evictor is the worst kind of vacuous, because "it
/// retired nothing" is both the healthy outcome and what a permanently-refusing
/// sweeper looks like. `CodeCacheSweepState::plan` is the decision with the
/// artifacts replaced by the three numbers it reads from each of them, so each
/// arm below fails for a stated reason when the arm is removed.
#[cfg(test)]
mod code_cache_sweep_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod r19_syncdoor3_tests;

#[cfg(test)]
mod r19_sync5_tests;

#[cfg(test)]
mod r19_handler7_tests;

#[cfg(test)]
mod r19_mon10_tests;

#[cfg(test)]
mod r19_mon12_tests;

/// Round 9 wave 3, lane `runtime3`: tests for the pages this lane resolved.
#[cfg(test)]
mod r9w3_runtime3_tests;

/// Round 9 wave 5, lane `volatile5`:
/// `volatile-instance-fields-are-plain-accesses-in-compiled-code`.
#[cfg(test)]
mod r9w5_volatile5_tests;

/// Round 9 wave 7, lane `mutrec7`: the guarded cycle-edge cell
/// (`perf-mutual-recursion-forces-every-call-through-invoke-dispatch-20260918.md`).
#[cfg(test)]
mod r9w7_mutrec7_tests;

/// Round 9 wave 9, lane `evict9`: retire cells
/// (`evicted-callee-stays-reachable-through-a-compiled-callers-binding-FIXED-20260918.md`).
#[cfg(test)]
mod r9w9_evict9_tests;

/// t1 wave 2, lane jit2 (B5 of `jit-proposals/jit-r24-alloc-and-ffm-loop-residuals-proposals-RETIRED-20261026.md`,
/// [`jit_osr_late_static_cell_enabled`]): an OSR body's late-bind cell waits
/// for its callee's publication ([`JitCache::publish_osr_body_late_cells`]).
#[cfg(test)]
mod t1_jit2_osr_late_cell_tests;

#[cfg(test)]
mod r9w10_call10_tests;

/// Interpreter round i1 wave 10, lane L5: the adapter fold is armed by the
/// constant pool (`lambda_int_to_double_runs_proven`), not by a class name.
#[cfg(test)]
mod lambda_int_to_double_proof_tests;

/// Interpreter round i1 wave 17, lane L3: a spliced body's substituted owner
/// travels with its id (`i14-L2-proposal-substitutions-keyed-by-call-site`,
/// stage 1).
#[cfg(test)]
mod i17_l3_inline_owner_id_tests;

/// Interpreter round i1 wave 17, lane L3: an inherited-name call's declaring
/// class is named by identity in the recursion-cycle checks
/// (`i13-L3-proposal-callee-resolvers-answer-the-bound-method-identity`,
/// stage 1).
#[cfg(test)]
mod i17_l3_declaring_identity_tests;

/// gc-common w36-c: the class-unload batch forms (`UnloadedClassSet`) are
/// equivalent to a loop of their per-class forms
/// (`docs/feature-designs/jit-ir-optimizations.md`).
#[cfg(test)]
mod w36c_unload_batch_tests;

/// Interpreter round i1 wave 40, lane L2 follow-up: nothing is published
/// while a multi-class redefinition call holds the cache
/// (`JitCache::hold_publications`), and publication resumes once it is
/// released.
#[cfg(test)]
mod i40_l2_publication_hold_tests;

#[cfg(test)]
mod r16w15_throwfill_walk_lookup_tests;



#[cfg(test)]
mod r16w16_chains2_sync_method_chains_dbg_tests;

/// Round 16 wave 17 (lane mega5, NIO2-1): the `ScopedMemoryAccess` bind
/// admission.


#[cfg(test)]
mod unloaded_static_splice_admission_tests;

#[cfg(test)]
mod byte_array_view_read_shape_tests;











#[cfg(test)]
mod nio20w6_l1_buffer_trap_and_plain_static_tests;





/// Interpreter round i6 wave 121, lane L2 (stage 3a of
/// `i43-L1-proposal-deoptimize-a-compiled-frame-for-the-frame-commands`):
/// [`CompiledMethod::debugger_exit_after_call`] answers only a recorded call
/// bci, and nothing for a body compiled without a debugger.
#[cfg(test)]
mod i121_l2_debugger_exit_after_call_tests;







/// JIT round 21 wave 4 (lane codecache): a body whose mapping is rescued into
/// the retirement queue keeps the side tables its code addresses
/// (`RescuedSideTables`, `CRATONVM_JIT_RESCUE_KEEPS_SIDE_TABLES`).
#[cfg(test)]
mod r21w4_codecache_rescued_side_tables_tests;



/// Review rev-w45-jit item 1 (JIT round 21 wave 7, lane codecache): the
/// holder count does NOT send a reader's handle on a still-published body
/// through the retirement queue. The cache's own shard-map entry is a counted
/// `RetainedCode`, so a per-thread invoke-cache handle on a published body is
/// never the last holder and drops as a plain decrement. Only a body the cache
/// no longer holds (superseded here; withdrawn, unpublished or memo-only bodies
/// alike) has its last handle routed, which is the case CODECACHE21-2 closes.
#[cfg(test)]
mod r21w7_codecache_published_handle_tests;

/// Review rev-x64 item 8 (JIT round 21 wave 5, lane codecache): only a
/// decompilation charges the recompile cutoff.
#[cfg(test)]
mod r21w5_codecache_charged_withdrawal_tests;

/// JIT round 22, lane sync (SYNC22-1): an inline-cache owner published into a
/// rescued body's slot AFTER the body's drop withdrew its owners leaves through
/// the queue when the rescued side tables are released, not as a field drop.
#[cfg(test)]
mod r22_sync_rescued_tables_rewithdraw_tests;

#[cfg(test)]
mod r22_throws_indy_loop_sites_tests;

/// JIT round 23 wave 1, lane L6 (SYNC22-3): `direct_native_helper` asks the
/// VM's bind admission in `--compatible` too, and the `_for_impl` variant asks
/// the implementing row there.
#[cfg(test)]
mod r23_w1_l6_compat_direct_bind_tests;



/// JIT round 23 wave 9 (lane W9-L2, CF-W9-2; `CRATONVM_JIT_IR_PRUNE_RARE_ARM`):
/// a rarely taken arm is pruned like a never-taken one, in a splice and in the
/// compiling method's own code.


/// JIT round 23 wave 4 (lane L1, CF23-2; `CRATONVM_JIT_IR_SPLICE_NEVER_TAKEN`):
/// the planner half of the in-splice never-taken prune.


#[cfg(test)]
mod r23w4b_l2_mh_constant_condy_receiver_tests;

/// JIT round 25 wave 7, lane W7-B (`CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE`,
/// default ON since the wave-10 flip, `=0` kills it; `docs/feature-designs/regex-jit-contract.md`,
/// "Wave 7 (W7-B)"): nested profile-guarded splices of a DISPATCH CHAIN.
///
/// A chain site is a virtual or interface call inside a spliced body whose
/// selector (name and descriptor) is the body's own: `next.match(matcher, i,
/// seq)` inside `Pattern$X.match(matcher, i, seq)`, the shape of every
/// `Pattern$Node` subclass, of filter / interceptor chains and of visitor
/// walks. HotSpot inlines those links from the receiver profile at each site
/// (`FreqInlineSize` once the site has run `InlineFrequencyCount` times, under
/// its inlining depth and size bounds). The IR tier held every link one level
/// down as a call:
///
/// * the nest-exact row of the constant-pool class (`Pattern$Node`, whose own
///   27-byte body never arrives) pre-empted the profiled route;
/// * the profiled body (`GroupHead` 47, `BmpCharPropertyGreedy` 89,
///   `GroupTail` 110 bytes) is over the cold cap and is not a leaf, so the
///   nested hot rule (`ir_nested_hot_body_admitted`) refused it;
/// * the body's own interface calls (`predicate.is`, `seq.charAt`) had no
///   nested route unless `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE` was on,
///   so a link spliced without them would trade one call per position for
///   one call per character.
///
/// Under the switch a chain link is admitted by its own rule,
/// [`chain_link_verdict`]: the site is hot (in a loop of the enclosing body,
/// on its entry prefix, or [`CHAIN_HOT_MIN_CALLS`] receiver samples), the
/// body fits the multi-body hot cap, and every call the body makes is either
/// spliced in turn (it has a nested row) or is itself the chain's next link.
/// A spliced link therefore removes one call and keeps only the next-link
/// call it made anyway. The VM's nested resolver
/// (`jit_bridge::resolve_inline_site_from`) and the planner
/// ([`ir_splice_nested_exact_admitted_in`]) ask the same function of the same
/// body and row. The guard, its miss edge (the site's call) and the deopt
/// chain snapshots are the existing nested guarded row's, unchanged.
pub mod r25_w7b_chain_splice {
    use super::*;

    /// The switch's token.
    pub const SWITCH: &str = "CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE";

    /// HotSpot's `InlineFrequencyCount` (100 on x86-64): receiver samples at
    /// which a chain site outside every loop and past the entry prefix of its
    /// enclosing body counts as hot.
    pub const CHAIN_HOT_MIN_CALLS: u32 = 100;

    /// Nested hot-cap slots a compile may register beyond the top-level
    /// guarded hot sites' [`GUARDED_SPLICE_HOT_MAX_SITES`] under the switch:
    /// a regex chain is three to six links over the cold cap, and the
    /// per-compile byte budget (`ir_inline_total_budget`) still bounds them.
    pub const CHAIN_NESTED_HOT_EXTRA_SITES: usize = 8;

    /// `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` (default ON since the r25 wave-10 flip, `=0` kills it). Read per call, no
    /// `static`.
    pub fn ir_nested_chain_splice_enabled() -> bool {
        true
    }

    /// Preserve a standalone body's deeper splices instead of cutting its tree.
    /// Shared by the resolver and transitive cost rule; the existing kill switch
    /// permits a finite retained call without changing any guard or frame limit.
    pub fn keep_loop_splice_caller_enabled() -> bool {
        true
    }

    /// Round rx27 wave 3 (lane chain, W7B-3): `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS`
    /// (**default ON** since the 2026-11-09 shipping-policy A/B, `=0` kills it:
    /// `regex-matches` -840 and `regex-groups` -1,063 instructions/op; the earlier
    /// rx27 cbl-v-def screen kept it off at a different base;
    /// inert without the chain switch, nested guarded row and bimorphic splice).
    /// Read per call, no `static`.
    ///
    /// `GroupTail.match` pc 70 in `regex-groups` is a 50 / 49 % `next.match`
    /// pair (`LastNode` / `BmpCharProperty`). The nested two-class row that
    /// splices it exists since round 19 (SPLICE19-4), but only under the
    /// global `CRATONVM_JIT_IR_NESTED_BIMORPHIC_SPLICE`, which opens a pair
    /// at every nested virtual site of every compile. Under this switch a
    /// pair is admitted at a chain site only ([`row_is_chain`]): the
    /// resolver's `pair` arm, the planner's strip and append, and the
    /// builder ask [`nested_pair_admitted`] instead of the global switch. The
    /// rows, guards and miss edges are SPLICE19-4's, unchanged.
    pub fn chain_bimorphic_links_enabled() -> bool {
        ir_nested_chain_splice_enabled()
            && ir::ir_nested_guarded_splice_enabled()
            && ir::ir_bimorphic_guarded_splice_enabled()
            && true
    }

    /// May a nested site hold a two-class row? Everywhere under the global
    /// nested bimorphic switch; at a chain site (`chain_site`) also under
    /// [`chain_bimorphic_links_enabled`].
    pub fn nested_pair_admitted(chain_site: bool) -> bool {
        ir::ir_nested_bimorphic_splice_enabled()
            || (chain_site && chain_bimorphic_links_enabled())
    }

    /// The builder's form: the planner already dropped every pair that
    /// [`nested_pair_admitted`] refuses, so a registered nested pair is
    /// admitted under either switch.
    pub fn nested_pair_switch_on() -> bool {
        ir::ir_nested_bimorphic_splice_enabled()
            || chain_bimorphic_links_enabled()
            || chain_bimorphic_predicates_enabled()
    }

    /// Round rx27 wave 4 (lane chain): `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`
    /// (**default ON** since the 2026-10-29 bundle flip, `=0` kills it,
    /// `rx27_chain_entry::bundle_flip_evidence`; inert without the chain switch, the nested guarded row and the
    /// bimorphic guarded splice). Read per call, no `static`.
    ///
    /// A greedy node's `predicate.is` is one bytecode site shared by every
    /// instance of the class, so `[a-z]+\d*` (`regex-matches`) or
    /// `[\w.+-]+@[\w-]+\.[a-z]{2,}` (`regex-find-email`) read two predicate
    /// lambda classes there. One class takes the profiled interface row
    /// (W7-B); the other stays an inline-cache call into the lambda's own
    /// compiled body, paid per character with its argument moves and frame.
    /// Under this switch an INTERFACE site of a chain body may hold the
    /// SPLICE19-4 two-class row too, as [`chain_bimorphic_links_enabled`]
    /// lets a chain site hold one.
    pub fn chain_bimorphic_predicates_enabled() -> bool {
        ir_nested_chain_splice_enabled()
            && ir::ir_nested_guarded_splice_enabled()
            && ir::ir_bimorphic_guarded_splice_enabled()
            // rx27: default ON since the 2026-10-29 bundle flip (`=0` kills).
            && true
    }

    /// The VM resolver's form: an interface call (`invoke_kind` 2) inside a
    /// chain body (`chain_body`) under [`chain_bimorphic_predicates_enabled`].
    pub fn chain_predicate_pair_site(chain_body: bool, invoke_kind: u8) -> bool {
        chain_body && invoke_kind == 2 && chain_bimorphic_predicates_enabled()
    }

    /// The planner's form of the pair admission for nested row `n` of
    /// `parent`: [`nested_pair_admitted`] at a chain site, or an interface
    /// site of a chain body ([`chain_predicate_pair_site`]).
    pub fn nested_pair_admitted_row(parent: &InlineSite, n: &NestedInlineSite) -> bool {
        nested_pair_admitted(row_is_chain(parent, n))
            || chain_predicate_pair_site(
                body_is_chain_shaped(parent),
                parent
                    .invoke_targets
                    .iter()
                    .find(|(pc, _)| *pc == n.callee_pc)
                    .map_or(0, |(_, t)| t.invoke_kind),
            )
    }

    /// Is a call of kind `invoke_kind` (`jit_invoke_dispatch`'s encoding) to
    /// `call_name` `call_desc` inside a body `body_name` `body_desc` a chain
    /// site: a virtual or interface call of the body's own selector?
    pub fn is_chain_call(
        invoke_kind: u8,
        call_name: &str,
        call_desc: &str,
        body_name: &str,
        body_desc: &str,
    ) -> bool {
        matches!(invoke_kind, 0 | 2) && call_name == body_name && call_desc == body_desc
    }

    /// W7-B fix (round 2): [`is_chain_call`] for a call row, and never a call
    /// whose constant-pool class is `java/lang/Object`. `AbstractSet.hashCode`
    /// -> `e.hashCode()`, `AbstractList.equals` -> `o1.equals(o2)` and every
    /// `toString` delegation have the body's own selector but are no dispatch
    /// chain: the receiver is an element, not the next link of the body's own
    /// class family. The VM resolver also asks that the enclosing body's class
    /// is a subclass of the call's constant-pool class
    /// (`jit_bridge::resolve_inline_site_from`, `Pattern$GroupHead` of
    /// `Pattern$Node`), which only it can answer.
    pub fn is_chain_target(t: &InlineInvokeTarget, body_name: &str, body_desc: &str) -> bool {
        t.class_name != "java/lang/Object"
            && is_chain_call(
                t.invoke_kind,
                &t.method_name,
                &t.descriptor,
                body_name,
                body_desc,
            )
    }

    /// Does `site` hold a chain site (it calls its own selector virtually)?
    pub fn body_is_chain_shaped(site: &InlineSite) -> bool {
        site.invoke_targets
            .iter()
            .any(|(_, t)| is_chain_target(t, &site.method_name, &site.descriptor))
    }

    /// Why [`chain_link_verdict`] refused a body.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ChainRefusal {
        /// The switch, or a switch the nested guarded row needs, is off.
        SwitchedOff,
        /// A static body (a chain link is always an instance method).
        Static,
        /// Over the multi-body hot cap; the cap is carried.
        OverCap(usize),
        /// Not in a loop, not on the entry prefix, under the sample floor.
        ColdSite,
        /// The body keeps a call at this pc that is neither spliced nor the
        /// chain's next link.
        KeptCall(usize),
        /// W7-B fix 2: the site runs on fewer than
        /// [`CHAIN_MIN_FREQUENCY_PERMILLE`] of the enclosing body's entries
        /// (the per-mille share is carried).
        RareSite(u32),
        /// Round rx27 wave 4 (`CRATONVM_JIT_IR_CHAIN_DEEP_SPINE`): the body's
        /// row at this pc, in a loop of the body, is spliced but keeps a call
        /// in its own tree ([`crate::rx27_deep_spine::first_half_spliced_row`]).
        HalfSpliced(usize),
    }

    impl std::fmt::Display for ChainRefusal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::SwitchedOff => write!(f, "switched off"),
                Self::Static => write!(f, "static body"),
                Self::OverCap(cap) => write!(f, "over the chain cap {cap}"),
                Self::ColdSite => write!(
                    f,
                    "cold site (not in a loop, not on the entry prefix, under {CHAIN_HOT_MIN_CALLS} samples)"
                ),
                Self::KeptCall(pc) => write!(
                    f,
                    "keeps a call at body pc {pc} that is neither spliced nor the next link"
                ),
                Self::RareSite(permille) => write!(
                    f,
                    "rare site ({permille} per mille of the body's entries, under \
                     {CHAIN_MIN_FREQUENCY_PERMILLE})"
                ),
                Self::HalfSpliced(pc) => write!(
                    f,
                    "half-spliced row at body pc {pc} (in a loop of the body, it keeps a call \
                     in its own tree; deep spine)"
                ),
            }
        }
    }

    /// Is the chain site at `callee_pc` of the enclosing body
    /// `code[..code_len]` (its receiver row `row`) hot: in a loop there (the
    /// top-level rule's `InLoop`), on the body's entry prefix (it runs on every
    /// execution of the link that holds it), or [`CHAIN_HOT_MIN_CALLS`]
    /// receiver samples?
    pub fn chain_site_is_hot(
        row: Option<&profile::ReceiverCounts>,
        code: &[u8],
        code_len: usize,
        callee_pc: usize,
    ) -> bool {
        ir_nested_row_samples(row) >= CHAIN_HOT_MIN_CALLS
            || ir_splice_pc_in_entry_prefix(code, code_len, callee_pc)
            || bytecode_pc_in_loop(code, code_len, callee_pc)
    }

    /// The first call of `site` that would stay a call and is not the
    /// chain's next link, if any. A pc with a nested row counts as spliced.
    pub fn first_kept_non_link_call(site: &InlineSite) -> Option<usize> {
        site.invoke_targets
            .iter()
            .find(|(pc, t)| {
                !site.nested_sites.iter().any(|n| n.callee_pc == *pc)
                    && !is_chain_target(t, &site.method_name, &site.descriptor)
                    // W10-3b: a call the inline String prefix answers
                    // (`CharSequence.charAt` on a String) is no real call.
                    && !(crate::r25_w10_3b::prefix_kept_enabled()
                        && crate::r25_w10_3b::site_answered_by_string_prefix(
                            &t.class_name,
                            &t.method_name,
                            &t.descriptor,
                            t.invoke_kind,
                        ))
            })
            .map(|(pc, _)| *pc)
    }


    /// W7-B fix 2: C2's `InlineFrequencyRatio` (0.25), as per mille. A chain
    /// link whose site runs on fewer of its enclosing body's entries than this
    /// is not frequent, and HotSpot inlines such a site only up to
    /// `MaxInlineSize` (the cold cap here), which every chain body is over.
    pub const CHAIN_MIN_FREQUENCY_PERMILLE: u32 = 250;

    /// W7-B fix 2: the frequency half of the chain rule, asked by the VM
    /// resolver, which holds both counts. `samples` are the receiver samples
    /// at the chain site in the enclosing body's own profile and
    /// `invocations` that body's invocation count, the same interpreted
    /// window. `invocations == 0` is no evidence: the structural rule
    /// ([`chain_link_verdict`]) decides alone.
    ///
    /// The structural rule took any site in a loop or on a loop-rooted chain.
    /// That kept links that run on a few per cent of their parent's entries --
    /// the `\s*` after `,` in `,\s*` (one position in twenty), a group's
    /// tail after `=` -- whose bodies then sit in the frame of a body entered
    /// on every text position: its fresh-frame clear grows with every spliced
    /// level, and the link almost never runs.
    pub fn chain_frequency_verdict(samples: u32, invocations: u32) -> Result<(), ChainRefusal> {
        if invocations == 0 {
            return Ok(());
        }
        let permille = chain_frequency_permille(samples, invocations);
        if permille < CHAIN_MIN_FREQUENCY_PERMILLE {
            return Err(ChainRefusal::RareSite(permille));
        }
        Ok(())
    }

    /// `samples` per thousand `invocations`, saturating (0 with no
    /// invocations).
    pub fn chain_frequency_permille(samples: u32, invocations: u32) -> u32 {
        if invocations == 0 {
            return 0;
        }
        u32::try_from(u64::from(samples) * 1000 / u64::from(invocations)).unwrap_or(u32::MAX)
    }



    /// The strip's re-check ([`ir_splice_strip_unadmitted_exact_nested`]):
    /// after the rows under `n` were stripped, does `n` -- admitted before --
    /// still pass? Only the chain rule depends on a body's own rows (a
    /// stripped row can only turn a body INTO a leaf), so this asks nothing
    /// with the switch off.
    pub(crate) fn withdrawn_after_strip(
        parent: &InlineSite,
        n: &NestedInlineSite,
        chain_fences: bool,
    ) -> bool {
        ir_nested_chain_splice_enabled()
            && n.guard_class_id != 0
            && !ir_splice_nested_exact_admitted_in(Some(parent), n, chain_fences)
    }

    /// The nested hot-cap pcs of one compile ([`ir_nested_hot_cap_pcs`]):
    /// `body(key)` is the row's body length and whether it is a chain link the
    /// cost rule kept. W7-B fix (round 2): only those links take the
    /// [`CHAIN_NESTED_HOT_EXTRA_SITES`] slots; every other nested row keeps the
    /// `base_slots` (what the top-level hot sites left of
    /// [`GUARDED_SPLICE_HOT_MAX_SITES`]) it had before the switch. Round 1
    /// added the slots to every row, so non-chain bodies of 36-325 bytes the
    /// slots had held to the cold cap (`Pattern.compile`'s helpers on the rows
    /// that compile a pattern per op) were spliced too. Switch off: exactly
    /// [`ir_nested_hot_cap_pcs`].
    pub(crate) fn nested_hot_cap_pcs(
        rows: &[(usize, u32)],
        body: impl Fn(usize) -> Option<(usize, bool)>,
        base_slots: usize,
    ) -> Vec<usize> {
        let len = |key: usize| body(key).map(|(len, _)| len);
        if !ir_nested_chain_splice_enabled() {
            return ir_nested_hot_cap_pcs(rows, len, base_slots);
        }
        let (links, plain): (Vec<(usize, u32)>, Vec<(usize, u32)>) = rows
            .iter()
            .copied()
            .partition(|&(key, _)| body(key).is_some_and(|(_, link)| link));
        let mut out = ir_nested_hot_cap_pcs(&plain, len, base_slots);
        for pc in ir_nested_hot_cap_pcs(&links, len, CHAIN_NESTED_HOT_EXTRA_SITES) {
            if !out.contains(&pc) {
                out.push(pc);
            }
        }
        out
    }

    /// Is nested row `n` of `parent` at a chain site of `parent` (the call
    /// at `n.callee_pc` is [`is_chain_target`] of `parent`'s selector)?
    pub fn row_is_chain(parent: &InlineSite, n: &NestedInlineSite) -> bool {
        parent.invoke_targets.iter().any(|(pc, t)| {
            *pc == n.callee_pc && is_chain_target(t, &parent.method_name, &parent.descriptor)
        })
    }

    /// W7-B fix (round 2): the link bytecode one top-level splice tree may
    /// add, HotSpot's `FreqInlineSize` (a quarter of the compile's default
    /// splice budget, [`IR_INLINE_MAX_TOTAL_BYTES`]). Counted per link as its
    /// body plus every non-link body under it (`predicate.is`, `charAt`),
    /// in tree order, so the links nearest the loop that roots the chain are
    /// the ones kept.
    pub const CHAIN_TREE_MAX_BYTES: usize = MAX_INLINE_BYTECODE_SIZE;

    /// Round rx27 (lane chain, W7B-2): the tree cap this compile uses,
    /// `CRATONVM_JIT_IR_CHAIN_TREE_MAX_BYTES` (a byte count, clamped to
    /// `0..=`[`IR_INLINE_MAX_TOTAL_BYTES`]), else [`CHAIN_TREE_MAX_BYTES`].
    /// Read per call, no `static`. `FreqInlineSize` is HotSpot's bound for ONE
    /// callee, not for a tree (C2 bounds a tree by `MaxInlineLevel` 15 and
    /// `DesiredMethodLimit`), so the default cap is stricter than HotSpot
    /// where a chain is long: `regex-groups`' `(\w+)=(\w+)` keeps the first
    /// group's links (about 320 bytes) and the second group's stay calls.
    /// A knob for the A/B only; the W8-5 frame rule still prices every link.
    pub fn chain_tree_max_bytes() -> usize {
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .map_or(CHAIN_TREE_MAX_BYTES, |n| n.min(IR_INLINE_MAX_TOTAL_BYTES))
    }

    /// What [`strip_chain_tree`] did to one top-level splice tree: the census
    /// fields of the planner's `ir-chain-tree` line.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ChainTreeReport {
        /// Chain rows kept (a bimorphic pair counts two).
        pub kept: usize,
        /// Dropped: no loop roots the chain above the row.
        pub unrooted: usize,
        /// Dropped: over [`CHAIN_TREE_MAX_BYTES`].
        pub capped: usize,
        /// Link bytes kept (see [`CHAIN_TREE_MAX_BYTES`]).
        pub bytes: usize,
        /// The `max_locals` those kept bodies add to the compile's frame.
        pub added_locals: usize,
        /// The deepest kept link, in splice levels below the top-level body.
        pub max_depth: usize,
        /// W8-5: dropped by the frame-growth cost rule ([`link_pays_for_its_frame`]).
        pub costly: usize,
        /// W8-5: the estimated frame bytes of the top-level body and its
        /// non-link rows ([`est_frame_bytes`]), before the chain links.
        pub est_frame_before: usize,
        /// W8-5: the same with the kept links' estimated frame bytes added.
        pub est_frame_after: usize,
        /// W8-5: the least entries per thousand top-level-body entries of
        /// any kept link (`None`: no link kept).
        pub min_entry_permille: Option<u64>,
    }

    /// W8-5: what one compiled-to-compiled call costs beyond its callee's
    /// fresh-frame clear, in executed instructions: the frame record, the
    /// shadow watermark save and restore, the entry poll, the inline-cache
    /// cascade and the argument stores (wave 8b: 4.3-4.6 ns a call against
    /// HotSpot's 2.07 on `NodeCall poly`, about 40 instructions).
    pub const CHAIN_CALL_INSTRS: u64 = 40;

    /// W8-5: frame bytes one fresh-frame clear instruction zeroes (one
    /// 32-byte vector store).
    pub const CLEAR_BYTES_PER_INSTR: u64 = 32;

    /// W8-5: the estimated IR frame bytes of a spliced body of `bytecode`
    /// bytes and `locals` locals: 8 bytes a local and 8 a bytecode byte (the
    /// homes of its values). Calibrated on `BmpCharPropertyGreedy.match`, 89
    /// bytes, whose own optimizing frame is 912 bytes (wave 12).
    pub fn est_frame_bytes(bytecode: usize, locals: usize) -> usize {
        (bytecode + locals) * 8
    }

    /// W8-5: how often the call at `callee_pc` of `parent` runs per thousand
    /// entries of `parent`: its receiver samples over `parent`'s invocation
    /// count, from the same interpreted window. `None` without an
    /// invocation count (no evidence).
    pub fn site_entry_permille(parent: &InlineSite, callee_pc: usize) -> Option<u64> {
        if parent.callee_invocations == 0 {
            return None;
        }
        let samples = ir_nested_row_samples(
            parent
                .callee_receiver_profile
                .iter()
                .find(|(pc, _)| *pc == callee_pc)
                .map(|(_, counts)| counts),
        );
        Some(u64::from(samples) * 1000 / u64::from(parent.callee_invocations))
    }

    /// W8-5, the frame-growth cost rule. A link that runs `entry_permille`
    /// times per thousand entries of the top-level body saves that many calls
    /// into its own artifact, each a call ([`CHAIN_CALL_INSTRS`]) plus that
    /// artifact's fresh-frame clear. Spliced, its frame (`frame_bytes`,
    /// [`est_frame_bytes`]) joins the compile root's, whose clear runs on
    /// every entry whether the link runs or not. The link pays when
    /// `entry x (call + clear(frame)) >= 1000 x clear(frame)`, in instructions
    /// per thousand entries; for a 760-byte link that is an entry share of
    /// about 0.37.
    pub fn link_pays_for_its_frame(entry_permille: u64, frame_bytes: usize) -> bool {
        let clear = (frame_bytes as u64).div_ceil(CLEAR_BYTES_PER_INSTR);
        entry_permille.saturating_mul(CHAIN_CALL_INSTRS + clear) >= 1000 * clear
    }

    /// A link's own cost: its body plus every body under it that is not
    /// itself a chain row (those are judged as links of their own), as
    /// (bytecode bytes, `max_locals`).
    fn link_cost(site: &InlineSite) -> (usize, usize) {
        let mut bytes = site.callee_code_len;
        let mut locals = site.callee_max_locals;
        for n in &site.nested_sites {
            if n.guard_class_id != 0 && row_is_chain(site, n) {
                continue;
            }
            let (b, l) = link_cost(&n.site);
            bytes += b;
            locals += l;
            if let Some(second) = &n.bimorphic_second {
                let (b, l) = link_cost(&second.site);
                bytes += b;
                locals += l;
            }
        }
        (bytes, locals)
    }

    /// W7-B fix (round 2): the planner's cost rule over one top-level splice
    /// tree, run after the admission strips. A chain row is kept only when
    ///
    /// * a loop ROOTS its chain: the row's site is in a loop of its enclosing
    ///   body, or the enclosing body is itself a kept chain row (or the
    ///   top-level body, when `top_rooted`: its top-level site is in a loop of
    ///   the compiling method). That is the chain running per iteration of a
    ///   loop, which is where splicing a link removes calls per root entry
    ///   (`Start.match`'s position loop, `Branch.match`'s alternatives, the
    ///   back-off loop of a greedy node). A chain entered once per root entry
    ///   (`Node.study` under `Pattern.compile`, every `split` row compiles its
    ///   pattern per op) only grew the root's frame and took splice budget;
    /// * the tree's kept link bytes stay within [`CHAIN_TREE_MAX_BYTES`].
    ///
    /// Dropping a row keeps the call at its pc, as every strip does, and never
    /// withdraws an admitted parent: the link rule lets a link keep its
    /// next-link call. A no-op with the switch off.
    ///
    /// `keys` receives the method key (`Class.name:desc`, the site table's
    /// `method_key`) of every kept link body, for [`nested_hot_cap_pcs`].
    pub(crate) fn strip_chain_tree(
        site: &mut InlineSite,
        top_rooted: bool,
        keys: &mut Vec<String>,
    ) -> ChainTreeReport {
        let mut report = ChainTreeReport::default();
        if ir_nested_chain_splice_enabled() {
            let (bytes, locals) = link_cost(site);
            report.est_frame_before = est_frame_bytes(bytes, locals);
            let _ = (site, top_rooted, keys);
            report.est_frame_after =
                report.est_frame_before + est_frame_bytes(report.bytes, report.added_locals);
        }
        report
    }

    /// Would `n` (and its second body) be admitted under `parent` by the
    /// plain nested rules of [`ir_splice_nested_exact_admitted_in`], without
    /// the chain rule? Its size admissions only: the switches and chain fences
    /// that rule also asks are the same for both.
    fn plain_row_admitted(parent: &InlineSite, n: &NestedInlineSite) -> bool {
        let fits = |site: &InlineSite, bodies: usize| {
            site.callee_code_len
                <= ir_splice_nested_exact_size_cap(
                    &parent.callee_code,
                    parent.callee_code_len,
                    n.callee_pc,
                    site.callee_num_args,
                )
                || ir_nested_hot_body_admitted(
                    site,
                    ir_nested_row_hot_cap(parent, n.callee_pc, bodies),
                )
                || ir_psplice_nested_exact_row_admitted(
                    site,
                    parent
                        .callee_receiver_profile
                        .iter()
                        .find(|(pc, _)| *pc == n.callee_pc)
                        .map(|(_, counts)| counts),
                    &parent.callee_code,
                    parent.callee_code_len,
                    n.callee_pc,
                    site.callee_num_args,
                )
        };
        let bodies = 1 + usize::from(n.bimorphic_second.is_some());
        fits(&n.site, bodies) && n.bimorphic_second.as_ref().is_none_or(|s| fits(&s.site, 2))
    }

    fn method_key(site: &InlineSite) -> String {
        format!(
            "{}.{}:{}",
            site.class_name, site.method_name, site.descriptor
        )
    }


    /// JIT r25 wave 9 (W9-3): the chain links one compile kept, summed over
    /// its top-level trees, for the `chain-compile` line printed with the
    /// compile's outcome. It answers, in one host run, the three suspects
    /// left after W8-5 for the rows the switch still loses (`split-ws`,
    /// `cached`, `matchonly`, `regex-split`):
    ///
    /// 1. **The acceptance gate.** A compile whose spliced chain left calls
    ///    the lowerer had to route through the blind dispatch helper is
    ///    priced as a regression (`CompileRecord::added_ns_per_execution`:
    ///    171 ns a blind dispatch against 4 ns a spliced frame) and its
    ///    optimizing body is DISCARDED: the root then runs its single-pass
    ///    body, and every splice of the round with it. `verdict=` and
    ///    `blind_in_splice=` / `added_ns=` name it.
    /// 2. **The real frame.** `real_frame=` is the lowered body's frame
    ///    (`CompiledMethod::osr_frame_size`, the `SUB RSP` size) beside
    ///    `est_frame_added=`, the cost rule's estimate of the links' share. A
    ///    real frame far above the estimate means W8-5 priced the wrong
    ///    frame.
    /// 3. **Field proofs.** Not on this line: the same compile's
    ///    `[cratonvm-jitc] ir proven-receiver-fields: loads full= ...
    ///    method=` line (`CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`), whose
    ///    `full=` count for the root against the link's own artifact says
    ///    whether the spliced link lost the proofs its own body made.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct ChainCompileCensus {
        /// Top-level trees that kept at least one link.
        pub trees: usize,
        /// Links kept, over every tree.
        pub links: usize,
        /// Links the frame-growth rule dropped.
        pub costly: usize,
        /// The kept links' estimated frame bytes ([`est_frame_bytes`]).
        pub est_frame_added: usize,
        /// The least entries per thousand top-level entries of a kept link.
        pub min_entry_permille: Option<u64>,
    }

    impl ChainCompileCensus {
        /// Fold one tree's report in.
        pub fn note(&mut self, r: &ChainTreeReport) {
            if r.kept > 0 {
                self.trees += 1;
            }
            self.links += r.kept;
            self.costly += r.costly;
            self.est_frame_added += r.est_frame_after.saturating_sub(r.est_frame_before);
            if let Some(m) = r.min_entry_permille {
                self.min_entry_permille = Some(self.min_entry_permille.map_or(m, |o| o.min(m)));
            }
        }

        /// The `chain-compile` line for compile `method`: whether it lowered
        /// and whether the acceptance gate kept it, its real frame bytes and
        /// the gate's inputs.
        pub fn line(
            &self,
            method: &str,
            lowered: bool,
            accepted: bool,
            real_frame: Option<i32>,
            rec: Option<ir_evidence::CompileRecord>,
        ) -> String {
            let verdict = if !lowered {
                "NOT-LOWERED (single-pass body runs)"
            } else if accepted {
                "accepted"
            } else {
                "REFUSED by the acceptance gate (single-pass body runs)"
            };
            let gate = match rec {
                Some(rec) => format!(
                    "spliced_frames={} blind_in_splice={} added_ns={}",
                    rec.spliced_frames,
                    rec.blind_dispatches_in_splice,
                    rec.added_ns_per_execution()
                ),
                None => "evidence=unjudged".to_string(),
            };
            format!(
                "[cratonvm-jitc] chain-compile {method}: trees={} links={} costly={} \
                 min_entry_permille={} est_frame_added={} real_frame={} verdict={verdict} {gate}",
                self.trees,
                self.links,
                self.costly,
                self.min_entry_permille
                    .map_or("-".to_string(), |m| m.to_string()),
                self.est_frame_added,
                real_frame.map_or("-".to_string(), |f| f.to_string()),
            )
        }
    }

    /// JIT r25 wave 10 (W10-3): `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH`,
    /// **default ON under the chain switch** (`=0` kill; inert with
    /// [`ir_nested_chain_splice_enabled`] off). Read per call, no `static`.
    ///
    /// The c22 disassembly of `Matcher.search` on `split-ws` (`=1`) shows the
    /// cost the chain splices added. The links take nest levels from their
    /// own callees. The chain `search` -> `Start` (0) -> `Greedy` (1) ->
    /// `CharPredicate.is` forwarder (2) -> `lambda$ASCII_SPACE$0` (3) ->
    /// `ASCII.isSpace` (4) -> `ASCII.isType` (5) reaches the cap at
    /// `ASCII.getType` (6). It is refused there (`nest-static ...getType ...
    /// depth=6 -> refused`): a 17-byte body, over the leaf tier's 16. So every
    /// character the `\s+` scan reads makes a real compiled call into
    /// `getType`. That call costs 15 frame-block home clears, the
    /// frame-record and stack checks, the call itself, the callee's prologue
    /// and frame clear and the post-call thread stores: about 70 instructions.
    /// `Greedy`'s own artifact had spliced it (depth 4). Over the 96-character
    /// `split-ws` text that is ~115 such calls, about 8 000 instructions per
    /// op, the whole c12 loss (64 912 against 56 862).
    ///
    /// Two changes give the links' callees their levels back:
    ///
    /// * the unset `CRATONVM_JIT_IR_INLINE_MAX_NEST` becomes the deepest
    ///   nesting the builder opens ([`ir_inline_max_nest_frame_cap`]: 7
    ///   with the leaf tier, 8 frames), rather than 6. HotSpot's
    ///   `MaxInlineLevel` is 15, so depth is not a limit for it on these chains;
    /// * the leaf tier past the cap admits a call-free static body of up to
    ///   [`CHAIN_LEAF_MAX_BYTES`] (`getType`, 17 bytes, with its `ctype` read
    ///   and branch), as [`chain_leaf_splice_body`] judges.
    ///
    /// The splice-depth bound stays the builder's (`ir::MAX_IR_SPLICE_DEPTH`).
    /// The byte budget and every admission are unchanged.
    pub fn ir_nested_chain_depth_enabled() -> bool {
        // JIT r25 wave 10 (W10-4): or under the lambda-body interface rows,
        // whose `ASCII_WORD` predicate chain ends at the same `getType`.
        ir_nested_chain_splice_enabled()
            || crate::r25_w10_4_lambda_iface::ir_nested_lambda_iface_enabled()
    }

    /// `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH_SCOPED` (r25 W11-7 review fix;
    /// default ON, `=0` restores the global raise). Read per call, no `static`.
    ///
    /// W10-3 raised the unset nest depth and the leaf tier for every optimizing
    /// compile, while its reason (the regex `\s+` chain reaching
    /// `ASCII.getType` at depth 6) is a dispatch chain's. Measured on 64.177
    /// against `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH=0` (compile-worker CPU, 3-5
    /// interleaved reps): javac compiling three probe sources 82.1 s against
    /// 72.7 s (+13 %), an H2 SQL workload 8.7 s against 8.3 s (+5 %),
    /// CratonBench and `split-ws` flat. So the raise now holds only in a
    /// splice tree below a chain body or a lambda body whose interface rows
    /// W10-4 opens (the VM resolver's `IR_CHAIN_TREE`).
    pub fn chain_depth_scoped_enabled() -> bool {
        true
    }

    /// Does the W10-3 raise apply to a resolution inside (`chain_tree`) or
    /// outside a chain splice tree?
    pub fn raise_applies(chain_tree: bool) -> bool {
        ir_nested_chain_depth_enabled() && (chain_tree || !chain_depth_scoped_enabled())
    }

    /// The optimizing tier's nest depth when `CRATONVM_JIT_IR_INLINE_MAX_NEST`
    /// is unset, inside a chain tree: [`default_nest_depth_in`]`(true)`.
    pub fn default_nest_depth() -> usize {
        default_nest_depth_in(true)
    }

    /// [`MAX_IR_INLINE_NEST_DEPTH`], or where [`raise_applies`] the builder's
    /// frame cap.
    pub fn default_nest_depth_in(chain_tree: bool) -> usize {
        if raise_applies(chain_tree) {
            ir_inline_max_nest_frame_cap(ir::ir_splice_leaf_past_depth_enabled())
                .max(MAX_IR_INLINE_NEST_DEPTH)
        } else {
            MAX_IR_INLINE_NEST_DEPTH
        }
    }

    /// W10-3: the largest call-free static body the leaf tier takes past the
    /// nest cap under [`ir_nested_chain_depth_enabled`] (16 otherwise).
    pub const CHAIN_LEAF_MAX_BYTES: usize = 24;

    /// W10-3: [`ir::ir_leaf_splice_body`]'s shape rule (no invoke,
    /// allocation, `athrow`, monitor or switch) up to `max` bytes.
    pub fn leaf_body_within(code: &[u8], code_len: usize, max: usize) -> bool {
        let len = code_len.min(code.len());
        if len == 0 || len > max {
            return false;
        }
        let mut pc = 0usize;
        while pc < len {
            if matches!(
                code[pc],
                0xaa | 0xab | 0xb6..=0xbf | 0xc2 | 0xc3 | 0xc4 | 0xc5
            ) {
                return false;
            }
            let step = crate::bytecode_analysis::step(code, pc);
            if step == 0 {
                return false;
            }
            pc += step;
        }
        true
    }

    /// The leaf tier's body rule: [`ir::ir_leaf_splice_body`], or under
    /// [`ir_nested_chain_depth_enabled`] the same shape up to
    /// [`CHAIN_LEAF_MAX_BYTES`].
    pub fn chain_leaf_splice_body(code: &[u8], code_len: usize) -> bool {
        chain_leaf_splice_body_in(code, code_len, true)
    }

    /// [`chain_leaf_splice_body`] inside (`chain_tree`) or outside a chain
    /// splice tree: the 24-byte rule only where [`raise_applies`].
    pub fn chain_leaf_splice_body_in(code: &[u8], code_len: usize, chain_tree: bool) -> bool {
        ir::ir_leaf_splice_body(code, code_len)
            || (raise_applies(chain_tree) && leaf_body_within(code, code_len, CHAIN_LEAF_MAX_BYTES))
    }

    #[cfg(test)]
    mod tests;
}

/// Round rx27 wave 1 (lane chain, `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`,
/// opt-in; `docs/feature-designs/regex-jit-contract.md`,
/// "Round rx27 wave 1 (lane chain)"): a loop-rooted body whose in-loop rows
/// the caller's compile cannot hold stays a call, and keeps its own compile.
///
/// Three measured cases share one premise. A body with a loop, spliced into
/// a caller, takes its in-loop call sites with it; they are planned there
/// under what is left of the CALLER's budget and as nested rows, where its
/// own compile plans them as top-level in-loop sites with the whole budget.
/// When the caller cannot hold them they become calls, paid once per loop
/// iteration, to remove one call per entry:
///
/// * W13-6 `cached`: `Pattern.split(CharSequence,int,boolean)` moved up
///   through three callers (`left=181`), and `Matcher.find` / `subSequence`
///   became calls in its split loop (+12.6 %);
/// * W13-6 `matchonly`: with `Matcher.<init>` spliced, `Start.match` (the
///   position loop) resolved into `Matcher.search` pc 121 with three of its
///   nested bodies pruned (+8.3 %);
/// * round 16 wave c: `boxHelper(I)J`'s loop spliced into `run(II)J` lost
///   its `intValue()` intrinsic (16 -> 195 ns/op).
///
/// W13-6 fixed the first by its trigger (an `anewarray` callee with a back
/// edge, `ir::splice_anewarray_refuses_loop_callee`). This rule asks the
/// premise instead, after the append, from what the append actually did:
///
/// * `resolved`: the tree's rows under a loop of the tree (a row whose call is
///   in a loop of its enclosing body, and every row below it) as the resolver
///   built them, less the rows the caller's compile could not splice in the
///   callee's own compile either (the String-prefix carve-out, the append's
///   static refusals);
/// * `planned`: the same count after every nested strip of this compile;
/// * `spliced`: the same count read back from the site table the append
///   wrote (the bodies really appended, under the caller's budget).
///
/// `spliced < planned` means the append ran out of budget (or the nested
/// cold reserve) inside a loop: the site is rolled back and stays a call.
/// With `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS=1` also
/// `spliced < resolved` (a nested strip of this compile dropped an in-loop
/// row), which is the `matchonly` reading if its pruned bodies were in-loop.
///
/// Sound: it only ever keeps a call the planner could keep (the same
/// rollback as a refused bimorphic second body). Bimorphic and polymorphic
/// top-level sites, synchronized bodies and self-recursion plans are not
/// judged.
pub mod rx27_loop_root {
    use super::*;

    /// The switch (default on).
    pub const SWITCH: &str = "CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE";
    /// Also count the nested strips as losses (default on, inert alone).
    pub const STRIPS_SWITCH: &str = "CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS";

    /// `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE`. Read per call, no `static`.
    pub fn enabled() -> bool {
        cratonvm_types::flags::runtime_flag_on(SWITCH)
    }

    /// `CRATONVM_JIT_IR_LOOP_ROOT_OWN_COMPILE_STRIPS`, under [`enabled`].
    pub fn strips_counted() -> bool {
        enabled() && cratonvm_types::flags::runtime_flag_on(STRIPS_SWITCH)
    }

    /// The rows of `site`'s tree under a loop of the tree: a row whose call
    /// pc is in a loop of its enclosing body, and every row below one. First
    /// bodies only (a bimorphic second is not counted, here or in
    /// [`spliced_in_loop_rows`]).
    pub fn in_loop_rows(site: &InlineSite) -> usize {
        rows_under(site, false)
    }

    fn rows_under(site: &InlineSite, under: bool) -> usize {
        site.nested_sites
            .iter()
            .map(|n| {
                let in_loop = under
                    || bytecode_pc_in_loop(&site.callee_code, site.callee_code_len, n.callee_pc);
                usize::from(in_loop) + rows_under(&n.site, in_loop)
            })
            .sum()
    }

    /// The `resolved` count: [`in_loop_rows`] of a copy of `site` with only
    /// the strips that bind the callee's own compile too (the nested String
    /// prefix carve-out, and the append's static refusals,
    /// [`ir_splice_prune_refused_nested`], when the planner runs it).
    /// `None` when the tree has no row under a loop: nothing to judge.
    pub(crate) fn resolved_in_loop_rows(
        site: &InlineSite,
        flags: ir::SpliceFenceRelax,
        chain_fences: bool,
    ) -> Option<usize> {
        if in_loop_rows(site) == 0 {
            return None;
        }
        let mut copy = site.clone();
        let _ = ir_splice_strip_string_prefix_nested(&mut copy);
        {
            let _ = ir_splice_prune_refused_nested(&mut copy, flags, chain_fences);
        }
        let rows = in_loop_rows(&copy);
        (rows > 0).then_some(rows)
    }

    /// The `spliced` count, read back from the append's site table `sites`
    /// (keyed by caller pc in the combined buffer `combined`): the rows under
    /// a loop of the tree whose top-level body is keyed `top`. A row's
    /// children are the keys inside its body's range; a bimorphic or
    /// polymorphic second body's tagged key is in no range.
    pub fn spliced_in_loop_rows(
        sites: &HashMap<usize, ir::IrInlineSite>,
        combined: &[u8],
        top: usize,
    ) -> usize {
        spliced_under(sites, combined, top, false)
    }

    fn spliced_under(
        sites: &HashMap<usize, ir::IrInlineSite>,
        combined: &[u8],
        key: usize,
        under: bool,
    ) -> usize {
        let Some(row) = sites.get(&key) else {
            return 0;
        };
        let end = row.base.saturating_add(row.code_len);
        let code = combined.get(row.base..end.min(combined.len())).unwrap_or(&[]);
        sites
            .keys()
            .filter(|&&k| k >= row.base && k < end && k != key)
            .map(|&k| {
                let in_loop = under || bytecode_pc_in_loop(code, code.len(), k - row.base);
                usize::from(in_loop) + spliced_under(sites, combined, k, in_loop)
            })
            .sum()
    }

    /// The verdict: does the site stay a call? See the module comment.
    pub fn keeps_call(resolved: usize, planned: usize, spliced: usize, strips: bool) -> bool {
        spliced < planned || (strips && spliced < resolved)
    }

    /// The census line (`CRATONVM_DBG_JITC=1`), one per judged site.
    #[allow(clippy::too_many_arguments)]
    pub fn census_line(
        callee: &str,
        pc: usize,
        root: &str,
        resolved: usize,
        planned: usize,
        spliced: usize,
        budget_left: usize,
        kept_call: bool,
    ) -> String {
        format!(
            "[cratonvm-jitc] ir-loop-root-own-compile {callee} at pc={pc} in {root}: in-loop rows \
             resolved={resolved} planned={planned} spliced={spliced} budget_left={budget_left} -> {}",
            if kept_call {
                "kept as a call (compiled as its own root)"
            } else {
                "spliced (this compile holds its in-loop rows)"
            }
        )
    }

    #[cfg(test)]
    mod tests;
}

/// Round rx27 wave 4 (lane chain, `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`, opt-in;
/// page `docs/feature-designs/jit-ir-optimizations.md`):
/// the call that ENTERS a dispatch chain from a non-chain body is judged by the
/// chain link rule.
///
/// `regex-matches` runs `WORDNUM.matcher(s).matches()` per subject. With the
/// lambda spliced into the driver, `matches()` -> `match()` splice as
/// forwarder / exact rows, and `match()`'s `parentPattern.matchRoot.match(..)`
/// is a profiled nested row of `BmpCharPropertyGreedy.match` (89 bytes, with
/// calls). The plain nested rule admits a body over 35 bytes only as a leaf,
/// and the chain rule only at a call of the body's own selector, which
/// `Matcher.match(II)Z` -> `Node.match(Matcher;I;CharSequence)Z` is not. So the
/// greedy node's own artifact is called once per subject: a 928-byte frame
/// clear, a stack guard and argument moves (`docs/feature-designs/jit-ir-optimizations.md`,
/// 7.2k instr/op). HotSpot inlines it.
///
/// Under the switch a profiled nested row whose BODY is chain-shaped
/// ([`r25_w7b_chain_splice::body_is_chain_shaped`]) is admitted by
/// [`r25_w7b_chain_splice::chain_link_verdict`] (hot site, the multi-body hot
/// cap, and every call of the body spliced or its next link), in the VM's
/// resolver and the planner alike (`chain_row_admitted_in`). The planner's
/// cost rule (`strip_chain_tree`) judges it as a link, rooted when its site or
/// any site above it is in a loop. Rows, guards and miss edges are the nested
/// guarded row's, unchanged.
///
/// Census: `[cratonvm-jitc] chain-entry <cp>.match.. at callee_pc=P depth=D in
/// <body>` followed by that site's `nest-chain ... SPLICED | refused: <why>`.
pub mod rx27_chain_entry {
    use super::*;

    /// The switch (opt-in; inert without the chain switch).
    pub const SWITCH: &str = "CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE";

    /// `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE`, under the chain switch. **Opt-in**
    /// (`=1`). It was default ON in the rx27 bundle flip of 2026-10-29
    /// ([`bundle_flip_evidence`]) and went back to opt-in the same day: on
    /// bimodal-10 (14 rows, 5 reps) it was the one bundle member that regressed
    /// `cached` (+19 %) and `split-ws` (+14 %). With it off and the deep spine
    /// on ("dse0"), `cached` read 45.1k -> 35.8k, `split-ws` 49.3k -> 42.9k,
    /// `regex-split` -5 %, every other row within 1.5 %. Read per call, no
    /// `static`.
    pub fn enabled() -> bool {
        r25_w7b_chain_splice::ir_nested_chain_splice_enabled()
            && cratonvm_types::flags::runtime_flag_on(SWITCH)
    }

    /// The rx27 bundle flip (2026-10-29, coordinator) initially turned this
    /// switch, `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`,
    /// `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_PREDICATES`,
    /// `CRATONVM_JIT_IR_CHAIN_BIMORPHIC_LINKS` and
    /// `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM` default ON together.
    /// `CHAIN_BIMORPHIC_LINKS` returned to opt-in after the direct GroupTail
    /// A/B recorded in the fixed rx27 issue document. This function remains a
    /// historical evidence anchor for the bundle. The original evidence
    /// (lane prof, icount2 main/op, 5 reps each, against the default):
    /// * regex-matches -8.6 % (b7), -8 % (b8), -11.8 % (b9), ranges disjoint
    ///   each time;
    /// * regex-groups -11.5 % (b7), overlapping on b9;
    /// * find-email / find-ci -0.3 / -0.4 %, disjoint;
    /// * facts and the GC battery (34/34) identical under the bundle on b9.
    ///
    /// They flip together because the entry alone regressed regex-groups by
    /// 11 % (b8): it must not run without the predicate pairs.
    ///
    /// Revised the same day ("dse0", lane bimodal's A/B on bimodal-10): chain
    /// entry is opt-in again (inside the bundle it cost `cached` +19 % and
    /// `split-ws` +14 %). The nested-in-loop hot cap, predicate pairs and cold
    /// handler arm remain ON; chain links are opt-in after the GroupTail A/B.
    /// `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE` is default ON
    /// (`rx27_deep_spine::enabled`). A doc anchor only.
    pub const fn bundle_flip_evidence() {}

    /// The VM resolver's form: may the resolved body `site` be judged as a
    /// chain entry?
    pub fn entry_body(site: &InlineSite) -> bool {
        enabled() && !site.callee_is_static && r25_w7b_chain_splice::body_is_chain_shaped(site)
    }

    /// Round rx27 wave 7 (lane chain): the receiver row a chain entry is
    /// judged by when the enclosing body's interpreted profile has none at
    /// the site (`has_row` false) and the class came from that body's own
    /// optimizing artifact (`from_guard_class`), which guard-spliced `site`
    /// there. A body over the cold cap is guard-spliced only at a site that
    /// compile judged hot (`ir_guarded_site_is_hot`), so the row carries
    /// [`r25_w7b_chain_splice::CHAIN_HOT_MIN_CALLS`] samples of `class_id`,
    /// or the enclosing body's interpreted `invocations` when that is more
    /// (the planner's frame rule divides the row by that count: one sample
    /// per invocation reads as a site run on every entry, which a site the
    /// artifact spliced unguarded-by-branch is).
    /// `Matcher.match` pc 121 is the case: `Matcher.match` compiles before
    /// the row lambda that splices it, its interpreted profile stops there,
    /// and its artifact splices the greedy node at that pc.
    pub fn artifact_hot_row(
        entry_site: bool,
        from_guard_class: bool,
        has_row: bool,
        site: &InlineSite,
        class_id: u32,
        invocations: u32,
    ) -> Option<profile::ReceiverCounts> {
        if !entry_site
            || !from_guard_class
            || has_row
            || class_id == 0
            || site.callee_code_len <= MAX_INLINE_SIZE_COLD
            || !enabled()
        {
            return None;
        }
        let mut row = profile::ReceiverCounts::default();
        row.insert(class_id, r25_w7b_chain_splice::CHAIN_HOT_MIN_CALLS.max(invocations));
        Some(row)
    }

}

/// Round rx27 wave 4 (lane chain, `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`, opt-in;
/// same page): a nested call in a loop of its enclosing body is hot.
///
/// The nested hot cap ([`ir_nested_guarded_body_cap_in`]) counted a nested row
/// hot from 256 interpreted receiver samples or on its body's entry prefix,
/// and the planner's cold prune ([`ir_splice_prune_cold_nested`]) kept a
/// statically bound nested body over 35 bytes only on the entry prefix or the
/// profiled dominant path. A call in a loop of a spliced body (the
/// `regex-groups` lambda's `m.group(1)` in its `while (m.find())` loop, once
/// the lambda is spliced into the driver) is neither, so `Matcher.group`
/// stays a call: 3.6k instr/op. HotSpot's C2 inlines a site by its frequency,
/// which a loop makes high. Under the switch:
///
/// * a site in a loop of its enclosing body takes the nested hot cap (the
///   profiled rows keep their leaf rule);
/// * a nest-exact row's tree may hold a loop there
///   ([`ir_psplice_nested_exact_row_admitted`]: `Matcher.<init>`'s `groups`
///   fill under `Pattern.matcher` in `regex-matches`);
/// * the cold prune keeps a row in a loop of its enclosing body, and every
///   row under one.
///
/// The VM's resolver and the planner read the same functions, so they agree.
/// Bounded by the hot cap, the per-compile budget and every other rule.
pub mod rx27_nested_in_loop_hot {
    use super::*;

    /// The switch (opt-in).
    pub const SWITCH: &str = "CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT";

    /// `CRATONVM_JIT_IR_NESTED_IN_LOOP_HOT`. **Default ON** since the rx27
    /// bundle flip (2026-10-29, `rx27_chain_entry::bundle_flip_evidence`),
    /// `=0` kills it. Read per call, no `static`.
    pub fn enabled() -> bool {
        cratonvm_types::flags::runtime_flag_default_on(SWITCH)
    }

    /// Under the switch, is the call at `callee_pc` of `code[..code_len]` in a
    /// loop of that body?
    pub fn site_in_loop(code: &[u8], code_len: usize, callee_pc: usize) -> bool {
        enabled() && bytecode_pc_in_loop(code, code_len, callee_pc)
    }

    #[cfg(test)]
    mod tests;
}

/// Round rx27 wave 4 (lane chain, `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE`, default ON since 2026-10-29;
/// page `docs/feature-designs/jit-ir-optimizations.md`):
/// a dispatch-chain spine deeper than the builder's 8 frames, with the cost
/// rule made transitive so a deeper link never trades a call per position for
/// a call per character.
///
/// `regex-groups`' `(\w+)=(\w+)` from `Matcher.search` pc 121 is `Start` 0,
/// `GroupHead` 1, `Greedy` 2, `GroupTail` 3, `BmpCharProperty` 4, `GroupHead`
/// 5, `Greedy` 6, and each `Greedy`'s `\w` predicate adds forwarder, lambda,
/// `isWord`, `isType`, `getType` (5 levels) under it. The second `Greedy`
/// therefore needs 12 splice frames. Three parts, one switch:
///
/// 1. **Frames.** [`ir::ir_max_splice_depth`] is [`SPLICE_DEPTH`] (12), and
///    so the chain tree's unset nest depth (`ir_inline_max_nest_frame_cap`)
///    is 11 plus the leaf tier. Outside chain trees the unset depth stays 6
///    (W11-7's scope rule).
/// 2. **Resume budget.** [`crate::deopt::max_osr_inline_resume_depth`] is
///    [`RESUME_DEPTH`] (13, one more than the frames, as 9 is to 8), read by
///    every admission and transfer check on both sides (the jit's OSR-exit
///    policy and chain publication, the VM's `deopt_resume` sinks). The sinks
///    build the chain in a `Vec` and refuse before the first push, so the
///    budget bounds work, not storage.
/// 3. **Transitive link rule.** The chain link rule asked only that each call
///    of the link has a row; a row that itself keeps a call counted as
///    spliced. With more frames that admits the second `Greedy` with its
///    forwarder spliced and the `\w` lambda left as a call once per
///    character (W10-3's loss). Under the switch, a link is refused
///    ([`r25_w7b_chain_splice::ChainRefusal::HalfSpliced`]) when a row at a pc
///    in a loop of the link keeps a call anywhere in its own tree
///    ([`first_half_spliced_row`]).
///
/// Census: `nest-chain ... refused: half-spliced row at body pc P` (the VM
/// resolver's chain line), `[cratonvm-jitc] chain-deep-spine splice at pc=..
/// depth=D` (the builder, per frame past 8), and the `ir-chain-tree` line's
/// `max_depth`.
pub mod rx27_deep_spine {
    use super::*;

    /// The switch (default ON since 2026-10-29, `=0` kills it; inert without
    /// the chain switch).
    pub const SWITCH: &str = "CRATONVM_JIT_IR_CHAIN_DEEP_SPINE";
    /// The builder's splice-frame cap under the switch.
    pub const SPLICE_DEPTH: usize = 12;
    /// The inline resume budget under the switch.
    pub const RESUME_DEPTH: usize = SPLICE_DEPTH + 1;

    /// `CRATONVM_JIT_IR_CHAIN_DEEP_SPINE`, under the chain switch. **Default
    /// ON** since 2026-10-29 (`=0` kills it), with
    /// `CRATONVM_JIT_IR_CHAIN_ENTRY_SPLICE` back to opt-in ("dse0", coordinator
    /// decision from lane bimodal's A/B on bimodal-10, 14 rows, 5 reps, against
    /// the bundle default):
    /// * `cached` 45.1k -> 35.8k (-21 %), `split-ws` 49.3k -> 42.9k (-13 %),
    ///   `regex-split` -5 %, ranges separate;
    /// * every other row within 1.5 %;
    /// * facts, the GC probes and `J27DeoptRows` identical in both modes.
    ///
    /// The deep spine alone lost on find-ci and matchonly only together with
    /// the entry. Read per call, no `static`.
    pub fn enabled() -> bool {
        r25_w7b_chain_splice::ir_nested_chain_splice_enabled()
            && cratonvm_types::flags::runtime_flag_default_on(SWITCH)
    }

    /// Does `site`'s tree keep a real call: a call with no nested row, other
    /// than one the inline String prefix answers (W10-3b) or a constructor
    /// (`<init>`, the exception a guard path builds)? Bimorphic second bodies
    /// are asked too.
    pub fn tree_keeps_call(site: &InlineSite) -> bool {
        let kept_here = site.invoke_targets.iter().any(|(pc, t)| {
            !site.nested_sites.iter().any(|n| n.callee_pc == *pc)
                && t.method_name != "<init>"
                && !crate::r25_w10_3b::site_answered_by_string_prefix(
                    &t.class_name,
                    &t.method_name,
                    &t.descriptor,
                    t.invoke_kind,
                )
        });
        kept_here
            || site.nested_sites.iter().any(|n| {
                tree_keeps_call(&n.site)
                    || n.bimorphic_second
                        .as_ref()
                        .is_some_and(|s| tree_keeps_call(&s.site))
            })
    }

}

/// JIT r25 wave 10, lane W10-4 (`CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE`,
/// default ON since the wave-10 flip, `=0` kills it; `docs/feature-designs/regex-jit-contract.md`,
/// "Wave 10 W10-4"): profile-guarded nested rows for the interface calls inside
/// a spliced LAMBDA IMPLEMENTATION body.
///
/// The `regex-find-email` profile (host c22, default arm, `perf record -e
/// instructions:u`, `CRATONVM_JIT_PERF_MAP=1`) puts 20.5 % of user
/// instructions in the compiled body of `CharPredicates.lambda$ASCII_WORD$0`
/// and 4.1 % in `Pattern$BitClass.is`. Each is called once per character.
/// `[\w.+-]+` compiles to `BmpCharPropertyGreedy` over a UNION predicate,
/// `CharPredicate.union`'s lambda `ch -> is(ch) || p.is(ch)`. The greedy
/// node's own compile splices its `predicate.is` site as the union lambda
/// (forwarder plus `lambda$union$N`, the lambda route). That body's two
/// calls are `invokeinterface CharPredicate.is` on the captured predicates
/// (the `\w` lambda proxy and a `BitClass`), and they stay calls: one level
/// down, an interface site has a profiled nested route only under the
/// global, opt-in `CRATONVM_JIT_IR_NESTED_GUARDED_INTERFACE`.
///
/// Under this switch those sites, and only sites inside a body whose method is
/// a javac lambda implementation (`lambda$...`), take the existing
/// `nest-profiled-iface` row. That row is the callee profile's dominant class
/// (80 %, or the compiled cache's), spliced behind an exact class test, with
/// the site's unchanged interface call (and its inline cache) on the miss
/// edge. A lambda-proxy receiver resolves through the nested lambda route
/// (forwarder plus implementation). HotSpot inlines the same sites from the
/// same per-bci profile.
///
/// `CRATONVM_JIT_IR_NESTED_CHAIN_DEPTH` (W10-3) applies under this switch too:
/// the `\w` predicate's chain (`lambda$ASCII_WORD$0` -> `ASCII.isWord` ->
/// `isType` -> `getType`) ends past the default nest cap from a greedy
/// node's own compile (forwarder 0, union implementation 1, `\w` forwarder 2,
/// its implementation 3, `isWord` 4, `isType` 5, `getType` 6).
pub mod r25_w10_4_lambda_iface {
    /// The switch's token.
    pub const SWITCH: &str = "CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE";

    /// `CRATONVM_JIT_IR_NESTED_LAMBDA_IFACE` (default ON since the r25 wave-10
    /// flip; `=0` kills it). Read per call, no
    /// `static`.
    pub fn ir_nested_lambda_iface_enabled() -> bool {
        true
    }

    /// Is `method` a javac lambda implementation (`lambda$<enclosing>$<n>`,
    /// JLS-unnamed, synthetic)? javac names every desugared lambda body this
    /// way. A user method can never start with `lambda$` and also be an
    /// enclosing body of a CharPredicate splice by accident, and if it does,
    /// its interface sites only take the profiled guarded row every other
    /// nested site may take.
    pub fn is_lambda_impl_name(method: &str) -> bool {
        method.starts_with("lambda$")
    }

    /// May an interface site inside the body `method` take the profiled
    /// nested row under this switch?
    pub fn admits_body(method: &str) -> bool {
        ir_nested_lambda_iface_enabled()
            && crate::ir::ir_nested_guarded_splice_enabled()
            && is_lambda_impl_name(method)
    }

    #[cfg(test)]
    mod tests;
}

/// JIT r25 wave 10, lane W10-5 (`CRATONVM_JIT_LAMBDA_ADAPTER_SUPERSEDE`,
/// **default ON**, `=0` kill; fix): a lambda-adapter thunk stops entering a
/// superseded implementation body.
///
/// [`crate::lambda_adapter`] builds one thunk per `(proxy class,
/// implementation entry, shape)` and installs it in inline caches as if it
/// were the receiver's compiled method. The thunk tail-jumps to the fixed
/// entry it was built for. When the implementation's body is superseded (C1
/// -> C2), the supersede path retargets the caches that publish the displaced
/// BODY (`retarget_superseded_inline_caches`). The caches that publish a
/// THUNK into it hold the thunk as their owner, so they were never reached:
/// every call kept entering the C1 body, and its not-entrant stub once the
/// body was redirected. `regex-split` spent 2.9 % in `Pattern.lambda$Single$0
/// [c1]`, and `regex-find-email` 0.8 % plus 0.27 % in
/// `not-entrant-stub lambda$Single$0`.
///
/// On supersede, every thunk built for the displaced body (matched by its
/// recorded `(entry, artifact id)`, so an address the allocator reused never
/// matches) is taken out of the thunk map and treated like a superseded body:
/// it is marked `superseded`, every MIC / PIC slot holding it is cleared
/// (`retarget_superseded_inline_caches` on the THUNK), it leaves the shared
/// megamorphic table, and it is marked `retired` (refused by every installer)
/// and handed to the retirement queue. The next call through each cache
/// misses into the VM's lambda helper. The helper re-reads the site's
/// implementation body after the generation bump and builds a thunk for the
/// current body. `lambda_adapter::adapter_entry` refuses to build one for a
/// superseded body, with the same two-check pairing it uses for a retired one,
/// so a thunk raced in after this sweep is never findable.
///
/// Lifetime: a thunk roots its implementation (`_direct_callee_roots`), so the
/// displaced body stays mapped until the thunk itself leaves the retirement
/// queue. That happens once no thread can still be executing it, which is the
/// rule baked callers already follow. No new statics.
mod r25_w10_5_adapter_supersede {
    use super::*;

    impl JitCache {
        /// The supersede half for thunks into `old`. Called with `mutation`
        /// held, after `old.superseded` was set. Returns the thunks withdrawn.
        pub(crate) fn retarget_superseded_lambda_adapters(&self, old: &CompiledMethod) -> usize {
            if !crate::lambda_adapter::w10_5_supersede::lambda_adapter_supersede_enabled() {
                return 0;
            }
            let thunks = crate::lambda_adapter::w10_5_supersede::take_adapters_into(
                &self.lambda_adapters,
                old,
            );
            if thunks.is_empty() {
                return 0;
            }
            let mut cleared = 0usize;
            let count = thunks.len();
            for thunk in thunks {
                thunk_withdraw(&thunk, &mut cleared);
                if let Some(table) = self.mega_dispatch_table_if_any() {
                    table.retire_entry(thunk.entry_ptr() as usize);
                }
                defer_jit_owner(Some(thunk));
            }
            if crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] lambda-adapter-supersede {} thunks={count} cache_slots_cleared={cleared}",
                    old.method_label,
                );
            }
            count
        }
    }

    /// Mark `thunk` superseded, then retired, and clear every cache slot that
    /// publishes it, adding their count to `cleared`. The superseded mark comes
    /// first, as for a body: a racing publication either lands in the holder
    /// list the walk takes, or sees the flag and withdraws itself
    /// (`note_published_owner`).
    pub(crate) fn thunk_withdraw(thunk: &Arc<CompiledMethod>, cleared: &mut usize) {
        thunk
            .superseded
            .store(true, std::sync::atomic::Ordering::Release);
        thunk
            .retired
            .store(true, std::sync::atomic::Ordering::SeqCst);
        *cleared += thunk.retarget_superseded_inline_caches();
    }

    #[cfg(test)]
    mod tests;
}

/// JIT round 25 wave 10, lane W10-2: hot JDK leaf methods that run their
/// single-pass (`[c1]` in the perf map) body in steady state. Page:
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// The perf map's `[c1]` / `[c2]` names the BACKEND of the published body
/// (`CompiledMethod::used_ir_backend`), not the manager's tier, so a method
/// whose C2 task's optimizing attempt was refused -- an admission pin, the
/// acceptance gate -- publishes its single-pass fall-through, is settled at
/// C2, and reads `[c1]` for the rest of the run. That is what this lane
/// found for `Arrays.copyOfRange([BII)`: the String-intrinsic pin keeps it on
/// the single-pass backend because its cold throw arm builds the
/// `IllegalArgumentException` message with `StringBuilder`
/// ([`string_pin_skips_cold_throw_arms_enabled`]). That skip lost its A/B in
/// round 23 only because the IR body then copied through the bound helper
/// instead of inline; with `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` on it
/// copies inline, so the skip now follows that switch unless its own key is
/// set.
pub(crate) mod r25_w10_2_steady_c1 {
    /// With `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS` unset: skip the
    /// cold throw arms' String sites exactly when the IR tier copies byte
    /// arrays inline (`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY`, opt-in), so
    /// the default stays the round-23 pin and the two switches can no longer
    /// be A/B'd apart into the losing combination. Read per compile.
    pub(crate) fn cold_arm_skip_follows_inline_copy() -> bool {
        crate::ir_lower::ir_inline_byte_arraycopy_enabled()
    }

    /// `CRATONVM_JIT_IR_CONTEXT_ABI_OF_BASELINE` (JIT r25 wave 10, W10-2b;
    /// **default ON** since wave 13 (W13-2), `=0` kills). c30 named the
    /// refusal it removes on real methods: `c2-supersede <init>` and
    /// `indexOf`, `superseded body KEPT entrant (context ABI: the C1 body
    /// takes the VM context, the C2 body does not)`. It changes only a body
    /// whose graph lost every context node while its single-pass body takes
    /// one -- exactly the pairs that refusal names -- so it is a no-op
    /// everywhere else (the c27 opt-in A/B: neutral on all 8 rows). Read per
    /// compile.
    pub(crate) fn context_abi_of_baseline_enabled() -> bool {
        true
    }

    /// W10-2b: `Graph::context_abi_floor` for an optimizing build of a method
    /// whose bytecode scan says `scan_needs_heap` -- the single-pass body's
    /// hidden VM-context argument (`x64::driver`: `try_new_with_context` on
    /// `needs_heap`).
    ///
    /// Why: a C2 supersede forwards the C1 body's baked callers only to a body
    /// of the same ABI (`not_entrant::supersede_forward_admissible`). A small
    /// leaf whose optimized graph lost every context-taking node -- a cold
    /// throw arm pruned to a guard, a static read only that arm used, an
    /// inlined callee -- publishes a context-free C2 body over a context-taking
    /// C1 body. The forward is then refused, the C1 body stays entrant, and
    /// every caller that baked it runs C1 for the rest of the process:
    /// `String.checkBoundsBeginEnd` (a `getstatic` + an `invokestatic`) and
    /// `Preconditions.checkFromToIndex` (an `invokestatic` in its throw arm),
    /// both accepted at C2 and still `[c1]` in the steady-state profile. With
    /// the floor the C2 body reserves the context slot (one store in its
    /// prologue, the caller's one extra argument move) and the forward
    /// applies.
    pub(crate) fn context_abi_floor(scan_needs_heap: bool) -> bool {
        scan_needs_heap && context_abi_of_baseline_enabled()
    }

    /// `CRATONVM_JIT_IR_CONTEXT_ABI_FLOOR_COUNTS_LDC` (t1 wave 9, lane LCAP;
    /// **default OFF**, `=1` enables; page
    /// `docs/feature-designs/jit-ir-optimizations.md`).
    /// Read per compile.
    pub(crate) fn context_abi_floor_counts_ldc_enabled() -> bool {
        false
    }

    /// The part of the single-pass body's context ABI that [`context_abi_floor`]
    /// cannot see: the bytecode scan leaves `needs_heap` false for a string or
    /// class `ldc`, and `x64::driver` turns the context form on afterwards
    /// (`needs_heap || !ldc_string_info.is_empty() || !ldc_class_info.is_empty()`).
    /// When the optimizing graph then loses every `ConstString`/`ConstClass`
    /// (a pruned cold arm, `ir_fold_string_literal_queries`), its body is
    /// context-free over a context-taking C1 body and
    /// `not_entrant::supersede_forward_refusal` keeps the baked callers on C1
    /// -- the refusal W10-2b removed for the scan's own reasons. Needs the
    /// W10-2b switch too: this widens that floor, it is not a second one.
    pub(crate) fn ldc_context_abi_floor(ir_ldc_takes_context: bool) -> bool {
        ir_ldc_takes_context
            && context_abi_of_baseline_enabled()
            && context_abi_floor_counts_ldc_enabled()
    }

    /// `CRATONVM_JIT_IR_CONTEXT_ABI_OF_INSTALLED` (t1 wave 10, lane LCAP2;
    /// **default OFF**, `=1` enables; pages
    /// `docs/feature-designs/jit-ir-optimizations.md`
    /// and `t1-lcap-implicit-exception-edges-give-c2-a-context-c1-lacks-20261026.md`).
    /// Read per compile.
    pub(crate) fn context_abi_of_installed_enabled() -> bool {
        false
    }

    /// The floor from the single-pass body's REAL entry ABI instead of the
    /// bytecode scan's: `installed` is the published body's
    /// `needs_context` (`CompileRequest::installed_body_needs_context`), the
    /// body the C2 body will supersede. Every reason the driver and the table
    /// builder widen `needs_heap` for -- a string/class `ldc`, a deferred field
    /// site, a self-locking body, and the table builder's own arms -- is in
    /// that one bit, so none is copied here. Widens the W10-2b floor (needs
    /// that switch too); `None` (no published body) adds nothing.
    pub(crate) fn installed_context_abi_floor(installed: Option<bool>) -> bool {
        installed == Some(true)
            && context_abi_of_baseline_enabled()
            && context_abi_of_installed_enabled()
    }

    /// The opposite direction: the published body is context-FREE, so a C2
    /// body that reserves the context can never take over its baked callers.
    /// The optimizing tier then withholds its implicit exception edges
    /// (`CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES`), whose `CallThrew` pads
    /// are the context reason a single-pass body never has for an `idiv` /
    /// array access inside a `try` (the single-pass handler is armed only
    /// with `needs_heap`). Such a build is exactly the build with that switch
    /// off: the site deopts to the interpreter's handler, as on C1.
    pub(crate) fn installed_body_withholds_implicit_edges(installed: Option<bool>) -> bool {
        installed == Some(false) && context_abi_of_installed_enabled()
    }
}



/// JIT r25 wave 10, lane W10-3b: the two remaining costs of
/// `CRATONVM_JIT_IR_NESTED_CHAIN_SPLICE` on the `\s+` rows (`split-ws`,
/// `cached`, `matchonly`), read off the host c24 captures
/// (`docs/feature-designs/regex-jit-contract.md`,
/// "Wave 10 W10-3b").
///
/// 1. **`CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT`** (default ON under the
///    chain or lambda-body interface switch, `=0` kill). The chain rule opened
///    the profiled interface route for every interface site of a chain body,
///    `seq.charAt(i)` included. In the default build that site stays a call,
///    and the inline String prefix answers it (`IrStringSearchKind::SeqCharAt`:
///    the String class test, the coder byte, one bounds compare and a load;
///    about 22 instructions on `Start.match`'s c24 listing). Spliced, it became
///    `String.charAt` -> `isLatin1` -> `StringLatin1.charAt` -> `checkIndex`,
///    with the `COMPACT_STRINGS` static, two field reads behind the layout
///    flag, two index checks and stores and reloads: about 45 instructions per
///    character. Over ~115 characters per op that is about 2 600
///    instructions, most of the measured `split-ws` / `matchonly` loss. Under
///    the switch a site the prefix answers keeps its call (the VM resolver), and
///    the chain link rule counts it as answered rather than as a kept call
///    ([`super::r25_w7b_chain_splice::first_kept_non_link_call`]).
/// 2. **`CRATONVM_JIT_IR_CHAIN_LOOP_ROOTED_HOT`** (default ON under the chain
///    switch, `=0` kill). `matchonly` is bimodal (101 G vs 108-110 G
///    instructions). In fast runs, `Matcher.search`'s optimizing body carries
///    the `Start` -> `Greedy` chain (22 864 bytes). In slow runs it is a 7 821-byte
///    body with no `ir-chain-tree` line at all. The planner drops every nested
///    row over the cold cap under a top-level site it does not call hot
///    (`ir_splice_strip_cold_nested_hot_rows`: in a loop, or 256 recorded
///    calls), and `search`'s `root.match` site is neither when `search`
///    reaches the optimizing tier early. But a chain link that runs per
///    iteration of the top-level body's OWN loop (`Start`'s position loop)
///    is hot whatever the top-level count. Under the switch, such a tree keeps
///    its nested rows.
pub mod r25_w10_3b {
    use super::*;

    /// `CRATONVM_JIT_IR_CHAIN_STRING_PREFIX_KEPT`: see the module note. Read
    /// per site; no `static`.
    pub fn prefix_kept_enabled() -> bool {
        r25_w7b_chain_splice::ir_nested_chain_splice_enabled()
            || r25_w10_4_lambda_iface::ir_nested_lambda_iface_enabled()
    }

    /// `CRATONVM_JIT_IR_CHAIN_LOOP_ROOTED_HOT`: see the module note. Read per
    /// planned site; no `static`.
    pub fn loop_rooted_hot_enabled() -> bool {
        r25_w7b_chain_splice::ir_nested_chain_splice_enabled()
    }

    /// Does the inline String prefix answer a call of kind `invoke_kind` to
    /// `class.name descriptor` that stays a call (the IR builder's prefix over
    /// the kept `Op::Call`)? The prefix's own gates decide: the
    /// prefix switches and the inline-field layout (`ir_string_search_prefix_handled_kind`).
    pub fn site_answered_by_string_prefix(
        class: &str,
        name: &str,
        descriptor: &str,
        invoke_kind: u8,
    ) -> bool {
        ir_string_search_prefix_handled_kind(class, name, descriptor)
            .is_some_and(|k| k.admits_invoke_kind(invoke_kind))
    }

    /// General interface admission must preserve a chain/lambda site's choice
    /// to use its inline String prefix. Enabling the broader route does not
    /// make the bytecode splice cheaper than the prefix it would replace.
    pub fn nested_profiled_kind(
        invoke_kind: u8,
        general_interfaces: bool,
        specialized_body: bool,
        prefix_kept: bool,
    ) -> bool {
        invoke_kind == 0
            || (invoke_kind == 2
                && (general_interfaces || specialized_body)
                && !(specialized_body && prefix_kept))
    }

    /// Is there, anywhere in `site`'s tree, a chain row whose site is in a
    /// loop of its enclosing body? `false` with the switch off.
    pub(crate) fn tree_has_loop_rooted_chain(site: &InlineSite) -> bool {
        loop_rooted_hot_enabled() && has_loop_rooted_chain(site)
    }

    fn has_loop_rooted_chain(site: &InlineSite) -> bool {
        site.nested_sites.iter().any(|n| {
            (n.guard_class_id != 0
                && r25_w7b_chain_splice::row_is_chain(site, n)
                && bytecode_pc_in_loop(&site.callee_code, site.callee_code_len, n.callee_pc))
                || has_loop_rooted_chain(&n.site)
        })
    }


}



/// JIT r25 wave 11 (W11-2): `Arrays.copyOfRange([BII)` spliced into its
/// optimizing callers (`StringLatin1.newString`, and through it `substring`,
/// `subSequence`, `Matcher.group`, `Pattern.split`), as HotSpot inlines it.
///
/// What refused it: neither the String-intrinsic pin (the resolver masks the
/// cold `IllegalArgumentException` arm, so its `StringBuilder` rows never
/// reach `splice_loses_string_intrinsic`), nor `arraylength` (single-pass
/// only), nor the allocation (`newarray` lowers in a splice), nor the byte
/// budget (82 bytes). It is the planner's unbindable-call rule: the body's
/// surviving `System.arraycopy` is a statically bound row with no
/// `direct_entry` (a native: the resolver's binder declines it), so
/// `append_ir_inline_site` refused the site
/// (`ir-splice-refused ir-splice-unbindable-surviving-call
/// java/util/Arrays.copyOfRange([BII)[B at pc=16 in
/// java/lang/StringLatin1.newString`), and as a nested body the VM's resolver
/// turned it back into its call (`ir_splice_body_has_unbindable_call`). The
/// premise is false for that one row: after the build every
/// `System.arraycopy` call node, relocated ones included, is bound to
/// `ir_arraycopy_entry`'s direct helper with its inline prefixes -- the rule
/// `CRATONVM_JIT_IR_SPLICE_PAST_ARRAYCOPY` already relies on in the resolver.
/// JIT round 22 (lane memory; MEM20-8's residual): a spliced body's
/// `Reference.reachabilityFence(Object)` row and the unbindable-call rule.
///
/// JDK 25's `DirectByteBuffer.getInt(J)I` (and every other scoped buffer
/// accessor) ends in `finally { Reference.reachabilityFence(this); }`. The
/// resolver keeps the fence as a call row ("kept as a keep-alive call") and
/// never binds it (the empty body is never compiled), so the unbindable-call
/// rule refused every splice that carried one: `ByteBuffer.getInt(I)I` stayed
/// a real call in each caller loop, once per access. The builder never emits
/// that call: it builds the row as an [`ir::Op::KeepAlive`] (a use of the
/// referent at that point, no code; `CRATONVM_JIT_IR_REACHABILITY_FENCE_KEEPALIVE`),
/// for spliced rows as for the method's own. So such a row is no unbindable
/// call.
#[cfg(test)]
mod ir_splice_fresh_array_argument_tests;

pub(crate) mod r22_fence_row {
    use crate::InlineInvokeTarget;

    /// `CRATONVM_JIT_IR_SPLICE_FENCE_ROW_KEEPALIVE` (perf, **default ON**,
    /// `=0` restores the refusal). Read per row, no static.
    pub(crate) fn enabled() -> bool {
        true
    }

    /// Does the unbound row `t` of a spliced body count as bound for the
    /// unbindable-call rule? Only the static `reachabilityFence(Object)`, and
    /// only while the builder takes it as a keep-alive.
    pub(crate) fn counts_keepalive(t: &InlineInvokeTarget) -> bool {
        t.invoke_kind == 3
            && t.direct_entry.is_none()
            && crate::is_reachability_fence_site(&t.class_name, &t.method_name, &t.descriptor)
            && crate::ir::ir_reachability_fence_keepalive_enabled()
            && enabled()
    }
}

#[cfg(test)]
mod r22_fence_row_tests;

pub(crate) mod r25_w11_2_arraycopy_row {
    use crate::InlineInvokeTarget;

    /// `CRATONVM_JIT_IR_SPLICE_ARRAYCOPY_ROW_BOUND` (perf, **default ON**
    /// since 2026-10-27, `=0` kill). With its own key unset it also turns on
    /// the inline byte copy (`CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY`) and,
    /// through that, the String pin's cold-throw-arm skip. The 64.177 A/B of
    /// the three together (r25 W10 page's retirement): instructions per op
    /// -27 % `regex-groups` (40280 -> 29396), -27 % `split-ws`, -29 %
    /// `regex-split` (quiesced `icount2`, 3 reps); thread-CPU ns/op lower on
    /// all five rows (7 interleaved reps); `facts` = HotSpot.
    /// Read per row, no static.
    pub(crate) fn enabled() -> bool {
        true
    }

    /// Does the unbound statically bound row `t` of a spliced body count as
    /// bound for the unbindable-call rule? Only a static `System.arraycopy`,
    /// under the switch, when the post-build bind applies
    /// (`crate::ir_splice_keeps_arraycopy_bind`: its descriptor and gates).
    pub(crate) fn counts_bound(t: &InlineInvokeTarget) -> bool {
        t.invoke_kind == 3
            && t.direct_entry.is_none()
            && enabled()
            // JIT r25 wave 13 (W13-2): only with the inline byte copy (which
            // this switch implies when its own key is unset): c30 measured the
            // splice alone 22-27 % slower, so that arm is unreachable.
            && crate::ir_lower::ir_inline_byte_arraycopy_enabled()
            && crate::ir_splice_keeps_arraycopy_bind(&t.class_name, &t.method_name, &t.descriptor)
    }
}





#[cfg(test)]
mod r25_w13_2_row_bound_pairs_inline_copy_tests;

/// JIT round 21 (lane lambda2, TLOCAL21-1 / LAMBDATRAP21-3): the door's plan
/// of SAM calls built as their lambda's impl call (`ir_lambda_impl_plan`):
/// which receivers straight-line code proves, which rows it rewrites, and the
/// switch.
#[cfg(test)]
mod jr21_lambda2_plan_tests;



/// JIT round 21 (lane tl2, `CRATONVM_JIT_IR_SPLICE_TL_THIN`): a spliced body's
/// surviving `ThreadLocal.get()` / `set(Object)` call finds the thin helper's
/// cell; nothing else does, and `=0` finds none.
#[cfg(test)]
mod jr21_tl2_splice_tl_thin_tests;

/// JIT round 21 (lane lambda3; `CRATONVM_JIT_IR_LAMBDA_IMPL_SPLICED_CAPTURES`):
/// a capturing lambda's SAM call stays its impl call only where the impl
/// splices -- the plan drops a capturing call whose splice a rebuild dropped,
/// and the per-site census names the route and the proxy.
#[cfg(test)]
mod jr21_lambda3_tests;

#[cfg(test)]
mod jr21_ci_nested_case_prefix_tests;



/// JIT round 21 lane indylate (`972af884ca`, re-landed t1 wave 8 lane
/// RL-REFL): `JitVerdictRegistry::forget_ir_input_refusal` drops the
/// acceptance gate's input-stamped refusal and keeps a sticky site-trap ban.
#[cfg(test)]
mod rlrefl_indylate_refusal_forget_tests;



/// JIT round 21, lane strintr2 (STRINTR21-1): the withdrawn-site set and the
/// three String accessor call-prefix kinds.


#[cfg(test)]
mod jr21_mhbind2_bound_plan_tests;

/// JIT round 21 wave 20 (lane mhfilter; MHBIND2-1): a filter species' site
/// (`invoke_kind 5`) keeps its plan only with a PRE row that fits; the PRE
/// call joins the invoke plan at the site's `getstatic` pc, named under
/// `MH_FILTER_PRE_KEY`.
#[cfg(test)]
mod jr21_mhfilter_plan_tests;

/// j27 wave 6 (lane T1LAM2; item 1 of
/// `j27-mh-real-astype-converting-bind-proposal-FIXED-20261101.md`): a bound STATIC
/// constant handle's site (`invoke_kind 6`) is planned as an `invokestatic` of
/// its member only with its `argL1` row, under the static gate, and never
/// takes the virtual bound site's receiver class.
#[cfg(test)]
mod j27_t1lam2_bound_static_plan_tests;

mod lib_parts;
#[allow(unused_imports)]
pub use lib_parts::jit_code_region::*;
#[allow(unused_imports)]
use lib_parts::jit_code_region::*;
#[allow(unused_imports)]
pub use lib_parts::compiled_method::*;
#[allow(unused_imports)]
use lib_parts::compiled_method::*;
#[allow(unused_imports)]
pub use lib_parts::ir_sync_splice_body_scan::*;
#[allow(unused_imports)]
use lib_parts::ir_sync_splice_body_scan::*;
#[allow(unused_imports)]
pub use lib_parts::ir_nested_hot_cap_pcs::*;
#[allow(unused_imports)]
use lib_parts::ir_nested_hot_cap_pcs::*;
#[allow(unused_imports)]
pub use lib_parts::string_field_layout::*;
#[allow(unused_imports)]
use lib_parts::string_field_layout::*;
#[allow(unused_imports)]
pub use lib_parts::ir_ffm_element_call_admits::*;
#[allow(unused_imports)]
use lib_parts::ir_ffm_element_call_admits::*;
#[allow(unused_imports)]
pub use lib_parts::sp_nested_guard_strict_share_enabled::*;
#[allow(unused_imports)]
use lib_parts::sp_nested_guard_strict_share_enabled::*;
#[allow(unused_imports)]
pub use lib_parts::jit_code_reclamation_stats::*;
#[allow(unused_imports)]
use lib_parts::jit_code_reclamation_stats::*;
#[allow(unused_imports)]
pub use lib_parts::verdict_stamp::*;
#[allow(unused_imports)]
use lib_parts::verdict_stamp::*;
#[allow(unused_imports)]
pub use lib_parts::redefine_epoch::*;
#[allow(unused_imports)]
use lib_parts::redefine_epoch::*;
#[allow(unused_imports)]
pub use lib_parts::ir_string_indexof_supplementary::*;
#[allow(unused_imports)]
use lib_parts::ir_string_indexof_supplementary::*;
#[allow(unused_imports)]
pub use lib_parts::debugger_write_door::*;
#[allow(unused_imports)]
use lib_parts::debugger_write_door::*;
#[allow(unused_imports)]
pub use lib_parts::mh_constant_receiver_push::*;
#[allow(unused_imports)]
use lib_parts::mh_constant_receiver_push::*;
#[allow(unused_imports)]
pub use lib_parts::ir_tier_attempt::*;
#[allow(unused_imports)]
use lib_parts::ir_tier_attempt::*;
#[allow(unused_imports)]
pub use lib_parts::backend_parity_enabled::*;
#[allow(unused_imports)]
use lib_parts::backend_parity_enabled::*;
#[allow(unused_imports)]
pub use lib_parts::build_single_pass_tables::*;
#[allow(unused_imports)]
use lib_parts::build_single_pass_tables::*;
#[allow(unused_imports)]
pub use lib_parts::set_selfrec_direct_override::*;
#[allow(unused_imports)]
use lib_parts::set_selfrec_direct_override::*;
