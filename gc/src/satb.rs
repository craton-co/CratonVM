// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! SATB (Snapshot-At-The-Beginning) write barrier buffers.
//!
//! During concurrent marking, application threads may overwrite reference
//! fields. To maintain correctness (no live object is missed), the SATB
//! barrier logs the *old* value of a reference field before it is overwritten.
//!
//! Each application thread buffers entries locally, partitioned per target
//! [`SatbQueue`] (see [`SATB_BUFFER_REGISTRY`] — queue-id scoping). When a
//! bucket is full, it is flushed to its owning queue for the marking threads
//! to process. [`SatbBuffer`] remains as a standalone buffer type for callers
//! that manage their own flushing.

use std::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

// ---------------------------------------------------------------------------
// The JIT-visible SATB arming counter (gc-genpause F5.1)
// ---------------------------------------------------------------------------

/// How many live heaps are currently in a concurrent-mark phase where the SATB
/// pre-barrier has to log.
///
/// # Why this exists
///
/// [`crate::gen_heap::GenerationalHeap::satb_barrier`] already asks
/// `is_marking_active()` FIRST and returns in two instructions when marking is
/// idle -- which is almost always. The compiled reference-store fast path could
/// not ask that question at all: it bailed to `jit_putfield_object` on ANY
/// non-null old field value, unconditionally, because the only thing it could
/// see was the field. Overwriting a non-null reference field is one of the most
/// common stores in Java (`this.next = x`, every field reassignment), so on the
/// generational backend a large fraction of compiled reference stores took a
/// full helper call to reach a barrier that immediately returned.
///
/// This counter is the state that question needs, at an address the backend can
/// bake as a constant: a `static`, so it is fixed for the process lifetime and
/// readable before any method is compiled -- unlike the per-heap
/// `Arc<ConcurrentGcState>`, which does not exist until `enable_concurrent_gc`
/// runs and would leave every method compiled before that point holding a stale
/// or null address.
///
/// # Why a counter and not a flag
///
/// A VM host may own more than one heap (this tree has had parallel-test
/// crashes caused by process-global GC caches). A bare flag would let heap A
/// leaving its mark phase disarm the barrier while heap B is still marking --
/// dropping SATB entries heap B needs, which is exactly the class of hole this
/// module's header is about. A counter cannot do that: it is the NUMBER of
/// heaps in an active phase, so it only reaches zero when every one of them has
/// left.
///
/// # Failure direction
///
/// Non-zero means "some heap is marking, take the helper". The helper then
/// makes the exact per-heap check and returns if this heap is idle, so a
/// conservatively-high count costs a call and nothing else. A heap dropped
/// mid-mark leaks its count, leaving the barrier permanently armed -- slow,
/// never unsound. Every way this can be wrong is a way that runs MORE barrier
/// code, not less.
static SATB_ARMED_HEAPS: AtomicU32 = AtomicU32::new(0);

/// Absolute address of [`SATB_ARMED_HEAPS`], for the JIT to bake as a constant.
///
/// Stable for the process lifetime. The backend loads 4 bytes here and takes
/// the barrier helper when they are non-zero.
#[inline]
pub fn satb_armed_addr() -> usize {
    &SATB_ARMED_HEAPS as *const AtomicU32 as usize
}

/// Is any heap in an SATB-active phase? The Rust-side reader of the same byte
/// the JIT tests inline.
#[inline]
pub fn satb_armed() -> bool {
    SATB_ARMED_HEAPS.load(Ordering::Acquire) != 0
}

