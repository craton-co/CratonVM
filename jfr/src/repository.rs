// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// AUDIT 2026-05-16: std HashMap unused (replaced by FxHashMap below).
use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use rustc_hash::FxHashMap;

use crate::event::{EventInstance, EventTypeId};

// 2026-05-17 CRIT-fix (Bug 2): the per-thread shard was `Arc<Mutex<VecDeque>>>`,
// which took a `std::sync::Mutex` on every event push — directly contradicting
// the docs that promised "SPSC/lock-free per-thread ring". This file now ships
// a real lock-free single-producer/single-consumer ring (`SpscEventRing`)
// backed by atomics. The producer is the owning thread (registers the shard
// via `register_current_thread`, pushes via `push_to_thread_ring`); the
// consumer is whichever thread calls `drain_all` (the dump path).
//
// Round-4 (2026-05-17): the registry-level lock is now a
// `parking_lot::RwLock` instead of `std::sync::Mutex` — drains and shard
// counts proceed concurrently under read locks; only first-time registration
// takes the write lock.

/// In-memory event storage with ring-buffer eviction.
///
/// Uses a `VecDeque` for O(1) front eviction and maintains a secondary
/// `type_index` mapping `EventTypeId` to buffer indices for O(1) type queries.
///
/// Round-4 (2026-05-17) eviction fix: the per-type index is now a
/// `VecDeque<usize>` instead of `Vec<usize>`. The previous design used
/// `Vec::position(...).swap_remove(...)` on every eviction — a *linear scan*
/// of the indices for the evicted event's type (O(N_type) per push at
/// steady-state once the ring is full, where N_type could be in the
/// thousands for hot types like `jdk.ObjectAllocationSample`).
///
/// Because events are pushed in monotonic `abs_index` order, the evicted
/// event's index for its type is *always* the front of the per-type deque,
/// so `pop_front()` gives O(1) eviction without any search.
pub struct EventRepository {
    events: VecDeque<EventInstance>,
    max_events: usize,
    /// obsaudit D12 (2026-07-26): retention age in nanoseconds, from
    /// `RecordingSettings::max_age`. `None` (the default, via [`Self::new`])
    /// means no age-based eviction — only `max_events` bounds the ring, same
    /// as before this field existed. Set via [`Self::with_max_age`].
    max_age_nanos: Option<u64>,
    total_recorded: u64,
    /// Index from event type_id to the set of logical indices (offset from
    /// `total_recorded - events.len()`). Maintained on push/evict/clear.
    /// T10.9.B: FxHashMap — EventTypeId is internal.
    /// Round-4: per-type list is a `VecDeque<usize>` so eviction is
    /// `pop_front()` (O(1)) instead of `position()` + `swap_remove`
    /// (O(N_type) linear scan).
    type_index: FxHashMap<EventTypeId, VecDeque<usize>>,
    /// The absolute index of the first element currently in `events`.
    /// Equals `total_recorded - events.len()` after each push.
    base_index: u64,
}

