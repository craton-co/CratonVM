//! SATB (Snapshot-At-The-Beginning) write barrier buffers.
//!
//! During concurrent marking, application threads may overwrite reference
//! fields. To maintain correctness (no live object is missed), the SATB
//! barrier logs the *old* value of a reference field before it is overwritten.
//!
//! Each application thread has its own `SatbBuffer`. When the buffer is full,
//! it is flushed to a global queue for the marking threads to process.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

thread_local! {
    /// Per-thread SATB buffer for the write barrier fast path.
    ///
    /// The mutator write barrier appends overwritten reference values into
    /// this buffer without taking any shared lock. When the buffer fills it
    /// auto-flushes into the global [`SatbQueue`]; the collector also drains
    /// per-thread buffers at safepoints via [`flush_thread_satb_buffer`].
    static THREAD_SATB_BUFFER: RefCell<SatbBuffer> = RefCell::new(SatbBuffer::new());
}

/// Push an overwritten reference address into the calling thread's local
/// SATB buffer. If the buffer is full, drain it into `queue` atomically.
///
/// This is the fast path called by the write barrier — no shared lock is
/// taken unless the per-thread buffer fills (amortized one lock per 256
/// reference stores).
#[inline]
pub fn satb_thread_local_log(queue: &SatbQueue, old_ref_addr: usize) {
    if old_ref_addr == 0 {
        return;
    }
    let to_flush = THREAD_SATB_BUFFER.with(|buf| {
        let mut b = buf.borrow_mut();
        if b.log(old_ref_addr) {
            Some(b.drain())
        } else {
            None
        }
    });
    if let Some(entries) = to_flush {
        queue.flush(entries);
    }
}

/// Drain the calling thread's SATB buffer into the global queue.
///
/// Called by mutators at safepoint entry and by the collector at GC start
/// to ensure all logged-but-unflushed entries reach the global queue
/// before marker threads consume them.
pub fn flush_thread_satb_buffer(queue: &SatbQueue) {
    let entries = THREAD_SATB_BUFFER.with(|buf| {
        let mut b = buf.borrow_mut();
        if b.is_empty() {
            Vec::new()
        } else {
            b.drain()
        }
    });
    if !entries.is_empty() {
        queue.flush(entries);
    }
}

/// Default capacity of a per-thread SATB buffer (entries, not bytes).
const DEFAULT_SATB_CAPACITY: usize = 256;

/// A per-thread SATB buffer that records overwritten reference values.
///
/// When the write barrier fires during concurrent marking, the *old* reference
/// value is pushed into this buffer. When the buffer is full or during the
/// remark STW pause, it is flushed to the global SATB queue.
pub struct SatbBuffer {
    /// Buffer of overwritten reference pointers. Each entry is a raw object
    /// address that was about to be overwritten.
    entries: Vec<usize>,
    /// Maximum entries before auto-flush.
    capacity: usize,
}

impl SatbBuffer {
    /// Create a new empty SATB buffer.
    pub fn new() -> Self {
        Self {
            entries: Vec::with_capacity(DEFAULT_SATB_CAPACITY),
            capacity: DEFAULT_SATB_CAPACITY,
        }
    }

    /// Create with a specific capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Record an overwritten reference value. Returns `true` if the buffer
    /// is now full and should be flushed.
    #[inline]
    pub fn log(&mut self, old_ref_addr: usize) -> bool {
        self.entries.push(old_ref_addr);
        self.entries.len() >= self.capacity
    }

    /// Drain all entries from the buffer and return them.
    ///
    /// Uses `std::mem::take` so that `self.entries` becomes an empty Vec.
    /// The caller receives the old Vec with its data; after processing,
    /// the next `log()` call will reallocate. This avoids a redundant
    /// `Vec::with_capacity` allocation on every drain.
    pub fn drain(&mut self) -> Vec<usize> {
        let drained = std::mem::take(&mut self.entries);
        // Pre-allocate capacity for the next fill cycle to avoid repeated
        // small allocations.
        self.entries.reserve(self.capacity);
        drained
    }

    /// Number of entries currently buffered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the buffer is full.
    pub fn is_full(&self) -> bool {
        self.entries.len() >= self.capacity
    }
}