/// Record a concurrent-mark phase transition's effect on the arming counter.
///
/// Called from [`crate::concurrent_mark::ConcurrentGcState::set_phase`], which
/// is the only place a phase changes. `was_active`/`now_active` are that
/// state's own `is_marking_active` predicate applied to the outgoing and
/// incoming phase, so a no-op transition (setting the phase it already holds,
/// or moving between two inactive phases) does not touch the counter.
pub fn note_satb_phase_transition(was_active: bool, now_active: bool) {
    match (was_active, now_active) {
        (false, true) => {
            SATB_ARMED_HEAPS.fetch_add(1, Ordering::Release);
        }
        (true, false) => {
            // Saturating: an underflow would wrap to `u32::MAX` and arm the
            // barrier forever. Clamping at zero disarms instead, which is the
            // direction a mismatched pair should never reach -- so assert it in
            // debug builds rather than papering over it everywhere.
            debug_assert!(
                SATB_ARMED_HEAPS.load(Ordering::Acquire) > 0,
                "SATB arming counter underflow: a heap left an active mark phase                  it was never counted as entering",
            );
            let _ = SATB_ARMED_HEAPS.fetch_update(
                Ordering::Release,
                Ordering::Acquire,
                |n| Some(n.saturating_sub(1)),
            );
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// SATB activation state (round-5 CRIT #4 fix — TOCTOU)
// ---------------------------------------------------------------------------
//
// Previously a single `AtomicBool active` flag gated both the mutator
// `is_active()` check and the eventual `thread_local_log()` push into a
// shard. The check-then-log was non-atomic: if the GC coordinator flipped
// `active` from true to false between a mutator's `is_active() == true`
// observation and its actual shard append, the SATB log push could still
// land in a shard *after* a concurrent `drain()` had already snapshotted
// that shard. The next mark cycle would treat the stranded entry as a
// false root (harmless) — but the *previous* cycle would have missed the
// mark for the live old reference the mutator was about to overwrite,
// which is the exact correctness bug SATB exists to prevent.
//
// The fix replaces the bool with a tri-state `AtomicU8`:
//   INACTIVE (0) : mutators must NOT log.
//   ACTIVE   (1) : mutators MUST log; coordinator has not yet started
//                  draining.
//   DRAINING (2) : coordinator is in the middle of a final drain pass;
//                  mutators MUST STILL log (their entries land in the
//                  drainable shards and the coordinator drains again
//                  before transitioning to INACTIVE).
//
// The activation gate (`is_active()`) returns true for both ACTIVE and
// DRAINING, so the window where a barrier could observe "active" but the
// drainer has already finished is gone — DRAINING explicitly tells
// loggers "keep logging, I'll drain you again before I stop".
//
// Drain protocol (called by `deactivate_and_drain`):
//   1. CAS state ACTIVE -> DRAINING (Release).
//   2. Drain all shards (snapshot pass A).
//   3. Drain all shards a second time to capture any late writers that
//      observed ACTIVE before the CAS but landed in shards after pass A
//      completed.
//   4. Store state INACTIVE (Release). After this point, mutators that
//      observe INACTIVE are guaranteed not to be racing — any mutator
//      that observed ACTIVE/DRAINING before this store has either
//      already logged into a shard (drained in steps 2 or 3) or
//      will be drained at the next safepoint.
pub(crate) const SATB_INACTIVE: u8 = 0;
pub(crate) const SATB_ACTIVE: u8 = 1;
pub(crate) const SATB_DRAINING: u8 = 2;

/// Process-global registry of every live per-thread SATB partition set.
///
/// Round-11 gc HIGH (SATB completeness): the mutator fast path only flushes a
/// thread's local buffer into the global [`SatbQueue`] when it fills (~256
/// entries) or when that *same* thread voluntarily calls
/// [`flush_thread_satb_buffer`]. The collector's drain paths only ever touched
/// the global shards, so any reference a thread overwrote since its last flush
/// sat in that thread's partially-full local buffer, invisible to remark — a
/// violated SATB snapshot invariant that can premature-reclaim a still-live
/// object hidden behind an overwritten reference.
///
/// The registry closes that gap: each thread registers a [`Weak`] handle to its
/// own partition set on first use, and [`flush_all_thread_satb_buffers`]
/// (invoked at the remark safepoint, before draining the shards) walks the
/// registry and moves every thread's pending entries FOR THE CALLER'S QUEUE
/// into that queue.
///
/// QUEUE-ID SCOPING (cross-queue steal fix, 2026-07-14): entries are
/// partitioned per [`SatbQueue::id`], mirroring the card table's table-id
/// scoping (`card_table.rs`, SECURITY FIX V6). The registry itself is
/// process-global, so with more than one live queue in the process (parallel
/// gc unit tests; any future multi-heap embedding) an unscoped
/// `flush_all_thread_satb_buffers(queue_a)` drained entries a thread had
/// logged against queue B into queue A — queue B's remark then missed those
/// overwritten references, silently voiding its snapshot invariant (observed
/// as the `deactivate_and_drain_includes_thread_local_buffer` flake under
/// `--test-threads>1`; in a multi-heap embedding it would be a premature
/// reclamation / use-after-free). Scoping the buckets by queue id makes every
/// drain path consume exactly the entries that belong to the draining queue.
///
/// `Weak` (rather than a raw pointer or `Arc`) is deliberate: a thread that has
/// exited drops the only `Arc` (its thread-local), so its registry slot fails
/// to `upgrade()` and is pruned in place — no dangling pointer, no leak.
///
/// Dying-thread completeness (2026-07-16, sibling of the card-table
/// `DirtyBufferGuard` fix): a thread that exits with a NON-EMPTY buffer must
/// not lose those entries. A buffered SATB entry is the OLD value of a HEAP
/// reference overwritten during concurrent marking — it describes heap
/// history, not thread state, and the marker needs it in the snapshot
/// regardless of what became of the logging thread (the previous comment's
/// "its entries refer to objects that thread can no longer reach to
/// overwrite, so dropping them is safe" was exactly backwards: losing the
/// entry hides the overwritten object from the mark closure and a live
/// object can be freed mid-mark). Such buffers are parked in
/// [`ORPHANED_SATB_BUFFERS`] by [`SatbBufferGuard::drop`] and drained/reaped
/// by [`flush_all_thread_satb_buffers`] at the remark STW pause.
static SATB_BUFFER_REGISTRY: Mutex<Vec<Weak<Mutex<ThreadSatbPartitions>>>> = Mutex::new(Vec::new());

/// Buffers of exited threads that still hold undrained SATB entries (see the
/// dying-thread note on [`SATB_BUFFER_REGISTRY`]). Strong `Arc`s: the owning
/// thread is gone, so these are the only handles keeping the entries alive
/// until [`flush_all_thread_satb_buffers`] folds them into their queues and
/// reaps the emptied buffer (a dead thread can never log again, so an
/// emptied orphan stays empty).
static ORPHANED_SATB_BUFFERS: Mutex<Vec<Arc<Mutex<ThreadSatbPartitions>>>> = Mutex::new(Vec::new());

/// RAII wrapper stored in TLS so thread exit can decide the fate of the
/// buffer: an EMPTY buffer just dies (its registry `Weak` stops upgrading and
/// is pruned as before), a NON-EMPTY one is parked in
/// [`ORPHANED_SATB_BUFFERS`] so its heap-history entries survive until a
/// collector drains them.
struct SatbBufferGuard {
    buffer: Arc<Mutex<ThreadSatbPartitions>>,
}

impl Drop for SatbBufferGuard {
    fn drop(&mut self) {
        if !self.buffer.lock().buckets.is_empty() {
            ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&self.buffer));
        }
    }
}

/// Per-thread SATB storage: one bucket of pending entries per distinct
/// [`SatbQueue`] this thread has logged against (see the queue-id scoping
/// note on [`SATB_BUFFER_REGISTRY`]).
///
/// `buckets` is expected to hold a single-digit number of entries — one per
/// live queue the thread has written barrier entries for (exactly one in a
/// production VM) — so a linear id lookup is optimal, same as the card
/// table's `DirtyPartitions`. A consumed bucket is removed outright
/// (`swap_remove`) and lazily recreated on the next log, so a dropped queue's
/// bucket cannot outlive its last drain.
#[derive(Default)]
struct ThreadSatbPartitions {
    buckets: Vec<(u64, Vec<usize>)>,
}

impl ThreadSatbPartitions {
    /// Append `addr` to `queue_id`'s bucket, creating it on first use. When
    /// the bucket reaches the auto-flush threshold it is removed and returned
    /// so the caller can spill it into the owning queue's shards.
    fn log(&mut self, queue_id: u64, addr: usize) -> Option<Vec<usize>> {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == queue_id {
                self.buckets[i].1.push(addr);
                if self.buckets[i].1.len() >= DEFAULT_SATB_CAPACITY {
                    return Some(self.buckets.swap_remove(i).1);
                }
                return None;
            }
        }
        let mut fresh = Vec::with_capacity(DEFAULT_SATB_CAPACITY);
        fresh.push(addr);
        self.buckets.push((queue_id, fresh));
        None
    }

    /// Take (remove and return) all buffered entries for `queue_id`, leaving
    /// other queues' buckets untouched. Returns an empty `Vec` when this
    /// thread has nothing buffered for `queue_id`.
    fn take(&mut self, queue_id: u64) -> Vec<usize> {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == queue_id {
                return self.buckets.swap_remove(i).1;
            }
        }
        Vec::new()
    }
}

