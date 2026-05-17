// AUDIT 2026-05-16: std HashMap unused (replaced by FxHashMap below).
use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use rustc_hash::FxHashMap;

use crate::event::{EventInstance, EventTypeId};

// NOTE: The foundation contract requested `parking_lot::Mutex`, but this crate
// (`rustjvm-jfr`) intentionally does not depend on `parking_lot` (see
// Cargo.toml comment from the 2026-05-16 audit). Since this task restricts
// edits to `jfr/src/repository.rs` only, we cannot add the dependency, and we
// fall back to `std::sync::Mutex`. The API contract and semantics are
// preserved; callers that hold `Arc<Mutex<VecDeque<EventInstance>>>` work
// identically with either Mutex implementation. Migrating to
// `parking_lot::Mutex` later is a mechanical type swap.

/// In-memory event storage with ring-buffer eviction.
///
/// Uses a `VecDeque` for O(1) front eviction and maintains a secondary
/// `type_index` mapping `EventTypeId` to buffer indices for O(1) type queries.
pub struct EventRepository {
    events: VecDeque<EventInstance>,
    max_events: usize,
    total_recorded: u64,
    /// Index from event type_id to the set of logical indices (offset from
    /// `total_recorded - events.len()`). Maintained on push/evict/clear.
    /// T10.9.B: FxHashMap — EventTypeId is internal.
    type_index: FxHashMap<EventTypeId, Vec<usize>>,
    /// The absolute index of the first element currently in `events`.
    /// Equals `total_recorded - events.len()` after each push.
    base_index: u64,
}

impl EventRepository {
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max_events,
            total_recorded: 0,
            type_index: FxHashMap::default(),
            base_index: 0,
        }
    }

    /// Push an event, evicting the oldest if over the limit. O(1) amortized.
    pub fn push(&mut self, event: EventInstance) {
        if self.events.len() >= self.max_events {
            // Evict oldest (front) — O(1) with VecDeque
            if let Some(evicted) = self.events.pop_front() {
                // Remove evicted event from type index
                if let Some(indices) = self.type_index.get_mut(&evicted.type_id) {
                    if let Some(pos) = indices.iter().position(|&i| i == self.base_index as usize) {
                        indices.swap_remove(pos);
                    }
                    if indices.is_empty() {
                        self.type_index.remove(&evicted.type_id);
                    }
                }
                self.base_index += 1;
            }
        }
        // Add to type index
        let abs_index = self.total_recorded as usize;
        self.type_index
            .entry(event.type_id)
            .or_default()
            .push(abs_index);

        self.events.push_back(event);
        self.total_recorded += 1;
    }

    /// Return a slice-like view of all current events.
    /// Note: `VecDeque::make_contiguous` is called to allow returning a slice.
    pub fn events(&mut self) -> &[EventInstance] {
        self.events.make_contiguous();
        let (front, _) = self.events.as_slices();
        front
    }

    /// Return an iterator over all current events (does not require contiguous layout).
    pub fn iter(&self) -> impl Iterator<Item = &EventInstance> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Total number of events ever recorded, including evicted ones.
    pub fn total_recorded(&self) -> u64 {
        self.total_recorded
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.type_index.clear();
        self.base_index = self.total_recorded;
    }

    /// Return references to events matching the given type id.
    /// Uses the type_index for O(1) lookup of matching indices.
    pub fn events_by_type(&self, type_id: EventTypeId) -> Vec<&EventInstance> {
        match self.type_index.get(&type_id) {
            Some(abs_indices) => {
                abs_indices
                    .iter()
                    .filter_map(|&abs_idx| {
                        let rel = abs_idx.checked_sub(self.base_index as usize)?;
                        self.events.get(rel)
                    })
                    .collect()
            }
            None => vec![],
        }
    }

    /// Return references to events whose start_time falls within [start, end].
    pub fn events_in_range(&self, start: u64, end: u64) -> Vec<&EventInstance> {
        self.events
            .iter()
            .filter(|e| e.start_time >= start && e.start_time <= end)
            .collect()
    }
}

impl Default for EventRepository {
    fn default() -> Self {
        Self::new(100_000)
    }
}

// ---------------------------------------------------------------------------
// Per-thread lock-free SPSC event ring (foundation for ThreadEventRing API)
// ---------------------------------------------------------------------------

/// Default capacity for per-thread event rings.
///
/// Each emitting thread allocates a single `VecDeque` of this size. When the
/// ring is full, the oldest event is dropped (head-eviction) to keep producers
/// non-blocking on the hot path.
pub const DEFAULT_THREAD_RING_CAPACITY: usize = 1024;

