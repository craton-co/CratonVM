// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent-mark thread controller for ZGC (task #55).
//!
//! Mirrors the G1 `ConcurrentMarkController` shape (see `g1_concurrent.rs`
//! on `dev`; the API is intentionally narrow so the two converge once the
//! ZGC simulation gains real backing pages) but operates on ZGC's
//! colored-pointer mark word instead of G1's mark bitmap + SATB queue.
//!
//! ## Why a parallel implementation rather than reuse
//!
//! The G1 controller is statically bound to `Arc<G1Collector>` because it
//! calls `g1.concurrent_mark_step(BUDGET)` on the collector directly.
//! Making it generic over a "concurrent-markable collector" trait would
//! require pulling `concurrent_mark_step` onto a public trait that both
//! collectors implement; given ZGC is still a simulation (no real backing
//! storage — see `concurrent_relocate`'s long comment in `zgc.rs`) we
//! defer that abstraction. **TODO(converge):** once ZGC moves off the
//! simulation, lift `concurrent_mark_step` into a `ConcurrentMarkable`
//! trait and parameterise both controllers over it.
//!
//! ## Phase split (mirrors G1)
//!
//! 1. **STW initial mark** — caller flips the load barrier's good-color
//!    set to the next mark-color via `ZgcCollector::pause_mark_start`,
//!    which also seeds the mark stack with root page bases. Caller holds
//!    the STW token.
//!
//! 2. **Concurrent mark** — `ZgcConcurrentMarkController::spawn` launches
//!    a background thread that calls `ZgcCollector::concurrent_mark_step`
//!    in a loop with a per-call budget. ZGC's SATB-equivalent is the load
//!    barrier itself: mutators that load a colored pointer with the wrong
//!    color take the slow path, which flips them to the current good
//!    color and effectively re-greys the reference. No STW token required.
//!
//! 3. **STW remark** — `request_stop_and_join` quiesces the worker; the
//!    caller then re-runs `pause_mark_end` to drain stragglers.
//!
//! ## Page-storage simulation status
//!
//! ZGC pages in this crate are pure metadata: `ZPage::virtual_start` is a
//! synthetic `u64` handed out by `ZgcHeap::next_virtual_addr`, not a
//! pointer to mapped memory. As a consequence:
//!
//! - The mark loop can only manipulate `ZPage::live_bytes` and the
//!   collector's `phase` / `load_barrier.good_colors`. It cannot walk
//!   real object headers or follow real references — there's nothing to
//!   walk.
//! - The "color invariant" we can preserve here is the *bookkeeping*
//!   invariant: across a concurrent-mark cycle the load barrier's
//!   `good_colors` are flipped in the right order and the collector
//!   ends in the `ZgcPhase::None` state with the post-mark color set.
//! - A truly faithful test of "every live colored pointer ends up with a
//!   marked-color bit" would require attaching the load barrier to a
//!   real heap. That's gated on the rewrite described in
//!   `concurrent_relocate`.
//!
//! What this module ships:
//!
//! - The full controller lifecycle (spawn, run, stop, join, drop).
//! - A `concurrent_mark_step` on `ZgcCollector` that does one bounded
//!   drain of the mark stack and returns whether the stack is empty.
//! - Three tests covering spawn/join, stop honouring, and the colour-flip
//!   invariant across a full cycle.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

use crate::zgc::ZgcCollector;

/// Gray pointers drained per `concurrent_mark_step` call. Small enough to
/// observe a stop request within microseconds, large enough to amortise
/// the per-call lock acquisition.
const WORKER_STEP_BUDGET: usize = 256;

/// Park-with-timeout interval when the mark stack is empty but stop has
/// not been signalled. Bounded so a missed `notify_work_available` (which
/// shouldn't happen given the lock discipline, but defensive) still makes
/// progress.
const WORKER_POLL_MS: u64 = 5;

// ---------------------------------------------------------------------------
// Shared coordinator/worker state
// ---------------------------------------------------------------------------

/// Shared coordination state between the VM thread that started the cycle
/// and the background mark worker. Held inside an `Arc<>` so the worker
/// keeps it alive after the coordinator drops its handle.
pub struct ZgcConcurrentMarkState {
    /// Set by `request_stop`; the worker exits on the next poll.
    pub should_stop: AtomicBool,
    /// Telemetry: number of `concurrent_mark_step` calls completed.
    pub steps_performed: AtomicU64,
    /// Telemetry: cumulative budget granted across all steps.
    pub work_units_done: AtomicU64,

    /// Park flag protected by the mutex: the worker sets it `true` before
    /// waiting on the cvar so a coordinator wake racing with park is not
    /// lost (parking_lot lock-then-wait discipline).
    pub parked: Mutex<bool>,
    /// Wake condition: coordinator notifies after pushing new gray
    /// pointers or after setting `should_stop`.
    pub done_cvar: Condvar,
}

