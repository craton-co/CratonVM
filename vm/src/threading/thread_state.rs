// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Explicit runtime thread-execution states (report item P1).
//!
//! ## What this is
//!
//! A thread's execution state is today spread across half a dozen independent
//! booleans and counters that no single piece of code owns:
//!
//! | Encoding | Owner | Meaning |
//! |---|---|---|
//! | `ThreadEntry::alive` | [`crate::threading::thread_registry`] | not `Terminated` |
//! | `ThreadEntry::stw_ready` | same | past `Starting` |
//! | `GcBlockState::in_blocked_region` | [`crate::threading::jvm_thread`] | `NativeBlocked` (identity census) |
//! | `GcBarrier::threads_blocked` | [`crate::threading::gc_barrier`] | `NativeBlocked` (anonymous census) |
//! | `GcBarrier::arrived` / `excluded_blocked` | same | `SafepointParked` |
//! | `GLOBAL_JIT_DEPTH` / `JIT_ENTRY_CHAIN` | `crate::jit::conservative_roots` | `CompiledUninterruptible` |
//! | `JIT_SIGNALS.deopt` | `crate::jit::helpers` | `Deoptimizing` |
//!
//! No two of them are updated under a common lock, none of them is a state
//! *machine*, and the barrier's own comments (`gc_barrier.rs:69-91`) record
//! that guessing a thread's participation status from any one of them
//! produced the MTChurn / BinaryTrees heap-corruption family.
//!
//! This module introduces the vocabulary those flags were approximating —
//! [`ThreadExecState`] — plus a legality table derived from what the code
//! actually does, a shadow recorder, and a per-state census.
//!
//! ## What this is NOT (yet)
//!
//! **The existing mechanism stays authoritative.** Nothing here gates a
//! safepoint, changes an arrival decision, or alters how a thread parks,
//! blocks or arrives. [`record_transition`] performs one relaxed store into a
//! per-thread cell; the legality check is a verification tripwire that runs
//! only under `debug_assertions` or `CRATONVM_STRESS_THREAD_STATES=1`.
//!
//! The intended progression (documented in
//! `docs/threading/thread-transition-states.md`) is:
//!
//! 1. *this pass* — shadow record + tripwire + census,
//! 2. make [`thread_state_census`] the single input to
//!    `GcBarrier::request_stw_counted_with_live_blocked`'s exclusion set,
//! 3. retire `threads_blocked` / `in_blocked_region` / `stw_ready` in favour
//!    of the state word.
//!
//! ## Scope: the PROCESS, not a VM (gc-common w7-g, 2026-09-24)
//!
//! The recorder is keyed by OS thread and its census walks one process-wide
//! registry ([`thread_state_census`], [`thread_state_roster`]): with two VMs in
//! one process it counts both VMs' threads together, and a roster id is a VM
//! thread id that another VM may also use. That is harmless for what reads it
//! today -- two opt-in diagnostics (`CRATONVM_DBG_RELOCATION_BLOCKERS` in
//! `memory/roots.rs`, `CRATONVM_DBG_CARRIER` in `virtual_threads.rs`) and the
//! instrument census -- and it is why step 2 above must NOT be taken as
//! written: a pause's exclusion set is per VM (`GcBarrier`, the thread
//! registry), so a census feeding it has to be per VM first (a registry per
//! `SharedVm`, or cells tagged with `vm_identity` and filtered by it). A
//! caller that needs one VM's answer today has none here.
//!
//! **No pause path CONSUMES the count (re-audited gc-common w8-d, 2026-09-24),
//! so it stays process-wide.** Every reader of the process-wide census, by
//! `rg -n "thread_state_census|thread_state_roster|illegal_transition_count" vm/src`:
//!
//! * `memory/roots.rs` `collect_roots`, behind `CRATONVM_DBG_RELOCATION_BLOCKERS`
//!   -- the one reader that runs inside a pause. It PRINTS the census next to
//!   the relocation verdict; the verdict itself (`coverage_proven`) is computed
//!   before and without it. With two VMs its per-state columns include the
//!   other VM's threads, so a `peer_blockers=N` there is a process figure.
//! * `threading/virtual_threads.rs`, the starvation watchdog's
//!   `CRATONVM_DBG_CARRIER` line -- a print, outside every pause.
//! * `runtime/instrument_census.rs` -- the exit census (`vm.illegal_transition_count`,
//!   `vm.thread_state_roster`).
//!
//! What pause paths DO read here is [`current_state`], and that is the calling
//! thread's own cell (`gc_barrier.rs` restores it after a park,
//! `jit/conservative_roots.rs::leaving_compiled_state` picks the pop target),
//! which is per OS thread and therefore correct whichever VM the thread is in.
//! The quota each pause waits for comes from the per-VM thread registry
//! (`StwCensus`), never from this module. Making the census per VM becomes
//! necessary only when step 2 above is taken.
//!
//! ## Per-state GC contract
//!
//! Each state answers three questions the collector actually asks. See
//! [`ThreadExecState::counts_toward_safepoint_quota`],
//! [`ThreadExecState::may_hold_unrewritable_object_refs`] and
//! [`ThreadExecState::relocation_rule`], and §"Per-state rules" of the
//! companion document.

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::RwLock;

// ---------------------------------------------------------------------------
// The states
// ---------------------------------------------------------------------------