impl EventRepository {
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max_events,
            max_age_nanos: None,
            total_recorded: 0,
            type_index: FxHashMap::default(),
            base_index: 0,
        }
    }

    /// Like [`Self::new`], but also enforces a retention age (obsaudit D12).
    /// `max_age_nanos` is compared against each pushed event's own
    /// `start_time` as the reference "now" — the repository has no wall
    /// clock of its own, and using the just-pushed event's timestamp keeps
    /// eviction driven purely by the event stream's own ordering, with no
    /// dependency on system time inside this library.
    pub fn with_max_age(max_events: usize, max_age_nanos: Option<u64>) -> Self {
        Self {
            max_age_nanos,
            ..Self::new(max_events)
        }
    }

    /// Evict the single oldest (front) event, fixing up `type_index` and
    /// `base_index` to match. No-op if the repository is empty. Shared by
    /// the size-cap and age-cap eviction passes in [`Self::push`].
    fn evict_front(&mut self) {
        if let Some(evicted) = self.events.pop_front() {
            // Remove evicted event from type index. Round-4 fix:
            // because events are pushed in monotonic abs-index order,
            // the front of the per-type deque is always the evicted
            // event's index — no search needed. O(1) instead of the
            // prior O(N_type) `position()` linear scan.
            let drop_type = if let Some(indices) = self.type_index.get_mut(&evicted.type_id) {
                // Defensive: the front *should* equal base_index, but if
                // a future caller mutates state out of order we still
                // produce a correct (if slower) answer by scanning.
                let target = self.base_index as usize;
                if indices.front().copied() == Some(target) {
                    indices.pop_front();
                } else if let Some(pos) = indices.iter().position(|&i| i == target) {
                    // Fallback path — preserves correctness if the
                    // monotonic invariant is ever broken.
                    indices.remove(pos);
                }
                indices.is_empty()
            } else {
                false
            };
            if drop_type {
                self.type_index.remove(&evicted.type_id);
            }
            self.base_index += 1;
        }
    }

    /// Push an event, evicting the oldest if over the size limit and/or (if
    /// `max_age_nanos` is set — obsaudit D12) older than the retention
    /// window. Both eviction passes are O(1) amortized per evicted event.
    pub fn push(&mut self, event: EventInstance) {
        if self.events.len() >= self.max_events {
            // Evict oldest (front) — O(1) with VecDeque
            self.evict_front();
        }
        if let Some(max_age_nanos) = self.max_age_nanos {
            let cutoff = event.start_time.saturating_sub(max_age_nanos);
            while let Some(front) = self.events.front() {
                if front.start_time >= cutoff {
                    break;
                }
                self.evict_front();
            }
        }
        // Add to type index
        let abs_index = self.total_recorded as usize;
        self.type_index
            .entry(event.type_id)
            .or_default()
            .push_back(abs_index);

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

    /// Random-access read of the `rel`-th currently-buffered event (0-based,
    /// relative to `base_index`). O(1) — the backing `VecDeque` indexes in
    /// constant time.
    ///
    /// Round-5 HIGH-fix (Bug 5, 2026-05-17): `EventStream::next_event` was
    /// calling `iter().nth(rel)` for each call, which is O(rel) and made a
    /// full filtered drain quadratic in the number of events. Callers
    /// should use this method when they already know the relative index.
    #[inline]
    pub fn get(&self, rel: usize) -> Option<&EventInstance> {
        self.events.get(rel)
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

    /// Return an iterator over events matching the given type id.
    /// Uses the type_index for O(1) lookup of matching indices.
    ///
    /// Round-5: this is the allocation-free primary API. Use `events_by_type`
    /// only when a `Vec` is genuinely required (e.g. by `.len()` from older
    /// test code). Callers that just iterate should call this directly.
    pub fn iter_by_type(&self, type_id: EventTypeId) -> impl Iterator<Item = &EventInstance> {
        // `into_iter` of `Option<&VecDeque<usize>>` -> flatten yields the
        // empty iterator when no indices exist for this type, avoiding the
        // allocation that the old `Vec::new()` branch required.
        let base = self.base_index as usize;
        self.type_index
            .get(&type_id)
            .into_iter()
            .flat_map(|abs_indices| abs_indices.iter())
            .filter_map(move |&abs_idx| {
                let rel = abs_idx.checked_sub(base)?;
                self.events.get(rel)
            })
    }

    /// Return references to events matching the given type id.
    ///
    /// Prefer [`iter_by_type`](Self::iter_by_type) for iteration — this
    /// allocator-eager wrapper exists only for callers that need to take
    /// `.len()` or pass a slice elsewhere.
    pub fn events_by_type(&self, type_id: EventTypeId) -> Vec<&EventInstance> {
        self.iter_by_type(type_id).collect()
    }

    /// Return references to events whose start_time falls within [start, end].
    ///
    /// Perf note: this is intentionally a full linear scan. A binary-search
    /// lower bound would be tempting, but `events` is NOT reliably sorted by
    /// `start_time` — `ThreadRingRegistry::drain_all` concatenates per-thread
    /// shards in producer-insertion order with "no global ordering enforced
    /// across shards" (see recording.rs / the dumper sorts only for output).
    /// A binary search on unsorted data would silently drop matching events,
    /// so the O(n) scan is required for correctness.
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
static NEXT_THREAD_RING_REGISTRY_ID: AtomicUsize = AtomicUsize::new(1);

/// Per-thread bounded ring of pending events.
///
/// This type is a thin wrapper around `RefCell<VecDeque<EventInstance>>` that
/// exposes a "push or drop-oldest" semantics. It is intentionally `!Sync` and
/// `!Send` (because of `RefCell`), since it is only ever borrowed from the
/// owning thread. Cross-thread drainage of the *same* event stream is handled
/// separately via `ThreadRingRegistry`, which holds `Arc<SpscEventRing>`
/// shards (a real lock-free SPSC ring; producers and the dump consumer
/// touch disjoint atomic counters and disjoint slots).
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

// ---------------------------------------------------------------------------
// Lock-free SPSC per-thread event ring (CRIT-fix Bug 2, 2026-05-17)
// ---------------------------------------------------------------------------
//
// The shard a producer thread pushes into, and the dump thread drains from,
// is now a true single-producer / single-consumer ring with no mutex on the
// hot path. The invariants are:
//
//   * The owning producer thread is the *only* writer. It is the unique caller
//     of `SpscEventRing::push` for this shard (enforced by the thread-local
//     `THREAD_REGISTERED_RINGS`).
//   * Consumers are *serialized* per ring (round-5 CRIT-fix, 2026-05-17): a
//     `consumer_busy: AtomicBool` on each `SpscEventRing` is CAS-acquired by
//     the would-be consumer in `try_pop`. If the CAS fails, `try_pop` returns
//     `None` and the contending consumer simply leaves the events for the
//     next drain pass. This preserves SPSC semantics (still exactly one
//     consumer touching the slot array at any instant) while allowing
//     multiple unsynchronized call sites of `drain_all` (`recording.rs`,
//     `dump.rs`, test fixtures) to be safe under `RwLock::read()` sharing.
//     Producer-becomes-consumer is also safe: a producer that enters
//     `drain_all` for its own shard will lose the CAS to any concurrent
//     drainer and skip the shard, rather than racing on `assume_init_read`.
//
// Memory ordering follows the standard Lamport/Vyukov SPSC pattern:
//   producer: load tail (Acquire) → write slot → store head (Release)
//   consumer: load head (Acquire) → read slot → store tail (Release)
//
// Capacity is rounded up to a power of two so `idx & mask` indexes the
// backing array. Head and tail are monotonically increasing `usize`s and are
// masked at access time. Because the counters are unbounded, the full-check
// is `head - tail == capacity` (using `wrapping_sub` to survive rollover);
// the empty-check is `head == tail`. This sidesteps the classic ring-buffer
// "lose one slot to distinguish full from empty" by using the full counter
// range, paying with a slightly more complex full-check.
//
// Bounded-overflow behaviour: when the producer finds the ring full, it
// DROPS THE INCOMING EVENT. This is a deliberate change from the previous
// `drop-oldest` (which a true SPSC ring cannot do without violating the
// producer/consumer split). For JFR's "best-effort completeness, never block
// the producer" contract, drop-newest is acceptable and matches the spec in
// the fix prompt.

/// Round a positive capacity request up to the next power of two, with a
/// minimum of 1. A power-of-two capacity lets us mask indices instead of
/// modding them.
#[inline]
fn next_power_of_two(n: usize) -> usize {
    let n = n.max(1);
    if n.is_power_of_two() {
        n
    } else {
        n.next_power_of_two()
    }
}

/// A single-producer / serialized-consumer bounded ring of `EventInstance`s.
///
/// Lock-free in the steady state: producer and consumer touch disjoint atomic
/// counters (`head` / `tail`) and disjoint slots. The slot array is a `Vec`
/// of `UnsafeCell<MaybeUninit<EventInstance>>` — the producer initializes
/// slots in `[tail, head)`, the consumer takes ownership when it pops.
///
/// Safety contract: this type is `Sync` because (a) the producer is unique
/// per ring (enforced by the thread-local `THREAD_REGISTERED_RINGS`) and
/// (b) consumers are *serialized* by the `consumer_busy` CAS gate inside
/// `try_pop` — at any instant exactly one thread holds the consumer role
/// for a given ring. Round-5 CRIT-fix (2026-05-17): the previous single-
/// declared-consumer contract was not enforceable because `drain_all` is
/// invoked from 5+ unsynchronized sites in `recording.rs` and `dump.rs`
/// under `RwLock::read()`, which allows concurrent readers. The CAS gate
/// converts those would-be races into a clean "skip this shard, the other
/// drainer is consuming it" return path.
pub struct SpscEventRing {
    /// Backing storage; length = capacity, all slots logically uninitialised
    /// outside `[tail, head)` (mod capacity).
    slots: Box<[UnsafeCell<MaybeUninit<EventInstance>>]>,
    /// Producer-owned write index (monotonic, masked at access).
    head: AtomicUsize,
    /// Consumer-owned read index (monotonic, masked at access).
    tail: AtomicUsize,
    /// `capacity - 1`. Capacity is a power of two so `idx & mask` indexes.
    mask: usize,
    /// Producer-private cache of the last-observed `tail`. Used to skip the
    /// Acquire load of the consumer counter on every `push`: the producer
    /// only reloads when `head - cached_tail >= capacity` (i.e. the ring
    /// *might* be full according to a stale snapshot). On x86 the cost is
    /// nominal, but on ARM64 this elides an LDAR per emit. Accessed only
    /// from the (unique) producer thread, so `UnsafeCell<usize>` is sound.
    /// (Round-5 perf finding 6, 2026-05-17.)
    cached_tail: UnsafeCell<usize>,
    /// Consumer-side serialization gate. `try_pop` CAS-acquires this before
    /// touching the slot array; on contention it returns `None` and leaves
    /// the events for the holder of the gate to drain. Round-5 CRIT-fix
    /// (2026-05-17) for the multi-consumer UB hazard.
    consumer_busy: AtomicBool,
    /// Count of events discarded by the drop-newest overflow policy.
    ///
    /// Bug 2 fix (silent event loss): a full ring previously dropped the
    /// incoming event with no record, so a JFR consumer had no way to tell
    /// the recording was incomplete. Every `push` that hits the full-ring
    /// path increments this counter; read it via `dropped_events()` (and
    /// `total_dropped_events()` for the process-wide sum across all shards).
    ///
    /// Incremented by the (unique) producer with a Relaxed RMW — it is a
    /// pure diagnostic statistic and needs no ordering relative to the slot
    /// stores.
    dropped: AtomicU64,
    /// Bounded wait applied in `Drop` when a consumer is observed to hold the
    /// `consumer_busy` gate. After this many wall-clock nanoseconds elapse
    /// the drop path falls through to the best-effort drain. See the
    /// `Drop` impl for the full shutdown contract.
    ///
    /// Defaults to [`DEFAULT_SPSC_SHUTDOWN_TIMEOUT`] (1 second), well below
    /// the prior ~100s spin-park ramp. Construct with
    /// [`SpscEventRing::with_shutdown_timeout`] to override (mainly for tests
    /// that intentionally wedge a consumer).
    shutdown_timeout_nanos: AtomicU64,
    /// Set to `true` by the owning producer thread's `Drop` guard
    /// (`RegisteredRingGuard` held in `THREAD_REGISTERED_RINGS`) when that
    /// thread exits. A retired ring will never receive another producer push
    /// (its producer is gone), so once it has also been fully drained the
    /// registry can drop its clone and reclaim the 1024-slot shard.
    ///
    /// Registry-leak fix (HIGH, 2026-06-17): `ThreadRingRegistry::rings`
    /// previously only ever grew — `register_current_thread` pushed a clone
    /// per producer thread and nothing was ever removed, so a finished
    /// producer thread leaked its shard forever and `drain_all` paid an
    /// O(dead_threads) cost walking corpses. This flag lets `drain_all`
    /// `retain()` out retired + empty shards lazily under its write lock.
    ///
    /// Relaxed: this is a one-way `false -> true` liveness hint, not a
    /// synchronisation point. The producer that flips it (in its thread-exit
    /// Drop) has, by definition, completed all of its own pushes before the
    /// thread can unwind; a drainer that observes `retired == true` then
    /// independently checks `is_empty()` (Acquire loads of head/tail) before
    /// reclaiming, so it never drops a ring with unread events.
    retired: AtomicBool,
}

/// Default `SpscEventRing` shutdown timeout — bounded wait for an in-flight
/// consumer at drop time. Chosen at 1 second: large enough to absorb a
/// realistic full-buffer drain (1024 slots * a few μs per pop ≪ 1 ms in
/// practice), small enough that VM shutdown does not visibly stall on a
/// wedged consumer. Tests can override per-ring via
/// [`SpscEventRing::with_shutdown_timeout`].
pub const DEFAULT_SPSC_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

// SAFETY: `SpscEventRing` enforces the SPSC discipline at runtime:
//   * The producer is unique by construction (one Arc<SpscEventRing> per
//     producing thread, kept in `THREAD_REGISTERED_RINGS`).
//   * Consumers are serialized by the `consumer_busy` AtomicBool CAS in
//     `try_pop`, so only one thread at a time reads the slot array.
// The `UnsafeCell<MaybeUninit<EventInstance>>` slots are partitioned by
// the atomic head/tail counters so producer and (the single active)
// consumer never touch the same slot at the same time. The producer-only
// `UnsafeCell<usize>` cached_tail is never touched by consumers.
// `EventInstance` is `Send`.
unsafe impl Sync for SpscEventRing {}
// SAFETY: the same SPSC ownership and atomic-publication argument above means
// ownership of the ring may move between threads without racing slot access.
unsafe impl Send for SpscEventRing {}

impl SpscEventRing {
    /// Create a new ring with capacity rounded up to the next power of two
    /// (minimum 1).
    ///
    /// `pub(crate)`: the `unsafe impl Sync` is sound only because the producer
    /// is unique per ring (enforced by the thread-local `THREAD_REGISTERED_RINGS`
    /// in `ThreadRingRegistry::register_current_thread`). Exposing construction
    /// to other crates would let callers create rings outside that discipline
    /// and `push` from two threads → data race UB. External code must obtain
    /// rings via `ThreadRingRegistry`.
    pub(crate) fn new(requested_capacity: usize) -> Self {
        Self::with_shutdown_timeout(requested_capacity, DEFAULT_SPSC_SHUTDOWN_TIMEOUT)
    }

    /// Like [`SpscEventRing::new`], but with an explicit shutdown timeout.
    ///
    /// The timeout bounds how long `Drop` will wait for a consumer that is
    /// observed mid-`try_pop` / `drain_into`. After it elapses, `Drop` falls
    /// through to a best-effort drain (see the `Drop` impl for the full
    /// contract). Use this constructor in tests that intentionally wedge a
    /// consumer; production code can rely on the 1-second default via
    /// [`SpscEventRing::new`].
    pub(crate) fn with_shutdown_timeout(requested_capacity: usize, timeout: Duration) -> Self {
        let capacity = next_power_of_two(requested_capacity);
        let mut slots: Vec<UnsafeCell<MaybeUninit<EventInstance>>> = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            slots.push(UnsafeCell::new(MaybeUninit::uninit()));
        }
        // Saturate at u64::MAX nanoseconds (~584 years) — effectively unbounded
        // for any sensible Duration.
        let timeout_nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        Self {
            slots: slots.into_boxed_slice(),
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
            mask: capacity - 1,
            cached_tail: UnsafeCell::new(0),
            consumer_busy: AtomicBool::new(false),
            dropped: AtomicU64::new(0),
            shutdown_timeout_nanos: AtomicU64::new(timeout_nanos),
            // Registry-leak fix (2026-06-17): live until the owning thread exits.
            retired: AtomicBool::new(false),
        }
    }

    /// Override the shutdown timeout after construction. Atomic; callable
    /// from any thread. Has no effect on a `Drop` already in progress.
    pub fn set_shutdown_timeout(&self, timeout: Duration) {
        let nanos = u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX);
        self.shutdown_timeout_nanos.store(nanos, Ordering::Relaxed);
    }

    /// Current shutdown timeout (see [`SpscEventRing::set_shutdown_timeout`]).
    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_nanos(self.shutdown_timeout_nanos.load(Ordering::Relaxed))
    }

    /// Capacity (number of slots; always a power of two).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.mask + 1
    }

    /// Number of events this ring has discarded under the drop-newest
    /// overflow policy since creation. Non-zero means this thread's JFR
    /// event stream is incomplete — a consumer can surface this as a
    /// "dropped events" diagnostic.
    #[inline]
    pub fn dropped_events(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Producer-side push. Returns `Err(ev)` if the ring is full (drop-newest
    /// semantics — the caller decides what to do with the rejected event).
    ///
    /// SAFETY contract (caller-upheld): must be called from a single producer
    /// thread per ring. Multiple concurrent producers would race on `head`.
    ///
    /// `pub(crate)`: this is the producer half of the SPSC contract that the
    /// `unsafe impl Sync` relies on. Keeping it crate-private prevents external
    /// code from pushing concurrently from two threads (data race UB). The
    /// single-producer invariant is enforced by `THREAD_REGISTERED_RINGS`.
    pub(crate) fn push(&self, ev: EventInstance) -> Result<(), EventInstance> {
        // The producer is the only writer to `head`, so a Relaxed self-load
        // is fine — we already observe our own prior stores.
        let head = self.head.load(Ordering::Relaxed);
        let capacity = self.mask + 1;
        // Cached-tail fast path (round-5 perf finding 6, 2026-05-17). The
        // producer keeps a private snapshot of the last `tail` it observed.
        // Tail only ever advances, so if `head - cached_tail < capacity`
        // there is *definitely* free space and we can skip the Acquire load
        // of the consumer's counter. We only pay the Acquire when the
        // cached snapshot says the ring is full — at that point we must
        // re-check the real tail before dropping the event.
        //
        // SAFETY: `cached_tail` is producer-owned (only this thread reads
        // or writes it). The single-producer invariant is enforced by the
        // thread-local `THREAD_REGISTERED_RINGS` (see module header).
        let cached_tail = unsafe { *self.cached_tail.get() };
        let in_flight = head.wrapping_sub(cached_tail);
        let tail = if in_flight >= capacity {
            // Slow path: cached snapshot says we're full — reload the real
            // tail and refresh the cache. Acquire to synchronize with the
            // consumer's tail-Release in `try_pop`, so we observe the
            // consumer's freed slots.
            let real_tail = self.tail.load(Ordering::Acquire);
            // SAFETY: producer-only field (see above).
            unsafe {
                *self.cached_tail.get() = real_tail;
            }
            real_tail
        } else {
            cached_tail
        };
        // head/tail are unbounded monotonically-increasing counters; the
        // number of in-flight events is `head - tail` (wrapping). Full when
        // this equals capacity (every slot occupied). `wrapping_sub` keeps
        // the arithmetic correct across `usize` rollover.
        if head.wrapping_sub(tail) >= capacity {
            // Drop-newest overflow: the ring is full, so this event is
            // discarded. Bug 2 fix (silent event loss): record the drop so
            // a JFR consumer can tell the stream is incomplete. Relaxed is
            // sufficient — this is a diagnostic counter, not a sync point,
            // and the producer is its only writer.
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Err(ev);
        }
        let idx = head & self.mask;
        // SAFETY: producer is the unique writer to slot `idx` until we
        // publish via the head-Release below. The slot is logically
        // uninitialised at this point — either never written, or the
        // consumer popped it on a prior wrap (which moved tail past it,
        // synchronised via our tail-Acquire above).
        unsafe {
            (*self.slots[idx].get()).write(ev);
        }
        // Release so the consumer's Acquire-load of head sees a fully
        // initialised slot.
        self.head.store(head.wrapping_add(1), Ordering::Release);
        Ok(())
    }

    /// Consumer-side pop. Returns `None` if the ring is empty *or* if
    /// another consumer currently holds the serialization gate.
    ///
    /// Round-5 CRIT-fix (2026-05-17): consumers are serialized by a
    /// `consumer_busy` AtomicBool CAS. `drain_all` is invoked from many
    /// unsynchronized sites in `recording.rs` and `dump.rs` under
    /// `RwLock::read()`, which would otherwise let two threads enter
    /// `try_pop` for the same shard simultaneously and race on
    /// `assume_init_read` (undefined behaviour). On CAS failure we simply
    /// return `None` — the holder of the gate will drain the events, and
    /// the next call to `drain_all` will pick up anything pushed after.
    pub fn try_pop(&self) -> Option<EventInstance> {
        // Acquire the consumer gate. Acquire ordering pairs with the
        // Release store on the unlock side below, so the moment we observe
        // `consumer_busy == false` we also observe every slot read and
        // `tail` advance performed by the previous consumer.
        if self
            .consumer_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Another consumer is mid-drain on this shard. Skip — they will
            // drain what's there; we will catch up on the next pass.
            return None;
        }
        // From here until the Release store below, we are the unique
        // consumer of this ring (SPSC invariant holds).
        // Consumer owns `tail` while the gate is held — Relaxed self-load is fine.
        let tail = self.tail.load(Ordering::Relaxed);
        // Acquire to synchronize with producer's head-Release in `push`, so
        // we see the slot writes that preceded it.
        let head = self.head.load(Ordering::Acquire);
        if head == tail {
            // Empty — release the gate and report empty.
            self.consumer_busy.store(false, Ordering::Release);
            return None;
        }
        let idx = tail & self.mask;
        // SAFETY: producer published an initialised value at slot `idx` via
        // its head-Release; our head-Acquire above synchronises that write.
        // No other consumer races us — `consumer_busy` is held above.
        let ev = unsafe { (*self.slots[idx].get()).assume_init_read() };
        // Release so the producer's Acquire-load of tail in `push` sees the
        // slot as freed before observing the new tail value.
        self.tail.store(tail.wrapping_add(1), Ordering::Release);
        // Release the consumer gate. Release ordering publishes the slot
        // read + tail advance to the next consumer.
        self.consumer_busy.store(false, Ordering::Release);
        Some(ev)
    }

    /// Consumer-side drain into a Vec.
    ///
    /// Acquires the consumer-serialization gate once and pops events under
    /// that single CAS critical section, rather than CAS-acquiring per
    /// event as repeated `try_pop` calls would. If another consumer
    /// currently holds the gate this is a no-op (events will be drained
    /// by the holder, or on the next pass).
    pub fn drain_into(&self, out: &mut Vec<EventInstance>) {
        // Acquire the consumer gate (see `try_pop` for the full reasoning).
        if self
            .consumer_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        // Now the unique consumer. Pop in a tight loop without re-CAS.
        let mut tail = self.tail.load(Ordering::Relaxed);
        loop {
            let head = self.head.load(Ordering::Acquire);
            if head == tail {
                break;
            }
            let idx = tail & self.mask;
            // SAFETY: producer published this slot via head-Release; our
            // head-Acquire synchronises that write. No other consumer
            // races — `consumer_busy` is held.
            let ev = unsafe { (*self.slots[idx].get()).assume_init_read() };
            out.push(ev);
            tail = tail.wrapping_add(1);
            // Publish each tail advance so the producer (which may be
            // running in parallel) sees freed slots promptly.
            self.tail.store(tail, Ordering::Release);
        }
        // Release the consumer gate.
        self.consumer_busy.store(false, Ordering::Release);
    }

    /// Approximate count (non-atomic snapshot — useful for tests/diagnostics).
    pub fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    pub fn is_empty(&self) -> bool {
        self.head.load(Ordering::Acquire) == self.tail.load(Ordering::Acquire)
    }

    /// Mark this ring as retired — its owning producer thread has exited and
    /// will never push again. Registry-leak fix (2026-06-17): called from the
    /// per-thread `RegisteredRingGuard::drop`. Idempotent; Relaxed because it
    /// is a one-way liveness hint (see the `retired` field doc).
    #[inline]
    pub(crate) fn mark_retired(&self) {
        self.retired.store(true, Ordering::Relaxed);
    }

    /// True once the owning producer thread has exited (see [`mark_retired`]).
    /// A retired ring receives no further pushes, so once it is also empty the
    /// registry may drop its clone.
    #[inline]
    pub fn is_retired(&self) -> bool {
        self.retired.load(Ordering::Relaxed)
    }
}

