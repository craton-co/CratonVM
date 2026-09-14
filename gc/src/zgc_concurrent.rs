// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent-mark **cycle driver** for ZGC.
//!
//! This module owns the phase sequencing of one ZGC marking cycle: it runs
//! the concurrent phase on a background thread, drives the mark-end
//! safepoint, implements the **restart loop** that ZGC's read-barrier
//! marking requires, and runs the non-strong-reference phase with the
//! re-drain that a resurrection obliges. The marking *engine* — the worker
//! pool, the striped work queues, work stealing and the termination
//! handshake — is [`crate::zgc::mark`]; this module never touches an object
//! header itself.
//!
//! ## What changed, and why the old module doc was wrong
//!
//! Until this rewrite this file drove [`crate::zgc::ZgcCollector`], the
//! **metadata-only simulation**: pages with synthetic `u64` addresses, no
//! backing storage, and a "concurrent" mark step that could only bump
//! `ZPage::live_bytes`. It could not mark an object graph because there was
//! no object graph to mark. That dependency is gone. The controller now
//! drives [`ZMarkCoordinator`], which marks a real graph through the
//! [`ZMarkContext`](crate::zgc::mark::ZMarkContext) seam.
//!
//! ## Why this is not `g1_concurrent.rs` with the names changed
//!
//! [`crate::g1_concurrent::ConcurrentMarkController`] and this type share a
//! thread lifecycle (spawn a background worker, park with a bounded wait,
//! stop via a flag, join deterministically) and that resemblance is
//! deliberate. The **termination protocol is not shared**, and cannot be:
//!
//! * G1 marks under a SATB **pre-write** barrier. The live set is a snapshot
//!   taken at initial mark, so the producer set is bounded and the remark
//!   pause — which quiesces the only producer — is final by construction.
//!   One pass, one pause, done.
//! * OpenJDK's ZGC has **no write barrier at all**. It marks on *read*, in the
//!   load barrier, so **every mutator thread is a producer** and stays one
//!   until it is stopped. "All queues are empty" is a *fixed point*, not a
//!   completion: the very next `getfield` any thread executes can publish
//!   new mark work.
//!
//! **`ZgcRealHeap` is not that.** It publishes from an SATB PRE-WRITE barrier
//! (`satb_pre_barrier`) and never arms its load barrier outside unit tests, so
//! its producer set is bounded like G1's rather than unbounded like ZGC's. The
//! restart loop below is therefore justified by a mechanism this backend does
//! not run; see `zgc::mark`'s header for what that does and does not imply
//! about whether the loop is needed.
//!
//! So the mark-end safepoint is a **decision point**, not a conclusion. With
//! mutators stopped and every per-thread mark buffer flushed,
//! [`ZMarkCoordinator::try_end_mark`] answers
//! [`ZMarkEndResult::Restart`] if that flush produced anything, and the whole
//! concurrent phase runs again. [`ZgcConcurrentMarkController`] is the thing
//! that implements that loop. It is the single biggest structural difference
//! from the G1 controller and the reason the two cannot be collapsed into one
//! generic "concurrent-markable collector".
//!
//! ## Phase sequence this driver executes
//!
//! ```text
//!   caller, at the mark-start safepoint:
//!       ZGoodMask::flip_to_mark()          (barrier module)
//!       coordinator.begin_cycle()
//!       coordinator.push_roots(&roots)     (the VM's root set)
//!       ZgcConcurrentMarkController::spawn(params)
//!
//!   this driver, on the "zgc-concurrent-mark" thread:
//!   +-> start_marking()                    arm the pool
//!   |   await fixed point                  concurrent; mutators run
//!   |   ---- mark-end safepoint ----
//!   |     ZgcMarkSafepoint::begin_mark_end_safepoint()   (VM stops mutators)
//!   |     coordinator.pause_for_safepoint()              (mark workers yield)
//!   |     ZgcMarkSafepoint::flush_mutator_buffers()      (per-thread buffers)
//!   |     try_end_mark()
//!   |       |-- Restart --------------------------------+
//!   |       `-- Complete
//!   |   ---- reference processing (still at a safepoint) ----
//!   |     process_non_strong_refs(hook)
//!   |       `-- resurrected > 0 -> re-drain -------------+
//!   |
//!   `-> outcome published; thread exits
//!
//!   caller, at the mark-end/relocate-start safepoint:
//!       controller.join_cycle()            -> ZgcMarkCycleOutcome
//!       coordinator.end_cycle()
//! ```
//!
//! ## What is real here, and what still is not
//!
//! Real:
//!
//! * The restart loop, its ceiling, and the "incomplete mark set" verdict.
//! * The reference-processing re-drain (a resurrected soft referent drags an
//!   unscanned subgraph behind it).
//! * The safepoint handshake with the mark workers, via
//!   [`ZMarkCoordinator::pause_for_safepoint`].
//! * Deterministic shutdown that cannot hang: every wait in this module is a
//!   bounded `wait_for`, and the stop flag is re-read at every loop head.
//!
//! **Not** real yet, and this must not be overclaimed — stale ZGC docs are
//! how this tree got into trouble the first time:
//!
//! * ~~**No `ZMarkContext` implementation for `ZgcRealHeap` exists.**~~
//!   **Landed since this was written.** `impl mark::ZMarkContext for
//!   ZgcRealHeap` is in `gc/src/zgc.rs` and satisfies the requirements listed
//!   under "What the wiring step must provide" below, including the atomic
//!   `try_mark` (it goes through `ObjectHeader::try_add_gc_flags`, a CAS
//!   loop). What is still missing is not the context but the **owner**: no
//!   coordinator is pointed at a `ZgcRealHeap`, because `ZMarkCoordinator`
//!   takes an `Arc<dyn ZMarkContext>` and the heap is held by value inside
//!   `VmHeap`. `ZgcRealHeap::collect_garbage` therefore remains the
//!   single-threaded stop-the-world mark-sweep it has always been.
//! * **The mutator ingress is wired, and it is a WRITE barrier, not the load
//!   barrier this module's design assumes.** Nothing calls
//!   [`ZMarkHandle::mark_live_offset`] from a `getfield` and nothing is
//!   planned to: `VmHeap::satb_barrier` is already called before every
//!   reference store in this VM for G1's sake, its ZGC arm was empty, and
//!   since 2026-08-13 it reaches `ZgcRealHeap::satb_pre_barrier`, which
//!   publishes the overwritten reference into that heap's own
//!   [`ZMarkIngress`](crate::zgc::mark::ZMarkIngress) when a cycle is armed.
//!
//!   **That substitution changes what the restart loop is for, and the
//!   difference must not be papered over.** This module's termination design
//!   is built on ZGC's read-barrier discipline, where every mutator is a
//!   producer until it is stopped and "all queues empty" is a fixed point
//!   rather than a completion. Under snapshot-at-the-beginning the producer
//!   set really *is* bounded, so
//!   [`try_end_mark`](ZMarkCoordinator::try_end_mark) is expected to answer
//!   [`Complete`](ZMarkEndResult::Complete) after the mark-end flush rather
//!   than restarting indefinitely. The loop stays correct and stays required
//!   — the flush can still produce work — but a `Restart` under SATB means
//!   the flush found buffered work, not that a mutator raced the marker.
//!   SATB is also *conservative*: an object that dies mid-cycle survives to
//!   the next one. That is a throughput cost, not a correctness one.
//!
//!   `mark_active` is never set by production code today, so the barrier's
//!   slow path is unreachable in a real run and its fast path is one relaxed
//!   load.
//! * The relocation half of ZGC (forwarding, remap, compaction) is not this
//!   module's business and is not driven from here.
//!
//! ## Safepoints: how a caller connects this to the VM's STW protocol
//!
//! [`crate::zgc::mark`] deliberately does **not** use `gc/src/safepoint.rs`.
//! That module is the GPU-critical-section token behind the `gpu-offload`
//! feature; it is not the VM safepoint protocol and has nothing to do with
//! stopping mutators. The VM's stop-the-world proof token is
//! [`crate::collector::StopTheWorldToken`], constructed by the orchestrator
//! in `vm/src/runtime/interpreter.rs` only after `gc_barrier.wait_for_all()`
//! reports every other mutator parked.
//!
//! This module does **not** invent a safepoint mechanism. It declares the
//! [`ZgcMarkSafepoint`] seam and calls it. A production implementor is
//! expected to:
//!
//! 1. `begin_mark_end_safepoint` — drive the VM's existing safepoint request
//!    (the same path that produces a `StopTheWorldToken`) and block until
//!    every mutator is parked. Holding the token for the duration of the
//!    callback triple is the intended pattern; it is not passed through this
//!    trait because the token is `!Send` by intent and this driver runs on
//!    its own thread. An implementor therefore owns the token on the thread
//!    that actually performs the stop.
//! 2. `flush_mutator_buffers` — walk every thread that owns a
//!    [`ZMarkMutatorBuffer`](crate::zgc::mark::ZMarkMutatorBuffer) and call
//!    [`ZMarkHandle::flush_buffer`] on it. This is the exact analogue of
//!    `g1_concurrent`'s remark calling
//!    [`crate::satb::flush_thread_satb_buffer`], and skipping it is the exact
//!    analogue of the bug that motivated that call: an address still sitting
//!    in a per-thread buffer is invisible to `try_end_mark`, which then
//!    answers `Complete` with a live object unscanned.
//!
//!    **Construct those buffers with
//!    [`ZMarkHandle::new_buffer`](crate::zgc::mark::ZMarkHandle::new_buffer),
//!    not `ZMarkMutatorBuffer::new`.** A thread that exits *between*
//!    safepoints — which no `flush_mutator_buffers` call can reach — is
//!    covered by the buffer's `Drop`, but only for an **attached** buffer.
//!    `ZMarkMutatorBuffer::new` produces a detached one, covered by nothing:
//!    `mark_live_buffered` sets the mark bit *before* the address reaches the
//!    ingress, so a detached buffer dropped non-empty leaves objects
//!    marked-and-unscanned — permanently invisible to the mark, since the
//!    mark bit is what dedups them. That is a use-after-free, not a missed
//!    optimisation.
//! 3. `end_mark_end_safepoint` — resume the mutators.
//!
//! The mark workers are quiesced by this driver, not by the implementor:
//! [`ZMarkCoordinator::pause_for_safepoint`] is taken *inside* the VM
//! safepoint and released before it ends.
//!
//! ## What the wiring step must provide
//!
//! To connect [`ZgcRealHeap`](crate::zgc::ZgcRealHeap) to this driver, a
//! future change to `gc/src/zgc.rs` must implement
//! [`ZMarkContext`](crate::zgc::mark::ZMarkContext) for it. Every method has
//! an existing counterpart inside `ZgcRealHeap::collect_garbage`, and the two
//! that are easy to get silently wrong are called out:
//!
//! * `is_in_heap(addr)` — `self.registry.lock().contains(&addr)`, i.e. the
//!   `registered.contains(&addr)` gate the current marker already applies to
//!   every child pointer. Refusals are counted as
//!   `ZMarkStats::off_heap_children`, which is today's `wild_skipped`.
//! * `try_mark(addr)` — must be an **atomic** test-and-set of
//!   `GC_FLAG_MARKED`. The current code does a plain read-modify-write of
//!   `header.gc_flags` under STW, which is *not* sufficient once N workers
//!   and arbitrary mutators race on the same header: two `true` answers for
//!   one object means it is pushed twice (merely wasteful), and a lost update
//!   means an object is popped once but never scanned — a live object with
//!   unscanned out-edges, i.e. a use-after-free. This is the one place the
//!   wiring step must change the header protocol rather than reuse it.
//! * `is_marked(addr)` — query only. It must never set the bit, or every
//!   weak referent becomes immortal.
//! * `visit_refs(addr, f)` — **strong edges only.** `ZgcRealHeap` already has
//!   exactly the right mechanism: `enumerate_references(base, work,
//!   skip_for(addr))`, where `skip_for` returns `Some(0)` for any address in
//!   `ref_skip_objs` (the registered `Weak`/`Soft`/`Phantom`/`Cleaner`
//!   `Reference` object addresses, from
//!   `ReferenceProcessor::reference_object_addresses()`). Slot 0 of a
//!   `Reference` is the referent and **must not** be reported, or the
//!   referent is trivially reachable through its own `Reference` and can
//!   never be cleared — `WeakReference` and `Cleaner` then silently stop
//!   working. The skip set must be snapshotted at mark start and held for the
//!   whole cycle (it is read concurrently by every worker). The same call
//!   must also report the three pin edges the current marker pushes by hand:
//!   `loader_pin::loader_pin_addr(class_id)`,
//!   `mirror_pin::mirrors_for_loader(addr)` and
//!   `metadata_pin::roots_for_loader(addr)` — dropping those is a class-loader
//!   unloading bug, not a marking optimisation.
//! * `object_size(addr)` — `Self::alloc_size(header)`, for per-page liveness.
//! * The [`ZNonStrongRefHook`] implementation is the *rest* of
//!   `collect_garbage`'s reference block: call
//!   `ReferenceProcessor::process_references(&is_marked, free_mb, now_ms)`,
//!   then hand `soft_survivor_referents()` and the finalizer referents to
//!   `keep_alive`. Everything handed to `keep_alive` is re-published as mark
//!   work and this driver re-drains it. That re-drain is the `INT-8 remark`
//!   block in `zgc.rs` expressed as a phase.
//!
//! ## No process-global state
//!
//! Everything here is instance-owned and reachable only from a controller.
//! No `static`, no `OnceLock`, no `thread_local!` — this tree has had
//! parallel-test crashes from process-global GC caches, and the test harness
//! hosts more than one heap per process.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::zgc::mark::{ZMarkCoordinator, ZMarkEndResult, ZMarkHandle, ZNonStrongRefHook};

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Bounded park interval for the driver thread while it waits for the mark
/// pool to reach a fixed point.
///
/// **RETIRED 2026-08-17, and the reasoning it carried was wrong about the API it
/// described.** It said:
///
/// > [`ZMarkTerminator`] does expose a blocking `wait_for_fixed_point`, but it
/// > can only be released by the *pool's* stop flag, not by this controller's —
/// > so waiting on it would make `request_stop_and_join` unable to interrupt a
/// > cycle.
///
/// `ZMarkTerminator::wait_for_fixed_point(&self, should_stop: &AtomicBool)`
/// **takes the stop flag as a parameter**. It is `ZMarkCoordinator`'s no-argument
/// *wrapper* that hardwires the pool's flag, and the comment described the
/// wrapper while the driver had the terminator in hand. So the driver polled a
/// 5 ms grid to avoid a hazard the API it was avoiding does not have — and the
/// condvar it polled instead (`ZgcConcurrentMarkState::done_cvar`) had no
/// notifier on any ZGC path at all, so every wait ran to its full timeout.
///
/// `await_fixed_point` now passes `state.should_stop` to the terminator's method:
/// completion is immediate, and the stop flag is still re-read every
/// `Z_MARK_PARK_POLL_MS` *and* kicked directly by
/// `ZMarkTerminator::wake_blocked_waiters`, so interruption is faster than it
/// was rather than slower. `stopping_the_driver_mid_cycle_does_not_hang` is the
/// proof, and it was run against a deliberately wedged pool.
///
/// Left as a doc comment with no constant because the constant had one use.
const _DRIVER_POLL_MS_RETIRED: () = ();