/// One thread's execution state, as a checked machine.
///
/// The discriminants are stable (`as_u8` / [`ThreadExecState::from_u8`]) so the
/// value can live in an `AtomicU8` and be decoded by a peer thread or a future
/// JFR event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum ThreadExecState {
    /// Registered in the [`crate::threading::thread_registry::ThreadRegistry`]
    /// and `alive`, but not yet `stw_ready`: the carrier has not reached a
    /// point where it can answer a stop-the-world barrier.
    ///
    /// `Thread.start()` publishes the Java mirror before the spawned carrier
    /// exists, so such an entry is alive for `Thread.isAlive()` while being
    /// deliberately absent from `expected`
    /// (`thread_registry.rs:127-133`, `:1981`, `:2044`).
    Starting = 0,
    /// Executing bytecode in the interpreter. Polls `stw_requested` at
    /// allocation sites and backward branches
    /// (`runtime/interpreter.rs::safepoint_check`).
    JavaRunning = 1,
    /// Executing VM runtime code (Rust) on behalf of Java: class loading and
    /// resolution, the allocation slow path, reflection plumbing, JIT
    /// compilation requests.
    ///
    /// Not distinguished from [`Self::JavaRunning`] by any production flag
    /// today — the only existing approximation is the opt-in `vm_state`
    /// breadcrumb (`jvm_thread.rs:634-640`, `CRATONVM_DBG_VM_STATE`). It is a
    /// separate state here because the *rooting* rule differs: a VM helper
    /// holds raw `ObjectRef`s in Rust locals that no pointer map rewrites.
    VmRunning = 2,
    /// Executing a registered native / JNI method that has **not** declared a
    /// blocking region. Counted by the STW census and deliberately waited for:
    /// "a *running* native still holds raw `ObjectRef`s in Rust locals and must
    /// be waited for so the copying collector does not relocate objects under
    /// it" (`gc_barrier.rs:44-47`). It arrives when it returns to the
    /// interpreter.
    NativeRunning = 3,
    /// Parked inside a declared blocking region — `Object.wait`,
    /// `LockSupport.park`, `Thread.sleep`/`join`, monitor acquisition,
    /// `ReferenceQueue.remove`, selector `select`, virtual-thread unmount.
    ///
    /// Marked by `GcBlockState::in_blocked_region` (identity census) plus
    /// `GcBarrier::threads_blocked` (anonymous census). Excluded from
    /// `expected`; its roots are the deposited snapshot, maintained across
    /// missed collections by `fold_pointer_map_into_blocked` and applied on
    /// wake by `check_post_block_gc`.
    NativeBlocked = 4,
    /// Arrived at the stop-the-world barrier and waiting for the initiator's
    /// `complete_gc` (`GcBarrier::arrive_and_wait_inner`). The thread has
    /// already filled its quota slot (or was recorded as excluded), executes
    /// no code, and its frames are rewritable in place.
    SafepointParked = 5,
    /// Executing JIT-compiled code. Never polls the cooperative flag unless
    /// `CRATONVM_JIT_SAFEPOINT_POLLS` is on, so a stop-the-world initiator
    /// resolves it by OS-level takeover: `SuspendThread`, conservative
    /// register+stack scan, then `GcBarrier::reduce_expected`
    /// (`jit/xt_root_scan.rs`, `gc_barrier.rs:523-540`).
    ///
    /// Tracked by `conservative_roots::GLOBAL_JIT_DEPTH` /
    /// `JIT_ENTRY_CHAIN`.
    CompiledUninterruptible = 6,
    /// Materialising interpreter frames from a compiled frame after a
    /// deoptimization trap (`jit/helpers.rs::set_jit_deopt_pending` →
    /// `runtime/interpreter.rs::resume_from_ir_deopt` /
    /// `real_frame_deopt_resume_and_despeculate`).
    ///
    /// The in-flight `FrameValue` buffers hold object references that are in
    /// neither the compiled frame's map nor the not-yet-built interpreter
    /// frame, so this window is its own state.
    Deoptimizing = 7,
    /// `ThreadEntry::alive == false` (`thread_registry.rs::mark_dead`).
    /// Holds no roots and is never counted.
    Terminated = 8,
}

impl ThreadExecState {
    /// Every state, in discriminant order. Iteration anchor for the census and
    /// the transition-table tests.
    pub const ALL: [ThreadExecState; 9] = [
        ThreadExecState::Starting,
        ThreadExecState::JavaRunning,
        ThreadExecState::VmRunning,
        ThreadExecState::NativeRunning,
        ThreadExecState::NativeBlocked,
        ThreadExecState::SafepointParked,
        ThreadExecState::CompiledUninterruptible,
        ThreadExecState::Deoptimizing,
        ThreadExecState::Terminated,
    ];

    /// Number of distinct states — the width of a [`ThreadStateCensus`].
    pub const COUNT: usize = ThreadExecState::ALL.len();

    /// Stable wire value for the `AtomicU8` cell / a future JFR event.
    #[inline]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]; `None` for an unknown discriminant.
    #[inline]
    pub const fn from_u8(v: u8) -> Option<ThreadExecState> {
        match v {
            0 => Some(ThreadExecState::Starting),
            1 => Some(ThreadExecState::JavaRunning),
            2 => Some(ThreadExecState::VmRunning),
            3 => Some(ThreadExecState::NativeRunning),
            4 => Some(ThreadExecState::NativeBlocked),
            5 => Some(ThreadExecState::SafepointParked),
            6 => Some(ThreadExecState::CompiledUninterruptible),
            7 => Some(ThreadExecState::Deoptimizing),
            8 => Some(ThreadExecState::Terminated),
            _ => None,
        }
    }

    /// Short, stable name for diagnostics and panic messages.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            ThreadExecState::Starting => "Starting",
            ThreadExecState::JavaRunning => "JavaRunning",
            ThreadExecState::VmRunning => "VmRunning",
            ThreadExecState::NativeRunning => "NativeRunning",
            ThreadExecState::NativeBlocked => "NativeBlocked",
            ThreadExecState::SafepointParked => "SafepointParked",
            ThreadExecState::CompiledUninterruptible => "CompiledUninterruptible",
            ThreadExecState::Deoptimizing => "Deoptimizing",
            ThreadExecState::Terminated => "Terminated",
        }
    }

    /// Whether a thread in this state is included in the stop-the-world
    /// `expected` quota computed by
    /// `GcBarrier::request_stw_counted_with_live_blocked`.
    ///
    /// Mirrors `ThreadRegistry::alive_count_blocked_and_os_tids`
    /// (`thread_registry.rs:2037-2057`): counted iff `alive && stw_ready &&
    /// !in_blocked_region`.
    ///
    /// Note the asymmetry this exposes: [`Self::CompiledUninterruptible`]
    /// answers `true` — the census has no way to see that the thread is in
    /// compiled code — yet such a thread never arrives. The quota is repaired
    /// after the fact by `GcBarrier::reduce_expected` once the OS-level
    /// takeover has frozen it. That is the one state whose census answer is
    /// deliberately optimistic; see the companion document, §"Unsound or
    /// unmodelled transitions", item 2.
    #[inline]
    pub const fn counts_toward_safepoint_quota(self) -> bool {
        match self {
            ThreadExecState::JavaRunning
            | ThreadExecState::VmRunning
            | ThreadExecState::NativeRunning
            | ThreadExecState::SafepointParked
            | ThreadExecState::CompiledUninterruptible
            | ThreadExecState::Deoptimizing => true,
            ThreadExecState::Starting
            | ThreadExecState::NativeBlocked
            | ThreadExecState::Terminated => false,
        }
    }

    /// Whether a thread in this state may be holding raw `ObjectRef`s that
    /// **no** pointer-map consumer rewrites.
    ///
    /// `true` means a moving collection completing while a thread sits here
    /// leaves that thread with stale addresses: the refs live in Rust locals,
    /// CPU registers or JIT spill slots rather than in a frame slot,
    /// `handle_slots`, `native_pin_roots` or a deposited snapshot. This is the
    /// `ObjectRef` contract's §5 "Hold across an interpreter safepoint" /
    /// "Hold in a JIT frame / register" rows restated per state.
    #[inline]
    pub const fn may_hold_unrewritable_object_refs(self) -> bool {
        match self {
            ThreadExecState::VmRunning
            | ThreadExecState::NativeRunning
            | ThreadExecState::CompiledUninterruptible
            | ThreadExecState::Deoptimizing => true,
            ThreadExecState::Starting
            | ThreadExecState::JavaRunning
            | ThreadExecState::NativeBlocked
            | ThreadExecState::SafepointParked
            | ThreadExecState::Terminated => false,
        }
    }

    /// What a collector may do to objects a thread in this state references.
    #[inline]
    pub const fn relocation_rule(self) -> RelocationRule {
        match self {
            // A running mutator must first transition to `SafepointParked`
            // (or `NativeBlocked`); relocation *while it is here* is exactly
            // the corruption `GcBarrier`'s finding-1 comments describe.
            ThreadExecState::JavaRunning
            | ThreadExecState::VmRunning
            | ThreadExecState::NativeRunning
            | ThreadExecState::Deoptimizing => RelocationRule::Forbidden,
            // Registers and JIT spill slots are not rewritable, so a
            // collection that contributes a frozen peer's conservative roots
            // must call `gc_quiescence::mark_moving_young_coverage_incomplete_because`
            // (or pin the peer's G1 regions).
            ThreadExecState::CompiledUninterruptible => RelocationRule::Forbidden,
            // Parked: frames, operand stacks, `monitor_on_exit`, JNI locals and
            // handle slots are all rewritten by `apply_pointer_map_to_thread`.
            ThreadExecState::SafepointParked => RelocationRule::PermittedWithRewrite,
            // Blocked: `fold_pointer_map_into_blocked` remaps the deposited
            // snapshot and composes `fixup` / `slot_origins`; the thread
            // applies them in `check_post_block_gc`.
            //
            // That was only true of INTERPRETER frames until 2026-09-08. A
            // thread blocked in a native with COMPILED frames below it keeps
            // its live oops in JIT frame slots, register images and the shadow
            // stack, and the wake remapped none of them — the JIT half existed
            // but was wired into the leaked-region fallback and gated off by
            // default. `apply_blocked_wake_jit_remap` now runs on the ordinary
            // wake, which is what makes `PermittedWithRewrite` a true statement
            // about this state rather than one about half of it.
            ThreadExecState::NativeBlocked => RelocationRule::PermittedWithRewrite,
            // Not yet `stw_ready`: the startup loop waits the pause out via
            // `arrive_and_wait_excluded` and applies the returned map.
            ThreadExecState::Starting => RelocationRule::PermittedWithRewrite,
            ThreadExecState::Terminated => RelocationRule::PermittedNoRefsHeld,
        }
    }
}

