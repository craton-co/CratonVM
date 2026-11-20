// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent-mark thread controller for G1 (task #54).
//!
//! Splits the G1 mark phase into:
//!
//! 1. **STW initial mark** — `G1Collector::start_concurrent_mark` followed
//!    by `remark(initial_roots)` queues the root set as the gray frontier.
//!    Caller must hold the STW token from task #26 (or equivalent).
//!
//! 2. **Concurrent mark** — `ConcurrentMarkController::spawn` launches a
//!    background thread that repeatedly calls `concurrent_mark_step`,
//!    draining the gray queue while mutators run. The SATB pre-barrier
//!    (tasks #25/#42/#43) keeps the queue refilled as mutators overwrite
//!    references. **No STW token is required for this phase** — the
//!    coordination invariant is "mutators race the marker; the SATB log
//!    captures any reference the marker has not yet seen".
//!
//! 3. **STW remark** — `request_stop_and_join` waits for the worker to
//!    quiesce, the caller re-acquires the STW token, drains any remaining
//!    SATB log via `g1.remark(stw, roots)`, and finishes with `g1.cleanup(stw)`.
//!
//! ## Termination
//!
//! The worker loop runs until BOTH conditions hold:
//!
//! - `mark_worklist` is empty (no gray pointers).
//! - `satb_queue` is empty OR the worker is told to stop (the STW remark
//!   will drain any straggler SATB entries with full mutator coordination).
//!
//! When both hold, the worker parks on the `done_cvar` until either:
//!
//! - A new gray pointer is pushed (via SATB drain → worklist) and the
//!   coordinator calls `notify_work_available`; OR
//! - `request_stop()` flips `should_stop` and the coordinator calls
//!   `notify_work_available` to release the park.
//!
//! ## Thread lifecycle
//!
//! ```text
//!   start_concurrent_mark (STW)
//!         │
//!         │  ConcurrentMarkController::spawn(g1_arc)
//!         ▼
//!   ConcurrentMark   ◄── mutators live, SATB barrier active
//!         │
//!         │  worker loop:
//!         │    while !should_stop { step(BUDGET) }
//!         │
//!         │  request_stop_and_join (STW)
//!         ▼
//!   Remark (STW)  →  cleanup → Idle
//! ```
//!
//! ## Scope notes
//!
//! - ZGC has an analogous `concurrent_mark` entry point in `zgc.rs:654`,
//!   but ZGC is a stubbed simulation (no real backing storage; see the
//!   long comment in `zgc::concurrent_relocate`). Adding a background
//!   thread there would not exercise the SATB barrier any differently
//!   than the synchronous call. **TODO(task #54, ZGC):** when ZGC moves
//!   off the simulation to real backing pages, mirror this controller —
//!   the API (spawn / request_stop / join) is intentionally narrow so it
//!   can be reused.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::g1::G1Collector;

/// How many gray pointers the worker drains per `concurrent_mark_step`
/// call before yielding to its loop body (which checks `should_stop`).
/// Chosen small enough that a stop request is observed within microseconds
/// even on a deep object graph, large enough to amortise the per-step
/// region-lock acquisition.
const WORKER_STEP_BUDGET: usize = 256;

/// Polling interval the worker uses when it observes an empty gray set
/// but has not been told to stop. Park-with-timeout so SATB pushes that
/// race past the `notify_work_available` signal still get picked up.
///
/// Ten-findings item 9b: this was 5 ms, and it was the PRIMARY wake — nothing
/// on the mutator side notified a parked worker when a SATB buffer spilled,
/// so a parked marker took the region guard and the SATB shards 200 times a
/// second for the whole cycle to discover work it was never told about. The
/// collector now wakes the workers itself (`G1Collector::wake_marker`, from
/// the SATB spill, `push_gray_or_mark` and `remark`), and this is the
/// fallback it was documented as.
pub const WORKER_POLL_MS: u64 = 250;