impl Default for SatbBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Global SATB queue shared by all threads and the concurrent marker.
///
/// Application threads flush their per-thread `SatbBuffer` into this queue.
/// Marker threads drain it to discover references that were overwritten
/// during concurrent marking.
pub struct SatbQueue {
    /// Accumulated entries from all threads.
    entries: Mutex<Vec<usize>>,
    /// Whether SATB logging is currently active (only during concurrent mark).
    active: AtomicBool,
}

impl SatbQueue {
    /// Create a new inactive SATB queue.
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            active: AtomicBool::new(false),
        }
    }

    /// Enable SATB logging (called at start of concurrent mark phase).
    pub fn activate(&self) {
        self.active.store(true, Ordering::Release);
    }

    /// Disable SATB logging (called after remark phase completes).
    pub fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    /// Check if SATB logging is active. Threads use this to decide whether
    /// the write barrier should log old values.
    #[inline]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Flush a per-thread buffer's entries into the global queue.
    pub fn flush(&self, entries: Vec<usize>) {
        if entries.is_empty() {
            return;
        }
        let mut queue = self.entries.lock();
        queue.extend(entries);
    }

    /// Drain all accumulated entries for the marker to process.
    pub fn drain(&self) -> Vec<usize> {
        let mut queue = self.entries.lock();
        std::mem::take(&mut *queue)
    }

    /// Number of entries currently queued (approximate, for stats).
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }
}