impl std::fmt::Display for ThreadExecState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// What a moving collector may do while a thread sits in a given state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelocationRule {
    /// The collection must not relocate objects this thread can reach — the
    /// thread must first move to a rewritable state, or the collection must
    /// mark its moving-young coverage incomplete.
    Forbidden,
    /// Relocation is permitted; every address this thread holds is reachable
    /// by a pointer-map consumer and is rewritten before it runs again.
    PermittedWithRewrite,
    /// Relocation is permitted and there is nothing to rewrite.
    PermittedNoRefsHeld,
}

// ---------------------------------------------------------------------------
// The transition table
// ---------------------------------------------------------------------------

/// Every transition the VM is known to perform, with the audited call site.
///
/// **Derivation rule:** an edge appears here only if the audit found code that
/// performs it. Transitions that the code performs but that look *wrong* are
/// deliberately NOT listed — they are enumerated in
/// `docs/threading/thread-transition-states.md` §"Unsound or unmodelled
/// transitions" instead, so the tripwire reports them rather than blessing
/// them.
///
/// Self-edges (`s -> s`) are handled by [`is_legal`] and are not listed.
pub const TRANSITIONS: &[(ThreadExecState, ThreadExecState)] = TABLE;

/// Terse alias so the table below reads as a table rather than a wall of
/// `ThreadExecState::` prefixes.
use self::ThreadExecState as S;