/// Shared state between the coordinator (typically the VM thread that
/// initiated the GC cycle) and the background mark worker.
///
/// Held inside `Arc<>` so the worker thread keeps it alive even after
/// the coordinator transiently drops its handle.
pub struct ConcurrentMarkState {
    /// Set by `request_stop`; the worker exits its loop on next poll.
    pub should_stop: AtomicBool,
    /// Set by the worker when it reaches a marking fixed point (the worklist
    /// drained AND no overflow rescan pending) and parks; cleared when work is
    /// pushed (`notify_work_available`) or the worker resumes stepping. This is
    /// the completion signal the coordinator polls — the worker PARKS rather
    /// than EXITS at a fixed point (the cycle ends only via `request_stop`), so
    /// "worker exited" (`!is_running()`) would deadlock: the worker never exits
    /// on its own, and `request_stop` is only issued after completion is
    /// detected. Quiescence ("drained to fixed point") is the correct signal.
    pub quiesced: AtomicBool,
    /// Telemetry: number of `concurrent_mark_step` calls performed.
    pub steps_performed: AtomicU64,
    // LANE W2-C (2026-09-20) removed `work_units_done`, whose doc said "total
    // gray pointers processed" and whose `worker_loop` credited
    // `WORKER_STEP_BUDGET` after every step regardless of what that step
    // scanned. A step that parked immediately on an empty gray set credited 256
    // objects it never looked at, so the field was exactly
    // `steps_performed * 256` and anything reading it as throughput read a
    // number with no information in it beyond the one beside it.
    //
    // Deleted rather than corrected: nothing in the workspace read it, and the
    // real per-worker scan count already exists and is already exported —
    // `G1Collector::dbg_mark_worker_scans_public()`, which
    // `concurrent_mark_step_worker` maintains from the actual budget it spent.
    // Two counters for one quantity, one of them wrong, is how the wrong one
    // gets cited.
    /// Park lock: a worker holds this while parking on `done_cvar`.
    /// The mutex protects the parked COUNT — the coordinator must reacquire
    /// the lock before signalling so a wake notification is not lost.
    ///
    /// F-12 turned this from a bool into a count, because there are now N
    /// workers and any subset of them may be parked. Nothing depends on the
    /// count's exact value (the park is bounded by `WORKER_POLL_MS` regardless);
    /// it exists so `notify_work_available` can skip the wake when nobody is
    /// waiting, which is the whole point of the fast path.
    pub parked: Mutex<usize>,
    /// Wake condition: coordinator notifies after pushing to the
    /// worklist OR after setting `should_stop`.
    pub done_cvar: Condvar,
    /// Lane-C — monotone count of work PUBLICATIONS (every
    /// [`Self::notify_work_available`] bumps it, before it touches `parked`).
    ///
    /// This is the sequence number that makes the park decision race-free: a
    /// worker samples it before its step and refuses to park if it has moved
    /// by the time it holds the `parked` lock. See the guard in
    /// [`Self::park_for_work`] for the interleaving it rules out. It is NOT a
    /// count of gray entries and nothing may treat it as one — a publication
    /// that pushed nothing (a drain that found the shards empty) bumps it just
    /// the same, and the only cost of that is one extra empty step.
    pub work_epoch: AtomicU64,

    // --- lane W2-C: a convergence signal that describes the CYCLE ---
    //
    // `quiesced` above is one `AtomicBool` shared by every worker, so it
    // records whatever the LAST worker to write it thought — worker A publishes
    // "converged" and worker B clears it a microsecond later on its way back
    // from a park, while A is still parked and the gray set is still empty.
    // The direction is safe (a spurious `false` only delays completion) but it
    // means the coordinator's completion poll can be told "not yet" by a worker
    // that has nothing to say about the cycle, and the F-12 test
    // `a_deep_chain_is_fully_marked_before_the_markers_report_convergence`
    // asserts something empirical rather than something structural.
    //
    // gci i42/mk: the bool can ALSO read a stale `true`, which the sentence
    // above missed. A worker whose step found the gray set empty BEFORE a
    // publication stores `true` AFTER that publication's `notify_work_available`
    // stored `false` (nothing orders the two stores), and it stays `true` until
    // the worker's park returns at once on the moved `work_epoch` and clears
    // it. A completion poll inside that window starts the final remark over
    // unscanned gray work. Not a liveness or safety hole -- the remark drains
    // the gray set to a fixed point inside its pause
    // (`VmHeap::g1_final_remark_and_cleanup`) -- only pause time. The epoch arm
    // below cannot make that mistake: the converger's epoch was sampled before
    // its step, so the publication's bump fails `is_quiesced`'s equality.
    //
    // These two make it a statement about the cycle: EVERY worker is at a fixed
    // point, and no work has been published since the step that found it. See
    // `ConcurrentMarkController::is_quiesced` for how they are read together
    // and `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` for the switch.
    //
    // This is the measurable core of proposal P1, NOT the whole of it: the
    // `WORKER_POLL_MS` fallback park and the per-step `gray_set_is_empty()`
    // probe both stay, because removing either changes the liveness of the
    // protocol rather than the accuracy of its signal, and neither can be
    // soaked inside this wave.
    /// How many workers are currently parked at a fixed point.
    pub converged_workers: AtomicUsize,
    /// The `work_epoch` the LAST worker to converge had sampled BEFORE its
    /// final step.
    ///
    /// Sampled-before rather than read-at-convergence, and that is the whole
    /// correctness of it: a publication that lands DURING a worker's last step
    /// bumps `work_epoch` past this value, so `is_quiesced` reports false for
    /// work the converging worker could not have seen. Reading the live epoch
    /// at convergence instead would record the publisher's new value and
    /// declare a fixed point over gray work nobody has scanned.
    pub converged_epoch: AtomicU64,
}

