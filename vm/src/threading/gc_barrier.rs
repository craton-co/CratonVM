// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Stop-the-world GC barrier for multi-threaded execution.
//!
//! When garbage collection is needed, the triggering thread requests a
//! stop-the-world (STW) pause. All other threads, at their next safepoint
//! (allocation site or backward branch), deposit their root ObjectRefs and
//! wait for GC to complete. After collection, each thread applies the
//! pointer map to update its own frame references.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use parking_lot::{Condvar, Mutex};

use crate::threading::jvm_thread::ThreadId;

/// Coordinates stop-the-world pauses for garbage collection.
///
/// The barrier uses a cheap `AtomicBool` flag (`stw_requested`) that threads
/// poll at safepoints. When set, threads deposit their roots and wait for
/// the GC initiator to finish collection.
pub struct GcBarrier {
    /// Cheap flag polled at every safepoint. Only requires an atomic load.
    pub stw_requested: AtomicBool,
    /// GC generation counter — incremented after each collection.
    /// Threads compare their local generation to detect missed GCs.
    pub gc_generation: AtomicU64,
    /// T19.H1 — count of threads currently parked in a *blocking* native
    /// operation (`Object.wait`, `Thread.sleep`, `LockSupport.park`,
    /// `Thread.join`, `ReferenceQueue.remove`, selector `select`, …).
    ///
    /// Such a thread is GC-safe: before blocking it deposits its frame
    /// roots into the thread-registry snapshot (`deposit_root_snapshot`),
    /// and on wake it applies the GC pointer map (`check_post_block_gc`).
    /// While parked it executes no code and cannot reach an interpreter
    /// safepoint to call `arrive_and_wait`.
    ///
    /// The stop-the-world barrier must therefore NOT include parked
    /// threads in `expected` — otherwise `wait_for_all` deadlocks waiting
    /// for a thread that is (correctly) blocked indefinitely, e.g. the
    /// Reference Handler parked in `ReferenceQueue.remove`. This is the
    /// JVM `_thread_blocked` state, scoped precisely to genuine blocking
    /// operations (NOT every native call — a *running* native still
    /// holds raw `ObjectRef`s in Rust locals and must be waited for so
    /// the copying collector does not relocate objects under it).
    ///
    /// Maintained by `enter_blocked` / `leave_blocked`, called from
    /// `deposit_root_snapshot` / `check_post_block_gc`.
    threads_blocked: AtomicU64,
    /// Protected coordination state.
    inner: Mutex<GcBarrierInner>,
    /// Signaled when all expected threads have arrived at the barrier.
    all_arrived: Condvar,
    /// Signaled when GC is complete and threads can resume.
    gc_complete: Condvar,
}

struct GcBarrierInner {
    /// Thread that initiated the current STW pause.
    initiator: Option<ThreadId>,
    /// Number of active (non-blocked) threads expected to arrive.
    expected: u32,
    /// Number of threads that have arrived at the barrier.
    arrived: u32,
    /// Pointer map from the last GC, shared with threads for frame updates.
    pointer_map: HashMap<usize, usize>,
}

impl GcBarrier {
    /// Create a new GC barrier with no active STW.
    pub fn new() -> Self {
        Self {
            stw_requested: AtomicBool::new(false),
            gc_generation: AtomicU64::new(0),
            threads_blocked: AtomicU64::new(0),
            inner: Mutex::new(GcBarrierInner {
                initiator: None,
                expected: 0,
                arrived: 0,
                pointer_map: HashMap::new(),
            }),
            all_arrived: Condvar::new(),
            gc_complete: Condvar::new(),
        }
    }

    /// Request a stop-the-world pause. Called by the GC-initiating thread.
    ///
    /// `alive_count` is the total number of alive threads (including the initiator).
    /// Returns `true` if the request was accepted, `false` if another STW is in progress.
    pub fn request_stw(&self, initiator: ThreadId, alive_count: u32) -> bool {
        let mut inner = self.inner.lock();
        if self.stw_requested.load(Ordering::Acquire) {
            return false;
        }
        inner.initiator = Some(initiator);
        // T19.H1 — exclude threads currently parked in a blocking native
        // from the set we wait for. They are GC-safe (roots already
        // deposited via `deposit_root_snapshot`) and execute no code, so
        // they will not reach an interpreter safepoint. Without this, a
        // STW initiated while e.g. the Reference Handler thread is parked
        // in `ReferenceQueue.remove` deadlocks `wait_for_all` forever.
        //
        // Race analysis (the count may change after this read):
        //  * blocked→running after the read: the waking thread runs
        //    `check_post_block_gc`, sees `stw_requested`, and calls
        //    `arrive_and_wait` — which only ever *over*-counts `arrived`
        //    (harmless: `wait_for_all` uses a `<` test, and the extra
        //    `notify_all` is a no-op once already signalled).
        //  * running→blocked after the read: handled by `enter_blocked`,
        //    which — if a STW is already active — makes the thread
        //    arrive at the barrier *before* it parks, so the initiator
        //    is not left waiting for a thread that counted in `expected`
        //    and then vanished into a block.
        let blocked = self.threads_blocked.load(Ordering::Acquire);
        let blocked_u32 = u32::try_from(blocked).unwrap_or(u32::MAX);
        inner.expected = alive_count
            .saturating_sub(1)
            .saturating_sub(blocked_u32);
        inner.arrived = 0;
        inner.pointer_map.clear();
        self.stw_requested.store(true, Ordering::Release);
        true
    }