const TABLE: &[(ThreadExecState, ThreadExecState)] = &[
    // --- Starting -----------------------------------------------------------
    // `mark_stw_ready` under `run_if_no_stw_requested`
    // (thread_registry.rs:971, vm/vm_exec.rs:2754, :10195).
    (S::Starting, S::JavaRunning),
    // The startup retry loop: a pause was already active, so the not-yet-ready
    // thread drains it via `arrive_and_wait_excluded`
    // (vm/vm_exec.rs:2760, :10201).
    (S::Starting, S::SafepointParked),
    // A carrier that fails before becoming ready (native/jni.rs:568).
    (S::Starting, S::Terminated),
    // --- JavaRunning --------------------------------------------------------
    // Entry into any VM runtime helper (allocation slow path, resolution,
    // reflection). Not separately flagged today — shadow-only.
    (S::JavaRunning, S::VmRunning),
    // `safe_native_call` (vm/vm_exec.rs:1296, :1318).
    (S::JavaRunning, S::NativeRunning),
    // `deposit_root_snapshot` raises the flag, then `enter_blocked` /
    // `mark_blocked_region_enter` (vm/vm_exec.rs:3220, :3604, :14238;
    // gc_barrier.rs:311, :345).
    (S::JavaRunning, S::NativeBlocked),
    // `safepoint_check` -> `arrive_and_wait_auto`
    // (runtime/interpreter.rs:4298, :4341).
    (S::JavaRunning, S::SafepointParked),
    // `push_jit_entry` / `push_entry_full` (jit/conservative_roots.rs:608-637).
    (S::JavaRunning, S::CompiledUninterruptible),
    // `mark_dead` (thread_registry.rs:811, vm/vm_exec.rs:2961).
    (S::JavaRunning, S::Terminated),
    // --- VmRunning ----------------------------------------------------------
    (S::VmRunning, S::JavaRunning),
    (S::VmRunning, S::NativeRunning),
    (S::VmRunning, S::NativeBlocked),
    // VM helpers poll at allocation sites (runtime/interpreter.rs:4298).
    (S::VmRunning, S::SafepointParked),
    // `JitEntryGuard::enter_with_compiled` from a runtime helper.
    (S::VmRunning, S::CompiledUninterruptible),
    (S::VmRunning, S::Terminated),
    // --- NativeRunning ------------------------------------------------------
    // Return to the interpreter — this is the arrival the STW census
    // deliberately waits for (gc_barrier.rs:44-47).
    (S::NativeRunning, S::JavaRunning),
    (S::NativeRunning, S::VmRunning),
    // `begin_blocking_region` / `begin_timed_blocking_region`
    // (vm/vm_exec.rs:11326, :11330, :14238).
    (S::NativeRunning, S::NativeBlocked),
    // A native that re-enters Java and hits the safepoint poll.
    (S::NativeRunning, S::SafepointParked),
    // A native callback that re-enters Java into compiled code.
    (S::NativeRunning, S::CompiledUninterruptible),
    // JNI `DetachCurrentThread` (native/jni.rs:6756).
    (S::NativeRunning, S::Terminated),
    // --- NativeBlocked ------------------------------------------------------
    // `end_blocking_region` -> `check_post_block_gc` ->
    // `leave_blocked_region_flagged` (vm/vm_exec.rs:11334, :3636;
    // gc_barrier.rs:491).
    (S::NativeBlocked, S::JavaRunning),
    (S::NativeBlocked, S::VmRunning),
    // `end_blocking_region_refs` returning into a still-running native poll
    // loop (vm/vm_exec.rs:11341).
    (S::NativeBlocked, S::NativeRunning),
    // Two distinct sites: `enter_blocked().pre_stw == true` makes the thread
    // arrive *before* it parks (vm/vm_exec.rs:2126, :9740, :14263;
    // native/jni.rs:623, :1019), and `BlockedGuard::drop` /
    // `mark_blocked_region_leave_after` / `leave_blocked_region_flagged` wait
    // an active pause out while still counted blocked (gc_barrier.rs:425,
    // :491, :838).
    (S::NativeBlocked, S::SafepointParked),
    // `BlockedGuard::finish_after` flips `alive=false` and releases the blocked
    // slot as one observation (gc_barrier.rs:829, vm/vm_exec.rs:2957).
    (S::NativeBlocked, S::Terminated),
    // --- SafepointParked ----------------------------------------------------
    // `arrive_and_wait_inner` returns, `apply_pointer_map_to_thread`
    // (gc_barrier.rs:672, runtime/interpreter.rs:4341-4356).
    (S::SafepointParked, S::JavaRunning),
    (S::SafepointParked, S::VmRunning),
    (S::SafepointParked, S::NativeRunning),
    // The blocked thread that drained a pause resumes being blocked
    // (gc_barrier.rs:425, :491, :838).
    (S::SafepointParked, S::NativeBlocked),
    // The startup retry loop re-attempts `mark_stw_ready`
    // (vm/vm_exec.rs:2752-2768, :10193-10209).
    (S::SafepointParked, S::Starting),
    // Cooperative JIT safepoint poll (`CRATONVM_JIT_SAFEPOINT_POLLS`) returning
    // into compiled code (gc_barrier.rs:95-123).
    (S::SafepointParked, S::CompiledUninterruptible),
    // --- CompiledUninterruptible --------------------------------------------
    // `pop_jit_entry` (jit/conservative_roots.rs:652-671) and the self-healing
    // `prune_returned_jit_entries` (:701-750).
    (S::CompiledUninterruptible, S::JavaRunning),
    // A compiled frame calling a Rust runtime helper — note the chain depth
    // stays elevated while `Rip` leaves the JIT range; see the companion
    // document, §"Transitions the code performs that look unsound", item 6.2.
    (S::CompiledUninterruptible, S::VmRunning),
    (S::CompiledUninterruptible, S::NativeRunning),
    // The A4 helper-window case: a compiled frame calls a native that blocks
    // (jit/xt_root_scan.rs, helper-window scan).
    (S::CompiledUninterruptible, S::NativeBlocked),
    // Cooperative JIT poll (gc_barrier.rs:95-123).
    (S::CompiledUninterruptible, S::SafepointParked),
    // `jit_set_deopt_pending` / `jit_service_callee_deopt`
    // (jit/helpers.rs:850, :874, :2192).
    (S::CompiledUninterruptible, S::Deoptimizing),
    // --- Deoptimizing -------------------------------------------------------
    // `resume_from_ir_deopt` / `real_frame_deopt_resume_and_despeculate`
    // (runtime/interpreter.rs:13290, :14142).
    // JVMTI exception callbacks can call JNI while the JIT deopt path is
    // reporting a compiled exception (`experimental-debug`). The nested native
    // call remains visible to the safepoint census until the callback returns.
    (S::Deoptimizing, S::NativeRunning),
    // JVMTI queries made from a callback may enter a compiled Java helper.
    (S::Deoptimizing, S::CompiledUninterruptible),
    (S::Deoptimizing, S::JavaRunning),
    (S::Deoptimizing, S::VmRunning),
    // Return from that callback to the still-active deopt operation.
    (S::NativeRunning, S::Deoptimizing),
    // --- Terminated ---------------------------------------------------------
    // The same OS thread re-attaching after `DetachCurrentThread`
    // (native/jni.rs::attach_foreign_thread). The registry entry is new; the
    // shadow cell is revived.
    //
    // This row is now redundant with `is_legal`, which blesses EVERY edge out
    // of `Terminated` because a carrier outlives the virtual threads whose
    // deaths it records. It stays because it is the one such edge anybody has
    // a call site for, and a reader looking for where re-attach is modelled
    // should find it in the table.
    (S::Terminated, S::Starting),
];

/// Whether `from -> to` is a transition the VM is known to perform.
///
/// Self-edges are legal: several sites re-record the state they are already in
/// (nested blocking regions share one `in_blocked_region` flag, and a nested
/// JIT entry only deepens `GLOBAL_JIT_DEPTH`). Treating those as violations
/// would report the *counting* discipline, not a state error.
///
/// # A cell in `Terminated` is not a dead OS thread, so every edge out of it
/// is legal
///
/// The shadow record is **per OS thread**; the states it tracks belong to a
/// **logical** thread. Those are the same object only for platform threads.
/// A carrier multiplexes virtual threads: when one of them dies, `mark_dead`
/// records `Terminated` on the CARRIER's cell, and the carrier then picks up
/// the next continuation and records whatever that one is doing. The tripwire
/// read every one of those as a dead thread coming back to life.
///
/// Measured 2026-09-11, one `VthreadProbe` run (10 000 virtual threads, debug
/// binary, `CRATONVM_STRESS_THREAD_STATES` at its `cfg!(debug_assertions)`
/// default):
///
/// ```text
/// 8314  Terminated -> JavaRunning      gc_barrier::leave_blocked_region_flagged
/// 1667  Terminated -> JavaRunning      thread_registry::mark_stw_ready
///    3  Terminated -> SafepointParked  gc_barrier::leave_blocked_region_flagged:drain
///    2  Terminated -> SafepointParked  gc_barrier::arrive_and_wait_inner:excluded
///    2  SafepointParked -> Terminated  gc_barrier::arrive_and_wait_inner:resume
/// ```
///
/// 9 986 reports, one per virtual thread, 4.2 MB of `tracing::error!` on a
/// probe whose real output is 759 bytes. `(Terminated, Starting)` in the table
/// is the SAME phenomenon seen once, at one site — a JNI thread re-attaching —
/// and blessing that single edge is what made the other four look like
/// defects. `try_record_transition` has always called `revive_current_cell()`
/// for `from == Terminated`, so revival was already a modelled concept; only
/// the legality check had not been told about it.
///
/// This deliberately gives up on one thing: a genuinely resurrected thread,
/// recorded on the same cell, now reads as a revival. The record cannot tell
/// the two apart — under M:N the cell is the carrier's and the identity is
/// the continuation's — so the choice is between missing that and reporting
/// every virtual thread in the process. See
/// `docs/feature-designs/real-jdk-mode.md`.
#[inline]
pub fn is_legal(from: ThreadExecState, to: ThreadExecState) -> bool {
    if from == to {
        return true;
    }
    // Revival, not resurrection -- see the section above.
    if from == ThreadExecState::Terminated {
        return true;
    }
    let mut i = 0;
    while i < TRANSITIONS.len() {
        let (f, t) = TRANSITIONS[i];
        if f == from && t == to {
            return true;
        }
        i += 1;
    }
    false
}