impl ConcurrentMarkState {
    fn new() -> Self {
        Self {
            should_stop: AtomicBool::new(false),
            quiesced: AtomicBool::new(false),
            steps_performed: AtomicU64::new(0),
            parked: Mutex::new(0),
            done_cvar: Condvar::new(),
            work_epoch: AtomicU64::new(0),
            // --- lane W2-C --- nobody has converged yet, and epoch 0 is the
            // value `work_epoch` starts at, so a fresh state must NOT read as
            // quiesced — `converged_workers == 0 != worker_count` is what says so.
            converged_workers: AtomicUsize::new(0),
            converged_epoch: AtomicU64::new(u64::MAX),
        }
    }

    /// Coordinator → worker: more gray pointers became available (the
    /// SATB log was drained into the worklist, or a young GC re-pushed
    /// entries via the fix in `young_collection`).
    ///
    /// Cheap fast-path: if the worker is not parked, this is a single
    /// atomic load + nothing else. Only acquires the mutex when a wake
    /// is actually needed.
    pub fn notify_work_available(&self) {
        // Lane-C — publish FIRST, and before the `parked` lock: a worker that
        // sampled the old value is then guaranteed to see this bump under the
        // lock and refuse to park. See `park_for_work`.
        self.work_epoch.fetch_add(1, Ordering::Release);
        // New work means the marker is no longer at a fixed point: clear the
        // quiescence signal so the coordinator's completion poll doesn't fire
        // while there are unprocessed gray pointers (e.g. freshly-seeded roots
        // or SATB entries).
        self.quiesced.store(false, Ordering::Release);
        // Acquire the mutex so the wake races correctly with a worker
        // that was about to park: parking_lot's pattern is "lock, set
        // flag, wait" so we must lock to observe a consistent flag.
        let parked = self.parked.lock();
        if *parked > 0 {
            // F-12 — `notify_all`, not `notify_one`. With several workers a
            // single wake leaves the rest asleep on a gray set that now has
            // work in it, and they would only find it on the 5 ms poll. A
            // spurious wake costs one empty step; a missed one costs latency on
            // every remaining worker.
            self.done_cvar.notify_all();
        }
    }

    /// Coordinator → worker: stop ASAP.
    pub fn request_stop(&self) {
        self.should_stop.store(true, Ordering::Release);
        // Also kick the cvar — every parked worker must wake to observe the
        // stop request.
        let _parked = self.parked.lock();
        self.done_cvar.notify_all();
    }

    /// Worker: park until either work appears or stop is requested.
    /// Bounded by `WORKER_POLL_MS` so a missed notification (shouldn't
    /// happen given the lock discipline above, but defensive) still
    /// makes progress.
    /// LANE W2-C — this worker has reached a fixed point and is about to park.
    ///
    /// `sampled_epoch` is the `work_epoch` value read BEFORE the step that
    /// converged, never the live one; see [`Self::converged_epoch`].
    fn note_converged(&self, sampled_epoch: u64) {
        // The epoch is published BEFORE the count, so a reader that observes a
        // full count also observes the epoch that goes with it. The reverse
        // order would let `is_quiesced` pair a full count with the PREVIOUS
        // converger's epoch.
        self.converged_epoch.store(sampled_epoch, Ordering::Release);
        self.converged_workers.fetch_add(1, Ordering::AcqRel);
    }