/// Per-thread bounded ring of pending events.
///
/// This type is a thin wrapper around `RefCell<VecDeque<EventInstance>>` that
/// exposes a "push or drop-oldest" semantics. It is intentionally `!Sync` and
/// `!Send` (because of `RefCell`), since it is only ever borrowed from the
/// owning thread. Cross-thread drainage of the *same* event stream is handled
/// separately via `ThreadRingRegistry`, which holds `Arc<Mutex<VecDeque<…>>>`
/// shards that producers push to without contention (each thread owns its
/// shard, so the mutex is uncontended on the producer side).
///
/// This struct is kept as part of the public API surface so other crates can
/// build local (non-registered) rings for testing or specialized buffering.
pub struct ThreadEventRing {
    inner: RefCell<VecDeque<EventInstance>>,
    capacity: usize,
}

impl ThreadEventRing {
    /// Create a new per-thread ring with the given fixed capacity.
    ///
    /// `capacity` is clamped to a minimum of 1 — a zero-capacity ring would
    /// drop every event, which is almost certainly a configuration bug.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            inner: RefCell::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Push an event onto the ring. If the ring is full, the oldest event
    /// (front of the deque) is dropped first to make room. This keeps the
    /// producer-side path bounded in both time and memory.
    pub fn push(&self, ev: EventInstance) {
        let mut buf = self.inner.borrow_mut();
        if buf.len() >= self.capacity {
            // Drop the oldest event to make room for the new one.
            let _ = buf.pop_front();
        }
        buf.push_back(ev);
    }

    /// Drain all currently-buffered events into `dst`, preserving order
    /// (oldest first). The ring is empty after this call.
    pub fn drain_into(&self, dst: &mut Vec<EventInstance>) {
        let mut buf = self.inner.borrow_mut();
        dst.reserve(buf.len());
        dst.extend(buf.drain(..));
    }

    /// Number of events currently buffered in the ring.
    pub fn len(&self) -> usize {
        self.inner.borrow().len()
    }

    /// True if the ring currently holds no events.
    pub fn is_empty(&self) -> bool {
        self.inner.borrow().is_empty()
    }

    /// Maximum number of events the ring will hold before drop-oldest kicks in.
    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

thread_local! {
    /// Thread-local instance of the simple `ThreadEventRing` (single-threaded
    /// view, kept for API completeness; production emit paths should use
    /// `push_to_thread_ring` which goes through the `ThreadRingRegistry`).
    pub static THREAD_EVENT_RING: ThreadEventRing =
        ThreadEventRing::new(DEFAULT_THREAD_RING_CAPACITY);

    /// Each thread's shared ring shard. Populated lazily on the first call to
    /// `push_to_thread_ring` (or `global_ring_registry().register_current_thread()`).
    /// The `Arc<Mutex<…>>` is also inserted into the global registry so the
    /// dumper can find and drain it from another thread.
    static THREAD_REGISTERED_RING: RefCell<Option<Arc<Mutex<VecDeque<EventInstance>>>>> =
        const { RefCell::new(None) };
}

/// Global registry of per-thread shared ring shards.
///
/// Each producer thread registers a single `Arc<Mutex<VecDeque<EventInstance>>>`
/// on its first JFR emit. Producers push to *their own* shard (the per-thread
/// mutex is therefore uncontended on the hot path); the dumper drains all
/// shards by locking each one briefly in turn.
///
/// The registry-level mutex is only taken at:
///   - first-emit registration (once per thread, lifetime)
///   - `drain_all` (typically once per dump interval)
///
/// Steady-state emits never touch the registry-level mutex.
pub struct ThreadRingRegistry {
    rings: Mutex<Vec<Arc<Mutex<VecDeque<EventInstance>>>>>,
    /// Bounded capacity propagated to each newly-registered thread shard.
    shard_capacity: usize,
}

impl ThreadRingRegistry {
    /// Create a registry where each newly registered thread shard will be
    /// bounded to `shard_capacity` events (drop-oldest on overflow).
    pub fn new(shard_capacity: usize) -> Self {
        Self {
            rings: Mutex::new(Vec::new()),
            shard_capacity: shard_capacity.max(1),
        }
    }