/// The successors of `from`, in table order. Diagnostics and doc generation.
pub fn legal_successors(from: ThreadExecState) -> Vec<ThreadExecState> {
    TRANSITIONS
        .iter()
        .filter(|(f, _)| *f == from)
        .map(|(_, t)| *t)
        .collect()
}

/// A rejected transition, as reported by [`try_record_transition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IllegalTransition {
    /// State the thread was recorded in before this attempt.
    pub from: ThreadExecState,
    /// State the call site tried to move to.
    pub to: ThreadExecState,
    /// VM thread id bound by [`bind_current_thread`], or `u64::MAX` if the
    /// recording thread never bound one.
    pub thread_id: u64,
    /// Static call-site tag passed by the instrumented site.
    pub site: &'static str,
}

impl std::fmt::Display for IllegalTransition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "illegal thread-state transition {from} -> {to} (tid={tid}, site={site}); \
             legal successors of {from} are {succ:?}",
            from = self.from,
            to = self.to,
            tid = self.thread_id,
            site = self.site,
            succ = legal_successors(self.from)
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>(),
        )
    }
}

// ---------------------------------------------------------------------------
// The shadow recorder
// ---------------------------------------------------------------------------

/// One thread's shadow state cell. Address-stable behind an `Arc` so the
/// owning thread keeps a thread-local clone and the census walks the registry.
struct ThreadStateCell {
    /// VM thread id, `u64::MAX` until [`bind_current_thread`] runs.
    thread_id: AtomicU64,
    /// [`ThreadExecState::as_u8`]. Written only by the owning thread, with a
    /// single relaxed store; read by the census.
    state: AtomicU8,
}

/// Global set of live shadow cells. Written only on first observation,
/// termination and OS-thread teardown — never on the transition hot path.
///
/// Lock discipline: nothing is called out to while this lock is held, and the
/// lock is never re-acquired while holding it, so it cannot participate in a
/// cycle even though [`with_cell`] can allocate inside `GcBarrier::inner`'s
/// critical section. It is deliberately NOT an `OrderedPlRwLock` — the
/// ordering checker has no level for a pure leaf that sits below everything.
static CELLS: OnceLock<RwLock<Vec<Arc<ThreadStateCell>>>> = OnceLock::new();

fn cells() -> &'static RwLock<Vec<Arc<ThreadStateCell>>> {
    CELLS.get_or_init(|| RwLock::new(Vec::new()))
}

/// Owning handle held in thread-local storage.
///
/// Its `Drop` is the thing that keeps the census registry O(live OS threads)
/// under thread churn: a Tomcat-style workload creates and tears down hundreds
/// of carriers, and not every teardown path reaches a self-attributed
/// `mark_dead` (`ThreadRegistry::join` marks the joinee dead from the joining
/// thread — see [`record_transition_for`]). Without this destructor those
/// cells would accumulate forever, each frozen in a stale state that the
/// census would keep reporting as live.
struct CellHandle(Arc<ThreadStateCell>);

impl Drop for CellHandle {
    fn drop(&mut self) {
        // TLS destructor. `CELLS.get()` (not `cells()`): if the registry was
        // never initialised there is nothing to clean, and a destructor must
        // not resurrect process-global state.
        let Some(registry) = CELLS.get() else { return };
        let mut registry = registry.write();
        let before = registry.len();
        registry.retain(|c| !Arc::ptr_eq(c, &self.0));
        if registry.len() != before {
            // Still a member ⇒ the thread never recorded `Terminated`
            // explicitly. Count it now, so the terminated column stays a
            // faithful "threads that went away" total.
            TERMINATED_TOTAL.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Cumulative count of threads that have reached [`ThreadExecState::Terminated`].
/// Terminated cells leave the registry so it stays O(live threads); the count
/// preserves the census's terminated column.
static TERMINATED_TOTAL: AtomicU64 = AtomicU64::new(0);

/// Number of illegal transitions observed. Non-fatal builds still count them,
/// so a soak run can assert "zero violations" without arming the panic.
static ILLEGAL_TRANSITIONS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// This OS thread's cell, allocated on first observation and dropped out
    /// of the census registry when the OS thread exits (see [`CellHandle`]).
    static SELF_CELL: std::cell::RefCell<Option<CellHandle>> =
        const { std::cell::RefCell::new(None) };
    /// Thread id supplied by [`bind_current_thread`] before the cell exists.
    /// Applied when [`with_cell`] allocates. Keeping this separate is what
    /// lets binding stay side-effect-free: it must never *create* a cell, or
    /// the thread's first genuine transition would be checked against an
    /// invented `Starting` baseline.
    static PENDING_THREAD_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(u64::MAX) };
}

/// Whether the legality tripwire is armed.
///
/// On in `debug_assertions` builds and whenever
/// `CRATONVM_STRESS_THREAD_STATES` is set to anything other than
/// `0`/`false`/`off`. Explicitly setting it to `0` stands the tripwire down in
/// a debug build, which is the bisection escape hatch if the table itself
/// turns out to be wrong (see `docs/known-issues/` convention: a hypothesis
/// can be wrong, not just stale).
#[inline]
pub fn stress_checks_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        match cratonvm_types::flags::runtime_var("CRATONVM_STRESS_THREAD_STATES").as_deref() {
            Ok("0") | Ok("false") | Ok("off") => false,
            Ok(_) => true,
            Err(_) => cfg!(debug_assertions),
        }
    })
}

/// Whether an observed illegal transition should panic (rather than be counted
/// and logged). Panics only when the env gate is explicitly armed, so a debug
/// build reports without aborting an otherwise-passing run.
#[inline]
fn violations_are_fatal() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        matches!(
            cratonvm_types::flags::runtime_var("CRATONVM_STRESS_THREAD_STATES").as_deref(),
            Ok(v) if !matches!(v, "0" | "false" | "off")
        )
    })
}

/// Associate the calling OS thread's shadow cell with a VM thread id, so
/// violation reports and the census name the thread.
///
/// Idempotent, and deliberately side-effect-free otherwise: it never creates a
/// cell. A thread bound before its first [`record_transition`] has the id
/// applied when that first record seeds the cell.
pub fn bind_current_thread(thread_id: u64) {
    PENDING_THREAD_ID.with(|p| p.set(thread_id));
    SELF_CELL.with(|c| {
        if let Some(handle) = c.borrow().as_ref() {
            handle.0.thread_id.store(thread_id, Ordering::Relaxed);
        }
    });
}