impl Drop for SpscEventRing {
    /// Shutdown contract (task #31, HIGH soundness, 2026-05-24):
    ///
    ///   * Bounded wait. Waits at most `self.shutdown_timeout()` (default 1s,
    ///     configurable via [`SpscEventRing::set_shutdown_timeout`] or the
    ///     [`SpscEventRing::with_shutdown_timeout`] constructor) for an
    ///     observed in-flight consumer (`consumer_busy == true`) to release
    ///     the gate. This replaces the prior ~100-second spin-park ramp,
    ///     which could stall VM shutdown for over a minute on a wedged
    ///     consumer.
    ///
    ///   * Happy path. If the consumer releases the gate within the timeout
    ///     (or no consumer is observed), every buffered slot in `[tail, head)`
    ///     is dropped exactly once — `Arc<str>` payloads are released, slot
    ///     storage is freed. No leak.
    ///
    ///   * Wedged-consumer fallback. If the timeout elapses with
    ///     `consumer_busy == true`, the consumer is stuck inside `try_pop`
    ///     or `drain_into`. We still drain the ring best-effort:
    ///         - The slot at the observed `tail` (Acquire load) is the
    ///           consumer's potential active slot. Dropping it would race
    ///           with the consumer's `assume_init_read` (double-read of a
    ///           moved-from value) or `assume_init_drop` (double-free of
    ///           Arc<str>). We skip *exactly that one slot* — at most one
    ///           `EventInstance` is leaked per wedged-consumer shutdown.
    ///         - Every other initialised slot in `(tail, head)` is dropped
    ///           in place: payload `Arc<str>` clones are released.
    ///         - UAF fix (MED, 2026-06-20): the slot-array *storage* itself is
    ///           deliberately **leaked** (not freed) on this path. A parked
    ///           consumer will dereference `self.slots[idx]` at least once when
    ///           it resumes; freeing the backing here would make that a
    ///           use-after-free of freed (and possibly reallocated) memory.
    ///           Leaking the box keeps the storage mapped for the rest of the
    ///           process so the resume reads valid memory. This converts the
    ///           prior "consumer may touch freed storage" UAF into a bounded
    ///           one-shot leak of `capacity` slots — the acceptable
    ///           wedged-shutdown trade-off.
    ///     A warning is logged to stderr so the wedged consumer is visible
    ///     to operators.
    ///
    ///   * May drop up to N - 1 buffered events on the wedged path, where N
    ///     is `head - tail` at drop time (i.e. `len()`); the single skipped
    ///     slot's payload may leak, and on the wedged path the slot-array
    ///     backing is intentionally leaked to preserve memory safety.
    fn drop(&mut self) {
        let shutdown_timeout =
            Duration::from_nanos(self.shutdown_timeout_nanos.load(Ordering::Relaxed));
        let deadline = Instant::now().checked_add(shutdown_timeout);
        let mut consumer_wedged = false;

        // Single bounded wait. `park_timeout` sleeps even without a paired
        // `unpark`, so this is a cooperative poll on the gate. The 10 ms
        // poll interval keeps the worst-case overshoot small while remaining
        // negligible compared to the 1 s default budget.
        const POLL_INTERVAL: Duration = Duration::from_millis(10);
        while self.consumer_busy.load(Ordering::Acquire) {
            let remaining = match deadline {
                Some(d) => d.saturating_duration_since(Instant::now()),
                // Timeout overflowed `Instant` arithmetic (saturating treat
                // as "infinite"). Cap each park at the poll interval anyway.
                None => POLL_INTERVAL,
            };
            if remaining.is_zero() {
                consumer_wedged = true;
                break;
            }
            std::thread::park_timeout(remaining.min(POLL_INTERVAL));
        }

        // We're now in one of two states:
        //   (a) consumer_wedged == false: the gate is currently free (Acquire
        //       above synchronised with the consumer's Release store). Because
        //       we hold `&mut self`, no consumer can re-enter — no Arc clones
        //       to the ring remain reachable from any consumer call site by
        //       the time `Drop` runs. Safe to drop every initialised slot
        //       in [tail, head) exactly once.
        //   (b) consumer_wedged == true: a drainer is parked inside its
        //       `consumer_busy` critical section. It may still touch slot
        //       `tail` (the slot it is mid-reading) once it resumes — that
        //       single slot is unsafe to drop here.
        //
        // In both cases we walk the ring and drop initialised slots; (b)
        // additionally skips one slot to avoid a double-read / double-free
        // race window. See the doc comment above for the full contract.
        if consumer_wedged {
            eprintln!(
                "WARN: SpscEventRing dropped with consumer still mid-pop \
                 after {:?}; draining best-effort (1 slot may leak)",
                shutdown_timeout
            );
        }

        // Choose the start-of-drain index. In case (a) (gate free, no
        // wedged consumer) the relaxed `get_mut` read is fine — by hypothesis
        // no other thread is touching this ring. In case (b) the wedged
        // consumer may have advanced `tail` via its `drain_into` Release
        // stores after `get_mut` was synthesised; we must use an Acquire
        // load to synchronise with those stores, otherwise slots in
        // `[tail_start, consumer_advanced_tail)` would be already-consumed
        // (uninitialised) and `assume_init_drop` on them would be UB.
        let head = *self.head.get_mut();
        let tail_start = if consumer_wedged {
            self.tail.load(Ordering::Acquire)
        } else {
            *self.tail.get_mut()
        };
        // In the wedged path we additionally skip the *single* slot at the
        // observed tail — see the Drop doc comment above for the full
        // race analysis. The skipped slot is exactly the one the consumer
        // is either about to read (still initialised → we leak one Arc) or
        // has just read but not yet committed `tail+1` for (uninitialised
        // → we would UB if we touched it).
        let racing_slot_tail = if consumer_wedged {
            Some(tail_start)
        } else {
            None
        };

        let mut tail = tail_start;
        while tail != head {
            let idx = tail & self.mask;
            let skip = match racing_slot_tail {
                Some(active) => tail == active,
                None => false,
            };
            if !skip {
                // SAFETY: case (a): we are the unique accessor of the slot
                // array (gate observed free, `&mut self` blocks re-entry).
                // Case (b) with skip == false: the slot lies strictly past
                // the wedged consumer's `tail` (Acquire-loaded above). The
                // consumer can only ever touch the slot at its current
                // `tail`; until it advances `tail` past `idx`, slot `idx`
                // is owned by us. The wedged consumer (by definition stuck
                // inside the current critical section) will at most touch
                // `tail_start` on resume, never `idx > tail_start`.
                // The slot was initialised by the producer (`push`) and not
                // yet consumed. Each slot is dropped at most once: we walk
                // monotonic `tail..head` with `wrapping_add(1)`.
                unsafe {
                    (*self.slots[idx].get()).assume_init_drop();
                }
            }
            tail = tail.wrapping_add(1);
        }

        // Slot-storage reclamation — UAF fix (MED, 2026-06-20).
        //
        // The slot backing is a `Box<[UnsafeCell<MaybeUninit<EventInstance>>]>`.
        // `MaybeUninit` has no drop glue, so freeing the box only releases the
        // *storage* — it never double-drops the payloads we just dropped in the
        // loop above. The remaining question is purely *when* it is safe to free
        // that storage.
        //
        //   * Happy path (`consumer_wedged == false`): the gate was observed
        //     free under an Acquire load and `&mut self` blocks re-entry, so no
        //     consumer can dereference `self.slots` after this point. Freeing
        //     the storage now is sound. We let the auto-derived field drop do it.
        //
        //   * Wedged path (`consumer_wedged == true`): a consumer is parked
        //     *inside* its `consumer_busy` critical section. When it resumes it
        //     will dereference `self.slots[idx]` (e.g. the `assume_init_read` in
        //     `try_pop` / `drain_into`) at least once before observing the gate
        //     state again. If we freed the box here, that dereference would be a
        //     use-after-free of freed-and-possibly-reallocated storage — memory
        //     unsafety. The previous code took exactly that risk ("the consumer
        //     may briefly touch the skipped slot before observing the freed
        //     storage").
        //
        //     We close the UAF by *not freeing the backing at all* on the wedged
        //     path: the slot array is intentionally leaked so it stays mapped
        //     for the rest of the process lifetime. The parked consumer's resume
        //     then reads valid (if logically stale) memory instead of a dangling
        //     pointer. This trades a bounded one-shot leak (`capacity` slots for
        //     a single wedged shutdown) for memory safety — the documented,
        //     acceptable wedged-shutdown contract. The `eprintln!` above already
        //     surfaces the wedge to operators.
        if consumer_wedged {
            // Replace `slots` with an empty boxed slice so the auto-derived
            // field drop frees nothing, then leak the real backing. `Box::leak`
            // returns a `&'static mut` we deliberately discard: the storage is
            // never reclaimed, which is exactly the safety property we want
            // while a consumer may still dereference it.
            let leaked = std::mem::replace(&mut self.slots, Box::new([]));
            let _: &'static mut [UnsafeCell<MaybeUninit<EventInstance>>] = Box::leak(leaked);
        }
        // Happy path: `slots` (the Box) is freed by the auto-derived Drop after
        // this function returns — sound, because no consumer can reach it.
    }
}