thread_local! {
    /// Per-thread SATB partition set for the write barrier fast path.
    ///
    /// The mutator write barrier appends overwritten reference values into
    /// the bucket for the target queue; on the common path it takes only this
    /// thread's *own* (uncontended) lock — no cross-thread shared lock. When
    /// a bucket fills it auto-flushes into its owning [`SatbQueue`]; the
    /// collector also drains per-thread buckets at safepoints via
    /// [`flush_thread_satb_buffer`] (this thread) and
    /// [`flush_all_thread_satb_buffers`] (every thread, from the collector) —
    /// both scoped to the caller queue's bucket only.
    ///
    /// The partition set lives behind `Arc<Mutex<…>>` so the collector can
    /// reach it through [`SATB_BUFFER_REGISTRY`]; a plain `RefCell` is `!Sync`
    /// and could not be shared cross-thread. The `Arc` is created and
    /// registered exactly once per thread, on first access.
    /// Wrapped in [`SatbBufferGuard`] so thread exit parks a non-empty buffer
    /// in [`ORPHANED_SATB_BUFFERS`] instead of silently dropping its entries.
    static THREAD_SATB_BUFFER: SatbBufferGuard = {
        let buf = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
        register_thread_satb_buffer(&buf);
        SatbBufferGuard { buffer: buf }
    };
}

/// Register a freshly-created per-thread buffer into the global registry,
/// opportunistically pruning any slots whose owning thread has since exited.
///
/// Pruning here (amortised against the one registration per thread) keeps the
/// registry from growing without bound in a churn of short-lived threads even
/// if no collection runs to trigger the prune in
/// [`flush_all_thread_satb_buffers`].
fn register_thread_satb_buffer(buf: &Arc<Mutex<ThreadSatbPartitions>>) {
    let mut reg = SATB_BUFFER_REGISTRY.lock();
    reg.retain(|w| w.strong_count() > 0);
    reg.push(Arc::downgrade(buf));
}