/// VM thread id bound to the calling thread's shadow cell, if any.
pub fn current_thread_id() -> Option<u64> {
    // `try_with`, and a destroyed cell answers "unbound" (gen r5w1/crash5):
    // `ThreadRegistry::mark_dead` reaches this from a thread-local destructor
    // when a foreign attachment whose OS thread exits attached is reaped
    // (`jni::ForeignThreadBox`). The cell may already be gone there, `with`
    // would panic, and a panic out of a thread-local destructor aborts. Nor may
    // the pending id answer for a destroyed cell: `record_transition_for` would
    // then call `record_transition`, which reaches the destroyed cell itself.
    let bound = SELF_CELL
        .try_with(|c| {
            c.borrow()
                .as_ref()
                .map(|handle| handle.0.thread_id.load(Ordering::Relaxed))
                .filter(|id| *id != u64::MAX)
        })
        .ok()?;
    if bound.is_some() {
        return bound;
    }
    // Bound before the first transition seeded a cell.
    let pending = PENDING_THREAD_ID.try_with(|p| p.get()).unwrap_or(u64::MAX);
    if pending == u64::MAX {
        None
    } else {
        Some(pending)
    }
}

/// [`record_transition`] for a site that names a thread by id rather than
/// running on it — `ThreadRegistry::mark_dead(tid)`, for instance, which
/// `ThreadRegistry::join` calls from the *joining* thread
/// (`thread_registry.rs:984`).
///
/// Records only when the calling thread's cell is bound to `thread_id`. An
/// unbound caller, or a caller acting on a peer, is a deliberate no-op:
/// attributing a peer's termination to the caller's cell would corrupt the
/// census far more than missing one transition does. Binding happens on the
/// thread's own first barrier interaction (`GcBarrier::arrive_and_wait*`,
/// `leave_blocked_region_flagged`) and at `mark_stw_ready`.
#[inline]
pub fn record_transition_for(thread_id: u64, to: ThreadExecState, site: &'static str) {
    if current_thread_id() == Some(thread_id) {
        record_transition(to, site);
    }
}

/// The state last recorded for the calling thread.
///
/// Returns [`ThreadExecState::Starting`] for a thread that has never recorded
/// a transition — "not yet observed" and "registered but not STW-ready" are
/// the same answer as far as the census is concerned (neither counts toward
/// the quota).
pub fn current_state() -> ThreadExecState {
    SELF_CELL.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|handle| ThreadExecState::from_u8(handle.0.state.load(Ordering::Relaxed)))
            .unwrap_or(ThreadExecState::Starting)
    })
}

/// Record that the calling thread has moved to `to`, checking legality when
/// the tripwire is armed.
///
/// `site` is a static tag naming the transition point (e.g.
/// `"gc_barrier::enter_blocked"`), reproduced verbatim in the violation
/// report.
///
/// Cost on the hot path: one thread-local access, one relaxed load and one
/// relaxed store. The legality scan runs only when [`stress_checks_enabled`].
///
/// # Panics
///
/// When `CRATONVM_STRESS_THREAD_STATES` is armed and `from -> to` is not in
/// [`TRANSITIONS`], reporting both states, the bound thread id and `site`.
#[inline]
pub fn record_transition(to: ThreadExecState, site: &'static str) {
    if let Err(violation) = try_record_transition(to, site) {
        ILLEGAL_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
        if violations_are_fatal() {
            panic!("CRATONVM_STRESS_THREAD_STATES: {violation}");
        }
        tracing::error!(
            from = violation.from.name(),
            to = violation.to.name(),
            tid = violation.thread_id,
            site = violation.site,
            "{violation}"
        );
    }
}

/// A borrowed handle on this thread's shadow cell, so a caller that must record
/// TWO transitions around one operation pays ONE thread-local access instead of
/// three.
///
/// ## Why this exists
///
/// `vm_exec::safe_native_call_impl` is the funnel every native dispatch in the
/// VM passes through, and it used to reach `SELF_CELL` three times per call:
/// [`current_state`] to learn what to restore, [`record_transition`] to record
/// `NativeRunning`, and [`record_transition`] again from its guard's `Drop`.
/// `native_funnel_profile::funnel_cost_breakdown` prices that trio at
/// **10.8-14.4 ns of a 29-36 ns funnel** — the single largest component,
/// against `catch_unwind` at 1.5 ns, the pin push at 0.7 and the STW probe at
/// 0.3. `current_state` ALONE measures 0.9 ns, which is what says the cost is
/// the repetition and not the read.
///
/// The `Arc` the cell lives in is owned by this thread's `SELF_CELL` handle and
/// by the census registry, and neither can release it while the thread is
/// inside a native call — the TLS handle is dropped at OS-thread teardown, and
/// a native call cannot outlive the thread running it. So a raw pointer taken
/// under `with_cell` stays valid for the call, and the restore is a relaxed
/// store with no thread-local access at all.
///
/// Nothing about the census changes: the cell is the same cell, the store is
/// the same store, and an illegal edge is still detected when the tripwire is
/// armed — [`NativeStateSpan::restore`] runs the same legality check
/// [`try_record_transition`] does.
pub struct NativeStateSpan {
    cell: *const ThreadStateCell,
    /// The state to put back, already corrected for the `Starting` case.
    prior: ThreadExecState,
}

impl NativeStateSpan {
    /// The state this span will restore.
    pub fn prior(&self) -> ThreadExecState {
        self.prior
    }

    /// Put the caller's state back. Consumes the span so it cannot run twice.
    pub fn restore(self, site: &'static str) {
        // SAFETY: see the type doc — the cell's `Arc` is held by this thread's
        // TLS handle and by the registry for at least as long as this thread is
        // inside the native call that opened the span.
        let cell = unsafe { &*self.cell };
        if stress_checks_enabled() {
            let from = ThreadExecState::from_u8(cell.state.load(Ordering::Relaxed))
                .unwrap_or(ThreadExecState::Starting);
            if !is_legal(from, self.prior) {
                ILLEGAL_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
                let violation = IllegalTransition {
                    from,
                    to: self.prior,
                    thread_id: cell.thread_id.load(Ordering::Relaxed),
                    site,
                };
                if violations_are_fatal() {
                    panic!("CRATONVM_STRESS_THREAD_STATES: {violation}");
                }
                tracing::error!(
                    from = violation.from.name(),
                    to = violation.to.name(),
                    tid = violation.thread_id,
                    site = violation.site,
                    "{violation}"
                );
            }
        }
        cell.state.store(self.prior.as_u8(), Ordering::Relaxed);
    }
}