    /// LANE W2-C — this worker has woken and is about to step again.
    fn note_resumed(&self) {
        // Saturating in spirit: `fetch_update` rather than a bare `fetch_sub`
        // so a count that has somehow reached zero cannot wrap to `usize::MAX`
        // and make `is_quiesced` report a fixed point forever. This is the
        // signal an STW pause waits on, so the failure it would cause is a
        // pause that proceeds over unscanned gray work — a counted floor, not
        // a `debug_assert!`, exactly as the rest of this module does it.
        let _ = self
            .converged_workers
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                Some(n.saturating_sub(1))
            });
    }

    fn park_for_work(&self, observed_epoch: u64) {
        let mut parked = self.parked.lock();
        if self.should_stop.load(Ordering::Acquire) {
            return;
        }
        // LOST-WAKEUP GUARD (lane C).
        //
        // `notify_work_available` does two things in this order: publish the
        // new work, then take THIS lock and wake whoever is counted in it. The
        // worker does the mirror image: decide it has nothing to do, then take
        // this lock and park. Nothing serialises the two halves, so a
        // coordinator whose `parked.lock()` wins the race observes the count as
        // ZERO — the worker has not incremented it yet — and skips the wake
        // entirely. The worker then parks on gray work that already exists and
        // does not learn about it until `WORKER_POLL_MS` expires.
        //
        // That is a quarter of a second of a marking worker doing nothing per
        // occurrence, and the occurrence is not rare: the window is exactly the
        // instant a marker runs dry, which is the instant a coordinator seeding
        // the gray set (remark, a pause's keep-alive, an SATB spill) is most
        // likely to arrive. The whole point of item 9b's waker was to stop the
        // marker learning about work from the fallback poll.
        //
        // `observed_epoch` closes it. The caller reads `work_epoch` BEFORE its
        // step; every publisher bumps it before touching this lock. Re-read it
        // under the lock and either the bump landed — we return and step again
        // over work that provably exists — or it has not yet reached the lock,
        // in which case the publisher will find our incremented count and wake
        // us. There is no third interleaving.
        //
        // Deliberately NOT `quiesced`: that flag is shared by every worker, so
        // a PEER that is still stepping clears it, and a guard keyed on it
        // would turn "park until woken" into a spin for as long as any peer has
        // work. `work_epoch` is monotone and per-publication, so it answers the
        // question this worker is actually asking.
        if self.work_epoch.load(Ordering::Acquire) != observed_epoch {
            return;
        }
        *parked += 1;
        // wait_for: returns either on notify or on timeout. In either
        // case we re-check `should_stop` in the loop body.
        let _ = self
            .done_cvar
            .wait_for(&mut parked, Duration::from_millis(WORKER_POLL_MS));
        *parked = parked.saturating_sub(1);
    }
}

impl Default for ConcurrentMarkState {
    fn default() -> Self {
        Self::new()
    }
}

/// Coordinator-side handle to a spawned concurrent-mark worker.
///
/// Drop semantics: dropping the controller without calling
/// `request_stop_and_join` requests stop and detaches the worker. The
/// worker will exit on its next poll. In tests we always `join` for
/// determinism, but production code can rely on the Drop impl as a
/// safety net.
pub struct ConcurrentMarkController {
    pub state: Arc<ConcurrentMarkState>,
    /// F-12 — one handle per marking worker. Was a single `Option<JoinHandle>`;
    /// `G1Collector::mark_worker_count()` decides how many there are, and a
    /// worker's index in this vector is the deque index it owns for its life.
    handles: Vec<JoinHandle<()>>,
    /// Item 9b — kept so the wake handle installed at `spawn` can be
    /// uninstalled when the cycle's workers are stopped.
    g1: Arc<G1Collector>,
}