/// Owns the producer thread's clone of its `Arc<SpscEventRing>` and, on the
/// thread's exit (when this guard's TLS slot is dropped), marks the ring
/// `retired` so the registry can later reclaim the shard.
///
/// Registry-leak fix (HIGH, 2026-06-17): without this, a finished producer
/// thread's shard stayed in `ThreadRingRegistry::rings` forever (unbounded
/// 1024-slot-per-dead-thread leak + O(dead_threads) drain cost). The guard's
/// `Drop` is the unregistration *signal*; the registry does the actual
/// `retain()` lazily in `drain_all` so we never touch the registry lock on a
/// thread-exit path (and never drop a ring that still has unread events).
///
/// The inner `Arc` is exposed via [`RegisteredRingGuard::ring`] so
/// `register_current_thread`'s fast path is a cheap clone of the same `Arc`
/// the registry holds.
struct RegisteredRingGuard {
    registry_key: usize,
    ring: Arc<SpscEventRing>,
}

impl RegisteredRingGuard {
    #[inline]
    fn ring(&self) -> &Arc<SpscEventRing> {
        &self.ring
    }

    #[inline]
    fn registry_key(&self) -> usize {
        self.registry_key
    }
}

impl Drop for RegisteredRingGuard {
    fn drop(&mut self) {
        // Thread is exiting: the (unique) producer for this shard is gone, so
        // flag the ring retired. The registry keeps its own clone alive; a
        // later `drain_all` will drop that clone once the ring is also empty.
        // We deliberately do NOT take the registry lock here — thread-exit TLS
        // destructors must stay cheap and lock-free.
        self.ring.mark_retired();
    }
}