/// Record `to` and hand back a span that restores what it replaced — one
/// thread-local access for the pair. See [`NativeStateSpan`].
///
/// `Starting` is folded into `JavaRunning` here rather than at the call site
/// because it is the same correction every caller of this function needs and
/// getting it wrong is silent: `Starting` is also the recorder's answer for a
/// thread it has never observed, and `Starting -> NativeRunning` is
/// deliberately absent from the table, so restoring it would assert the one
/// thing that cannot be true of a thread that just ran a native — and would
/// then repeat on that thread's every later native call.
pub fn enter_native_state(site: &'static str) -> NativeStateSpan {
    // SEED `JavaRunning`, not `NativeRunning`. `with_cell`'s seed is what a
    // thread's FIRST observation records, and this funnel is often the first
    // thing a carrier thread reaches — so seeding `NativeRunning` would make
    // `from` read back as `NativeRunning` on that first call, the `Starting`
    // fold below would never fire, and the span would restore `NativeRunning`.
    // The thread would then be recorded as permanently inside a native, which
    // is the one state the STW census WAITS for. Seeding `JavaRunning` records
    // exactly what the three-access version concluded (`Starting` observed,
    // resume as `JavaRunning`), and is ignored entirely for a thread that has
    // been observed before.
    //
    // Caught by `a_span_on_an_unobserved_thread_resumes_as_java_running`, which
    // was written before this line existed and failed on the first build.
    with_cell(ThreadExecState::JavaRunning, |cell| {
        let raw = cell.state.load(Ordering::Relaxed);
        let from = ThreadExecState::from_u8(raw).unwrap_or(ThreadExecState::Starting);
        if stress_checks_enabled() && !is_legal(from, ThreadExecState::NativeRunning) {
            ILLEGAL_TRANSITIONS.fetch_add(1, Ordering::Relaxed);
            let violation = IllegalTransition {
                from,
                to: ThreadExecState::NativeRunning,
                thread_id: cell.thread_id.load(Ordering::Relaxed),
                site,
            };
            if violations_are_fatal() {
                panic!("CRATONVM_STRESS_THREAD_STATES: {violation}");
            }
            tracing::error!(
                from = violation.from.name(),
                to = violation.to.name(),
                tid = violation.thread_id,
                site = violation.site,
                "{violation}"
            );
        }
        cell.state
            .store(ThreadExecState::NativeRunning.as_u8(), Ordering::Relaxed);
        NativeStateSpan {
            cell: cell as *const ThreadStateCell,
            prior: match from {
                ThreadExecState::Starting => ThreadExecState::JavaRunning,
                other => other,
            },
        }
    })
}

/// Non-panicking [`record_transition`]: the store always lands, and an illegal
/// edge is returned rather than reported. Used by the tests and available to
/// callers that want to decide for themselves.
///
/// The store lands even on a violation deliberately: the shadow record must
/// keep tracking reality, or one bad edge desynchronises every later check.
pub fn try_record_transition(
    to: ThreadExecState,
    site: &'static str,
) -> Result<(), IllegalTransition> {
    let mut violation = None;
    let from = with_cell(to, |cell| {
        let raw = cell.state.load(Ordering::Relaxed);
        let from = ThreadExecState::from_u8(raw).unwrap_or(ThreadExecState::Starting);
        if stress_checks_enabled() && !is_legal(from, to) {
            violation = Some(IllegalTransition {
                from,
                to,
                thread_id: cell.thread_id.load(Ordering::Relaxed),
                site,
            });
        }
        // The single relaxed store the shadow record costs.
        cell.state.store(to.as_u8(), Ordering::Relaxed);
        from
    });
    // Registry membership follows the state, and ONLY at the two boundaries
    // that change it — the ordinary transition never touches the global lock.
    // Done outside the cell closure so a caller's own critical section is
    // never held across more than the membership change itself.
    if to == ThreadExecState::Terminated {
        retire_current_cell();
    } else if from == ThreadExecState::Terminated {
        revive_current_cell();
    }
    match violation {
        Some(v) => Err(v),
        None => Ok(()),
    }
}

/// Run `f` against the calling thread's cell, allocating (and registering) one
/// seeded to `seed` if this is the thread's first observation.
///
/// Seeding is deliberately *not* a transition: the recorder cannot know what a
/// thread was doing before it was first instrumented, so the first record
/// establishes the baseline instead of being checked against `Starting`.
/// The steady-state path runs `f` against a **borrow** of the thread-local
/// handle. It used to `Arc::clone` the cell out of TLS and call `f` on the
/// owned copy, which put a refcount increment *and* a decrement on every
/// transition — and every native call in the VM performs two transitions
/// (`vm_exec::safe_native_call_impl` records `NativeRunning` on entry and
/// restores the caller's state on return).
///
/// That was measured, not guessed. `native_funnel_profile::funnel_cost_
/// breakdown` (`vm/src/vm/vm_exec.rs`) times each component of the native
/// funnel separately: with the clone in place, the two `record_transition`
/// calls were **67-115 ns of a ~110-128 ns funnel**, while every other
/// component — the A3 diagnostic mask, `catch_unwind`, the pin push, the STW
/// probe, the GC-pressure probes, the JNI-exception drain — measured 1-20 ns
/// each. `current_state()`, which reads the same TLS cell through the same
/// `RefCell` but does *not* clone, measured 3 ns. The refcount traffic was
/// the funnel's fixed cost.
///
/// The borrow is safe because `f` is confined: its one caller
/// ([`try_record_transition`]) touches only `cell.state` / `cell.thread_id`
/// and never re-enters `SELF_CELL`, so it cannot trip the `RefCell`. The
/// cold arm scopes the shared borrow before taking the mutable one — an
/// `if let` scrutinee temporary otherwise lives to the end of the whole
/// `if/else` and `borrow_mut()` would panic on a thread's first transition.
///
/// Nothing about the census changes: `CellHandle` still owns the `Arc`, the
/// registry still holds its own, and the cell is still dropped out of
/// `CELLS` by `CellHandle::drop`.
fn with_cell<R>(seed: ThreadExecState, f: impl FnOnce(&ThreadStateCell) -> R) -> R {
    SELF_CELL.with(|c| {
        {
            let borrowed = c.borrow();
            if let Some(existing) = borrowed.as_ref() {
                return f(&existing.0);
            }
        }
        // First observation on this OS thread — once per thread, ever.
        let fresh = Arc::new(ThreadStateCell {
            thread_id: AtomicU64::new(PENDING_THREAD_ID.with(|p| p.get())),
            state: AtomicU8::new(seed.as_u8()),
        });
        *c.borrow_mut() = Some(CellHandle(Arc::clone(&fresh)));
        cells().write().push(Arc::clone(&fresh));
        f(&fresh)
    })
}

