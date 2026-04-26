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
        inner.expected = alive_count.saturating_sub(1);
        inner.arrived = 0;
        inner.pointer_map.clear();
        self.stw_requested.store(true, Ordering::Release);
        true
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