thread_local! {
    /// Thread-local instance of the simple `ThreadEventRing` (single-threaded
    /// view, kept for API completeness; production emit paths should use
    /// `push_to_thread_ring` which goes through the `ThreadRingRegistry`).
    pub static THREAD_EVENT_RING: ThreadEventRing =
        ThreadEventRing::new(DEFAULT_THREAD_RING_CAPACITY);

    /// Each thread's shared ring shards, keyed by `ThreadRingRegistry`
    /// identity. Populated lazily on the first call to `push_to_thread_ring`
    /// (for the global registry) or to a specific registry's
    /// `register_current_thread()`. The `Arc<SpscEventRing>` is also inserted
    /// into that registry so the dumper can find and drain it from another
    /// thread.
    ///
    /// Registry-leak fix (2026-06-17): the cell now holds a
    /// `RegisteredRingGuard` (rather than a bare `Arc<SpscEventRing>`) whose
    /// `Drop` marks the ring `retired` when the thread exits, letting
    /// `drain_all` reclaim the shard.
    ///
    /// Registry-scope fix (2026-07-02): this is a Vec rather than one global
    /// slot so tests and embedders can use multiple `ThreadRingRegistry`
    /// instances on the same OS thread without leaking events across
    /// registries.
    static THREAD_REGISTERED_RINGS: RefCell<Vec<RegisteredRingGuard>> =
        const { RefCell::new(Vec::new()) };
}

/// Global registry of per-thread SPSC ring shards.
///
/// Each producer thread registers a single `Arc<SpscEventRing>` on its first
/// JFR emit. Producers push to *their own* shard (lock-free, no atomics on
/// other shards); the dumper drains all shards by popping each one to
/// exhaustion in turn.
///
/// The registry-level lock is only taken at:
///   - first-emit registration (once per thread, lifetime — write lock)
///   - `drain_all` (typically once per dump interval, read lock), to snapshot
///     the list of shards. The lock is released before we touch any shard.
///
/// Steady-state emits never touch the registry-level lock.
///
/// Round-4 (2026-05-17) drain_all race fix: the registry now uses a
/// `parking_lot::RwLock` instead of `std::sync::Mutex`. Critical reasoning:
///   * `register_current_thread` is the only writer (push to the shard list),
///     and runs at most once per producer thread's lifetime.
///   * `drain_all` and `registered_thread_count` are pure readers (clone Arcs
///     out and release the lock before touching the shards).
///   * Multiple dump threads no longer block each other on the registry-level
///     critical section; the SPSC discipline still requires that the *same*
///     shard never be drained by two threads concurrently, but the registry
///     itself is now read-shared safely.
///
/// The dumper consumer-side races are addressed at the shard level: each
/// `SpscEventRing` carries a `consumer_busy` AtomicBool CAS gate that
/// serializes any number of would-be drainers down to at most one active
/// consumer per shard at any instant (round-5 CRIT-fix, 2026-05-17). Losing
/// drainers simply skip the shard; the winner drains to empty. This is what
/// makes it safe for the many unsynchronized `drain_all` call sites to share
/// a `RwLock::read()` snapshot of the shard list without violating SPSC.
pub struct ThreadRingRegistry {
    registry_id: usize,
    rings: RwLock<Vec<Arc<SpscEventRing>>>,
    /// Bounded capacity propagated to each newly-registered thread shard.
    /// The actual ring capacity may be rounded up to the next power of two.
    shard_capacity: usize,
}

impl ThreadRingRegistry {
    /// Create a registry where each newly registered thread shard will be
    /// bounded to (at least) `shard_capacity` events. The actual ring may be
    /// slightly larger if `shard_capacity` is not already a power of two.
    /// Events pushed when the ring is full are dropped (drop-newest).
    pub fn new(shard_capacity: usize) -> Self {
        let registry_id = NEXT_THREAD_RING_REGISTRY_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("thread ring registry ID overflow");
        Self {
            registry_id,
            rings: RwLock::new(Vec::new()),
            shard_capacity: shard_capacity.max(1),
        }
    }

    #[inline]
    fn registry_key(&self) -> usize {
        self.registry_id
    }

    /// Returns the per-thread SPSC ring for the calling thread, creating
    /// and registering it on first call. Subsequent calls from the same
    /// thread return the same `Arc` (no registry-mutex traffic).
    ///
    /// The returned `Arc` is cloned into the registry so the dumper can drain
    /// it; the producer keeps its own clone in the thread-local cell for fast
    /// re-access.
    pub fn register_current_thread(&self) -> Arc<SpscEventRing> {
        let registry_key = self.registry_key();
        // Fast path: already registered for this thread. Clone the inner Arc
        // out of the thread-local `RegisteredRingGuard` (the guard itself stays
        // in the cell so its thread-exit Drop still fires).
        if let Some(existing) = THREAD_REGISTERED_RINGS.with(|cell| {
            cell.borrow()
                .iter()
                .find(|g| g.registry_key() == registry_key)
                .map(|g| Arc::clone(g.ring()))
        }) {
            return existing;
        }
        // Slow path: allocate, install a Drop guard into the thread-local, and
        // publish to registry.
        let ring = Arc::new(SpscEventRing::new(self.shard_capacity));
        // Registry-leak fix (2026-06-17): wrap the producer's clone in a
        // `RegisteredRingGuard` so the ring is marked `retired` when this
        // thread exits, letting `drain_all` reclaim the shard later.
        THREAD_REGISTERED_RINGS.with(|cell| {
            cell.borrow_mut().push(RegisteredRingGuard {
                registry_key,
                ring: Arc::clone(&ring),
            });
        });
        // Publish a clone to the global registry so drainers can find it.
        // Write lock — held only for the duration of the push (one Arc-clone +
        // one Vec push). `parking_lot::RwLock::write` does not return a Result.
        self.rings.write().push(Arc::clone(&ring));
        ring
    }