impl ZgcConcurrentMarkState {
    fn new() -> Self {
        Self {
            should_stop: AtomicBool::new(false),
            steps_performed: AtomicU64::new(0),
            work_units_done: AtomicU64::new(0),
            parked: Mutex::new(false),
            done_cvar: Condvar::new(),
        }
    }

    /// Coordinator → worker: new gray pointers may be on the stack now.
    /// Cheap fast-path: when the worker is not parked we only pay the
    /// mutex acquisition, no cvar work.
    pub fn notify_work_available(&self) {
        let mut parked = self.parked.lock();
        if *parked {
            *parked = false;
            self.done_cvar.notify_one();
        }
    }

    /// Coordinator → worker: stop ASAP. Also kicks the cvar so a parked
    /// worker wakes to observe the stop flag.
    pub fn request_stop(&self) {
        self.should_stop.store(true, Ordering::Release);
        let mut parked = self.parked.lock();
        *parked = false;
        self.done_cvar.notify_all();
    }

    /// Worker: park with timeout. Returns either on a notification or on
    /// the `WORKER_POLL_MS` timeout; either way the loop body re-checks
    /// `should_stop` and the mark stack.
    fn park_for_work(&self) {
        let mut parked = self.parked.lock();
        if self.should_stop.load(Ordering::Acquire) {
            return;
        }
        *parked = true;
        let _ = self
            .done_cvar
            .wait_for(&mut parked, Duration::from_millis(WORKER_POLL_MS));
        *parked = false;
    }
}

impl Default for ZgcConcurrentMarkState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Coordinator-side controller
// ---------------------------------------------------------------------------

/// Coordinator handle for a spawned ZGC concurrent-mark worker.
///
/// Drop semantics: dropping without calling `request_stop_and_join`
/// flips the stop flag and detaches the worker (best-effort cleanup, no
/// block in Drop). Production callers should always call
/// `request_stop_and_join` explicitly for deterministic shutdown.
pub struct ZgcConcurrentMarkController {
    pub state: Arc<ZgcConcurrentMarkState>,
    handle: Option<JoinHandle<()>>,
}

impl ZgcConcurrentMarkController {
    /// Spawn the background mark thread.
    ///
    /// **Caller responsibility**: `ZgcCollector::pause_mark_start` must
    /// have run first so the load barrier's good-colors are flipped and
    /// the mark stack is seeded with the root set. The worker assumes
    /// this seeding and will park immediately if the stack is empty.
    ///
    /// `collector` is `Arc<Mutex<ZgcCollector>>` because:
    ///
    /// - The worker mutates the mark stack, the page live-bytes, and the
    ///   load barrier counters — all are owned by `ZgcCollector`.
    /// - In a real implementation each mutator thread would also need to
    ///   reach into the collector via the load barrier slow path; the
    ///   mutex models that shared-mutation discipline cleanly.
    /// - Lock granularity is fine because a `concurrent_mark_step` is a
    ///   tight loop over a `Vec<u64>` pop + a single page-lookup; it
    ///   releases the lock between every step so mutators (and the
    ///   coordinator's eventual `request_stop`) can progress.
    pub fn spawn(collector: Arc<Mutex<ZgcCollector>>) -> Self {
        let state = Arc::new(ZgcConcurrentMarkState::new());
        let worker_state = Arc::clone(&state);
        let worker_collector = Arc::clone(&collector);

        let handle = std::thread::Builder::new()
            .name("zgc-concurrent-mark".to_string())
            .spawn(move || {
                Self::worker_loop(worker_collector, worker_state);
            })
            .expect("zgc-concurrent-mark thread spawn failed");

        Self {
            state,
            handle: Some(handle),
        }
    }

    /// Body of the background mark thread.
    ///
    /// Loop invariant:
    /// - On entry to each iteration, neither the mark stack nor the stop
    ///   flag have been observed for this iteration.
    /// - On exit, `should_stop` was observed `true` AND the worker
    ///   performed one final drain pass so any straggler the coordinator
    ///   pushed between the last step and the stop signal is processed.
    fn worker_loop(collector: Arc<Mutex<ZgcCollector>>, state: Arc<ZgcConcurrentMarkState>) {
        loop {
            // Drain under the budget. Take the lock for the step duration
            // only — release between steps so mutators / coordinator can
            // interleave.
            let drained = {
                let mut c = collector.lock();
                c.concurrent_mark_step(WORKER_STEP_BUDGET)
            };

            state.steps_performed.fetch_add(1, Ordering::Relaxed);
            state
                .work_units_done
                .fetch_add(WORKER_STEP_BUDGET as u64, Ordering::Relaxed);

            // Check stop between every step.
            if state.should_stop.load(Ordering::Acquire) {
                // One final drain so a coordinator that pushed and
                // stopped in the same instant doesn't leave work behind.
                let _ = {
                    let mut c = collector.lock();
                    c.concurrent_mark_step(WORKER_STEP_BUDGET)
                };
                return;
            }

            if drained {
                // Stack truly empty. Park until coordinator pushes more
                // work or requests stop. Cycle termination is the STW
                // coordinator's prerogative (it owns the transition to
                // the remark phase) — the worker never self-exits on a
                // drained stack.
                state.park_for_work();
            }
            // else: still gray pointers to chase, keep going.
        }
    }