    /// T19.H1 — mark the calling thread as entering a blocking native
    /// operation (about to park in `wait`/`park`/`sleep`/`select`/…) and
    /// return a [`BlockedGuard`] that clears the mark on drop.
    ///
    /// The guard makes the in-blocked accounting **leak-proof**: even if
    /// the blocking call returns `Err` via `?` or unwinds, `Drop` still
    /// decrements the counter, so `request_stw`'s `expected` can never
    /// drift permanently low (which would make GC stop waiting for live
    /// mutators).
    ///
    /// `pre_stw` on the returned guard is `true` if a stop-the-world
    /// pause was already in progress at the transition: the caller
    /// should then `arrive_and_wait` *before* parking, because
    /// `request_stw` may have counted this thread in `expected` before
    /// it became blocked.
    pub fn enter_blocked(&self) -> BlockedGuard<'_> {
        // Serialize the transition against `request_stw` (which computes
        // `expected` under the same lock): either our increment lands
        // BEFORE the count read (we are excluded AND `pre_stw` reads the
        // pre-request value `false`, so we park without arriving) or AFTER
        // (we are counted and `pre_stw=true` makes the caller arrive —
        // exactly once). Without the lock the two could interleave so that
        // an EXCLUDED thread arrives anyway; `arrived` is a plain counter,
        // so that spurious arrival releases `wait_for_all` while a counted
        // mutator is still running — a moving GC racing live frames.
        let inner = self.inner.lock();
        self.threads_blocked.fetch_add(1, Ordering::AcqRel);
        let pre_stw = self.stw_requested.load(Ordering::Acquire);
        drop(inner);
        BlockedGuard {
            barrier: self,
            pre_stw,
        }
    }

    /// T19.H1 — non-guard variant of `enter_blocked` for native methods
    /// that open a blocking region via `NativeContext::begin_blocking_region`
    /// (e.g. `ReferenceQueue.remove`'s poll loop). Must be balanced by
    /// exactly one `mark_blocked_region_leave`.
    ///
    /// Returns `true` if a stop-the-world pause is already in progress
    /// (caller should `arrive_and_wait`).
    pub fn mark_blocked_region_enter(&self) -> bool {
        // Serialized against `request_stw` — see `enter_blocked` for the
        // exact-counting rationale.
        let inner = self.inner.lock();
        self.threads_blocked.fetch_add(1, Ordering::AcqRel);
        let pre_stw = self.stw_requested.load(Ordering::Acquire);
        drop(inner);
        pre_stw
    }

    /// T19.H1 — end a region opened by `mark_blocked_region_enter`.
    ///
    /// Exact-counting contract (see `enter_blocked`): a thread may NOT
    /// transition blocked→running while a stop-the-world pause is active —
    /// it was EXCLUDED from that pause's `expected`, so the initiator will
    /// not wait for it, and arriving would inflate `arrived` for someone
    /// else's quota. Instead we wait the pause out while still counted as
    /// blocked (the GC maintains our roots via
    /// `fold_pointer_map_into_blocked`), and only then decrement — any
    /// LATER pause counts us in `expected` and we arrive exactly once via
    /// `check_post_block_gc`.
    pub fn mark_blocked_region_leave(&self) {
        let mut inner = self.inner.lock();
        while self.stw_requested.load(Ordering::Acquire) {
            self.gc_complete.wait(&mut inner);
        }
        self.threads_blocked.fetch_sub(1, Ordering::AcqRel);
    }

    /// Number of threads currently parked in a blocking native. Used by
    /// diagnostics and by the watchdog's hang report.
    pub fn blocked_count(&self) -> u64 {
        self.threads_blocked.load(Ordering::Acquire)
    }

    /// Wait for all expected threads to arrive at the barrier.
    /// Called by the GC initiator after `request_stw`.
    pub fn wait_for_all(&self) {
        let mut inner = self.inner.lock();
        while inner.arrived < inner.expected {
            self.all_arrived.wait(&mut inner);
        }
    }

    /// Signal that GC is complete and threads can resume.
    /// Called by the GC initiator after running collection.
    ///
    /// Stores the pointer map so threads can update their own frames.
    pub fn complete_gc(&self, pointer_map: HashMap<usize, usize>) {
        let mut inner = self.inner.lock();
        inner.pointer_map = pointer_map;
        inner.initiator = None;
        self.gc_generation.fetch_add(1, Ordering::Release);
        self.stw_requested.store(false, Ordering::Release);
        self.gc_complete.notify_all();
    }

    /// Called by non-initiator threads at safepoints when STW is active.
    ///
    /// The thread signals its arrival, then waits for GC to complete.
    /// Returns the pointer map for updating this thread's frame references.
    pub fn arrive_and_wait(&self, tid: ThreadId) -> HashMap<usize, usize> {
        let mut inner = self.inner.lock();
        // If this is the initiator or STW is not active, return immediately
        if !self.stw_requested.load(Ordering::Acquire) || inner.initiator == Some(tid) {
            return HashMap::new();
        }
        // Signal arrival
        inner.arrived += 1;
        if inner.arrived >= inner.expected {
            self.all_arrived.notify_all();
        }
        // Wait for GC to complete
        while self.stw_requested.load(Ordering::Acquire) {
            self.gc_complete.wait(&mut inner);
        }
        inner.pointer_map.clone()
    }

    /// Get the number of expected threads still outstanding.
    /// Used for diagnostics/testing.
    pub fn pending_count(&self) -> u32 {
        let inner = self.inner.lock();
        inner.expected.saturating_sub(inner.arrived)
    }

    /// Request a brief STW pause for concurrent GC phases (initial mark / remark).
    ///
    /// Unlike a full `request_stw` + `complete_gc`, this is designed for
    /// short pauses where the initiator runs a quick marking phase and then
    /// immediately releases all threads.
    ///
    /// Returns `true` if the STW was successfully acquired.
    pub fn brief_stw<F>(&self, initiator: ThreadId, alive_count: u32, work: F) -> bool
    where
        F: FnOnce(),
    {
        if !self.request_stw(initiator, alive_count) {
            return false;
        }
        self.wait_for_all();
        work();
        self.complete_gc(HashMap::new());
        true
    }
}