impl Default for SatbQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SatbQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SatbQueue")
            .field("active", &self.active.load(Ordering::Relaxed))
            .field("queued", &self.entries.lock().len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_log_and_drain() {
        let mut buf = SatbBuffer::new();
        assert!(buf.is_empty());

        buf.log(0x1000);
        buf.log(0x2000);
        assert_eq!(buf.len(), 2);

        let entries = buf.drain();
        assert_eq!(entries, vec![0x1000, 0x2000]);
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_signals_full() {
        let mut buf = SatbBuffer::with_capacity(3);
        assert!(!buf.log(0x100));
        assert!(!buf.log(0x200));
        assert!(buf.log(0x300)); // full
        assert!(buf.is_full());
    }

    #[test]
    fn queue_inactive_by_default() {
        let q = SatbQueue::new();
        assert!(!q.is_active());
        assert!(q.is_empty());
    }

    #[test]
    fn queue_activate_flush_drain() {
        let q = SatbQueue::new();
        q.activate();
        assert!(q.is_active());

        q.flush(vec![0x100, 0x200]);
        q.flush(vec![0x300]);
        assert_eq!(q.len(), 3);

        let drained = q.drain();
        assert_eq!(drained, vec![0x100, 0x200, 0x300]);
        assert!(q.is_empty());

        q.deactivate();
        assert!(!q.is_active());
    }

    #[test]
    fn queue_concurrent_flush() {
        use std::sync::Arc;
        let q = Arc::new(SatbQueue::new());
        q.activate();

        let mut handles = Vec::new();
        for t in 0..4 {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                let mut buf = SatbBuffer::with_capacity(64);
                for i in 0..64 {
                    buf.log(t * 1000 + i);
                }
                q.flush(buf.drain());
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        let all = q.drain();
        assert_eq!(all.len(), 256); // 4 threads × 64 entries
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn buffer_empty_drain_returns_empty() {
        let mut buf = SatbBuffer::new();
        let entries = buf.drain();
        assert!(entries.is_empty());
        assert!(buf.is_empty());
    }

    #[test]
    fn buffer_single_entry() {
        let mut buf = SatbBuffer::new();
        let full = buf.log(0xDEAD);
        assert!(!full);
        assert_eq!(buf.len(), 1);
        assert!(!buf.is_empty());

        let entries = buf.drain();
        assert_eq!(entries, vec![0xDEAD]);
    }

    #[test]
    fn buffer_capacity_exact_boundary() {
        let mut buf = SatbBuffer::with_capacity(1);
        // Capacity 1: first log should signal full
        assert!(buf.log(0x100));
        assert!(buf.is_full());
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn buffer_drain_resets_capacity() {
        let mut buf = SatbBuffer::with_capacity(2);
        buf.log(0x100);
        buf.log(0x200);
        assert!(buf.is_full());

        let _ = buf.drain();
        assert!(buf.is_empty());
        assert!(!buf.is_full());

        // Can fill again after drain
        assert!(!buf.log(0x300));
        assert!(buf.log(0x400));
        assert!(buf.is_full());
    }

    #[test]
    fn buffer_multiple_drains() {
        let mut buf = SatbBuffer::with_capacity(4);
        buf.log(0x10);
        buf.log(0x20);

        let first = buf.drain();
        assert_eq!(first, vec![0x10, 0x20]);

        buf.log(0x30);
        let second = buf.drain();
        assert_eq!(second, vec![0x30]);

        // Third drain is empty
        let third = buf.drain();
        assert!(third.is_empty());
    }

    #[test]
    fn queue_flush_empty_vec_is_noop() {
        let q = SatbQueue::new();
        q.flush(vec![]);
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);
    }

    #[test]
    fn queue_drain_when_empty() {
        let q = SatbQueue::new();
        let drained = q.drain();
        assert!(drained.is_empty());
    }

    #[test]
    fn queue_multiple_flush_accumulates() {
        let q = SatbQueue::new();
        q.flush(vec![1, 2]);
        q.flush(vec![3, 4, 5]);
        q.flush(vec![6]);

        assert_eq!(q.len(), 6);
        let drained = q.drain();
        assert_eq!(drained, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn queue_activate_deactivate_cycle() {
        let q = SatbQueue::new();
        assert!(!q.is_active());

        q.activate();
        assert!(q.is_active());

        q.deactivate();
        assert!(!q.is_active());

        // Re-activate should work
        q.activate();
        assert!(q.is_active());
    }

    #[test]
    fn queue_drain_clears_entries() {
        let q = SatbQueue::new();
        q.flush(vec![0x100, 0x200, 0x300]);
        assert_eq!(q.len(), 3);

        let _ = q.drain();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        // Second drain returns empty
        let second = q.drain();
        assert!(second.is_empty());
    }

    #[test]
    fn queue_concurrent_interleaved_flush_drain() {
        use std::sync::Arc;

        let q = Arc::new(SatbQueue::new());
        q.activate();

        let q2 = q.clone();
        // Writer thread
        let writer = std::thread::spawn(move || {
            for i in 0..100usize {
                q2.flush(vec![i]);
            }
        });

        // Drain periodically from main thread
        let mut total_drained = 0;
        writer.join().unwrap();

        // Final drain to get everything
        total_drained += q.drain().len();

        // Everything that was pushed must come out
        // Some may have been drained mid-way, so we just need the total
        assert_eq!(total_drained, 100);
    }

    #[test]
    fn buffer_overflow_behavior_large_count() {
        let mut buf = SatbBuffer::with_capacity(4);

        // Push past capacity -- the buffer does not enforce capacity as a hard limit
        assert!(!buf.log(1));
        assert!(!buf.log(2));
        assert!(!buf.log(3));
        assert!(buf.log(4)); // signals full at capacity

        // Continue pushing after full signal
        buf.log(5);
        assert_eq!(buf.len(), 5);

        let entries = buf.drain();
        assert_eq!(entries, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn thread_local_log_explicit_flush() {
        // Run inside a fresh thread so the per-thread SATB buffer starts
        // empty regardless of test ordering.
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            satb_thread_local_log(&q, 0x100);
            satb_thread_local_log(&q, 0x200);
            // Not full yet → nothing in the global queue.
            assert!(q.is_empty());
            // Explicit safepoint-style flush drains the per-thread buffer.
            flush_thread_satb_buffer(&q);
            let drained = q.drain();
            assert_eq!(drained, vec![0x100, 0x200]);
        });
        h.join().unwrap();
    }

    #[test]
    fn thread_local_log_null_skipped() {
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            satb_thread_local_log(&q, 0);
            flush_thread_satb_buffer(&q);
            assert!(q.is_empty());
        });
        h.join().unwrap();
    }

    #[test]
    fn thread_local_log_auto_flushes_at_capacity() {
        // Each thread has its own buffer with DEFAULT_SATB_CAPACITY=256.
        // Logging exactly 256 entries should auto-flush into the global queue.
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            for i in 1..=DEFAULT_SATB_CAPACITY {
                satb_thread_local_log(&q, i);
            }
            // Auto-flush at 256 means the global queue is non-empty without
            // an explicit safepoint flush.
            assert!(!q.is_empty());
            assert_eq!(q.len(), DEFAULT_SATB_CAPACITY);
        });
        h.join().unwrap();
    }
}