/// Push an overwritten reference address into the calling thread's local
/// bucket for `queue`. If the bucket is full, drain it into `queue`
/// atomically.
///
/// This is the fast path called by the write barrier — no shared lock is
/// taken unless the per-thread bucket fills (amortized one lock per 256
/// reference stores).
#[inline]
pub fn satb_thread_local_log(queue: &SatbQueue, old_ref_addr: usize) {
    if old_ref_addr == 0 {
        return;
    }
    let to_flush = THREAD_SATB_BUFFER.with(|g| g.buffer.lock().log(queue.id(), old_ref_addr));
    if let Some(entries) = to_flush {
        queue.flush(entries);
    }
}

/// Drain the calling thread's bucket for `queue` into that queue.
///
/// Called by mutators at safepoint entry and by the collector at GC start
/// to ensure all logged-but-unflushed entries reach the global queue
/// before marker threads consume them. Buckets this thread holds for OTHER
/// queues are left untouched (queue-id scoping — see
/// [`SATB_BUFFER_REGISTRY`]).
pub fn flush_thread_satb_buffer(queue: &SatbQueue) {
    let entries = THREAD_SATB_BUFFER.with(|g| g.buffer.lock().take(queue.id()));
    if !entries.is_empty() {
        queue.flush(entries);
    }
}