impl ConcurrentMarkController {
    /// Spawn the background mark thread. **Caller responsibility**: the
    /// STW initial-mark phase must have completed already (i.e. the
    /// initial root set has been pushed onto the mark worklist via
    /// `G1Collector::remark`). The worker assumes the worklist is
    /// already seeded; it will park immediately if it isn't.
    ///
    /// The `g1` argument is an `Arc<G1Collector>` so the worker thread
    /// can outlive any single coordinator-side reference. The `'static`
    /// bound on `spawn` is satisfied by the Arc.
    pub fn spawn(g1: Arc<G1Collector>) -> Self {
        let state = Arc::new(ConcurrentMarkState::new());
        // Item 9b — let the collector wake these workers directly.
        g1.install_mark_waker(Arc::clone(&state));
        // F-12 — one thread per gray deque. The collector fixed the count at
        // construction (`concurrent_mark_worker_count`, a quarter of the
        // evacuation width, or 1 under `CRATONVM_G1_PARALLEL_MARK=0`), because
        // a worker's id indexes the deque array.
        let workers = g1.mark_worker_count();
        let handles = (0..workers)
            .map(|id| {
                let worker_state = Arc::clone(&state);
                let worker_g1 = Arc::clone(&g1);
                std::thread::Builder::new()
                    // Numbered so a stuck cycle is identifiable in a native
                    // stack dump without cross-referencing thread ids, the same
                    // way the evacuation pool names its workers.
                    .name(format!("g1-concurrent-mark-{id}"))
                    .spawn(move || {
                        Self::worker_loop(worker_g1, worker_state, id);
                    })
                    .expect("g1-concurrent-mark thread spawn failed")
            })
            .collect();

        Self { state, handles, g1 }
    }

    /// Body of the background mark thread.
    ///
    /// Loop invariant:
    /// - On entry to each iteration, neither the worklist state nor the
    ///   stop flag have been observed for this iteration.
    /// - On exit (return), `should_stop` was observed `true` AND the
    ///   loop performed one final drain attempt so any straggler the
    ///   coordinator pushed before signalling stop is processed.
    fn worker_loop(g1: Arc<G1Collector>, state: Arc<ConcurrentMarkState>, id: usize) {
        loop {
            // Lane-C — sample the publication counter BEFORE the step, so any
            // work published from here on prevents the park below. Sampling it
            // after the step would reopen the very window the guard closes.
            let epoch = state.work_epoch.load(Ordering::Acquire);
            // Drain this worker's share under the current budget.
            // `concurrent_mark_step_worker` returns true iff the WHOLE gray set
            // (seed queue and every deque) is empty, no peer is mid-scan, and
            // no overflow rescan is pending — i.e. a global fixed point.
            let cpu = cratonvm_gc_core::thread_cpu::CpuSpan::begin();
            let drained = {
                let _phase =
                    cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcConcurrent);
                g1.concurrent_mark_step_worker(id, WORKER_STEP_BUDGET)
            };
            cpu.end(g1.conc_mark_cpu_counter());
            state.steps_performed.fetch_add(1, Ordering::Relaxed);

            // Check stop signal between every step so a coordinator
            // calling request_stop sees the worker quiesce promptly.
            if state.should_stop.load(Ordering::Acquire) {
                // One last drain pass: the coordinator may have pushed
                // SATB entries onto the worklist between our last step
                // and the stop signal. Honour them so the STW remark
                // has less work to do.
                let _phase =
                    cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcConcurrent);
                let _ = g1.concurrent_mark_step_worker(id, WORKER_STEP_BUDGET);
                return;
            }