/// Drop the calling thread's cell out of the census registry (it has
/// terminated) while keeping the thread-local copy so [`current_state`] still
/// answers `Terminated`.
fn retire_current_cell() {
    let mine = SELF_CELL.with(|c| c.borrow().as_ref().map(|h| Arc::clone(&h.0)));
    let Some(mine) = mine else { return };
    let mut registry = cells().write();
    let before = registry.len();
    registry.retain(|c| !Arc::ptr_eq(c, &mine));
    if registry.len() != before {
        TERMINATED_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
}

/// Re-admit a revived cell (same OS thread re-attaching after a detach).
/// No-op in the overwhelmingly common case where the cell is still a member.
fn revive_current_cell() {
    let mine = SELF_CELL.with(|c| c.borrow().as_ref().map(|h| Arc::clone(&h.0)));
    let Some(mine) = mine else { return };
    {
        let registry = cells().read();
        if registry.iter().any(|c| Arc::ptr_eq(c, &mine)) {
            return;
        }
    }
    let mut registry = cells().write();
    if !registry.iter().any(|c| Arc::ptr_eq(c, &mine)) {
        registry.push(mine);
    }
}

/// Number of illegal transitions observed since process start. A soak or
/// stress run can assert this is zero without arming the panic.
pub fn illegal_transition_count() -> u64 {
    ILLEGAL_TRANSITIONS.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Census
// ---------------------------------------------------------------------------

/// Per-state population, as of one walk of the shadow registry.
///
/// This is the single source of truth the safepoint census and a future JFR
/// `jdk.CratonThreadStates` event are meant to share. It is a *snapshot*, not
/// an atomic observation: threads transition while the walk runs, exactly as
/// `ThreadRegistry::alive_count_blocked_and_os_tids` does today.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ThreadStateCensus {
    /// Live population per state, indexed by [`ThreadExecState::as_u8`].
    pub per_state: [u64; ThreadExecState::COUNT],
    /// Cumulative number of threads that reached
    /// [`ThreadExecState::Terminated`] (retired cells are not walked).
    pub terminated_total: u64,
}

impl ThreadStateCensus {
    /// Population of one state.
    #[inline]
    pub fn get(&self, state: ThreadExecState) -> u64 {
        self.per_state[state.as_u8() as usize]
    }

    /// Total live (non-terminated) threads the recorder has observed.
    pub fn live_total(&self) -> u64 {
        self.per_state.iter().sum()
    }

    /// Threads a stop-the-world initiator would include in `expected`, per
    /// [`ThreadExecState::counts_toward_safepoint_quota`] — the shadow-side
    /// answer to `alive_count_blocked_and_os_tids`'s `alive - blocked`.
    ///
    /// Excludes the initiator, which the real census subtracts separately.
    pub fn safepoint_quota_candidates(&self) -> u64 {
        ThreadExecState::ALL
            .iter()
            .filter(|s| s.counts_toward_safepoint_quota())
            .map(|s| self.get(*s))
            .sum()
    }

    /// Threads whose state forbids relocation.
    ///
    /// **This count includes the COLLECTING thread and therefore discriminates
    /// nothing on its own.** `JavaRunning` and `VmRunning` are both
    /// [`RelocationRule::Forbidden`], and the thread taking the census is in
    /// one of them by construction, so the answer is never zero: measured at
    /// `blockers=1` on all 452 collector decisions across four reps, relocating
    /// and not. Anything built on `relocation_blockers() > 0` fires on every
    /// cycle — including this method's own former doc, which called a non-zero
    /// answer "the shadow-side statement of the
    /// `mark_moving_young_coverage_incomplete_because` obligation".
    ///
    /// Use [`Self::peer_relocation_blockers`] for that statement.
    pub fn relocation_blockers(&self) -> u64 {
        ThreadExecState::ALL
            .iter()
            .filter(|s| s.relocation_rule() == RelocationRule::Forbidden)
            .map(|s| self.get(*s))
            .sum()
    }

    /// PEER threads whose state both forbids relocation and says their refs are
    /// unrewritable — the shadow-side statement of the
    /// `mark_moving_young_coverage_incomplete_because` obligation, with the
    /// initiator subtracted so a zero is reachable.
    ///
    /// The two predicates are ANDed on purpose. `relocation_rule() ==
    /// Forbidden` alone counts a second mutator merely RUNNING, which a
    /// stop-the-world is about to park through a path that rewrites it
    /// ([`ThreadExecState::SafepointParked`] is `PermittedWithRewrite`). What
    /// the obligation is about is a peer whose refs live somewhere no pointer
    /// map consumer reaches — registers and JIT spill slots — which is exactly
    /// [`ThreadExecState::may_hold_unrewritable_object_refs`].
    ///
    /// `initiator` is the collecting thread's own state, and exactly one thread
    /// is subtracted from THAT state's population — not from the total. The
    /// distinction is the whole point: subtracting from the total would cancel
    /// a genuine `CompiledUninterruptible` peer against an initiator recorded
    /// in a different state, turning the one reading that matters into a zero.
    ///
    /// The census is taken without stopping the world, so the initiator may
    /// already have been recorded elsewhere; that shows up as a population of
    /// zero for its state and nothing is subtracted.
    pub fn peer_relocation_blockers(&self, initiator: ThreadExecState) -> u64 {
        let qualifies = |s: ThreadExecState| {
            s.relocation_rule() == RelocationRule::Forbidden
                && s.may_hold_unrewritable_object_refs()
        };
        let total: u64 = ThreadExecState::ALL
            .iter()
            .filter(|s| qualifies(**s))
            .map(|s| self.get(*s))
            .sum();
        if qualifies(initiator) && self.get(initiator) > 0 {
            total.saturating_sub(1)
        } else {
            total
        }
    }
}

/// Walk the shadow registry and tally the per-state population.
///
/// Takes only a read lock, and only for the duration of the walk. Safe to call
/// from a diagnostic path, a watchdog hang report, or the STW census — it does
/// not touch the barrier or the thread registry.
pub fn thread_state_census() -> ThreadStateCensus {
    let mut census = ThreadStateCensus {
        per_state: [0; ThreadExecState::COUNT],
        terminated_total: TERMINATED_TOTAL.load(Ordering::Relaxed),
    };
    for cell in cells().read().iter() {
        let raw = cell.state.load(Ordering::Relaxed);
        if let Some(state) = ThreadExecState::from_u8(raw) {
            census.per_state[state.as_u8() as usize] += 1;
        }
    }
    census
}

/// `(thread_id, state)` for every live shadow cell, for the watchdog's hang
/// report and `CRATONVM_DBG_STW_CENSUS`-style diagnostics.
pub fn thread_state_roster() -> Vec<(u64, ThreadExecState)> {
    cells()
        .read()
        .iter()
        .map(|cell| {
            (
                cell.thread_id.load(Ordering::Relaxed),
                ThreadExecState::from_u8(cell.state.load(Ordering::Relaxed))
                    .unwrap_or(ThreadExecState::Starting),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Prices [`with_cell`]'s borrow against the `Arc::clone` it replaced, in one
/// process, with the two arms interleaved.
///
/// A before/after taken from two separate runs cannot answer this. The build
/// host here is shared: between the two breakdown runs that motivated the
/// change, every rung on the board moved — `current_state()` went 3.6 ns to
/// 1.1 ns without being touched at all — so a cross-run delta prices the box,
/// not the code. (The A4b measurement in
/// `architecture-review-a1-a9.md` records the
/// same hazard and the same remedy: interleave, and quote minima.)
///
/// Both arms here run in the same process, alternating on every pass, against
/// the same TLS cell. `old_shape` is a verbatim copy of the pre-fix body — an
/// `Arc::clone` out of the thread-local handle, the closure against the owned
/// copy, then the drop — so the difference between the arms is exactly one
/// refcount increment and one decrement per call, which is what the fix
/// removes.
///
/// `#[ignore]`d: it is a measurement, not an assertion. Run it with
///
/// ```text
/// cargo test --release -p cratonvm-vm --lib with_cell -- --ignored --nocapture
/// ```
#[cfg(test)]
mod with_cell_ab;

#[cfg(test)]
mod tests;