/// Default ceiling on mark-end restarts within one cycle.
///
/// Each restart costs one real safepoint, so 64 of them means the mutators
/// out-raced the marker on 64 consecutive attempts. That is a *policy*
/// failure — the cycle was started too late — not a transient, and HotSpot
/// answers it by escalating to marking inside the pause. Until this tree has
/// that escalation, the honest response is to stop, log loudly, and report
/// [`ZgcMarkCycleOutcome::mark_set_complete`] as `false`.
///
/// An unbounded loop here would be a hang, and this codebase has a documented
/// history of GC livelocks presenting to the user as an unexplained freeze.
pub const DEFAULT_MAX_MARK_END_RESTARTS: usize = 64;

/// Default ceiling on reference-processing resurrection rounds.
///
/// Resurrection is monotone for a well-behaved hook — an object can only be
/// marked once, so the rounds strictly shrink — and two rounds is already
/// unusual. The bound exists because a *misbehaving* hook (one that resurrects
/// from a set it also mutates) would otherwise spin forever, and "the GC never
/// returns" is a far worse failure than "the GC says its answer is
/// incomplete".
pub const DEFAULT_MAX_RESURRECTION_ROUNDS: usize = 8;

// ---------------------------------------------------------------------------
// Safepoint seam
// ---------------------------------------------------------------------------