            if drained {
                // Worklist truly empty (fixed point). Publish quiescence so the
                // coordinator's completion poll (`is_quiesced`) can observe that
                // marking has converged, THEN park. We do NOT exit the loop
                // here: the cycle ends only via `request_stop`, which is the STW
                // coordinator's prerogative (it owns the transition to Remark).
                state.quiesced.store(true, Ordering::Release);
                // LANE W2-C — and the cycle-scoped half of the same statement.
                // `epoch` is the value sampled BEFORE this step; see
                // `converged_epoch` for why it must not be re-read here.
                //
                // Maintained unconditionally, not behind the flag: the counter
                // has to be correct before anything reads it, and a run that
                // flips `CRATONVM_G1_MARK_CONVERGENCE_EPOCH` must not find a
                // count that only started being kept when the flag was read.
                // Nothing consults it while the flag is off.
                state.note_converged(epoch);
                {
                    let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::Idle);
                    state.park_for_work(epoch);
                }
                // Woken (new work via notify, or the poll timeout): we are about
                // to step again, so we are no longer at a fixed point.
                state.note_resumed();
                state.quiesced.store(false, Ordering::Release);
            } else {
                // Still work to do — definitely not quiesced.
                state.quiesced.store(false, Ordering::Release);
            }
        }
    }

    /// Coordinator: wake the worker if it is parked on an empty worklist.
    /// Call this after pushing new gray pointers (e.g. after a young GC
    /// re-rewrites the worklist or a SATB buffer flush).
    pub fn notify_work_available(&self) {
        self.state.notify_work_available();
    }

    /// Coordinator: signal stop and join the worker. Must be called from
    /// the STW remark phase (caller holds the STW token from task #26).
    /// Returns the join handle's result so an OOM panic in the worker
    /// surfaces here.
    pub fn request_stop_and_join(mut self) -> std::thread::Result<()> {
        self.g1.clear_mark_waker();
        self.state.request_stop();
        // Join EVERY worker, and report the first panic rather than the last:
        // a worker that unwound has left the cycle incomplete, and the
        // coordinator is about to run the STW remark on that basis. Joining
        // them all even after a failure is not optional — a live marker thread
        // touching deques while the pause remaps them is the failure this
        // whole join exists to prevent.
        let mut first_error = None;
        for handle in self.handles.drain(..) {
            if let Err(payload) = handle.join() {
                if first_error.is_none() {
                    first_error = Some(payload);
                }
            }
        }
        match first_error {
            Some(payload) => Err(payload),
            None => Ok(()),
        }
    }

    /// Test/inspection helper: has the worker thread actually started?
    /// Used by the spawn/join test below.
    pub fn is_running(&self) -> bool {
        self.handles.iter().any(|h| !h.is_finished())
    }

    /// True once the worker has marked to a fixed point (worklist drained, no
    /// overflow rescan pending) and is idle — the correct "concurrent marking
    /// has converged" signal for the coordinator's completion poll. The worker
    /// PARKS rather than exits at a fixed point, so `is_running()` would stay
    /// true forever (deadlocking completion); this reflects convergence instead.
    pub fn is_quiesced(&self) -> bool {
        // LANE W2-C — `CRATONVM_G1_MARK_CONVERGENCE_EPOCH=1` reads the
        // cycle-scoped signal instead of the shared bool. Both are maintained
        // on every step; only the reading changes, so the switch is a true
        // single-binary A/B and the off arm is byte-for-byte what it was.
        //
        // What the on arm additionally requires: EVERY worker parked at a fixed
        // point, and no work published since the step that found it. The bool
        // requires neither — it records whichever worker wrote last, so a peer
        // returning from a park clears a convergence that is still true, and
        // the coordinator polls again for nothing.
        //
        // The A/B to run: `g1_mark_parallelism.rs` prints per-worker scans and
        // the cycle's wall time. The on arm should not be SLOWER; if it is, the
        // signal is being cleared by something this reasoning has not accounted
        // for and the flag stays off.
        if cratonvm_gc_core::gc_flags().g1_mark_convergence_epoch {
            let workers = self.handles.len().max(1);
            let converged = self.state.converged_workers.load(Ordering::Acquire);
            if converged < workers {
                return false;
            }
            return self.state.converged_epoch.load(Ordering::Acquire)
                == self.state.work_epoch.load(Ordering::Acquire);
        }
        self.state.quiesced.load(Ordering::Acquire)
    }

    /// Test/inspection helper: how many concurrent-mark steps has the
    /// worker performed since spawn?
    ///
    /// # LANE W6-A — this is the sweep's blind spot, re-checked
    ///
    /// `w3c-instrumentation-audit.md` §6 reports **0** accessors whose only
    /// reader is a `debug!`/`trace!` that `release_max_level_info` compiles
    /// out, and names this one as the nearest miss. Re-checked after three
    /// waves and the shape is unchanged: it has three readers outside tests
    /// and two of them are dead on a release build —
    /// `vm_heap.rs::g1_final_remark_and_cleanup` and
    /// `g1_signal_marking_complete` each bind it to a local that only a
    /// `tracing::debug!` consumes. The third, the `tracing::warn!` in
    /// `g1_start_concurrent_mark`, is the entire reason this accessor is not
    /// dark. Demote that one `warn!` and the counter goes unread with no other
    /// change to any file, and no caller grep would say so — the CALL is
    /// outside the macro, only the VALUE is inside it.
    pub fn steps_performed(&self) -> u64 {
        self.state.steps_performed.load(Ordering::Relaxed)
    }
}

impl Drop for ConcurrentMarkController {
    fn drop(&mut self) {
        // Best-effort cleanup if the user didn't call request_stop_and_join.
        // We can't block on join here (Drop is sync), but we can flip the
        // flag and let the OS reap the thread on exit.
        self.g1.clear_mark_waker();
        self.state.request_stop();
        // Detach every worker: don't block. Production code should call
        // `request_stop_and_join` explicitly.
        self.handles.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