    /// Returns the per-thread shared ring for the calling thread, creating
    /// and registering it on first call. Subsequent calls from the same
    /// thread return the same `Arc` (no registry-mutex traffic).
    ///
    /// The returned `Arc` is cloned into the registry so the dumper can drain
    /// it; the producer keeps its own clone in the thread-local cell for fast
    /// re-access.
    pub fn register_current_thread(&self) -> Arc<Mutex<VecDeque<EventInstance>>> {
        // Fast path: already registered for this thread.
        if let Some(existing) = THREAD_REGISTERED_RING.with(|cell| cell.borrow().clone()) {
            return existing;
        }
        // Slow path: allocate, install into thread-local, and publish to registry.
        let ring = Arc::new(Mutex::new(VecDeque::with_capacity(self.shard_capacity)));
        THREAD_REGISTERED_RING.with(|cell| {
            *cell.borrow_mut() = Some(Arc::clone(&ring));
        });
        // Publish a clone to the global registry so drainers can find it.
        if let Ok(mut rings) = self.rings.lock() {
            rings.push(Arc::clone(&ring));
        }
        ring
    }

    /// Drain every registered thread shard, returning the merged event stream.
    ///
    /// Events are **not** in strict timestamp order across shards — each shard
    /// is drained in insertion order, then concatenated. Callers that need a
    /// time-ordered stream (e.g. the dumper) should sort by `start_time`.
    pub fn drain_all(&self) -> Vec<EventInstance> {
        // Snapshot the Arc list under the registry lock so we don't hold it
        // while draining each shard. The Arc clones make the list cheap to copy.
        let shards: Vec<Arc<Mutex<VecDeque<EventInstance>>>> = match self.rings.lock() {
            Ok(guard) => guard.iter().map(Arc::clone).collect(),
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for shard in shards {
            if let Ok(mut q) = shard.lock() {
                out.reserve(q.len());
                out.extend(q.drain(..));
            }
        }
        out
    }

    /// Number of registered thread shards. Useful for tests and diagnostics.
    pub fn registered_thread_count(&self) -> usize {
        self.rings.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Bounded capacity used when registering new thread shards.
    pub fn shard_capacity(&self) -> usize {
        self.shard_capacity
    }
}

impl Default for ThreadRingRegistry {
    fn default() -> Self {
        Self::new(DEFAULT_THREAD_RING_CAPACITY)
    }
}

static GLOBAL_RING_REGISTRY: OnceLock<ThreadRingRegistry> = OnceLock::new();

/// Returns the process-wide `ThreadRingRegistry`. The first call lazily
/// initialises it with `DEFAULT_THREAD_RING_CAPACITY`.
pub fn global_ring_registry() -> &'static ThreadRingRegistry {
    GLOBAL_RING_REGISTRY.get_or_init(ThreadRingRegistry::default)
}

/// Push an event onto the calling thread's ring shard, registering it with
/// the global registry on first call.
///
/// Hot-path cost (steady state, no contention):
///   - 1 thread-local access to fetch the cached `Arc` clone
///   - 1 uncontended mutex lock + `VecDeque::push_back` + unlock
///   - possible `VecDeque::pop_front` if the ring is at capacity
///
/// First-call cost (once per thread, ever):
///   - 1 `Arc::new` + `Mutex::new` + `VecDeque::with_capacity` allocation
///   - 1 global-registry mutex lock to publish the new shard
pub fn push_to_thread_ring(ev: EventInstance) {
    // Try the thread-local fast path first to avoid even touching the registry
    // on every call. The thread-local cell holds an `Arc` clone of the shard.
    let ring = THREAD_REGISTERED_RING.with(|cell| cell.borrow().clone());
    let ring = match ring {
        Some(r) => r,
        None => global_ring_registry().register_current_thread(),
    };
    let capacity = global_ring_registry().shard_capacity();
    // Bind the lock result to a named let so its PoisonError variant's
    // destructor (which holds the MutexGuard borrowing from `ring`) drops
    // before `ring` itself at end-of-function — avoids E0597.
    let lock_result = ring.lock();
    if let Ok(mut q) = lock_result {
        if q.len() >= capacity {
            let _ = q.pop_front();
        }
        q.push_back(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: vec![],
        }
    }

    #[test]
    fn test_new_repository() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
        assert_eq!(repo.total_recorded(), 0);
    }

    #[test]
    fn test_default_repository() {
        let repo = EventRepository::default();
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
    }

    #[test]
    fn test_push_single() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        assert_eq!(repo.len(), 1);
        assert!(!repo.is_empty());
        assert_eq!(repo.total_recorded(), 1);
    }