/// The VM stop-the-world protocol, as the mark-end pause needs it.
///
/// See the module docs ("Safepoints") for how an implementor connects this to
/// [`crate::collector::StopTheWorldToken`]. The three calls are always made in
/// order and are balanced even if a later step panics — the driver wraps them
/// in an RAII scope.
///
/// # Contract
///
/// * `begin_mark_end_safepoint` must not return until **every** mutator
///   thread is stopped. Returning early makes `try_end_mark`'s answer a lie:
///   a running mutator can publish mark work between the flush and the probe.
/// * `flush_mutator_buffers` must flush **every** per-thread
///   [`ZMarkMutatorBuffer`](crate::zgc::mark::ZMarkMutatorBuffer), and returns
///   how many addresses it moved (telemetry only — the driver does not branch
///   on it; `try_end_mark` re-probes authoritatively).
/// * Those buffers must come from
///   [`ZMarkHandle::new_buffer`](crate::zgc::mark::ZMarkHandle::new_buffer).
///   A thread exiting between safepoints is covered by the buffer's `Drop`,
///   but ONLY for an attached buffer; `ZMarkMutatorBuffer::new` is detached
///   and is covered by nothing. See the module header, step 2.
/// * None of the three may block indefinitely. The driver checks its stop
///   flag at every loop head, but it cannot interrupt a call that is inside
///   this trait, so a blocking implementation turns
///   [`ZgcConcurrentMarkController::request_stop_and_join`] into a hang.
pub trait ZgcMarkSafepoint: Send + Sync {
    /// Stop every mutator thread. Blocks until they are all parked.
    fn begin_mark_end_safepoint(&self);

    /// Flush every per-thread mutator mark buffer into the ingress. Returns
    /// the number of addresses moved.
    fn flush_mutator_buffers(&self, handle: &ZMarkHandle) -> usize;

    /// Resume the mutators.
    fn end_mark_end_safepoint(&self);
}

/// A [`ZgcMarkSafepoint`] for a caller that has no other mutator threads.
///
/// Legal **only** when the thread driving the heap is the sole mutator — a
/// unit test, or a single-threaded embedding. It stops nothing and flushes
/// nothing, which is correct exactly when there is nothing to stop and no
/// per-thread buffer to flush. Using it in a multi-threaded VM makes every
/// `try_end_mark` answer unsound.
#[derive(Debug, Default, Clone, Copy)]
pub struct ZgcNoMutatorSafepoint;

impl ZgcMarkSafepoint for ZgcNoMutatorSafepoint {
    fn begin_mark_end_safepoint(&self) {}

    fn flush_mutator_buffers(&self, _handle: &ZMarkHandle) -> usize {
        0
    }

    fn end_mark_end_safepoint(&self) {}
}

/// RAII pairing of [`ZgcMarkSafepoint::begin_mark_end_safepoint`] with
/// [`ZgcMarkSafepoint::end_mark_end_safepoint`].
///
/// Exists so that an early return — or a panic inside `try_end_mark` — cannot
/// leave the world stopped. A leaked safepoint is a whole-VM freeze, which is
/// exactly the failure this module is written to avoid.
struct ZgcSafepointScope<'a> {
    safepoint: &'a dyn ZgcMarkSafepoint,
}

impl<'a> ZgcSafepointScope<'a> {
    fn enter(safepoint: &'a dyn ZgcMarkSafepoint) -> Self {
        safepoint.begin_mark_end_safepoint();
        ZgcSafepointScope { safepoint }
    }
}

impl Drop for ZgcSafepointScope<'_> {
    fn drop(&mut self) {
        self.safepoint.end_mark_end_safepoint();
    }
}

// ---------------------------------------------------------------------------
// Cycle parameters and outcome
// ---------------------------------------------------------------------------

/// Everything [`ZgcConcurrentMarkController::spawn`] needs.
///
/// A struct rather than five positional arguments: two of the fields are
/// `usize` budgets and two are trait-object `Arc`s, so a positional signature
/// would be easy to transpose and impossible to notice.
pub struct ZgcConcurrentMarkParams {
    /// The marking engine. Shared, not owned: a caller that keeps its own
    /// clone keeps the worker pool alive across cycles (spawning a pool per
    /// GC is exactly the cost the pool exists to avoid). When the last clone
    /// drops, [`ZMarkCoordinator`]'s own `Drop` stops and joins the workers.
    pub coordinator: Arc<ZMarkCoordinator>,

    /// The VM's stop-the-world protocol for the mark-end pause.
    pub safepoint: Arc<dyn ZgcMarkSafepoint>,

    /// The weak/soft/phantom/final reference phase. `None` skips the phase
    /// entirely, which is only correct for a heap with no registered
    /// `Reference` objects (i.e. a test).
    pub refs: Option<Arc<dyn ZNonStrongRefHook + Send + Sync>>,

    /// Ceiling on mark-end restarts. See [`DEFAULT_MAX_MARK_END_RESTARTS`].
    pub max_mark_end_restarts: usize,

    /// Ceiling on resurrection rounds. See
    /// [`DEFAULT_MAX_RESURRECTION_ROUNDS`].
    pub max_resurrection_rounds: usize,
}

impl ZgcConcurrentMarkParams {
    /// Parameters with the default budgets and no reference-processing hook.
    pub fn new(coordinator: Arc<ZMarkCoordinator>, safepoint: Arc<dyn ZgcMarkSafepoint>) -> Self {
        ZgcConcurrentMarkParams {
            coordinator,
            safepoint,
            refs: None,
            max_mark_end_restarts: DEFAULT_MAX_MARK_END_RESTARTS,
            max_resurrection_rounds: DEFAULT_MAX_RESURRECTION_ROUNDS,
        }
    }

    /// Attach the reference-processing hook.
    pub fn with_refs(mut self, refs: Arc<dyn ZNonStrongRefHook + Send + Sync>) -> Self {
        self.refs = Some(refs);
        self
    }

    /// Override the mark-end restart ceiling.
    pub fn with_max_mark_end_restarts(mut self, max: usize) -> Self {
        self.max_mark_end_restarts = max;
        self
    }

    /// Override the resurrection-round ceiling.
    pub fn with_max_resurrection_rounds(mut self, max: usize) -> Self {
        self.max_resurrection_rounds = max;
        self
    }
}

impl std::fmt::Debug for ZgcConcurrentMarkParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZgcConcurrentMarkParams")
            .field("workers", &self.coordinator.worker_count())
            .field("has_ref_hook", &self.refs.is_some())
            .field("max_mark_end_restarts", &self.max_mark_end_restarts)
            .field("max_resurrection_rounds", &self.max_resurrection_rounds)
            .finish()
    }
}

/// What one marking cycle did.
///
/// `Default` is the "nothing succeeded" shape — in particular
/// `mark_set_complete` defaults to `false`, so a partially-constructed or
/// abandoned outcome can never be mistaken for a usable mark set.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ZgcMarkCycleOutcome {
    /// Concurrent phases run (1 = no restart was needed).
    pub passes: usize,
    /// How many of those were forced by a non-empty mark-end flush.
    pub restarts: usize,
    /// Re-drains forced by reference processing resurrecting objects.
    pub redrains: usize,
    /// Objects the reference hook resurrected, summed over all rounds.
    pub resurrected: usize,
    /// **The load-bearing field.** `true` iff the transitive closure of the
    /// root set (plus every resurrection) is fully marked. A sweep against a
    /// mark set with this `false` is a use-after-free.
    pub mark_set_complete: bool,
    /// The mark-end restart ceiling was hit.
    pub restart_budget_exhausted: bool,
    /// The resurrection-round ceiling was hit.
    pub resurrection_budget_exhausted: bool,
    /// The controller was told to stop mid-cycle.
    pub stopped_early: bool,
}

// ---------------------------------------------------------------------------
// Shared coordinator/driver state
// ---------------------------------------------------------------------------

/// Shared state between the VM thread that opened the cycle and the
/// background driver thread. Held in an `Arc` so the driver keeps it alive
/// after the controller drops its handle.
pub struct ZgcConcurrentMarkState {
    /// Set by `request_stop`; the driver leaves the cycle at its next loop
    /// head. `Release` on store / `Acquire` on load so a driver that observes
    /// the stop also observes everything the stopper did beforehand.
    pub should_stop: AtomicBool,