    /// Coordinator: wake the worker if it's parked. Call after pushing
    /// new gray pointers onto the mark stack (e.g. via the load barrier
    /// slow path or after a mutator field-store).
    pub fn notify_work_available(&self) {
        self.state.notify_work_available();
    }

    /// Coordinator: signal stop and join the worker. Must be called from
    /// the STW remark phase. Returns the join handle's result so a panic
    /// in the worker surfaces here rather than being silently dropped.
    pub fn request_stop_and_join(mut self) -> std::thread::Result<()> {
        self.state.request_stop();
        if let Some(h) = self.handle.take() {
            return h.join();
        }
        Ok(())
    }

    /// Test/inspection helper: is the worker thread still alive?
    pub fn is_running(&self) -> bool {
        self.handle
            .as_ref()
            .map(|h| !h.is_finished())
            .unwrap_or(false)
    }

    /// Test/inspection helper: how many steps has the worker completed?
    pub fn steps_performed(&self) -> u64 {
        self.state.steps_performed.load(Ordering::Relaxed)
    }
}

impl Drop for ZgcConcurrentMarkController {
    fn drop(&mut self) {
        // Best-effort cleanup if the user forgot to call
        // `request_stop_and_join`. We can't block on join here (Drop is
        // sync, and the worker may legitimately be mid-step holding the
        // collector lock), but we flip the stop flag so the worker exits
        // on its next poll.
        self.state.request_stop();
        if let Some(h) = self.handle.take() {
            std::mem::drop(h);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zgc::{ZPageType, ZgcCollector, ZgcConfig, ZgcPhase};

    fn small_collector() -> Arc<Mutex<ZgcCollector>> {
        let mut cfg = ZgcConfig::default();
        cfg.heap_size = 8 * 1024 * 1024;
        let mut c = ZgcCollector::new(cfg);
        // Add a couple of pages so `pause_mark_start` seeds non-empty
        // roots — the worker should then take at least one step before
        // parking.
        c.heap.add_page(ZPageType::Small);
        c.heap.add_page(ZPageType::Small);
        Arc::new(Mutex::new(c))
    }

    /// Test #1 — the ZGC concurrent-mark thread spawns and joins cleanly.
    ///
    /// Mirrors `g1_concurrent::tests::concurrent_mark_thread_spawns_and_joins`.
    /// Verifies the basic lifecycle: spawn → run → stop → join — no panic.
    #[test]
    fn zgc_concurrent_mark_thread_spawns_and_joins() {
        let collector = small_collector();

        // STW initial mark: seed roots and flip the load barrier to the
        // marking good-colors.
        {
            let mut c = collector.lock();
            c.pause_mark_start();
            assert_eq!(c.phase, ZgcPhase::PauseMarkStart);
        }

        let controller = ZgcConcurrentMarkController::spawn(Arc::clone(&collector));
        assert!(controller.is_running(), "worker thread must be alive");

        // Let the worker step at least once.
        std::thread::sleep(Duration::from_millis(30));

        // Stop and join cleanly. The collector is left in
        // `ConcurrentMark` (the worker advanced it on its first step);
        // a real caller would now run `pause_mark_end` to finish.
        let result = controller.request_stop_and_join();
        assert!(result.is_ok(), "worker panic on join: {:?}", result.err());
    }

    /// Test #2 — the worker honors `request_stop` even when parked on an
    /// empty mark stack. Regression guard for a missed cvar notification
    /// (same shape as `g1_concurrent::tests::worker_stops_promptly_when_parked`).
    #[test]
    fn zgc_worker_stops_promptly_when_parked() {
        let collector = small_collector();

        // Don't seed roots → mark stack stays empty after pause_mark_start
        // pushes the (small) root set; the worker will drain it in one
        // step and park immediately on the second iteration.
        {
            let mut c = collector.lock();
            c.pause_mark_start();
        }

        let controller = ZgcConcurrentMarkController::spawn(Arc::clone(&collector));

        // Give the worker time to drain + park.
        std::thread::sleep(Duration::from_millis(20));

        // Now request stop. Even parked, the worker must wake within a
        // few poll intervals (5 ms each, plus the final drain).
        let start = std::time::Instant::now();
        controller
            .request_stop_and_join()
            .expect("worker joined cleanly");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "worker did not stop promptly when parked (took {:?})",
            elapsed
        );
    }

    /// Test #3 — the load barrier's colour invariant is preserved across
    /// a full concurrent-mark cycle.
    ///
    /// Page-storage simulation limitation (see module-doc): without real
    /// backing memory we can't verify "every live colored pointer ends
    /// the cycle with a marked-color bit". What we *can* verify is the
    /// bookkeeping invariant the controller wraps around the mark phase:
    ///
    /// 1. Before `pause_mark_start`, the load-barrier good-colors are
    ///    `REMAPPED` (the post-cycle steady state).
    /// 2. During concurrent mark, good-colors are the `MARKED0 | MARKED1`
    ///    union — load barrier accepts either parity's mark bit.
    /// 3. After `pause_mark_end`, good-colors are still the marked union
    ///    (the relocate phase flips them back to REMAPPED).
    /// 4. The collector's `phase` follows the documented sequence.
    ///
    /// All three invariants are preserved across the spawn/join cycle.
    #[test]
    fn zgc_color_invariant_preserved_across_concurrent_mark() {
        use crate::zgc::{
            ColoredPointer, ZGC_COLOR_MARKED0, ZGC_COLOR_MARKED1, ZGC_COLOR_REMAPPED,
        };

        let collector = small_collector();

        // ── Phase 0: idle. Good-colors are REMAPPED.
        {
            let c = collector.lock();
            assert_eq!(c.phase, ZgcPhase::None);
            assert_eq!(c.load_barrier.good_colors, ZGC_COLOR_REMAPPED);
        }

        // Build a sample colored pointer with the REMAPPED bit set —
        // i.e. a pointer that is "good" in the idle state.
        let p_remapped = ColoredPointer::new(0x1000, ZGC_COLOR_REMAPPED);
        let p_marked0 = ColoredPointer::new(0x2000, ZGC_COLOR_MARKED0);

        // ── Phase 1: STW initial mark.
        {
            let mut c = collector.lock();
            c.pause_mark_start();
            assert_eq!(c.phase, ZgcPhase::PauseMarkStart);
            // Load barrier flipped: marked union is now good, REMAPPED is not.
            assert_eq!(
                c.load_barrier.good_colors,
                ZGC_COLOR_MARKED0 | ZGC_COLOR_MARKED1
            );
            // The previously-good REMAPPED pointer would now miss the
            // fast path; a MARKED0 pointer is fast-path good.
            assert!(matches!(
                c.load_barrier.check(&p_remapped),
                crate::zgc::LoadBarrierResult::NeedsSlowPath(_)
            ));
            assert!(matches!(
                c.load_barrier.check(&p_marked0),
                crate::zgc::LoadBarrierResult::GoodColor(_)
            ));
        }

        // ── Phase 2: spawn worker, let it drive concurrent mark.
        let controller = ZgcConcurrentMarkController::spawn(Arc::clone(&collector));
        // Poll until the worker has at least one step under its belt
        // (the small seeded root set drains in a single budget'd step).
        for _ in 0..50 {
            if controller.steps_performed() > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // The worker transitioned the phase to ConcurrentMark on its
        // first step (via `concurrent_mark_step`).
        {
            let c = collector.lock();
            assert_eq!(c.phase, ZgcPhase::ConcurrentMark);
            // good_colors still the marked union — they only flip back
            // to REMAPPED at the relocate-start STW.
            assert_eq!(
                c.load_barrier.good_colors,
                ZGC_COLOR_MARKED0 | ZGC_COLOR_MARKED1
            );
        }

        // Capture the step count BEFORE consuming the controller via
        // `request_stop_and_join` (which takes self).
        let steps_during_concurrent = controller.steps_performed();

        // ── Phase 3: STW remark — coordinator stops worker, joins, and
        // drives `pause_mark_end` to drain stragglers.
        controller
            .request_stop_and_join()
            .expect("worker joined cleanly");
        {
            let mut c = collector.lock();
            c.pause_mark_end();
            assert_eq!(c.phase, ZgcPhase::PauseMarkEnd);
            // Color invariant still the marked union after remark — the
            // flip back to REMAPPED happens at the next STW
            // (PauseRelocateStart), which we don't run in this test.
            assert_eq!(
                c.load_barrier.good_colors,
                ZGC_COLOR_MARKED0 | ZGC_COLOR_MARKED1
            );
        }

        // The worker did real work: at least one step completed under
        // the budget. This guards against a regression where the worker
        // would silently park without ever entering `concurrent_mark_step`.
        assert!(
            steps_during_concurrent >= 1,
            "worker should have performed at least one mark step (got {})",
            steps_during_concurrent
        );
    }
}