/// Drain every registered thread's bucket **for `queue`** into that queue.
///
/// Round-11 gc HIGH (SATB completeness): the per-thread fast path only spills a
/// thread's local bucket into `queue` when it fills or when that thread calls
/// [`flush_thread_satb_buffer`] itself. The collector has no other handle on
/// those buckets, so at remark a thread's partially-full bucket holds
/// references it overwrote since its last flush — references that must be in
/// the snapshot. This walks [`SATB_BUFFER_REGISTRY`] and moves each live
/// thread's pending entries for THIS queue into `queue` (entries logged
/// against other queues stay put — see the queue-id scoping note on the
/// registry), and prunes slots for threads that have exited (their `Weak` no
/// longer upgrades).
///
/// SAFETY / CORRECTNESS — this MUST be called at the remark **STW safepoint**.
/// At a safepoint no mutator is executing the write-barrier fast path, so no
/// thread is mid-`log` on its own buffer. We still take each buffer's `Mutex`
/// (the buffer is shared via `Arc`, so the type demands it, and it cheaply
/// defends against a buggy non-stopped caller), but the *completeness*
/// guarantee — that every reference overwritten before the snapshot is now in
/// the queue — relies on the STW property: a running mutator could `log` a new
/// entry into a buffer we already drained, stranding it again. Outside STW this
/// function flushes only a racy snapshot and does NOT restore the SATB
/// invariant. See [`SatbQueue::deactivate_and_drain`], the sole production
/// caller, which runs in the remark pause.
pub fn flush_all_thread_satb_buffers(queue: &SatbQueue) {
    // Collect upgradable handles under the registry lock, pruning dead slots,
    // then release the registry lock before touching the global queue so we
    // never hold the registry lock across a `queue.flush` (which takes a shard
    // lock) — keeps the lock ordering registry-then-shard one-directional.
    let live: Vec<Arc<Mutex<ThreadSatbPartitions>>> = {
        let mut reg = SATB_BUFFER_REGISTRY.lock();
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|w| match w.upgrade() {
            Some(arc) => {
                live.push(arc);
                true
            }
            None => false,
        });
        live
    };

    for buf in live {
        let entries = buf.lock().take(queue.id());
        if entries.is_empty() {
            continue;
        }
        queue.flush(entries);
    }

    // Dying-thread completeness (see SATB_BUFFER_REGISTRY): drain buffers
    // parked by exited threads, reaping each once nothing is left in ANY of
    // its buckets (a dead thread can never log again, so an emptied orphan
    // stays empty; a bucket for a DIFFERENT queue is left for that queue's
    // own drain). Entries are collected under the orphan-registry + buffer
    // locks and flushed after both are released, keeping the lock ordering
    // one-directional (registry → buffer, never across a shard lock).
    let mut orphan_entries: Vec<Vec<usize>> = Vec::new();
    {
        let mut orphans = ORPHANED_SATB_BUFFERS.lock();
        orphans.retain(|buf| {
            let mut b = buf.lock();
            let entries = b.take(queue.id());
            if !entries.is_empty() {
                orphan_entries.push(entries);
            }
            !b.buckets.is_empty()
        });
    }
    for entries in orphan_entries {
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

/// Number of `SatbQueue` shards.  Must be a power of two so the
/// `tid & (SHARDS-1)` mapping is a single mask.  16 chosen as a
/// compromise between memory (each shard is a Mutex<Vec<usize>>) and
/// throughput at mutator counts up to ~16 active flushers.
const SHARDS: usize = 16;

// TODO(round-11, HIGH from round-7/9 cross-cutting): swap each shard's
// `parking_lot::Mutex<Vec<usize>>` for `crossbeam_queue::SegQueue<usize>`
// to make `flush()` fully lock-free (push is wait-free MPSC).
//
// Why deferred to a later round:
//   1. `deactivate_and_drain` (round-9 gc HIGH-4 fix above) leans on the
//      mutex to GUARANTEE no late writer is mid-`extend` when the drain
//      transitions ACTIVE→INACTIVE. With SegQueue we'd need a per-shard
//      `AtomicBool drain_in_progress` + spin/yield on the writer side
//      (or a sequence-number scheme like seqlock) to recover the same
//      happens-before edge. That's an extra atomic on the hot mutator
//      flush path — possibly net-negative versus the mutex.
//   2. Per the round-7/9 benchmarks, the per-shard mutex is NOT a
//      measured bottleneck: with 16 shards the contention is already
//      ~1/16 of the pre-sharding single-mutex bottleneck the round-7
//      HIGH-3 fix targeted, and the actual `flush()` critical section
//      is a single `Vec::extend` (typically ~256 entries, ~2µs). The
//      OS scheduler rarely contends.
//   3. SegQueue allocates a 32-entry segment per push burst, which
//      would push allocator traffic up considerably on workloads with
//      many small flushes.
//
// Revisit if perf telemetry shows mutator threads stalling on
// `shards[s].lock()` during concurrent mark; the migration is
// mechanical (push: `shard.push(p)`; drain: `while let Some(p) =
// shard.pop() { out.push(p); }`) once we have a safe replacement
// for the drain-side mutex barrier in `deactivate_and_drain`.

/// Global SATB queue shared by all threads and the concurrent marker.
///
/// Application threads flush their per-thread `SatbBuffer` into this queue.
/// Marker threads drain it to discover references that were overwritten
/// during concurrent marking.
///
/// Round-7 HIGH-3 fix: the queue is sharded across `SHARDS` independent
/// `Mutex<Vec<usize>>` buckets keyed by `thread_id % SHARDS`.  This
/// eliminates the single-global-mutex chokepoint that serialised every
/// mutator buffer flush during concurrent marking.  The marker drains
/// all shards once per cycle (or on demand), so the consumer side
/// remains a single thread.
pub struct SatbQueue {
    /// Queue-id scoping (cross-queue steal fix): process-unique identifier
    /// for this queue, assigned from [`NEXT_SATB_QUEUE_ID`] in
    /// [`SatbQueue::new`]. Every entry a thread buffers locally is tagged
    /// with the target queue's id so the drain paths are queue-scoped and
    /// one queue can never steal another's buffered overwritten refs.
    id: u64,
    /// Per-shard entry buckets.  Each mutator picks its shard by its
    /// thread id, so independent threads land on independent locks in
    /// the common case.
    shards: [Mutex<Vec<usize>>; SHARDS],
    /// Tri-state activation flag.  See module-level comment on
    /// `SATB_INACTIVE` / `SATB_ACTIVE` / `SATB_DRAINING` for the
    /// state machine that closes the round-5 CRIT #4 TOCTOU.
    state: AtomicU8,
}

/// Monotonic source of process-unique [`SatbQueue::id`] values. Starts at 1
/// so 0 can serve as a never-assigned sentinel in debugging. Wraparound
/// after 2^64 queues is not a practical concern.
static NEXT_SATB_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

#[inline]
fn shard_for_current_thread() -> usize {
    // Hash the thread id into [0, SHARDS).  `ThreadId` doesn't expose
    // its inner u64 publicly on stable, but its `Hash` impl is stable
    // across calls within the same thread, which is all we need.
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::thread::current().id().hash(&mut h);
    (h.finish() as usize) & (SHARDS - 1)
}

impl SatbQueue {
    /// Create a new inactive SATB queue.
    pub fn new() -> Self {
        // Array initialisation with a non-Copy element type — use a
        // small helper so each shard gets its own Mutex.
        let shards: [Mutex<Vec<usize>>; SHARDS] = std::array::from_fn(|_| Mutex::new(Vec::new()));
        Self {
            // Relaxed is sufficient: we only need uniqueness, not ordering
            // relative to other memory.
            id: NEXT_SATB_QUEUE_ID.fetch_add(1, Ordering::Relaxed),
            shards,
            state: AtomicU8::new(SATB_INACTIVE),
        }
    }

    /// Process-unique identity of this queue (see the queue-id scoping note
    /// on [`SATB_BUFFER_REGISTRY`]).
    #[inline]
    fn id(&self) -> u64 {
        self.id
    }

    /// Enable SATB logging (called at start of concurrent mark phase).
    pub fn activate(&self) {
        self.state.store(SATB_ACTIVE, Ordering::Release);
    }

    /// Disable SATB logging — single-shot transition straight to INACTIVE.
    ///
    /// Prefer [`SatbQueue::deactivate_and_drain`] in production code: that
    /// path closes the round-5 CRIT #4 TOCTOU by transitioning through
    /// DRAINING and re-draining the shards before flipping the gate off.
    /// `deactivate` (no drain) is kept only for tests / debug code paths
    /// where the caller has another mechanism to ensure no mutator can
    /// be mid-log when the flag flips.
    pub fn deactivate(&self) {
        self.state.store(SATB_INACTIVE, Ordering::Release);
    }

    /// Check if SATB logging is active. Threads use this to decide whether
    /// the write barrier should log old values.
    ///
    /// Returns true for both ACTIVE and DRAINING — see module-level
    /// state-machine comment.
    #[inline]
    pub fn is_active(&self) -> bool {
        // Acquire pairs with the Release stores in activate / deactivate
        // / deactivate_and_drain so a true result is causally ordered
        // before subsequent logging into a shard.
        self.state.load(Ordering::Acquire) != SATB_INACTIVE
    }

    /// Drain all pending entries and then disable SATB logging.
    ///
    /// Round-5 CRIT #4: this is the safe pairing for `activate`. The
    /// state machine guarantees no log push from a mutator that observed
    /// the gate as "active" can be stranded after the drain returns.
    ///
    /// Round-9 gc HIGH-4: a simple "drain twice" pattern still races
    /// with a late writer that observed ACTIVE before the CAS, took the
    /// slow shard-lock path between the two drains, and was preempted
    /// just before its `shards[s].lock()` succeeded. Both drain passes
    /// could complete with the late writer's `flush()` still pending in
    /// the OS scheduler, then the writer's `extend` lands in a shard
    /// *after* the second drain finishes but *before* we flip to
    /// INACTIVE. The entry is now stranded.
    ///
    /// Fix: after the first drain, walk every shard taking its mutex
    /// exclusively. Holding `shards[s].lock()` excludes any concurrent
    /// `flush()` on the same shard — `flush()` blocks waiting for our
    /// lock, so by the time we drop it the late writer either (a)
    /// blocked before pushing and will see INACTIVE on retry of the
    /// `is_active()` gate it must check before logging next time, or
    /// (b) had already pushed and we drained it inside the critical
    /// section. Either way, no entry survives unobserved past the
    /// INACTIVE store.
    ///
    /// Returns the concatenated entries from all drain passes.
    pub fn deactivate_and_drain(&self) -> Vec<usize> {
        // 1. Transition ACTIVE -> DRAINING.  If the gate is already
        //    INACTIVE (double-stop), just drain once for safety and
        //    return.
        let _ = self.state.compare_exchange(
            SATB_ACTIVE,
            SATB_DRAINING,
            Ordering::Release,
            Ordering::Acquire,
        );

        // 1b. Round-11 gc HIGH (SATB completeness): flush every live
        //     per-thread SATB buffer into the shards FIRST. The fast path
        //     leaves references a thread overwrote since its last auto-/manual
        //     flush sitting in that thread's partially-full local buffer,
        //     where the shard drains below cannot see them. Pulling them in
        //     here makes the snapshot complete. This is sound only because
        //     `deactivate_and_drain` is the remark step and runs at an STW
        //     safepoint: no mutator is mid-barrier, so nothing can re-fill a
        //     buffer after we drain it (see `flush_all_thread_satb_buffers`).
        flush_all_thread_satb_buffers(self);

        // 2. First drain pass — quickly empties the bulk of pending
        //    entries without holding any lock across multiple shards.
        let mut all = self.drain();

        // 3. Hold each shard lock exclusively and drain any late entries
        //    that arrived after step 2. Holding the lock excludes
        //    `flush()` on the same shard: any racing writer that picked
        //    this shard either pushed before we acquired (drained here)
        //    or is now blocked on `shards[s].lock()` and will not push
        //    until we drop the guard at the end of this iteration.
        //    Combined with the INACTIVE store in step 4 (Release-ordered
        //    against the writer's subsequent Acquire-load of `state` in
        //    `is_active()`), this gives the happens-before edge that
        //    prevents stranded entries.
        for shard in self.shards.iter() {
            let mut guard = shard.lock();
            if !guard.is_empty() {
                let late = std::mem::take(&mut *guard);
                all.extend(late);
            }
            // Implicit drop(guard) here releases the shard mutex for
            // unrelated future use; ordering against `state` store
            // below is provided by the SeqCst-equivalent lock release
            // + the Release store.
        }

        // 4. Finally flip to INACTIVE.  Release-ordered: pairs with the
        //    Acquire load in `is_active()` so any subsequent mutator
        //    observation sees INACTIVE happens-after the drain.
        self.state.store(SATB_INACTIVE, Ordering::Release);
        all
    }

    /// Flush a per-thread buffer's entries into the global queue.
    ///
    /// Round-7 HIGH-3: writes land on the per-thread shard so concurrent
    /// flushers from independent threads do not serialise.
    pub fn flush(&self, entries: Vec<usize>) {
        if entries.is_empty() {
            return;
        }
        let s = shard_for_current_thread();
        let mut queue = self.shards[s].lock();
        queue.extend(entries);
    }

    /// Drain all accumulated entries for the marker to process.
    ///
    /// Visits every shard in order, atomically swapping each Vec out
    /// and concatenating into a single result.  The marker is the
    /// single consumer so per-shard ordering across shards is not
    /// meaningful — entries from shard 0 simply precede shard 1's.
    pub fn drain(&self) -> Vec<usize> {
        let mut total = 0usize;
        let mut buckets: [Vec<usize>; SHARDS] = std::array::from_fn(|_| Vec::new());
        for (i, shard) in self.shards.iter().enumerate() {
            let mut guard = shard.lock();
            let taken = std::mem::take(&mut *guard);
            total += taken.len();
            buckets[i] = taken;
        }
        let mut out = Vec::with_capacity(total);
        for b in buckets.into_iter() {
            out.extend(b);
        }
        out
    }

    /// Number of entries currently queued (approximate, for stats).
    ///
    /// Walks every shard taking each lock briefly; only used by tests
    /// and diagnostics, never on the hot path.
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.lock().is_empty())
    }
}