    /// Telemetry: concurrent phases started, across the cycle. `Relaxed`:
    /// pure telemetry, no control flow reads it, nothing is published
    /// through it. (The authoritative per-cycle figures are in
    /// [`ZgcMarkCycleOutcome`], which is published under a mutex.)
    pub passes_performed: AtomicU64,
    /// Telemetry: mark-end restarts. `Relaxed`, as above.
    pub restarts_performed: AtomicU64,
    /// Telemetry: reference-processing re-drains. `Relaxed`, as above.
    pub resurrection_redrains: AtomicU64,

    /// The finished cycle's report. `None` until the driver publishes it,
    /// which it does as its last act before the thread exits. A mutex rather
    /// than a pile of atomics so the whole report is published as one value
    /// and no reader can see a half-updated cycle.
    outcome: Mutex<Option<ZgcMarkCycleOutcome>>,
}

impl ZgcConcurrentMarkState {
    fn new() -> Self {
        ZgcConcurrentMarkState {
            should_stop: AtomicBool::new(false),
            passes_performed: AtomicU64::new(0),
            restarts_performed: AtomicU64::new(0),
            resurrection_redrains: AtomicU64::new(0),

            outcome: Mutex::new(None),
        }
    }

    /// Coordinator -> driver: stop ASAP.
    ///
    /// Sets the flag only. The driver waits on the **terminator's** condvar and
    /// re-reads this flag every `Z_MARK_PARK_POLL_MS`, so the flag alone bounds
    /// interruption; `request_stop_and_join` additionally kicks that condvar
    /// through `ZMarkTerminator::wake_blocked_waiters` so the usual case is
    /// immediate.
    ///
    /// It used to also notify a condvar of its own, which existed so a *polling*
    /// driver could be woken early. Both are gone — see
    /// `_DRIVER_POLL_MS_RETIRED`: nothing on any ZGC path ever notified that
    /// condvar for the normal completion case, so the poll interval was paid in
    /// full on every pass of every cycle.
    pub fn request_stop(&self) {
        self.should_stop.store(true, Ordering::Release);
    }

    /// The finished cycle's report, or `None` while it is still running.
    pub fn outcome(&self) -> Option<ZgcMarkCycleOutcome> {
        *self.outcome.lock()
    }
}

impl Default for ZgcConcurrentMarkState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ZgcConcurrentMarkState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZgcConcurrentMarkState")
            .field("should_stop", &self.should_stop.load(Ordering::Relaxed))
            .field("passes", &self.passes_performed.load(Ordering::Relaxed))
            .field("restarts", &self.restarts_performed.load(Ordering::Relaxed))
            .field("outcome", &self.outcome.lock().as_ref().copied())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// Drives one ZGC marking cycle on a background thread.
///
/// One controller drives **one cycle**, then its thread exits. The expensive,
/// long-lived thing is the [`ZMarkCoordinator`] worker pool, which the caller
/// owns and reuses; spawning one driver thread per GC is noise next to the
/// cycle it drives, and a one-shot driver means "the cycle finished" is
/// simply "the thread joined" — no completion flag to get wrong.
///
/// Drop semantics: dropping without calling [`Self::join_cycle`] or
/// [`Self::request_stop_and_join`] flips the stop flag and **detaches** the
/// driver (best-effort cleanup; `Drop` is sync and the driver may be inside a
/// caller-supplied safepoint callback that this type cannot interrupt). The
/// detached driver holds its own `Arc<ZMarkCoordinator>`, so the pool cannot
/// be freed underneath it. Production callers should always join explicitly.
pub struct ZgcConcurrentMarkController {
    /// Shared driver state. Public for the same reason the G1 controller's is:
    /// callers poll telemetry off it. (It no longer carries a park flag or a
    /// condvar of its own — the driver waits on the terminator's, see
    /// `_DRIVER_POLL_MS_RETIRED`.)
    pub state: Arc<ZgcConcurrentMarkState>,
    coordinator: Arc<ZMarkCoordinator>,
    handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for ZgcConcurrentMarkController {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZgcConcurrentMarkController")
            .field("running", &self.is_running())
            .field("state", &self.state)
            .finish()
    }
}

impl ZgcConcurrentMarkController {
    /// Spawn the driver thread for a cycle the caller has already opened.
    ///
    /// **Caller responsibility**, all at the mark-start safepoint and all
    /// before this call:
    ///
    /// 1. Flip the good mask to the cycle's mark color
    ///    (`ZGoodMask::flip_to_mark`).
    /// 2. [`ZMarkCoordinator::begin_cycle`] — clears the previous cycle's
    ///    residue and opens the ingress.
    /// 3. [`ZMarkCoordinator::push_roots`] with the VM's root set.
    ///
    /// Root scanning is deliberately *not* a seam on this type. It is a
    /// stop-the-world operation the VM must perform with its own thread list,
    /// stack maps and JIT frame maps; a background driver has no business
    /// doing it, and pretending otherwise would put a root-capture gap behind
    /// an innocent-looking callback.
    ///
    /// Spawning without a seeded root set is legal and produces a cycle that
    /// reaches a fixed point immediately and marks nothing.
    pub fn spawn(params: ZgcConcurrentMarkParams) -> Self {
        let state = Arc::new(ZgcConcurrentMarkState::new());
        let coordinator = Arc::clone(&params.coordinator);
        let driver_state = Arc::clone(&state);

        let handle = std::thread::Builder::new()
            .name("zgc-concurrent-mark".to_string())
            .spawn(move || {
                let outcome = Self::run_cycle(&params, &driver_state);
                *driver_state.outcome.lock() = Some(outcome);
                if outcome.mark_set_complete {
                    tracing::debug!(
                        target: "zgc",
                        passes = outcome.passes,
                        restarts = outcome.restarts,
                        redrains = outcome.redrains,
                        resurrected = outcome.resurrected,
                        "ZGC concurrent mark cycle complete"
                    );
                } else {
                    tracing::warn!(
                        target: "zgc",
                        passes = outcome.passes,
                        restarts = outcome.restarts,
                        stopped_early = outcome.stopped_early,
                        restart_budget_exhausted = outcome.restart_budget_exhausted,
                        resurrection_budget_exhausted = outcome.resurrection_budget_exhausted,
                        "ZGC concurrent mark cycle ended with an INCOMPLETE mark set; \
                         it must not be swept against"
                    );
                }
            })
            .expect("zgc-concurrent-mark thread spawn failed");

        Self {
            state,
            coordinator,
            handle: Some(handle),
        }
    }

    // -- the cycle ----------------------------------------------------------

    /// The driver thread body: strong marking to completion, then the
    /// non-strong-reference phase with its re-drains.
    fn run_cycle(
        params: &ZgcConcurrentMarkParams,
        state: &ZgcConcurrentMarkState,
    ) -> ZgcMarkCycleOutcome {
        let mut outcome = ZgcMarkCycleOutcome::default();

        // ---- strong marking ------------------------------------------------
        if !Self::drive_to_mark_end(params, state, &mut outcome) {
            outcome.stopped_early = true;
            return outcome;
        }
        if outcome.restart_budget_exhausted {
            return outcome; // mark_set_complete stays false
        }

        // ---- weak / soft / phantom / final ---------------------------------
        //
        // Each round runs at a safepoint with the mark workers quiesced: the
        // liveness answers the hook sees must be final, and `is_marked` must
        // not be racing a worker that is still setting bits. A non-zero
        // return means objects were resurrected with unscanned out-edges, so
        // the strong-mark loop runs again — a resurrected soft referent drags
        // a whole subgraph behind it, and skipping that re-drain is precisely
        // the bug `zgc.rs`'s `INT-8 remark` block exists to fix.
        if let Some(hook) = params.refs.as_ref() {
            let mut round: usize = 0;
            loop {
                let resurrected = {
                    let _sp = ZgcSafepointScope::enter(&*params.safepoint);
                    let _pause = params.coordinator.pause_for_safepoint();
                    params.coordinator.process_non_strong_refs(&**hook)
                };
                if resurrected == 0 {
                    break;
                }
                outcome.resurrected += resurrected;
                outcome.redrains += 1;
                state.resurrection_redrains.fetch_add(1, Ordering::Relaxed);

                round += 1;
                if round > params.max_resurrection_rounds {
                    outcome.resurrection_budget_exhausted = true;
                    tracing::warn!(
                        target: "zgc",
                        round,
                        max = params.max_resurrection_rounds,
                        resurrected = outcome.resurrected,
                        "ZGC mark: reference processing is still resurrecting objects after \
                         the round budget; the mark set is INCOMPLETE and must not be swept \
                         against (a hook that resurrects unboundedly is the likely cause)"
                    );
                    return outcome;
                }

                if !Self::drive_to_mark_end(params, state, &mut outcome) {
                    outcome.stopped_early = true;
                    return outcome;
                }
                if outcome.restart_budget_exhausted {
                    return outcome;
                }
            }
        }

        outcome.mark_set_complete = true;
        outcome
    }