    #[test]
    fn test_push_multiple() {
        let mut repo = EventRepository::new(10);
        for i in 0..5 {
            repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(repo.len(), 5);
        assert_eq!(repo.total_recorded(), 5);
    }

    #[test]
    fn test_eviction_at_capacity() {
        let mut repo = EventRepository::new(3);
        for i in 0..5u64 {
            repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(repo.len(), 3);
        assert_eq!(repo.total_recorded(), 5);
        // Oldest events should be evicted
        let events = repo.events();
        assert_eq!(events[0].start_time, 200);
        assert_eq!(events[1].start_time, 300);
        assert_eq!(events[2].start_time, 400);
    }

    #[test]
    fn test_eviction_capacity_one() {
        let mut repo = EventRepository::new(1);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(1), 300, 400));
        assert_eq!(repo.len(), 1);
        assert_eq!(repo.total_recorded(), 2);
        assert_eq!(repo.events()[0].start_time, 300);
    }

    #[test]
    fn test_events_returns_slice() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 300, 400));
        let events = repo.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_id, EventTypeId(1));
        assert_eq!(events[1].type_id, EventTypeId(2));
    }

    #[test]
    fn test_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(1), 200, 300));
        assert_eq!(repo.len(), 2);
        repo.clear();
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
        // total_recorded is NOT cleared by clear()
        assert_eq!(repo.total_recorded(), 2);
    }

    #[test]
    fn test_events_by_type_single_type() {
        let mut repo = EventRepository::new(10);
        let t1 = EventTypeId(1);
        repo.push(make_event(t1, 100, 200));
        repo.push(make_event(t1, 200, 300));
        let matched = repo.events_by_type(t1);
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn test_events_by_type_mixed() {
        let mut repo = EventRepository::new(10);
        let t1 = EventTypeId(1);
        let t2 = EventTypeId(2);
        let t3 = EventTypeId(3);
        repo.push(make_event(t1, 100, 200));
        repo.push(make_event(t2, 200, 300));
        repo.push(make_event(t1, 300, 400));
        repo.push(make_event(t3, 400, 500));
        assert_eq!(repo.events_by_type(t1).len(), 2);
        assert_eq!(repo.events_by_type(t2).len(), 1);
        assert_eq!(repo.events_by_type(t3).len(), 1);
    }

    #[test]
    fn test_events_by_type_none_found() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        assert_eq!(repo.events_by_type(EventTypeId(99)).len(), 0);
    }

    #[test]
    fn test_events_by_type_empty_repo() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
    }

    #[test]
    fn test_events_in_range_inclusive() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        repo.push(make_event(EventTypeId(1), 200, 250));
        repo.push(make_event(EventTypeId(1), 300, 350));
        // Range [200, 300] should include events at start_time 200 and 300
        let in_range = repo.events_in_range(200, 300);
        assert_eq!(in_range.len(), 2);
    }

    #[test]
    fn test_events_in_range_none_match() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        repo.push(make_event(EventTypeId(1), 500, 550));
        let in_range = repo.events_in_range(200, 400);
        assert_eq!(in_range.len(), 0);
    }

    #[test]
    fn test_events_in_range_exact_boundary() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        // Range [100, 100] should include event at start_time 100
        let in_range = repo.events_in_range(100, 100);
        assert_eq!(in_range.len(), 1);
    }

    #[test]
    fn test_events_in_range_empty_repo() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.events_in_range(0, 1000).len(), 0);
    }

    #[test]
    fn test_total_recorded_persists_after_eviction() {
        let mut repo = EventRepository::new(2);
        for i in 0..10u64 {
            repo.push(make_event(EventTypeId(1), i, i + 1));
        }
        assert_eq!(repo.len(), 2);
        assert_eq!(repo.total_recorded(), 10);
    }

    #[test]
    fn test_push_after_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.clear();
        repo.push(make_event(EventTypeId(2), 300, 400));
        assert_eq!(repo.len(), 1);
        assert_eq!(repo.events()[0].type_id, EventTypeId(2));
    }

    #[test]
    fn test_type_index_after_eviction() {
        let mut repo = EventRepository::new(2);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 200, 300));
        repo.push(make_event(EventTypeId(1), 300, 400));
        // First event (type 1) was evicted, only the new type 1 remains
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 1);
        assert_eq!(repo.events_by_type(EventTypeId(2)).len(), 1);
    }

    #[test]
    fn test_type_index_cleared_on_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.clear();
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
    }

    #[test]
    fn test_iter() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 200, 300));
        let collected: Vec<_> = repo.iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].type_id, EventTypeId(1));
        assert_eq!(collected[1].type_id, EventTypeId(2));
    }

    // -----------------------------------------------------------------------
    // ThreadEventRing tests
    // -----------------------------------------------------------------------

    #[test]
    fn thread_event_ring_push_and_drain_round_trip() {
        let ring = ThreadEventRing::new(8);
        assert!(ring.is_empty());
        ring.push(make_event(EventTypeId(1), 100, 200));
        ring.push(make_event(EventTypeId(2), 300, 400));
        assert_eq!(ring.len(), 2);

        let mut out = Vec::new();
        ring.drain_into(&mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].start_time, 100);
        assert_eq!(out[1].start_time, 300);
        assert!(ring.is_empty());
    }

    #[test]
    fn thread_event_ring_drops_oldest_on_overflow() {
        let ring = ThreadEventRing::new(3);
        for i in 0..5u64 {
            ring.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(ring.len(), 3);
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        // Expect the last 3 inserts (start_time 200, 300, 400).
        let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
        assert_eq!(starts, vec![200, 300, 400]);
    }

    #[test]
    fn thread_event_ring_zero_capacity_is_clamped_to_one() {
        let ring = ThreadEventRing::new(0);
        assert_eq!(ring.capacity(), 1);
        ring.push(make_event(EventTypeId(1), 100, 200));
        ring.push(make_event(EventTypeId(1), 300, 400));
        assert_eq!(ring.len(), 1);
    }

    // -----------------------------------------------------------------------
    // ThreadRingRegistry / push_to_thread_ring tests
    // -----------------------------------------------------------------------

    #[test]
    fn push_to_thread_ring_records_event_visible_to_drain_all() {
        // Push an event with a unique tag, then verify it is reachable through
        // the same Arc shard that `register_current_thread` returns. This
        // avoids races against other tests that may concurrently call
        // `drain_all` on the global registry.
        let unique_start = 0xDEAD_BEEF_u64;
        push_to_thread_ring(make_event(EventTypeId(7), unique_start, unique_start + 1));

        // Re-fetch the same shard — `register_current_thread` is idempotent
        // and returns the cached Arc for the current thread.
        let shard = global_ring_registry().register_current_thread();
        let q = shard.lock().expect("shard mutex poisoned");
        assert!(
            q.iter().any(|e| e.start_time == unique_start && e.type_id == EventTypeId(7)),
            "expected pushed event to be visible in this thread's shard (len={})",
            q.len(),
        );
    }

    #[test]
    fn ring_drops_oldest_on_overflow() {
        // Construct an isolated registry so this test does not collide with
        // any other registry traffic in the test binary.
        let registry = ThreadRingRegistry::new(3);
        let ring = registry.register_current_thread();
        // Push 5 events, capacity = 3 — first two should be dropped.
        for i in 0..5u64 {
            let mut q = ring.lock().unwrap();
            if q.len() >= registry.shard_capacity() {
                let _ = q.pop_front();
            }
            q.push_back(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        let drained = registry.drain_all();
        let starts: Vec<u64> = drained.iter().map(|e| e.start_time).collect();
        assert_eq!(starts, vec![200, 300, 400]);
    }

    #[test]
    fn multiple_threads_register_independently() {
        use std::sync::Arc;
        use std::sync::Barrier;
        use std::thread;

        let registry = Arc::new(ThreadRingRegistry::new(64));
        let n_threads = 4usize;
        let barrier = Arc::new(Barrier::new(n_threads));

        let mut handles = Vec::with_capacity(n_threads);
        for t in 0..n_threads {
            let reg = Arc::clone(&registry);
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b.wait();
                let ring = reg.register_current_thread();
                // Each thread pushes a unique-tagged event.
                let mut q = ring.lock().unwrap();
                q.push_back(make_event(EventTypeId(t as u32 + 1), 1000 + t as u64, 2000 + t as u64));
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }

        // Each worker thread should have registered its own distinct shard.
        assert_eq!(registry.registered_thread_count(), n_threads);

        let drained = registry.drain_all();
        assert_eq!(drained.len(), n_threads);
        // Verify every type_id 1..=n_threads is represented exactly once.
        for t in 0..n_threads {
            let expected = EventTypeId(t as u32 + 1);
            let count = drained.iter().filter(|e| e.type_id == expected).count();
            assert_eq!(count, 1, "expected exactly one event for type {:?}", expected);
        }
    }
}