    /// Drain every registered thread shard, returning the merged event stream.
    ///
    /// Events are **not** in strict timestamp order across shards — each shard
    /// is drained in producer-insertion order, then concatenated. Callers
    /// that need a time-ordered stream (e.g. the dumper) should sort by
    /// `start_time`.
    ///
    /// SPSC invariant: round-5 CRIT-fix (2026-05-17). `drain_all` is called
    /// from many unsynchronized sites (`recording.rs` ~268/696/718,
    /// `dump.rs` ~1309/1571/1602/1651/1674), so the registry-level
    /// `RwLock::read()` does NOT serialize concurrent drainers. The
    /// per-shard `SpscEventRing::consumer_busy` CAS gate (inside
    /// `try_pop` / `drain_into`) enforces single-consumer-at-a-time on
    /// each shard: a losing drainer simply skips the shard and the
    /// winning drainer takes its events. No UB on `assume_init_read`.
    pub fn drain_all(&self) -> Vec<EventInstance> {
        // Snapshot the Arc list under a read lock so we don't hold it while
        // draining each shard. The Arc clones make the list cheap to copy.
        // Read lock: concurrent drains and concurrent `registered_thread_count`
        // calls can proceed in parallel; only `register_current_thread`
        // (which is once-per-thread-lifetime) takes the write lock.
        let shards: Vec<Arc<SpscEventRing>> = {
            let guard = self.rings.read();
            guard.iter().map(Arc::clone).collect()
        };
        let mut out = Vec::new();
        let mut reclaimable = false;
        for shard in shards {
            shard.drain_into(&mut out);
            // Registry-leak fix (HIGH, 2026-06-17): note whether any shard is
            // now both retired (its producer thread exited) AND empty (this
            // drain — or a prior one — took its last event). Such a shard can
            // never receive another event, so it is safe to drop from the
            // registry. We only *flag* it here under the read lock; the actual
            // `retain()` takes the write lock once below, and only if needed.
            if shard.is_retired() && shard.is_empty() {
                reclaimable = true;
            }
        }
        // Lazy reclamation: prune retired + empty shards under the write lock.
        // Cheap-path: skip the write lock entirely when nothing is reclaimable
        // (the common steady-state case where all producers are still live).
        if reclaimable {
            self.reclaim_retired_shards();
        }
        out
    }

    /// Drop registry clones of shards whose owning producer thread has exited
    /// (`is_retired()`) and which hold no unread events (`is_empty()`).
    ///
    /// Registry-leak fix (HIGH, 2026-06-17): the registry `rings` vector
    /// previously only grew. This `retain()` is the removal path that bounds
    /// it to (roughly) the live producer count.
    ///
    /// Correctness — never drops unread events:
    ///   * A ring is only removed when BOTH `is_retired()` and `is_empty()`
    ///     hold. `is_retired()` is set by the producer thread's exit Drop, so
    ///     once it is true no further `push` can occur (the unique producer is
    ///     gone). With no producer, `is_empty()` is therefore *stable* — a ring
    ///     that is retired+empty here cannot transition back to non-empty.
    ///   * A retired-but-non-empty ring (e.g. a shard whose last drain lost the
    ///     `consumer_busy` CAS, or whose events have not yet been drained) is
    ///     RETAINED — its events remain reachable to the next `drain_all`.
    ///   * Dropping the registry's `Arc` clone does not destroy the ring while
    ///     a producer clone still exists; but a retired ring's producer clone
    ///     was held only by the now-dropped thread-local guard, so after
    ///     removal the `Arc` strong count typically falls to zero and the shard
    ///     (its 1024 slots) is freed — exactly the leak we are fixing.
    fn reclaim_retired_shards(&self) {
        let mut guard = self.rings.write();
        guard.retain(|ring| !(ring.is_retired() && ring.is_empty()));
    }

    /// Number of registered thread shards. Useful for tests and diagnostics.
    pub fn registered_thread_count(&self) -> usize {
        self.rings.read().len()
    }

    /// Process-wide count of events discarded by the drop-newest overflow
    /// policy, summed across every registered thread shard.
    ///
    /// Bug 2 fix (silent event loss): a non-zero result means at least one
    /// thread's per-thread ring filled up and discarded events, so the JFR
    /// recording is incomplete. A dumper or operator can surface this as a
    /// "N events dropped" diagnostic instead of silently losing data.
    ///
    /// Takes the registry read lock to snapshot the shard list (concurrent
    /// drains and counts proceed in parallel); the per-shard loads are
    /// Relaxed atomics.
    pub fn total_dropped_events(&self) -> u64 {
        self.rings
            .read()
            .iter()
            .map(|ring| ring.dropped_events())
            .sum()
    }

    /// Bounded capacity used when registering new thread shards. (Note: the
    /// actual `SpscEventRing` capacity is rounded up to the next power of
    /// two; this value is the *requested* capacity.)
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

/// Process-wide serialization lock for tests that touch shared global state
/// (the [`global_ring_registry`] shards, [`crate::JFR_ENABLED`] via
/// `set_enabled`, or any `drain_all`/`drain_per_thread_into_repository`
/// path). The default `cargo test` runner executes tests on many threads in
/// parallel; because all of these tests emit into the single process-wide
/// ring registry and then destructively `drain_all()`, a concurrently-running
/// test can steal another's events, and `set_enabled` toggles can race the
/// `JFR_ENABLED` gate. Every such test acquires this lock for its whole
/// duration (a `parking_lot::Mutex`, which does not poison on panic, so one
/// failing test cannot wedge the rest).
///
/// `#[cfg(test)]` only — this is test scaffolding and adds nothing to the
/// production build.
#[cfg(test)]
pub(crate) static JFR_TEST_LOCK: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Acquire the global [`JFR_TEST_LOCK`] for the lifetime of the returned
/// guard. Call this at the very top of any test that emits into the global
/// ring registry, toggles `set_enabled`, or drains the rings, so such tests
/// cannot interleave and steal each other's events.
#[cfg(test)]
pub(crate) fn jfr_test_guard() -> parking_lot::MutexGuard<'static, ()> {
    JFR_TEST_LOCK.lock()
}