    /// **The restart loop.** Run concurrent phases until the mark-end
    /// safepoint reports [`ZMarkEndResult::Complete`], or the ceiling is hit.
    ///
    /// Returns `false` iff the controller was told to stop; the caller then
    /// abandons the cycle. Returning `true` means the loop *ended*, which is
    /// not the same as succeeding — check
    /// [`ZgcMarkCycleOutcome::restart_budget_exhausted`].
    ///
    /// # Why the loop exists
    ///
    /// See the module docs. In one line: `wait_for_fixed_point` establishes
    /// that no *worker* holds work, but mutators are still running and are
    /// producers, so only the safepoint — where they are stopped and their
    /// buffers are flushed — can tell whether the fixed point was final.
    ///
    /// # Ordering inside the safepoint
    ///
    /// 1. Stop the mutators (`ZgcSafepointScope::enter`).
    /// 2. Quiesce the mark workers
    ///    ([`ZMarkCoordinator::pause_for_safepoint`]). Cheap here — they are
    ///    already at a fixed point — but it is what makes "nothing is inside
    ///    `visit_refs`" true rather than merely likely, and the restart case
    ///    re-arms them afterwards.
    /// 3. Flush every per-thread mutator buffer. **Before** the probe: an
    ///    address still in a buffer is invisible to it.
    /// 4. Probe ([`ZMarkCoordinator::try_end_mark`]).
    ///
    /// Unwinding is the reverse (Rust drops locals in reverse declaration
    /// order): the workers resume, then the mutators.
    fn drive_to_mark_end(
        params: &ZgcConcurrentMarkParams,
        state: &ZgcConcurrentMarkState,
        outcome: &mut ZgcMarkCycleOutcome,
    ) -> bool {
        loop {
            params.coordinator.start_marking();
            outcome.passes += 1;
            state.passes_performed.fetch_add(1, Ordering::Relaxed);

            if !Self::await_fixed_point(params, state) {
                return false;
            }

            let result = {
                let _sp = ZgcSafepointScope::enter(&*params.safepoint);
                let _pause = params.coordinator.pause_for_safepoint();
                let handle = params.coordinator.handle();
                let flushed = params.safepoint.flush_mutator_buffers(&handle);
                if flushed > 0 {
                    tracing::debug!(
                        target: "zgc",
                        flushed,
                        "ZGC mark end: flushed per-thread mutator mark buffers"
                    );
                }
                params.coordinator.try_end_mark()
            };

            match result {
                ZMarkEndResult::Complete => return true,
                ZMarkEndResult::Restart => {
                    outcome.restarts += 1;
                    state.restarts_performed.fetch_add(1, Ordering::Relaxed);
                    if outcome.restarts > params.max_mark_end_restarts {
                        outcome.restart_budget_exhausted = true;
                        tracing::warn!(
                            target: "zgc",
                            restarts = outcome.restarts,
                            max_restarts = params.max_mark_end_restarts,
                            passes = outcome.passes,
                            "ZGC mark: mark-end restart budget exhausted with work still \
                             pending; the mark set is INCOMPLETE and must not be swept \
                             against. Marking is losing the race with the application — \
                             the cycle should have started earlier"
                        );
                        return true;
                    }
                }
            }

            // Re-read the stop flag before committing to another concurrent
            // phase, so a stop that arrived during the safepoint is honoured
            // without paying for a whole extra pass.
            if state.should_stop.load(Ordering::Acquire) {
                return false;
            }
        }
    }

    /// Wait for the mark pool to reach a fixed point. Returns `false` iff the
    /// controller was told to stop first.
    ///
    /// **Blocks on the terminator's own condvar**, which the termination edge
    /// notifies under the same lock this waits on
    /// (`worker_idle` sets `terminated` and calls `notify_all`). So the driver
    /// learns the mark has converged at the moment it converges.
    ///
    /// # What this used to do, and why it was invisible
    ///
    /// It polled `ZgcConcurrentMarkState`'s *own* condvar on a
    /// `DRIVER_POLL_MS` (5 ms) grid, and the only thing that ever notified that
    /// condvar outside a stop was `notify_work_available` -- which had **zero
    /// callers on any ZGC path** (`vm_heap`'s one call site is G1's controller).
    /// So every pass of every cycle waited out the full 5 ms before noticing a
    /// fixed point the workers had often reached immediately, and nothing said
    /// so: a wait that always times out behaves exactly like a notified one,
    /// only slower. `ZMarkTerminator::park_timeouts` now counts the expiries so
    /// a recurrence is a number rather than a slowdown someone has to attribute.
    ///
    /// This is **not** the whole of C5. §3c measured one worker at +98% against
    /// zero, and at `Z_PARMARK_RESTART_BUDGET = 1` this path costs at most two
    /// intervals -- ~5-10 ms of a ~100 ms gap. What it removes is a *floor*
    /// under the parallel-mark pause that no amount of worker scaling could
    /// reach, and a 5 ms quantum inside the loop that any per-cycle timing would
    /// otherwise have carried as instrument noise. Fix the instrument, then
    /// measure.
    ///
    /// [`ZMarkTerminator::is_terminated`](crate::zgc::mark::ZMarkTerminator::is_terminated)
    /// is the authoritative read (it takes the terminator's state lock); the
    /// lock-free `is_terminated_hint` deliberately is not used, because it can
    /// lag and a stale "terminated" would take the safepoint while workers are
    /// still tracing.
    fn await_fixed_point(params: &ZgcConcurrentMarkParams, state: &ZgcConcurrentMarkState) -> bool {
        let terminator = params.coordinator.shared().terminator();
        loop {
            if terminator.is_terminated() {
                return true;
            }
            if state.should_stop.load(Ordering::Acquire) {
                return false;
            }
            // Blocks on the terminator's condvar; returns on either predicate.
            // The loop shape is kept deliberately: this must NEVER return `true`
            // on anything but an authoritative `terminated`, because `true` is
            // what takes the mark-end safepoint, and taking it while workers are
            // still inside `visit_refs` is a sweep against an incomplete mark
            // set.
            terminator.wait_for_fixed_point(&state.should_stop);
        }
    }

    // -- coordinator-side API -----------------------------------------------

    /// The marking engine this controller drives.
    pub fn coordinator(&self) -> &ZMarkCoordinator {
        &self.coordinator
    }

    /// A load-barrier handle for the engine. Convenience for a caller that
    /// handed the coordinator over and kept only the controller.
    pub fn handle(&self) -> ZMarkHandle {
        self.coordinator.handle()
    }

    /// Wait for the cycle to finish and take its report.
    ///
    /// Does **not** request a stop: this is the normal end of a cycle, where
    /// the driver has already decided the mark set is final (or has decided,
    /// loudly, that it is not). A panic in the driver surfaces here as `Err`
    /// rather than being silently dropped.
    ///
    /// The caller runs [`ZMarkCoordinator::end_cycle`] afterwards, at the
    /// safepoint where it flips the good mask on to relocation.
    pub fn join_cycle(mut self) -> std::thread::Result<ZgcMarkCycleOutcome> {
        if let Some(h) = self.handle.take() {
            h.join()?;
        }
        Ok(self.state.outcome().unwrap_or_default())
    }