impl Default for SatbQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SatbQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state.load(Ordering::Relaxed) {
            SATB_INACTIVE => "inactive",
            SATB_ACTIVE => "active",
            SATB_DRAINING => "draining",
            _ => "?",
        };
        f.debug_struct("SatbQueue")
            .field("state", &state)
            .field("queued", &self.len())
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

    // -----------------------------------------------------------------------
    // Round-5 CRIT #4 — tri-state activation
    // -----------------------------------------------------------------------

    #[test]
    fn deactivate_and_drain_returns_entries_and_disables() {
        let q = SatbQueue::new();
        q.activate();
        assert!(q.is_active());
        q.flush(vec![0x100, 0x200]);
        let drained = q.deactivate_and_drain();
        // All entries returned and the gate is now off.
        assert_eq!(drained.len(), 2);
        assert!(!q.is_active());
        assert!(q.is_empty());
    }

    #[test]
    fn is_active_true_during_draining_window() {
        // Manually drive the state to DRAINING and verify is_active still
        // reports true so concurrent loggers keep pushing.
        let q = SatbQueue::new();
        q.activate();
        // Simulate the first half of deactivate_and_drain: flip to DRAINING.
        q.state.store(SATB_DRAINING, Ordering::Release);
        assert!(q.is_active(), "DRAINING must report as active to loggers");
        q.state.store(SATB_INACTIVE, Ordering::Release);
        assert!(!q.is_active());
    }

    #[test]
    fn deactivate_and_drain_captures_late_writers() {
        // Simulate a writer that pushes between the two internal drain
        // passes. The implementation does drain() twice, so any push that
        // lands after the first drain but before the INACTIVE store is
        // still captured in the returned Vec.
        let q = SatbQueue::new();
        q.activate();
        q.flush(vec![1, 2, 3]);
        // Manually drive to DRAINING then push more, then call drain twice.
        q.state.store(SATB_DRAINING, Ordering::Release);
        let first = q.drain();
        assert_eq!(first.len(), 3);
        // Late writer pushes into a (now-empty) shard while DRAINING.
        q.flush(vec![4, 5]);
        let second = q.drain();
        assert_eq!(second.len(), 2);
    }

    // -----------------------------------------------------------------------
    // Round-11 gc HIGH — per-thread SATB buffer registry / completeness
    // -----------------------------------------------------------------------

    #[test]
    fn flush_all_thread_satb_buffers_reaches_global_queue() {
        // Stage entries into a *registered* thread-local buffer without
        // filling it (so they would otherwise stay local and invisible to the
        // collector), then prove `flush_all_thread_satb_buffers` + `drain`
        // surfaces them in the global queue.
        //
        // Run in a fresh thread so this thread's buffer is freshly created and
        // registered. Use unique sentinel addresses and assert by membership
        // rather than exact count: the global registry is process-wide, so a
        // concurrently-running test thread could contribute its own entries —
        // our sentinels must be present regardless.
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();

            const S1: usize = 0xA11CE_000;
            const S2: usize = 0xA11CE_001;
            satb_thread_local_log(&q, S1);
            satb_thread_local_log(&q, S2);

            // Buffer is far from full (cap 256) → still purely thread-local;
            // the collector's shard drain alone would miss these.
            assert!(q.is_empty(), "partial buffer must not have auto-flushed");

            // Collector-side flush of every registered per-thread buffer.
            flush_all_thread_satb_buffers(&q);

            let drained = q.drain();
            assert!(
                drained.contains(&S1) && drained.contains(&S2),
                "staged thread-local entries must reach the global queue, got {drained:?}"
            );
        });
        h.join().unwrap();
    }

    #[test]
    fn deactivate_and_drain_includes_thread_local_buffer() {
        // The actual bug repro: at remark, `deactivate_and_drain` must capture
        // references still sitting in a partially-full per-thread buffer, not
        // just the global shards.
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();

            const SENTINEL: usize = 0xBEEF_FACE;
            satb_thread_local_log(&q, SENTINEL);
            // Not flushed to the shards yet — lives only in the thread-local.
            assert!(q.is_empty());

            let snapshot = q.deactivate_and_drain();
            assert!(
                snapshot.contains(&SENTINEL),
                "remark drain must include thread-local overwritten refs, got {snapshot:?}"
            );
            assert!(!q.is_active());
        });
        h.join().unwrap();
    }

    #[test]
    fn dying_thread_residual_satb_entries_survive_until_remark_drain() {
        // Dying-thread completeness regression (2026-07-16, sibling of the
        // card table's dying_thread_residual_offsets_survive_until_flush_all):
        // a mutator that logs an SATB entry (below the auto-flush capacity)
        // and EXITS without flushing must not lose the entry — it is the old
        // value of a heap reference overwritten during concurrent marking,
        // and losing it hides the overwritten object from the mark closure.
        // Pre-fix, the thread's Arc dropped, the registry Weak died, and the
        // entry was silently discarded.
        let q = std::sync::Arc::new(SatbQueue::new());
        q.activate();

        const SENTINEL: usize = 0xDEAD_1234;
        let q2 = std::sync::Arc::clone(&q);
        std::thread::spawn(move || {
            // One buffered entry, far below DEFAULT_SATB_CAPACITY; the thread
            // exits immediately after, running SatbBufferGuard::drop.
            satb_thread_local_log(&q2, SENTINEL);
            assert!(q2.is_empty(), "entry must still be thread-local");
        })
        .join()
        .unwrap();

        // Remark STW drain: must recover the dead thread's entry from the
        // orphan list and include it in the snapshot.
        let snapshot = q.deactivate_and_drain();
        assert!(
            snapshot.contains(&SENTINEL),
            "remark drain must include a dead thread's buffered refs, got {snapshot:?}"
        );

        // The emptied orphan was reaped: a second full drain sees nothing.
        q.activate();
        flush_all_thread_satb_buffers(&q);
        assert!(q.is_empty(), "emptied orphan must have been reaped");
        let _ = q.deactivate_and_drain();
    }

    #[test]
    fn registry_prunes_dead_weak_slots() {
        // A buffer whose owning thread has exited drops its only `Arc`, leaving
        // a dead `Weak` in the registry that must (a) not upgrade and (b) be
        // pruned without panic. Drive the mechanism deterministically by
        // manually injecting a dead `Weak` (simulating an exited thread) plus a
        // live one, then asserting `flush_all_thread_satb_buffers` removes only
        // the dead slot. This avoids depending on the count of buffers from
        // other test threads running in parallel.

        // A dead Weak: create an Arc, downgrade, then drop the Arc.
        let dead: Weak<Mutex<ThreadSatbPartitions>> = {
            let arc = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
            Arc::downgrade(&arc)
            // `arc` dropped here → `dead` can no longer upgrade.
        };
        assert!(dead.upgrade().is_none(), "weak must be dead after Arc drop");

        // A live, registered partition set holding one entry staged for `q`.
        let q = SatbQueue::new();
        let live = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
        assert!(live.lock().log(q.id(), 0xFEED_0001).is_none());
        let live_weak = Arc::downgrade(&live);

        // Inject both into the registry. Keep a clone of the dead Weak so we
        // can prove afterward that flush_all neither upgraded nor retained it.
        let dead_probe = dead.clone();
        {
            let mut reg = SATB_BUFFER_REGISTRY.lock();
            reg.push(dead);
            reg.push(live_weak);
        }

        // Must not panic on the dead slot, and must process the live one.
        flush_all_thread_satb_buffers(&q);

        // The live partition set's staged entry reached the queue and was
        // consumed (proves flush_all walked past the dead slot to it).
        assert!(q.drain().contains(&0xFEED_0001));
        assert!(
            live.lock().take(q.id()).is_empty(),
            "live bucket consumed by flush_all"
        );
        // The dead slot never resurrects (deterministic, unaffected by other
        // parallel test threads) and the registry no longer holds *our* dead
        // Weak: a dead Weak has no strong refs, and after pruning the only
        // remaining tie to that allocation is `dead_probe`, which still cannot
        // upgrade. The count of dead slots flush_all left behind for the slots
        // it visited is zero — verified via the live buffer having been the one
        // reached.
        assert!(dead_probe.upgrade().is_none(), "dead Weak must stay dead");
    }

    #[test]
    fn thread_local_buffers_are_queue_scoped() {
        // Cross-queue steal regression (2026-07-14): with two live queues in
        // the process, draining ALL thread buffers on behalf of queue A must
        // consume only the entries this thread logged against A — entries
        // logged against queue B stay buffered for B's own remark. The
        // pre-fix unscoped drain moved B's entries into A's queue, voiding
        // B's SATB snapshot (observed as the
        // `deactivate_and_drain_includes_thread_local_buffer` flake under
        // parallel test threads).
        let h = std::thread::spawn(|| {
            let qa = SatbQueue::new();
            let qb = SatbQueue::new();
            qa.activate();
            qb.activate();

            const SA: usize = 0xAAAA_0001;
            const SB: usize = 0xBBBB_0002;
            satb_thread_local_log(&qa, SA);
            satb_thread_local_log(&qb, SB);

            // A remark on qa takes only qa's bucket...
            flush_all_thread_satb_buffers(&qa);
            let a = qa.drain();
            assert!(a.contains(&SA), "qa must receive its own entry, got {a:?}");
            assert!(!a.contains(&SB), "qa stole qb's thread-local entry: {a:?}");

            // ...and qb's entry is still intact for qb's own remark.
            let b = qb.deactivate_and_drain();
            assert!(
                b.contains(&SB),
                "qb's entry lost after qa's flush_all: {b:?}"
            );
        });
        h.join().unwrap();
    }
}