impl Default for GcBarrier {
    fn default() -> Self {
        Self::new()
    }
}

/// T19.H1 — RAII guard returned by [`GcBarrier::enter_blocked`].
///
/// Holding the guard means "this thread is parked in a blocking native
/// and is GC-safe". Dropping it (normal return, `?` early-return, or
/// unwind) decrements the barrier's blocked-thread count, so the
/// accounting can never leak.
#[must_use = "dropping the guard immediately ends the blocked state"]
pub struct BlockedGuard<'a> {
    barrier: &'a GcBarrier,
    /// `true` if a stop-the-world pause was already active when the
    /// thread entered the blocked state. The caller should arrive at the
    /// barrier before parking — see `GcBarrier::enter_blocked`.
    pub pre_stw: bool,
}

impl Drop for BlockedGuard<'_> {
    fn drop(&mut self) {
        // Checked leave — identical contract to `mark_blocked_region_leave`:
        // wait out any active stop-the-world pause (we were excluded from
        // its `expected`; arriving or running would both be wrong) before
        // re-entering the mutator population.
        let mut inner = self.barrier.inner.lock();
        while self.barrier.stw_requested.load(Ordering::Acquire) {
            self.barrier.gc_complete.wait(&mut inner);
        }
        self.barrier
            .threads_blocked
            .fetch_sub(1, Ordering::AcqRel);
    }
}

impl std::fmt::Debug for GcBarrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GcBarrier")
            .field("stw_active", &self.stw_requested.load(Ordering::Relaxed))
            .field("generation", &self.gc_generation.load(Ordering::Relaxed))
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn barrier_no_stw_by_default() {
        let barrier = GcBarrier::new();
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn barrier_request_and_complete_single_thread() {
        let barrier = GcBarrier::new();
        // Single alive thread (the initiator) → expected = 0
        assert!(barrier.request_stw(ThreadId(0), 1));
        assert!(barrier.stw_requested.load(Ordering::Relaxed));

        // No other threads to wait for
        barrier.wait_for_all();

        // Complete with empty pointer map
        barrier.complete_gc(HashMap::new());
        assert!(!barrier.stw_requested.load(Ordering::Relaxed));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn barrier_two_threads() {
        let barrier = Arc::new(GcBarrier::new());

        // Request STW with 2 alive threads
        assert!(barrier.request_stw(ThreadId(0), 2));

        let barrier2 = barrier.clone();
        let handle = std::thread::spawn(move || {
            // Non-initiator thread arrives at safepoint
            barrier2.arrive_and_wait(ThreadId(1))
        });

        // Initiator waits for the other thread
        barrier.wait_for_all();

        // Complete GC with a pointer remap
        let mut pm = HashMap::new();
        pm.insert(0x1000, 0x2000);
        barrier.complete_gc(pm);

        // Non-initiator thread should receive the pointer map
        let result_map = handle.join().unwrap();
        assert_eq!(result_map.get(&0x1000), Some(&0x2000));
        assert_eq!(barrier.gc_generation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn barrier_duplicate_request_rejected() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Second request while first is active should fail
        assert!(!barrier.request_stw(ThreadId(1), 2));
        barrier.wait_for_all();
        barrier.complete_gc(HashMap::new());
    }

    #[test]
    fn barrier_initiator_not_blocked() {
        let barrier = GcBarrier::new();
        assert!(barrier.request_stw(ThreadId(0), 1));
        // Initiator calling arrive_and_wait should return immediately
        let map = barrier.arrive_and_wait(ThreadId(0));
        assert!(map.is_empty());
        barrier.wait_for_all();
        barrier.complete_gc(HashMap::new());
    }
}