    /// Abandon the cycle: signal stop and join the driver.
    ///
    /// The driver leaves at its next loop head — between concurrent phases, or
    /// out of the fixed-point wait as soon as the kick below reaches it. It
    /// cannot be interrupted while inside a [`ZgcMarkSafepoint`] callback, which
    /// is why that trait's contract forbids blocking indefinitely.
    ///
    /// The resulting mark set is incomplete
    /// ([`ZgcMarkCycleOutcome::stopped_early`]); the caller must discard it,
    /// not sweep against it.
    pub fn request_stop_and_join(mut self) -> std::thread::Result<()> {
        self.state.request_stop();
        // THE FLAG ALONE IS ENOUGH, and this makes it prompt. The driver's wait
        // re-reads the flag every `Z_MARK_PARK_POLL_MS`, so a missing kick costs
        // an interval rather than a hang; the kick changes no state, and a
        // spurious wake is always safe because every waiter re-checks its
        // predicate under the lock.
        self.coordinator
            .shared()
            .terminator()
            .wake_blocked_waiters();
        if let Some(h) = self.handle.take() {
            return h.join();
        }
        Ok(())
    }

    /// Is the driver thread still alive?
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// The cycle report, or `None` while the cycle is still running.
    pub fn outcome(&self) -> Option<ZgcMarkCycleOutcome> {
        self.state.outcome()
    }

    /// Telemetry: concurrent phases started so far.
    ///
    /// Replaces the simulation-era `steps_performed`, which counted
    /// `ZgcCollector::concurrent_mark_step` calls — a unit that no longer
    /// exists. One "pass" is one full concurrent phase driven to a fixed
    /// point, so this is the restart count plus one for a healthy cycle.
    pub fn passes_performed(&self) -> u64 {
        self.state.passes_performed.load(Ordering::Relaxed)
    }

    /// Telemetry: mark-end restarts so far. Persistently non-zero means the
    /// mutators are marking faster than the pool traces, i.e. the cycle is
    /// starting too late.
    pub fn restarts_performed(&self) -> u64 {
        self.state.restarts_performed.load(Ordering::Relaxed)
    }
}