/// Push an event onto the calling thread's SPSC ring shard, registering it
/// with the global registry on first call.
///
/// CRIT-fix (Bug 2, 2026-05-17): the hot path no longer takes any mutex.
/// The shard is a `SpscEventRing` that uses atomic head/tail counters; the
/// producer (the calling thread) is the only writer, so the push is
/// uncontended atomic ops + one slot write.
///
/// Hot-path cost (steady state):
///   - 1 thread-local access + borrow (no atomics)
///   - 1 Relaxed atomic load (own head) + 1 Acquire load (consumer tail)
///   - 1 unsynchronised slot write
///   - 1 Release store (own head)
///
/// First-call cost (once per thread, ever):
///   - 1 `Arc::new` + `SpscEventRing::new` (Box<[UnsafeCell<MaybeUninit>]>)
///   - 1 global-registry mutex lock to publish the new shard
///
/// Overflow policy: drop-newest. If the ring is full when `push` is called,
/// the event is discarded. JFR documents this as best-effort. The drop is
/// *not* silent: `SpscEventRing::push` increments a per-shard dropped-event
/// counter, readable via `SpscEventRing::dropped_events` or the process-wide
/// `ThreadRingRegistry::total_dropped_events`.
pub fn push_to_thread_ring(ev: EventInstance) {
    let registry = global_ring_registry();
    let registry_key = registry.registry_key();
    // Fast path: borrow the thread-local shard reference in place and push
    // by value. The closure returns the event back if there is no shard yet
    // so we can install one on the slow path.
    let leftover = THREAD_REGISTERED_RINGS.with(|cell| {
        let rings = cell.borrow();
        if let Some(guard) = rings.iter().find(|g| g.registry_key() == registry_key) {
            // Push may fail (full); on overflow the event is dropped
            // (drop-newest) and `push` bumps the shard's dropped counter so
            // the loss is observable rather than silent.
            let _ = guard.ring().push(ev);
            None
        } else {
            Some(ev)
        }
    });
    if let Some(ev) = leftover {
        // Slow path: register, install in thread-local, and push.
        let shard = registry.register_current_thread();
        let _ = shard.push(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smallvec::smallvec;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: smallvec![],
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
        // Serialize against other global-ring tests: a concurrent `drain_all`
        // could steal this thread's event before we inspect our own shard.
        let _g = jfr_test_guard();
        // Push an event with a unique tag, then verify it is reachable through
        // the same Arc shard that `register_current_thread` returns. We pop
        // every event out of the shard (SPSC consumer side) and search the
        // popped vector — this is destructive, but the test owns this
        // thread's shard for its duration so that's fine.
        //
        // The shard may already contain residue from prior tests on this
        // thread (the thread-local cell persists for the whole test binary),
        // so we drain everything and look for the unique start_time tag.
        let unique_start = 0xDEAD_BEEF_u64;
        push_to_thread_ring(make_event(EventTypeId(7), unique_start, unique_start + 1));

        // Re-fetch the same shard — `register_current_thread` is idempotent
        // and returns the cached Arc for the current thread.
        let shard = global_ring_registry().register_current_thread();
        let mut drained: Vec<EventInstance> = Vec::new();
        shard.drain_into(&mut drained);
        assert!(
            drained
                .iter()
                .any(|e| e.start_time == unique_start && e.type_id == EventTypeId(7)),
            "expected pushed event to be visible in this thread's shard (drained {} events)",
            drained.len(),
        );
    }

    #[test]
    fn ring_drops_newest_on_overflow() {
        // The SPSC ring drops the *newest* event on overflow (the prior
        // VecDeque-backed shard dropped the oldest, but a true SPSC ring
        // cannot move the consumer-owned `tail` from the producer thread).
        // Capacity is rounded up to the next power of two, so a request of
        // 3 actually gives 4. Verify the first 4 events make it in and the
        // 5th is dropped.
        let ring = SpscEventRing::new(3);
        assert_eq!(
            ring.capacity(),
            4,
            "capacity should round up to a power of two"
        );
        for i in 0..4u64 {
            ring.push(make_event(EventTypeId(1), i * 100, i * 100 + 50))
                .expect("not full yet");
        }
        // 5th push should fail (drop-newest).
        let rejected = ring.push(make_event(EventTypeId(1), 4 * 100, 4 * 100 + 50));
        assert!(
            rejected.is_err(),
            "ring should be full after `capacity` pushes"
        );

        let mut out = Vec::new();
        ring.drain_into(&mut out);
        let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
        assert_eq!(starts, vec![0, 100, 200, 300]);
    }

    #[test]
    fn spsc_ring_capacity_rounded_to_power_of_two() {
        assert_eq!(SpscEventRing::new(1).capacity(), 1);
        assert_eq!(SpscEventRing::new(2).capacity(), 2);
        assert_eq!(SpscEventRing::new(3).capacity(), 4);
        assert_eq!(SpscEventRing::new(5).capacity(), 8);
        assert_eq!(SpscEventRing::new(1024).capacity(), 1024);
        // Zero is clamped to 1.
        assert_eq!(SpscEventRing::new(0).capacity(), 1);
    }

    #[test]
    fn spsc_ring_push_pop_round_trip() {
        let ring = SpscEventRing::new(8);
        assert!(ring.is_empty());
        ring.push(make_event(EventTypeId(1), 100, 200)).unwrap();
        ring.push(make_event(EventTypeId(2), 300, 400)).unwrap();
        assert_eq!(ring.len(), 2);

        let a = ring.try_pop().unwrap();
        assert_eq!(a.start_time, 100);
        let b = ring.try_pop().unwrap();
        assert_eq!(b.start_time, 300);
        assert!(ring.try_pop().is_none());
        assert!(ring.is_empty());
    }

    #[test]
    fn spsc_ring_wraps_around() {
        // Push, pop, push again past the original capacity, verify we get
        // every pushed event back in FIFO order.
        let ring = SpscEventRing::new(4);
        for i in 0..3u64 {
            ring.push(make_event(EventTypeId(1), i, i + 1)).unwrap();
        }
        // Pop two so head moves ahead of tail.
        assert_eq!(ring.try_pop().unwrap().start_time, 0);
        assert_eq!(ring.try_pop().unwrap().start_time, 1);
        // Now push three more — index arithmetic must wrap correctly.
        for i in 3..6u64 {
            ring.push(make_event(EventTypeId(1), i, i + 1)).unwrap();
        }
        let mut out = Vec::new();
        ring.drain_into(&mut out);
        let starts: Vec<u64> = out.iter().map(|e| e.start_time).collect();
        assert_eq!(starts, vec![2, 3, 4, 5]);
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
                // Each thread pushes a unique-tagged event into its own SPSC
                // shard (single producer = this thread).
                ring.push(make_event(
                    EventTypeId(t as u32 + 1),
                    1000 + t as u64,
                    2000 + t as u64,
                ))
                .expect("SPSC ring should have room for one event");
            }));
        }
        for h in handles {
            h.join().expect("worker panicked");
        }

        // Each worker thread should have registered its own distinct shard.
        assert_eq!(registry.registered_thread_count(), n_threads);

        // `drain_all` is the single consumer for every shard (SPSC invariant).
        let drained = registry.drain_all();
        assert_eq!(drained.len(), n_threads);
        // Verify every type_id 1..=n_threads is represented exactly once.
        for t in 0..n_threads {
            let expected = EventTypeId(t as u32 + 1);
            let count = drained.iter().filter(|e| e.type_id == expected).count();
            assert_eq!(
                count, 1,
                "expected exactly one event for type {:?}",
                expected
            );
        }
    }

    #[test]
    fn same_thread_registries_have_independent_tls_shards() {
        let registry_a = ThreadRingRegistry::new(8);
        let registry_b = ThreadRingRegistry::new(8);

        let ring_a = registry_a.register_current_thread();
        ring_a.push(make_event(EventTypeId(1), 10, 11)).unwrap();

        let ring_b = registry_b.register_current_thread();
        ring_b.push(make_event(EventTypeId(2), 20, 21)).unwrap();

        assert!(
            !Arc::ptr_eq(&ring_a, &ring_b),
            "same-thread registration in distinct registries must not reuse a TLS shard"
        );
        assert_eq!(registry_a.registered_thread_count(), 1);
        assert_eq!(registry_b.registered_thread_count(), 1);

        let drained_a = registry_a.drain_all();
        let drained_b = registry_b.drain_all();

        assert_eq!(drained_a.len(), 1);
        assert_eq!(drained_a[0].type_id, EventTypeId(1));
        assert_eq!(drained_b.len(), 1);
        assert_eq!(drained_b[0].type_id, EventTypeId(2));
    }

    // Task #31: SpscEventRing::Drop must be bounded and must not leak the
    // entire ring on a wedged consumer. The two tests below exercise both
    // ends of the contract: the wedged-consumer fast-fail path, and the
    // active-consumer happy path.

    #[test]
    fn drop_with_wedged_consumer_finishes_within_timeout() {
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        // Use a short timeout so the test runs quickly. The default 1s is
        // fine for production but inflates CI time unnecessarily for tests.
        let ring = Arc::new(SpscEventRing::with_shutdown_timeout(
            8,
            Duration::from_millis(200),
        ));

        // Push a few events so the Drop path has real slots to walk. Use
        // dynamically-allocated `Arc<str>` payloads so the test would
        // demonstrate a leak via Miri's leak checker if `Drop` skipped
        // them. (We can't observe the leak without Miri, but at least the
        // code path that releases them is exercised.)
        use crate::event::{EventFields, EventValue};
        use smallvec::smallvec;
        for i in 0..6u64 {
            let payload: Arc<str> = Arc::from(format!("wedged-{}", i));
            let ev = EventInstance {
                type_id: EventTypeId(1),
                start_time: i,
                end_time: i + 1,
                thread_id: 1,
                fields: smallvec![EventValue::String(payload)] as EventFields,
            };
            ring.push(ev).expect("not full");
        }

        // Simulate a wedged consumer by directly latching `consumer_busy`.
        // No live consumer will touch the slots; the Drop path must still
        // bound its wait and then drain best-effort.
        assert!(
            ring.consumer_busy
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok(),
            "gate was unexpectedly already held"
        );

        // Drop the last Arc<SpscEventRing> reference and measure how long
        // `Drop` takes. With the 200ms timeout it must complete in well
        // under 2 seconds.
        let start = Instant::now();
        drop(ring);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(2),
            "Drop blocked for {:?} on a wedged consumer (expected < 2s)",
            elapsed
        );
        // And it must have waited at least roughly the configured budget —
        // i.e. we didn't accidentally short-circuit the wait.
        assert!(
            elapsed >= Duration::from_millis(150),
            "Drop returned in {:?}; expected to wait close to the 200ms budget",
            elapsed
        );
    }

    #[test]
    fn drop_with_active_consumer_drains_all_events() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::thread;

        // Active-consumer happy path: a consumer thread continually
        // drain_into's the ring while the producer pushes a known number
        // of events. When the producer drops its `Arc`, the consumer
        // should already have everything; the Drop path must not panic
        // and must release any remaining slots cleanly.
        const N_EVENTS: u64 = 64;
        let ring = Arc::new(SpscEventRing::new(16));
        let stop = Arc::new(AtomicBool::new(false));

        let drained = Arc::new(parking_lot::Mutex::new(Vec::<EventInstance>::new()));

        let consumer_ring = Arc::clone(&ring);
        let consumer_stop = Arc::clone(&stop);
        let consumer_out = Arc::clone(&drained);
        let consumer = thread::spawn(move || {
            let mut local: Vec<EventInstance> = Vec::new();
            // Spin-drain until told to stop AND the ring is empty.
            loop {
                consumer_ring.drain_into(&mut local);
                if consumer_stop.load(Ordering::Acquire) && consumer_ring.is_empty() {
                    break;
                }
                thread::yield_now();
            }
            // One final drain in case the producer pushed after our last
            // pass but before setting the stop flag.
            consumer_ring.drain_into(&mut local);
            consumer_out.lock().extend(local);
        });

        // Producer side: push N events with `Arc<str>` payloads so we can
        // verify payload contents (and exercise the Arc release path).
        use crate::event::{EventFields, EventValue};
        use smallvec::smallvec;
        for i in 0..N_EVENTS {
            // Tight retry loop: drop-newest semantics would lose events if
            // we push faster than the consumer drains. We instead retry
            // until the consumer makes room.
            let mut next = EventInstance {
                type_id: EventTypeId(1),
                start_time: i,
                end_time: i + 1,
                thread_id: 1,
                fields: smallvec![EventValue::String(Arc::from(format!("ev-{}", i)))]
                    as EventFields,
            };
            loop {
                match ring.push(next) {
                    Ok(()) => break,
                    Err(returned) => {
                        next = returned;
                        thread::yield_now();
                    }
                }
            }
        }

        // Signal stop and drop our producer-side Arc. The consumer holds
        // the other Arc, so this Drop path runs on the consumer thread
        // when it exits — which means we want the consumer to observe
        // everything first.
        stop.store(true, Ordering::Release);
        // Drop the producer-side Arc explicitly so any post-stop pushes
        // are impossible (there are none here, but be explicit).
        drop(ring);

        consumer.join().expect("consumer panicked");

        let drained = drained.lock();
        assert_eq!(
            drained.len(),
            N_EVENTS as usize,
            "expected exactly {} events drained, got {}",
            N_EVENTS,
            drained.len()
        );
        // Verify payload integrity — every event's string field must match
        // its start_time tag (rules out double-drop / use-after-free of
        // the Arc<str>).
        for ev in drained.iter() {
            let i = ev.start_time;
            match &ev.fields[0] {
                EventValue::String(s) => {
                    assert_eq!(
                        &**s,
                        format!("ev-{}", i),
                        "payload corrupted for event {}",
                        i
                    );
                }
                other => panic!("unexpected field variant: {:?}", other),
            }
        }
    }

    // -----------------------------------------------------------------------
    // Registry-leak fix (HIGH, 2026-06-17): retired-shard reclamation tests.
    // -----------------------------------------------------------------------

    #[test]
    fn drain_all_reclaims_retired_empty_shards() {
        // A retired + empty shard must be removed from the registry by
        // `drain_all`, bounding the otherwise unbounded `rings` growth.
        let registry = ThreadRingRegistry::new(8);

        // Register two shards directly (each Arc stands in for a producer
        // thread's clone). We don't spawn real threads here so the test is
        // deterministic — instead we manually mark one shard retired, which is
        // exactly what `RegisteredRingGuard::drop` does on real thread exit.
        let live = Arc::new(SpscEventRing::new(8));
        let dead = Arc::new(SpscEventRing::new(8));
        registry.rings.write().push(Arc::clone(&live));
        registry.rings.write().push(Arc::clone(&dead));
        assert_eq!(registry.registered_thread_count(), 2);

        // The dead thread emitted one event, then exited.
        dead.push(make_event(EventTypeId(1), 1, 2)).unwrap();
        dead.mark_retired();

        // First drain takes the dead shard's event. The shard is now retired
        // AND empty, so this same `drain_all` reclaims it.
        let drained = registry.drain_all();
        assert_eq!(
            drained.len(),
            1,
            "the dead shard's event must still be drained"
        );
        assert_eq!(
            registry.registered_thread_count(),
            1,
            "retired + empty shard must be reclaimed, leaving only the live one"
        );

        // The live shard is untouched and still usable.
        live.push(make_event(EventTypeId(2), 3, 4)).unwrap();
        let drained2 = registry.drain_all();
        assert_eq!(drained2.len(), 1);
        assert_eq!(registry.registered_thread_count(), 1);
    }

    #[test]
    fn drain_all_keeps_retired_shard_with_unread_events() {
        // A retired shard that still holds unread events (e.g. a drainer lost
        // the consumer CAS) must NOT be reclaimed — dropping it would lose
        // those events. We simulate "unread" by latching `consumer_busy` so
        // `drain_into` is a no-op for the dead shard during this pass.
        use std::sync::atomic::Ordering;

        let registry = ThreadRingRegistry::new(8);
        let dead = Arc::new(SpscEventRing::new(8));
        registry.rings.write().push(Arc::clone(&dead));

        dead.push(make_event(EventTypeId(1), 10, 11)).unwrap();
        dead.mark_retired();
        // Wedge the consumer gate so this drain cannot take the event.
        assert!(dead
            .consumer_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok());

        let drained = registry.drain_all();
        assert_eq!(
            drained.len(),
            0,
            "gate is held, so nothing drains this pass"
        );
        assert_eq!(
            registry.registered_thread_count(),
            1,
            "retired but non-empty shard must be retained — its event is unread"
        );

        // Release the gate; the next drain must recover the event and only then
        // reclaim the now-empty retired shard.
        dead.consumer_busy.store(false, Ordering::Release);
        let drained2 = registry.drain_all();
        assert_eq!(
            drained2.len(),
            1,
            "event must survive and drain on the next pass"
        );
        assert_eq!(
            registry.registered_thread_count(),
            0,
            "shard reclaimed only after its last event was drained"
        );
    }

    #[test]
    fn registered_ring_guard_marks_ring_retired_on_thread_exit() {
        // The thread-local Drop guard must flag the ring retired when the
        // producer thread exits. Spawn a thread that registers + emits, then
        // joins; afterwards the shard it created must report `is_retired()`.
        use std::thread;

        let registry = Arc::new(ThreadRingRegistry::new(8));
        let reg = Arc::clone(&registry);
        let shard = thread::spawn(move || {
            let ring = reg.register_current_thread();
            ring.push(make_event(EventTypeId(1), 1, 2)).unwrap();
            // Return the registry's view of this thread's shard.
            ring
        })
        .join()
        .expect("worker panicked");

        // The worker thread has fully exited, so its `RegisteredRingGuard`
        // TLS destructor ran and marked the shard retired.
        assert!(
            shard.is_retired(),
            "ring must be retired after its producer thread exits"
        );
        // It still holds the unread event, so it would NOT yet be reclaimed.
        assert!(!shard.is_empty());

        // Draining recovers the event and reclaims the now retired+empty shard.
        let drained = registry.drain_all();
        assert_eq!(drained.len(), 1);
        assert_eq!(registry.registered_thread_count(), 0);
    }

    #[test]
    fn drop_with_wedged_consumer_does_not_free_slot_backing() {
        // UAF fix (MED, 2026-06-20): on the wedged-consumer path, the slot
        // array storage must NOT be freed — a parked consumer will dereference
        // its slot when it resumes, after `Drop` returns. We can't directly
        // observe the leak/UAF without Miri/ASAN, but we can model the parked
        // consumer's post-drop dereference: capture a raw pointer to a slot
        // while the gate is latched, drop the ring, then read through that
        // pointer. Under the old code (which freed the box) this is a
        // use-after-free; under the fix the storage is leaked and stays mapped,
        // so the read observes valid memory.
        use crate::event::{EventFields, EventValue};
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        let ring = Arc::new(SpscEventRing::with_shutdown_timeout(
            8,
            Duration::from_millis(50),
        ));
        for i in 0..4u64 {
            let ev = EventInstance {
                type_id: EventTypeId(1),
                start_time: i,
                end_time: i + 1,
                thread_id: 1,
                fields: smallvec![EventValue::String(Arc::from(format!("uaf-{}", i)))]
                    as EventFields,
            };
            ring.push(ev).expect("not full");
        }

        // Latch the gate to simulate a wedged consumer, and capture a raw
        // pointer to the slot at the current `tail` (the consumer's "active"
        // slot — the one Drop must skip *and* must not free out from under us).
        assert!(ring
            .consumer_busy
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok());
        let active_tail = ring.tail.load(Ordering::Acquire);
        let active_idx = active_tail & ring.mask;
        let slot_ptr: *const UnsafeCell<MaybeUninit<EventInstance>> =
            &ring.slots[active_idx] as *const _;

        // Drop the last Arc → `SpscEventRing::drop` runs the wedged path.
        drop(ring);

        // Model the parked consumer resuming and dereferencing its slot. With
        // the fix the backing is leaked (still mapped), so this is a valid read
        // of the producer-initialised `EventInstance`. We read it without moving
        // it out (so we don't double-drop the skipped slot's payload).
        // SAFETY: this test deliberately models the consumer-resume half of
        // the wedged-drop contract. The fixed drop path leaks the slot backing
        // when the consumer gate is held, and `active_tail < head` proves this
        // producer-published slot is initialized and still mapped.
        let ev_ref: &EventInstance = unsafe { (*(*slot_ptr).get()).assume_init_ref() };
        assert_eq!(ev_ref.start_time, active_tail as u64);
        match &ev_ref.fields[0] {
            EventValue::String(s) => {
                assert_eq!(&**s, format!("uaf-{}", active_tail));
            }
            other => panic!("unexpected field variant: {:?}", other),
        }
    }

    #[test]
    fn drop_happy_path_releases_slots_without_consumer() {
        // No consumer ever touches the ring. Drop must traverse [tail, head)
        // and release every Arc<str> payload — a leak would show up under
        // Miri or a leak checker; here we just verify the Drop path runs
        // to completion without panicking and within a tight time budget.
        use crate::event::{EventFields, EventValue};
        use smallvec::smallvec;

        let ring = SpscEventRing::with_shutdown_timeout(8, Duration::from_millis(50));
        for i in 0..6u64 {
            let ev = EventInstance {
                type_id: EventTypeId(1),
                start_time: i,
                end_time: i + 1,
                thread_id: 1,
                fields: smallvec![EventValue::String(Arc::from(format!("hp-{}", i)))]
                    as EventFields,
            };
            ring.push(ev).expect("not full");
        }
        let start = Instant::now();
        drop(ring);
        // No wedged consumer → Drop should return essentially immediately
        // (no waiting at all). Generous bound of 100ms accommodates noisy
        // CI environments.
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "happy-path Drop took {:?}; expected immediate",
            start.elapsed()
        );
    }
}