impl Drop for ZgcConcurrentMarkController {
    fn drop(&mut self) {
        // Best-effort cleanup if the caller forgot to join. We do not block
        // here: `Drop` is sync and the driver may be inside a caller-supplied
        // safepoint callback we cannot interrupt. Detaching is safe because
        // the driver owns its own `Arc<ZMarkCoordinator>` clone, so the pool
        // (and the `ZMarkContext` behind it) outlives the detached thread —
        // the "detached worker outliving its heap" hazard that
        // `ZMarkCoordinator::drop` joins to avoid does not arise here.
        self.state.request_stop();
        // Same kick as `request_stop_and_join`, for the same reason: the driver
        // is detached here rather than joined, but it still has to LEAVE.
        self.coordinator
            .shared()
            .terminator()
            .wake_blocked_waiters();
        if let Some(h) = self.handle.take() {
            std::mem::drop(h);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
//
// Every assertion below is on counts, states or set equality. There are no
// wall-clock bounds anywhere: this tree has documented CI flakes from fixed
// timing assertions, and every spin in these tests is bounded and asserts
// nothing about how long it took.
//
// The three simulation-era tests this file used to carry are gone; see the
// module history. They asserted on `ZgcCollector::pause_mark_start`,
// `LoadBarrier::good_colors` and `ColoredPointer`, none of which this
// controller touches any more — and one of them asserted `elapsed < 500ms`.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zgc::mark::TestMarkContext;
    use rustc_hash::FxHashMap;
    use std::sync::atomic::AtomicUsize;

    /// Build a graph from `(node, children)` pairs, auto-inserting any child
    /// that was not declared as a leaf. Mirrors `zgc::mark`'s test helper so
    /// the two read the same way.
    fn graph(edges: &[(u64, &[u64])]) -> FxHashMap<u64, Vec<u64>> {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        for (node, children) in edges {
            g.insert(*node, children.to_vec());
            for c in children.iter() {
                g.entry(*c).or_default();
            }
        }
        g
    }

    /// A safepoint that hands the load barrier one scripted address per
    /// mark-end flush, newest entry last.
    ///
    /// This is how these tests make a mutator mark arrive *exactly* at the
    /// mark-end pause, deterministically: the driver calls
    /// `flush_mutator_buffers` inside the safepoint, so an address published
    /// from here lands in the ingress immediately before `try_end_mark`
    /// probes it. No sleeps, no races.
    struct ScriptedSafepoint {
        /// Popped from the back, so the script reads bottom-up. Empty means
        /// "flush nothing", i.e. a quiet safepoint.
        script: Mutex<Vec<u64>>,
        entered: AtomicUsize,
        left: AtomicUsize,
        flushes: AtomicUsize,
    }

    impl ScriptedSafepoint {
        fn new(script: Vec<u64>) -> Self {
            ScriptedSafepoint {
                script: Mutex::new(script),
                entered: AtomicUsize::new(0),
                left: AtomicUsize::new(0),
                flushes: AtomicUsize::new(0),
            }
        }

        fn balanced(&self) -> bool {
            self.entered.load(Ordering::Relaxed) == self.left.load(Ordering::Relaxed)
        }
    }

    impl ZgcMarkSafepoint for ScriptedSafepoint {
        fn begin_mark_end_safepoint(&self) {
            self.entered.fetch_add(1, Ordering::Relaxed);
        }

        fn flush_mutator_buffers(&self, handle: &ZMarkHandle) -> usize {
            self.flushes.fetch_add(1, Ordering::Relaxed);
            let next = self.script.lock().pop();
            match next {
                // `mark_live` is the load-barrier slow path: it marks and
                // hands the address to the ingress, which is exactly what a
                // per-thread buffer flush would have produced.
                Some(addr) => usize::from(handle.mark_live(0, addr)),
                None => 0,
            }
        }

        fn end_mark_end_safepoint(&self) {
            self.left.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Reference hook that resurrects everything in `candidates` that is not
    /// already strongly marked, and records what it observed as live.
    struct KeepAliveHook {
        candidates: Vec<u64>,
        observed_live: Mutex<Vec<u64>>,
        calls: AtomicUsize,
    }

    impl ZNonStrongRefHook for KeepAliveHook {
        fn process(&self, is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut live = self.observed_live.lock();
            for &c in &self.candidates {
                if is_marked(c) {
                    live.push(c);
                } else {
                    keep_alive(c);
                }
            }
        }
    }

    /// Open a cycle over `g` rooted at `roots`, returning the context and the
    /// pool. The caller drives the rest.
    fn open_cycle(
        g: FxHashMap<u64, Vec<u64>>,
        roots: &[u64],
        workers: usize,
    ) -> (Arc<TestMarkContext>, Arc<ZMarkCoordinator>) {
        let ctx = Arc::new(TestMarkContext::new(g));
        let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), workers));
        pool.begin_cycle();
        pool.push_roots(roots);
        (ctx, pool)
    }

    // -- lifecycle ----------------------------------------------------------

    /// The driver spawns, marks the reachable set, and joins cleanly.
    ///
    /// The replacement for the simulation-era
    /// `zgc_concurrent_mark_thread_spawns_and_joins`, which could only assert
    /// that a thread started and stopped — there was no object graph for it
    /// to mark. This one asserts the mark set.
    #[test]
    fn driver_marks_the_reachable_set_and_joins_cleanly() {
        let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4, 5]), (4, &[]), (5, &[])]);
        let (ctx, pool) = open_cycle(g, &[1], 2);

        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint.clone(),
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete, "{outcome:?}");
        assert_eq!(outcome.passes, 1, "a quiet cycle needs exactly one pass");
        assert_eq!(outcome.restarts, 0);
        assert_eq!(outcome.redrains, 0);
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
        assert!(
            safepoint.balanced(),
            "every begin_mark_end_safepoint must be paired with an end"
        );
        assert_eq!(safepoint.flushes.load(Ordering::Relaxed), 1);
    }

    /// **The driver's fixed-point wait is NOTIFIED, not polled.**
    ///
    /// # Why this is a count and not a timing
    ///
    /// The failure it guards is invisible by construction. Every wait in the
    /// marker is a `wait_for` and never a bare `wait`, so a missing notification
    /// does not hang — it costs a poll interval and behaves in every other way
    /// exactly like a working one. That is what was true here until 2026-08-17:
    /// the driver waited on `ZgcConcurrentMarkState`'s own condvar, which no ZGC
    /// path ever notified for the completion case, so every pass of every cycle
    /// ran the full 5 ms out before noticing a fixed point the workers had
    /// already reached. A wall-clock assertion could catch that and would be the
    /// wrong instrument: flaky on a loaded machine, and silent about why.
    ///
    /// # Why the mark is deliberately made SLOW
    ///
    /// `park_timeouts == 0` would be satisfied by a wait that was never entered,
    /// and that is exactly what a fast mark produces — the driver arrives after
    /// the workers have converged and takes the `is_terminated` fast path. So the
    /// hook holds the first visit for 37 ms, which guarantees the driver is
    /// inside the wait, and the assertion is on
    /// `park_termination_wakes`: the wait was released by the termination edge
    /// itself. Timeouts *during* those 37 ms are expected and are not asserted
    /// on.
    ///
    /// 37 and not 35 or 40: the interval is 5 ms, so a sleep at a multiple of it
    /// puts termination and a timeout in the same microseconds, and the test
    /// would flake on which won. 37 leaves the driver 3 ms into a fresh wait when
    /// the mark converges.
    #[test]
    fn the_drivers_fixed_point_wait_is_woken_by_the_termination_edge() {
        let g = graph(&[(1, &[2, 3]), (2, &[4]), (3, &[4, 5]), (4, &[]), (5, &[])]);
        let slept = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&slept);
        let ctx = Arc::new(TestMarkContext::new(g).with_visit_hook(Box::new(move |_| {
            // FIRST VISIT ONLY, so the mark takes ~37 ms rather than 37 ms per
            // object -- the point is to be slower than one poll interval, not to
            // be slow.
            if !s2.swap(true, Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(37));
            }
        })));
        let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), 2));
        pool.begin_cycle();
        pool.push_roots(&[1]);

        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));
        let outcome = controller.join_cycle().expect("driver joined");

        assert!(outcome.mark_set_complete, "{outcome:?}");
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3, 4, 5]);
        assert!(
            slept.load(Ordering::Relaxed),
            "the hook must have run, or the mark was not slowed and the driver \
             may never have entered its wait"
        );
        assert!(
            pool.shared().terminator().park_termination_wakes() >= 1,
            "the driver's wait must be released by the TERMINATION EDGE, not by a \
             timeout. Zero here is the 5 ms-per-pass floor coming back, and it \
             would otherwise show up only as a slower pause that somebody has to \
             attribute. timeouts={}",
            pool.shared().terminator().park_timeouts()
        );
        pool.end_cycle();
    }

    /// A cycle with no roots converges without marking anything, and the
    /// driver still publishes a well-formed outcome.
    #[test]
    fn an_empty_root_set_converges_in_one_pass() {
        let (ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[])]), &[], 3);
        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete);
        assert_eq!(outcome.passes, 1);
        assert_eq!(ctx.marked_count(), 0);
    }

    // -- the restart loop ---------------------------------------------------

    /// **The reason this module exists.** A mutator mark that arrives at the
    /// mark-end pause must force the concurrent phase to run again, and the
    /// object's whole closure must end up marked.
    ///
    /// Under SATB this cannot happen: the remark pause quiesces the only
    /// producer, so one pass is always enough. Under ZGC the mutators *are*
    /// producers until they are stopped, so the pause is a decision point.
    /// The scripted safepoint publishes `50` during the first flush, which is
    /// exactly the interleaving the restart loop exists for.
    #[test]
    fn a_mark_arriving_at_the_mark_end_pause_forces_a_restart() {
        let g = graph(&[
            (1, &[2]),
            (2, &[]),
            // Reachable only through the simulated load barrier.
            (50, &[51, 52]),
            (51, &[53]),
            (52, &[]),
            (53, &[]),
        ]);
        let (ctx, pool) = open_cycle(g, &[1], 2);

        let safepoint = Arc::new(ScriptedSafepoint::new(vec![50]));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint.clone(),
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete, "{outcome:?}");
        assert_eq!(
            outcome.restarts, 1,
            "the mark-end probe must have seen the mutator's mark exactly once"
        );
        assert_eq!(
            outcome.passes, 2,
            "one restart means two concurrent phases ran"
        );
        assert_eq!(
            ctx.marked_sorted(),
            vec![1, 2, 50, 51, 52, 53],
            "the mutator's object AND its transitive closure must be marked"
        );
        assert_eq!(
            safepoint.flushes.load(Ordering::Relaxed),
            2,
            "one flush per mark-end pause"
        );
        assert!(safepoint.balanced());
        assert!(
            pool.stats().mark_end_restarts.load(Ordering::Relaxed) >= 1,
            "the engine must have counted the restart too"
        );
    }

    /// Several late marks in a row are each folded in; the loop iterates as
    /// many times as it needs to and still terminates.
    #[test]
    fn successive_late_marks_each_force_another_pass() {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        g.insert(1, Vec::new());
        let mut expected = vec![1u64];
        let mut script = Vec::new();
        for i in 0..5u64 {
            let a = 200 + i * 2;
            let b = 201 + i * 2;
            g.insert(a, vec![b]);
            g.insert(b, Vec::new());
            expected.push(a);
            expected.push(b);
            script.push(a);
        }
        // Popped from the back: the order does not matter, only the count.
        let (ctx, pool) = open_cycle(g, &[1], 3);

        let safepoint = Arc::new(ScriptedSafepoint::new(script));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete, "{outcome:?}");
        assert_eq!(outcome.restarts, 5);
        assert_eq!(outcome.passes, 6);
        expected.sort_unstable();
        assert_eq!(ctx.marked_sorted(), expected);
    }

    /// The ceiling trips instead of looping forever.
    ///
    /// The script publishes a fresh object at **every** mark-end pause, so
    /// the fixed point is never final and an unbounded loop would never
    /// return — which is how a GC livelock presents to a user. With
    /// `max_mark_end_restarts = 2` the driver gives up on the third restart,
    /// logs a `tracing::warn!`, and — the part that actually matters —
    /// reports `mark_set_complete == false` so the caller cannot mistake the
    /// partial mark set for a sweepable one.
    #[test]
    fn the_restart_ceiling_trips_instead_of_looping_forever() {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        g.insert(1, Vec::new());
        // Far more injections than the budget allows, so the loop can only
        // end by hitting the ceiling.
        let mut script = Vec::new();
        for i in 0..64u64 {
            let a = 300 + i;
            g.insert(a, Vec::new());
            script.push(a);
        }
        let (_ctx, pool) = open_cycle(g, &[1], 2);

        let safepoint = Arc::new(ScriptedSafepoint::new(script));
        let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint.clone())
            .with_max_mark_end_restarts(2);
        let controller = ZgcConcurrentMarkController::spawn(params);

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(
            outcome.restart_budget_exhausted,
            "the ceiling must trip: {outcome:?}"
        );
        assert!(
            !outcome.mark_set_complete,
            "an exhausted budget leaves the mark set INCOMPLETE; reporting it as \
             complete would be a use-after-free waiting to happen"
        );
        assert_eq!(
            outcome.restarts, 3,
            "the budget is exceeded on the restart AFTER the budget'th one"
        );
        assert_eq!(outcome.passes, 3);
        assert!(!outcome.stopped_early);
        assert!(safepoint.balanced());
    }

    // -- reference processing ----------------------------------------------

    /// A resurrected referent drags an unscanned subgraph behind it, so the
    /// driver must re-drain after the hook — and must stop once the hook has
    /// nothing left to resurrect.
    #[test]
    fn a_resurrected_referent_triggers_a_redrain_of_its_closure() {
        let g = graph(&[
            (1, &[2]),
            (2, &[]),
            // Softly-reachable island: strongly unreachable, so the strong
            // mark cannot find it.
            (60, &[61]),
            (61, &[62]),
            (62, &[]),
        ]);
        let (ctx, pool) = open_cycle(g, &[1], 2);

        let hook = Arc::new(KeepAliveHook {
            candidates: vec![2, 60],
            observed_live: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
        });
        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint)
            .with_refs(hook.clone() as Arc<dyn ZNonStrongRefHook + Send + Sync>);
        let controller = ZgcConcurrentMarkController::spawn(params);

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete, "{outcome:?}");
        assert_eq!(
            outcome.resurrected, 1,
            "only 60 was dead; 2 was already live"
        );
        assert_eq!(
            outcome.redrains, 1,
            "the resurrection must force a re-drain"
        );
        assert_eq!(
            outcome.passes, 2,
            "one strong pass, then one more to trace the resurrection"
        );
        assert_eq!(
            ctx.marked_sorted(),
            vec![1, 2, 60, 61, 62],
            "a resurrected referent drags its whole subgraph with it"
        );
        assert_eq!(
            hook.calls.load(Ordering::Relaxed),
            2,
            "the hook runs again after the re-drain, and only then reports nothing \
             left to resurrect"
        );
        assert_eq!(
            hook.observed_live.lock().clone(),
            vec![2, 2, 60],
            "round 1 saw only the strongly-live 2; round 2 saw 2 and the now-live 60 \
             — the hook must never be told an object is live merely because it \
             itself just marked it"
        );
    }

    /// With no hook the phase is skipped entirely and the cycle is a single
    /// pass. Guards against a future refactor that runs an empty reference
    /// phase and pays a safepoint for it.
    #[test]
    fn no_reference_hook_means_no_reference_safepoint() {
        let (_ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[])]), &[1], 2);
        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint.clone(),
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete);
        assert_eq!(outcome.redrains, 0);
        assert_eq!(
            safepoint.entered.load(Ordering::Relaxed),
            1,
            "exactly one safepoint: the mark-end pause"
        );
    }

    /// A hook that resurrects without ever converging must hit the round
    /// ceiling rather than spinning forever.
    #[test]
    fn the_resurrection_ceiling_trips_instead_of_looping_forever() {
        /// Resurrects a *fresh* object every round, so the fixed point of
        /// "nothing left to resurrect" is never reached. A real hook cannot
        /// do this (marking is monotone), which is exactly why the bound is a
        /// guard against a broken hook rather than a policy knob.
        struct NeverConvergingHook {
            next: AtomicUsize,
        }
        impl ZNonStrongRefHook for NeverConvergingHook {
            fn process(&self, _is_marked: &dyn Fn(u64) -> bool, keep_alive: &mut dyn FnMut(u64)) {
                let i = self.next.fetch_add(1, Ordering::Relaxed);
                keep_alive(400 + i as u64);
            }
        }

        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        g.insert(1, Vec::new());
        for i in 0..64u64 {
            g.insert(400 + i, Vec::new());
        }
        let (_ctx, pool) = open_cycle(g, &[1], 2);

        let hook = Arc::new(NeverConvergingHook {
            next: AtomicUsize::new(0),
        });
        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let params = ZgcConcurrentMarkParams::new(Arc::clone(&pool), safepoint.clone())
            .with_refs(hook as Arc<dyn ZNonStrongRefHook + Send + Sync>)
            .with_max_resurrection_rounds(3);
        let controller = ZgcConcurrentMarkController::spawn(params);

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(
            outcome.resurrection_budget_exhausted,
            "the round ceiling must trip: {outcome:?}"
        );
        assert!(!outcome.mark_set_complete);
        assert_eq!(outcome.redrains, 4, "budget 3 means the 4th round gives up");
        assert!(safepoint.balanced());
    }

    // -- shutdown -----------------------------------------------------------

    /// Stopping the controller while marking is in flight must not hang.
    ///
    /// The pool is deliberately wedged: a visit hook holds a worker inside
    /// `visit_refs`, so the fixed point cannot be reached and the driver is
    /// parked in `await_fixed_point`. `request_stop_and_join` must still
    /// return — the driver's wait is bounded and it re-reads the stop flag every
    /// interval.
    ///
    /// **This doc used to end "if it waited on the pool's own condvar instead,
    /// this would deadlock, which is the whole reason `await_fixed_point`
    /// polls". That was wrong, and since 2026-08-17 the driver DOES wait on the
    /// terminator's condvar** — so this test is now the proof of the change
    /// rather than the reason against it. It cannot deadlock, because
    /// `ZMarkTerminator::wait_for_fixed_point` takes the *caller's* stop flag as
    /// a parameter and re-checks it under a bounded `wait_for`. See
    /// `_DRIVER_POLL_MS_RETIRED` for the API confusion that produced the
    /// original claim.
    ///
    /// Nothing here asserts on elapsed time; the test simply cannot pass
    /// without the join returning.
    #[test]
    fn stopping_the_driver_mid_cycle_does_not_hang() {
        let mut g: FxHashMap<u64, Vec<u64>> = FxHashMap::default();
        let mut children = Vec::new();
        for i in 0..64u64 {
            let c = 500 + i;
            g.insert(c, Vec::new());
            children.push(c);
        }
        g.insert(1, children);

        let release = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicUsize::new(0));
        let hook_release = Arc::clone(&release);
        let hook_entered = Arc::clone(&entered);
        let ctx = Arc::new(
            TestMarkContext::new(g).with_visit_hook(Box::new(move |addr: u64| {
                if addr != 1 {
                    return;
                }
                hook_entered.fetch_add(1, Ordering::SeqCst);
                // Bounded spin, exactly as `zgc::mark`'s own interleaving
                // tests do it: if the wedge does not materialise on this run
                // the test still checks that the join returns, it just does
                // not exercise the mid-cycle case that time.
                let mut spins: u64 = 0;
                while !hook_release.load(Ordering::Acquire) && spins < 20_000_000 {
                    spins += 1;
                    std::thread::yield_now();
                }
            })),
        );

        let pool = Arc::new(ZMarkCoordinator::new(ctx.clone(), 2));
        pool.begin_cycle();
        pool.push_roots(&[1]);

        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));

        // Wait (bounded, unasserted) until a worker is actually wedged inside
        // `visit_refs`, so the stop really does land mid-cycle.
        let mut spins: u64 = 0;
        while entered.load(Ordering::SeqCst) == 0 && spins < 20_000_000 {
            spins += 1;
            std::thread::yield_now();
        }

        controller
            .request_stop_and_join()
            .expect("driver must join after a stop request");

        // Let the wedged worker out so the pool can be torn down.
        release.store(true, Ordering::Release);
        pool.end_cycle();
        // The pool's own `Drop` stops and joins the workers when this last
        // clone goes; doing it explicitly keeps the teardown in the test.
        drop(pool);
    }

    /// Dropping the controller without joining must not hang either, and must
    /// leave the detached driver able to finish on its own.
    #[test]
    fn dropping_the_controller_requests_stop_without_blocking() {
        let (_ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[3]), (3, &[])]), &[1], 2);
        let safepoint = Arc::new(ScriptedSafepoint::new(Vec::new()));
        let state = {
            let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
                Arc::clone(&pool),
                safepoint,
            ));
            let state = Arc::clone(&controller.state);
            // Dropped here: flips the stop flag and detaches.
            state
        };
        assert!(
            state.should_stop.load(Ordering::Acquire),
            "Drop must request a stop"
        );
        pool.end_cycle();
    }

    // -- seam sanity --------------------------------------------------------

    /// The no-mutator safepoint is a legitimate `ZgcMarkSafepoint`: it stops
    /// nothing and flushes nothing, which is correct precisely when there is
    /// no other mutator. Pins the default so an accidental change to it is a
    /// test failure rather than a silent unsoundness in single-threaded
    /// embeddings.
    #[test]
    fn the_no_mutator_safepoint_drives_a_cycle() {
        let (ctx, pool) = open_cycle(graph(&[(1, &[2]), (2, &[3]), (3, &[])]), &[1], 1);
        let safepoint: Arc<dyn ZgcMarkSafepoint> = Arc::new(ZgcNoMutatorSafepoint);
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));

        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert!(outcome.mark_set_complete);
        assert_eq!(outcome.passes, 1);
        assert_eq!(ctx.marked_sorted(), vec![1, 2, 3]);
    }

    /// Telemetry on the shared state tracks the outcome. Guards the accessor
    /// surface a caller polls while the cycle is still running.
    #[test]
    fn state_telemetry_matches_the_cycle_outcome() {
        let g = graph(&[(1, &[2]), (2, &[]), (70, &[71]), (71, &[])]);
        let (_ctx, pool) = open_cycle(g, &[1], 2);
        let safepoint = Arc::new(ScriptedSafepoint::new(vec![70]));
        let controller = ZgcConcurrentMarkController::spawn(ZgcConcurrentMarkParams::new(
            Arc::clone(&pool),
            safepoint,
        ));

        let state = Arc::clone(&controller.state);
        let outcome = controller.join_cycle().expect("driver joined");
        pool.end_cycle();

        assert_eq!(
            state.passes_performed.load(Ordering::Relaxed),
            outcome.passes as u64
        );
        assert_eq!(
            state.restarts_performed.load(Ordering::Relaxed),
            outcome.restarts as u64
        );
        assert_eq!(state.outcome(), Some(outcome));
    }
}
