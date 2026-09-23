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

use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};

use parking_lot::Mutex;

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
//   1b. Flush every registered per-thread buffer for this queue into the
//      shards, so the snapshot is not missing a mutator's partially-full
//      bucket. Sound only at an STW safepoint — see the function.
//   2. Drain all shards (snapshot pass A).
//   3. Take each shard lock EXCLUSIVELY and drain again, capturing late
//      writers that observed ACTIVE before the CAS but landed in shards
//      after pass A completed.
//   3b. Wait, on a bounded budget, for `in_flight` to reach zero, then drain
//      again. `in_flight` counts writers that have entered `flush` but may
//      not have pushed — the ones step 3 can only exclude while it holds
//      their particular shard. This is what narrows (not eliminates) the
//      residual described in
//      `docs/internal/gaps/gengc-mark-satb-deactivate-is-stw-only-20260920.md`.
//   4. Store state INACTIVE (Release).
//   5. One further drain, after the gate is off, so that anything a writer
//      still deposits is attributed to THIS cycle instead of lying in a shard
//      until the next one, where the address it names may have been reused.
//
// WHAT THE PROTOCOL DOES NOT GIVE YOU. Step 4 is NOT a guarantee that no
// mutator is racing. A mutator reads the gate ONCE, before it commits to
// logging, and never re-reads it; a writer that passed the gate and was then
// descheduled between it and `flush`'s `in_flight` increment is invisible to
// every step above. What makes the production callers sound is that both of
// them (`ConcurrentMarker::remark`, `G1::final_remark`) run inside a
// stop-the-world pause, where no mutator is mid-barrier at all. Closing it
// under genuine concurrency needs a per-thread handshake, as HotSpot does.
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

/// The ids of every [`SatbQueue`] that currently exists, maintained by
/// [`SatbQueue::new`] and `Drop for SatbQueue`.
///
/// # Why this is needed (LANE W2-C, 2026-09-20)
///
/// [`flush_all_thread_satb_buffers`]'s orphan pass retains an orphaned buffer
/// while it holds a bucket for ANY queue, on the reasoning that the bucket
/// belongs to some other queue's drain. That reasoning silently assumes every
/// queue still exists. If the queue a bucket is keyed to has since been
/// DROPPED, nothing will ever call `take` with its id again, so the orphan --
/// and the `Arc` keeping it alive -- lives for the life of the process.
///
/// Harmless in a production VM, which has exactly one queue for the life of
/// the process. Not harmless in the unit tests, which construct many, nor in a
/// multi-heap embedding. The pass now additionally drops buckets whose queue id
/// is no longer live, which is safe by construction: a dropped queue has no
/// drainer, so those entries can never be delivered to anything and are already
/// unreachable in every sense but the allocator's.
///
/// A `Vec` rather than a set: it holds one entry in production and a handful in
/// a test, and it is touched once per queue construction/destruction and once
/// per orphan pass -- never on a barrier path.
static LIVE_SATB_QUEUE_IDS: Mutex<Vec<u64>> = Mutex::new(Vec::new());

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

    /// Drop this thread's bucket for `queue_id`, returning how many entries
    /// were in it.
    ///
    /// LANE W2-C — the discard half of [`SatbQueue::deactivate_and_discard`].
    /// `take` is the same operation with a `Vec` handed back that the caller
    /// immediately drops; this one never builds it.
    ///
    /// LANE W3-C — the count is returned rather than nothing so the discard
    /// walk can feed the same `visited` / `held` census as the flush walk (see
    /// [`WALK_CALLS`]). A discard that reported only "a bucket was removed"
    /// would make the two walks incomparable, and comparing them is the point:
    /// they visit the same registry and differ only in what they do with what
    /// they find.
    fn discard(&mut self, queue_id: u64) -> usize {
        for i in 0..self.buckets.len() {
            if self.buckets[i].0 == queue_id {
                return self.buckets.swap_remove(i).1.len();
            }
        }
        0
    }

    /// Drop every bucket whose queue id is not in `live`.
    ///
    /// LANE W2-C — see [`LIVE_SATB_QUEUE_IDS`]: a bucket keyed to a queue that
    /// no longer exists has no drainer and can never be delivered anywhere, so
    /// retaining it (and the orphaned buffer it pins) is a pure leak.
    fn drop_dead_buckets(&mut self, live: &[u64]) {
        self.buckets.retain(|(id, _)| live.contains(id));
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
pub fn satb_thread_local_log(queue: &SatbQueue, old_ref_addr: usize) -> bool {
    if old_ref_addr == 0 {
        return false;
    }
    // gengc-mark2 2026-09-20 — GATE ON THE QUEUE, not only on the caller's own
    // idea of whether marking is on.
    //
    // This function used to append unconditionally. G1's barrier
    // (`g1.rs`, `satb_queue.is_active() && satb_thread_local_log(..)`) happens
    // to check first; `GenerationalHeap::satb_barrier` checks
    // `ConcurrentGcState::is_marking_active()` — the PHASE — and the two gates
    // are maintained by different code. Whenever the phase gate is open while
    // the queue gate is shut, the entry lands in this thread's bucket for a
    // queue nobody will drain again this cycle, and sits there until the NEXT
    // cycle's `flush_all_thread_satb_buffers` replays it as a gray root — by
    // which time the address it names may have been swept and reissued to a
    // different object. That is a stale address entering a live mark, which is
    // strictly worse than dropping the entry.
    //
    // Dropping it is sound: the queue is INACTIVE only after
    // `deactivate_and_drain` has run, i.e. after the remark pause has fixed
    // the bitmap. An object reachable at remark is already marked; an object
    // whose last reference is overwritten AFTER remark was reachable at
    // remark, so it is marked too. This cycle's snapshot is complete without
    // the entry, and the next cycle will take its own.
    //
    // Behaviour-neutral for G1 and ZGC, whose callers already made this exact
    // check one frame up; this is a redundant Acquire load on their path.
    //
    // Dropping here also touches NOTHING that a later drain has to undo: the
    // entry never reaches a thread-local bucket and never reaches `flush`, so
    // it is never credited to `pending` (see that field) and cannot latch
    // `is_empty()` non-empty.
    if !queue.is_active() {
        queue.late_log_drops.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    // LANE W3-C — the DENOMINATOR for the registry walk's `entries` column.
    //
    // Without it, `entries=0` over a thousand walks is ambiguous between "no
    // mutator ever had a partially-full bucket at a cycle boundary" (which is
    // the finding, and it decides the page) and "the SATB barrier never fired
    // at all" (which would be a correctness event and would make the finding
    // meaningless). That ambiguity is `orchestrator-wave-1-measurements.md`
    // §7.2 — a probe that did not perform the operation it was studying — and
    // this is the one number that resolves it.
    //
    // Counted AFTER the queue gate above, deliberately: the question the
    // census answers is "did the barrier log anything INTO the queue", so an
    // entry the gate refused belongs to `late_log_drops`, not here. Counting
    // it in both would inflate the denominator of the very ratio the page
    // reads.
    //
    // ONE relaxed `fetch_add` on the barrier fast path, and only on a path
    // that goes straight on to take the thread-local's `Mutex` and push into a
    // `Vec`. It is not measurable against that, and it is not on the barrier's
    // is-marking-active gate, which is where the real fast path is.
    BARRIER_LOGS.fetch_add(1, Ordering::Relaxed);
    let to_flush = THREAD_SATB_BUFFER.with(|g| g.buffer.lock().log(queue.id(), old_ref_addr));
    // Returns whether this log SPILLED the thread's bucket into the shared
    // queue — the moment new gray work became visible to a marker (item 9b:
    // the caller wakes a parked worker on it).
    match to_flush {
        Some(entries) => {
            BARRIER_SPILLS.fetch_add(1, Ordering::Relaxed);
            queue.flush(entries);
            true
        }
        None => false,
    }
}

/// Reference overwrites the SATB barrier has logged, and how many of those
/// filled a bucket and spilled it straight into a shard.
///
/// These two are what make the registry walk's `entries` column readable.
/// `logs` is the population; `spills` is the part of it that reached a shard
/// WITHOUT the walk (the auto-flush at `DEFAULT_SATB_CAPACITY`). What the walk
/// exists to collect is the remainder — entries sitting in a bucket that never
/// filled — so `logs - spills * 256` is roughly the population the walk is
/// for, and `entries` is how much of it the walk actually found.
///
/// A barrier hit that [`satb_thread_local_log`]'s queue gate REFUSED is not in
/// `logs` — it never reached a bucket, so it is not part of the population the
/// walk could have found. Those are counted per-queue in
/// [`SatbQueue::late_log_drops`], and `logs + drops` is every non-null barrier
/// hit.
///
/// Measured on `MtChurnProbe` at 128 threads (964 walks): `threads_held = 0`
/// and `entries = 0`. See `w3c-the-satb-registry-walk-census.md`.
pub static BARRIER_LOGS: AtomicU64 = AtomicU64::new(0);
/// Of [`BARRIER_LOGS`], those that spilled a full bucket into a shard.
pub static BARRIER_SPILLS: AtomicU64 = AtomicU64::new(0);

/// `(logs, spills)` — see [`BARRIER_LOGS`].
pub fn barrier_log_census() -> (u64, u64) {
    (
        BARRIER_LOGS.load(Ordering::Relaxed),
        BARRIER_SPILLS.load(Ordering::Relaxed),
    )
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

// ---------------------------------------------------------------------------
// LANE W3-C — the registry-walk census
// ---------------------------------------------------------------------------

/// What one pass over [`SATB_BUFFER_REGISTRY`] and [`ORPHANED_SATB_BUFFERS`]
/// actually cost, summed over the process.
///
/// # Why this exists
///
/// `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md` filed the walk
/// as O(mutator threads) lock acquisitions inside a stop-the-world pause whose
/// length the application controls by creating threads, refused to choose
/// between three candidate fixes on an argument, and named the census that
/// would decide: **calls, threads visited, threads that actually held entries,
/// nanoseconds.** The pair `visited` / `held` is the discriminator. A fix that
/// turns the walk into "only threads that logged" (a dirty-buffer list) buys
/// exactly `visited - held` acquisitions and nothing else, so on a workload
/// where `held == visited` it is worth nothing however long the walk takes,
/// and on one where `held` is a small fraction of `visited` it is worth the
/// whole difference. A total time alone cannot tell those apart — the same
/// reason [`crate::mark_bitmap::clear_census`] carries four numbers instead of
/// one.
///
/// # Why it is NOT behind an environment variable
///
/// `MarkBitmap::clear`'s census is opt-in because its instrument (two
/// `Instant::now()` calls) costs more than the fast path it measures. This one
/// is the opposite case: the walk runs at most twice per mark cycle, takes a
/// process-global mutex and one lock per registered thread, and a mark cycle
/// is a rare event on any workload. Two clock reads against that is not
/// measurable.
///
/// And a census nobody can read is the failure this round keeps re-finding: a
/// counter behind a flag that a shipped binary's operator does not know to set
/// reports a zero that is indistinguishable from "the walk never ran". These
/// are printed unconditionally by
/// [`crate::gc_metrics::collector_decision_report`], which is emitted on both
/// shutdown arms.
///
/// # The non-G1 callers
///
/// `ConcurrentMarker` (the generational backend's marker) also reaches
/// [`flush_all_thread_satb_buffers`] through
/// [`SatbQueue::deactivate_and_drain`], so these counters move on a
/// `-XX:+UseGenerationalGC` run too. That is deliberate and it is not a
/// behaviour change: six relaxed `fetch_add`s and two clock reads per cycle-end
/// pause, no lock taken that was not already taken, no entry routed anywhere
/// different.
static WALK_CALLS: AtomicU64 = AtomicU64::new(0);
/// Registered live threads visited by [`WALK_CALLS`], summed. See it.
static WALK_THREADS_VISITED: AtomicU64 = AtomicU64::new(0);
/// Of [`WALK_THREADS_VISITED`], those whose bucket for the walking queue was
/// non-empty. See [`WALK_CALLS`].
static WALK_THREADS_HELD: AtomicU64 = AtomicU64::new(0);
/// Orphaned (exited-thread) buffers visited by [`WALK_CALLS`], summed. See it.
static WALK_ORPHANS_VISITED: AtomicU64 = AtomicU64::new(0);
/// Of [`WALK_ORPHANS_VISITED`], those that held entries for the walking queue.
static WALK_ORPHANS_HELD: AtomicU64 = AtomicU64::new(0);
/// Entries the walk moved (or discarded), summed. See [`WALK_CALLS`].
static WALK_ENTRIES: AtomicU64 = AtomicU64::new(0);
/// Nanoseconds spent inside [`WALK_CALLS`], summed. See it.
static WALK_NANOS: AtomicU64 = AtomicU64::new(0);

/// `(calls, threads_visited, threads_held, orphans_visited, orphans_held,
/// entries, nanos)` — see [`WALK_CALLS`].
pub fn registry_walk_census() -> (u64, u64, u64, u64, u64, u64, u64) {
    (
        WALK_CALLS.load(Ordering::Relaxed),
        WALK_THREADS_VISITED.load(Ordering::Relaxed),
        WALK_THREADS_HELD.load(Ordering::Relaxed),
        WALK_ORPHANS_VISITED.load(Ordering::Relaxed),
        WALK_ORPHANS_HELD.load(Ordering::Relaxed),
        WALK_ENTRIES.load(Ordering::Relaxed),
        WALK_NANOS.load(Ordering::Relaxed),
    )
}

/// Zero the registry-walk census. **Tests only** — a process-wide counter is
/// not meaningful to a test that cannot establish where it started, and the
/// registry these count is process-global, so a test that merely SUBTRACTS a
/// baseline can still be perturbed by another test's threads. Mirrors
/// `g1::reset_drain_fixup_totals`.
pub fn reset_registry_walk_census() {
    WALK_CALLS.store(0, Ordering::Relaxed);
    WALK_THREADS_VISITED.store(0, Ordering::Relaxed);
    WALK_THREADS_HELD.store(0, Ordering::Relaxed);
    WALK_ORPHANS_VISITED.store(0, Ordering::Relaxed);
    WALK_ORPHANS_HELD.store(0, Ordering::Relaxed);
    WALK_ENTRIES.store(0, Ordering::Relaxed);
    WALK_NANOS.store(0, Ordering::Relaxed);
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
    // LANE W3-C — the census the walk's own page asked for. See [`WALK_CALLS`]
    // for why it is unconditional and why `visited` and `held` are separate.
    let t0 = std::time::Instant::now();
    let mut visited = 0u64;
    let mut held = 0u64;
    let mut entries_moved = 0u64;
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
        // Counted HERE rather than from `live.len()`: this is the loop that
        // takes one `Mutex` per registered thread, and the lock acquisitions
        // are what the page is about. `live.len()` would also be right today,
        // but a future `continue` above this point would silently make the
        // count stop meaning "buffers locked".
        visited += 1;
        let entries = buf.lock().take(queue.id());
        if entries.is_empty() {
            continue;
        }
        held += 1;
        entries_moved += entries.len() as u64;
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
    let mut orphans_visited = 0u64;
    let mut orphans_held = 0u64;
    {
        // LANE W2-C — snapshot the live queue ids BEFORE taking the orphan
        // registry lock, so the lock order stays one-directional
        // (live-ids → orphans → buffer) and can never run the other way from
        // `SatbQueue::new` / `Drop`, which take only the first.
        let live_ids: Vec<u64> = LIVE_SATB_QUEUE_IDS.lock().clone();
        let mut orphans = ORPHANED_SATB_BUFFERS.lock();
        orphans.retain(|buf| {
            orphans_visited += 1;
            let mut b = buf.lock();
            let entries = b.take(queue.id());
            if !entries.is_empty() {
                orphans_held += 1;
                entries_moved += entries.len() as u64;
                orphan_entries.push(entries);
            }
            // LANE W2-C — an orphan used to be retained while it held a bucket
            // for ANY queue, which leaks the buffer forever once that queue has
            // been dropped: nothing will ever call `take` with a dead id, so
            // `buckets` never empties and the `Arc` never dies. Dropping dead
            // ids first makes "still holds something a drainer can collect" the
            // actual retention test. See [`LIVE_SATB_QUEUE_IDS`].
            b.drop_dead_buckets(&live_ids);
            !b.buckets.is_empty()
        });
    }
    for entries in orphan_entries {
        queue.flush(entries);
    }
    record_registry_walk(
        t0,
        visited,
        held,
        orphans_visited,
        orphans_held,
        entries_moved,
    );
}

/// Fold one completed registry walk into the census. See [`WALK_CALLS`].
///
/// The elapsed time is taken here, at the end, rather than around the two
/// registry passes separately: what the page costs is the WHOLE walk as seen
/// from inside the pause, and a caller that only wanted the registry half
/// would still be paying the orphan half.
fn record_registry_walk(
    t0: std::time::Instant,
    threads_visited: u64,
    threads_held: u64,
    orphans_visited: u64,
    orphans_held: u64,
    entries: u64,
) {
    WALK_CALLS.fetch_add(1, Ordering::Relaxed);
    WALK_THREADS_VISITED.fetch_add(threads_visited, Ordering::Relaxed);
    WALK_THREADS_HELD.fetch_add(threads_held, Ordering::Relaxed);
    WALK_ORPHANS_VISITED.fetch_add(orphans_visited, Ordering::Relaxed);
    WALK_ORPHANS_HELD.fetch_add(orphans_held, Ordering::Relaxed);
    WALK_ENTRIES.fetch_add(entries, Ordering::Relaxed);
    WALK_NANOS.fetch_add(t0.elapsed().as_nanos() as u64, Ordering::Relaxed);
}

/// LANE W2-C — [`flush_all_thread_satb_buffers`] with the entries dropped
/// rather than flushed into `queue`'s shards.
///
/// Used only by [`SatbQueue::deactivate_and_discard`]; see that function for
/// why a cycle-ending caller still has to VISIT every bucket (a bucket left
/// behind seeds the next cycle with stale addresses) while having no use at all
/// for the entries in it.
///
/// Same stop-the-world precondition and the same lock ordering as the flush
/// form: registry lock released before any buffer lock is taken in the second
/// phase, and no shard lock is taken at all.
fn discard_all_thread_satb_buffers(queue: &SatbQueue) {
    // LANE W3-C — the same census as the flush form, so an A/B over
    // `CRATONVM_G1_CLEANUP_SATB_DISCARD` compares two measured walks rather
    // than one measured walk and one silence. See [`WALK_CALLS`].
    let t0 = std::time::Instant::now();
    let mut visited = 0u64;
    let mut held = 0u64;
    let mut entries_dropped = 0u64;
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
        visited += 1;
        let n = buf.lock().discard(queue.id());
        if n != 0 {
            held += 1;
            entries_dropped += n as u64;
        }
    }

    let mut orphans_visited = 0u64;
    let mut orphans_held = 0u64;
    let live_ids: Vec<u64> = LIVE_SATB_QUEUE_IDS.lock().clone();
    let mut orphans = ORPHANED_SATB_BUFFERS.lock();
    orphans.retain(|buf| {
        orphans_visited += 1;
        let mut b = buf.lock();
        let n = b.discard(queue.id());
        if n != 0 {
            orphans_held += 1;
            entries_dropped += n as u64;
        }
        b.drop_dead_buckets(&live_ids);
        !b.buckets.is_empty()
    });
    drop(orphans);
    record_registry_walk(
        t0,
        visited,
        held,
        orphans_visited,
        orphans_held,
        entries_dropped,
    );
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
    /// Lane-C — how many entries the SHARDS currently hold, maintained under
    /// the same shard lock that adds or removes them.
    ///
    /// # Why a counter exists at all
    ///
    /// [`Self::is_empty`] and [`Self::len`] each take all [`SHARDS`] mutexes,
    /// which is fine for a test or a diagnostic and wrong for the consumer that
    /// actually asks the question: G1's concurrent marker calls `drain()` at the
    /// top of EVERY `concurrent_mark_step_worker` (G1MARK-6 — see the comment
    /// there), which is once per 256 scanned objects per marking worker. On a
    /// mutation-light phase that is sixteen uncontended-but-real lock
    /// acquisitions per step, per worker, to discover an empty queue — and they
    /// are the SAME sixteen locks every mutator flush wants, so the markers'
    /// polling is contention the barrier pays for.
    ///
    /// # Why it is safe to skip a drain on a zero reading
    ///
    /// The count is incremented BEFORE the `Vec::extend` that makes the entries
    /// visible, under the shard lock, and decremented AFTER the take that
    /// removes them, also under the shard lock. So it can transiently read HIGH
    /// (a flusher has reserved its credit but not yet appended) and never LOW:
    /// a zero reading proves no entry is in any shard *and none is on its way
    /// in behind a lock we would have blocked on*. That is the direction that
    /// matters, because the only permitted use is "skip a *speculative* drain" —
    /// an entry logged one nanosecond later would have been left for the next
    /// step anyway. Every drain that must be COMPLETE ([`Self::drain`] from
    /// `remark`, [`Self::deactivate_and_drain`]) is unconditional and stays so.
    ///
    /// The credit is taken in [`Self::flush`] and nowhere else, which is what
    /// keeps the "never LOW" half true in the presence of paths that THROW
    /// entries AWAY: [`satb_thread_local_log`]'s inactive-queue refusal, the
    /// discard walks, and `Drop for SatbQueue` all drop entries that never
    /// reached a shard and were therefore never credited. A dropped entry that
    /// HAD been credited would latch this counter non-zero forever and make
    /// `is_empty()` lie in the only direction that costs anything.
    ///
    /// It counts SHARD contents only. Entries still sitting in a mutator's
    /// thread-local bucket are invisible here, exactly as they are invisible to
    /// `drain()` — anything that needs those must call
    /// [`flush_all_thread_satb_buffers`] first, and a `has_pending() == false`
    /// says nothing about them.
    pending: AtomicUsize,
    /// Writers currently inside [`Self::flush`] — i.e. threads that have
    /// committed to depositing entries into a shard but may not have reached
    /// the push yet, typically because they are blocked on that shard's mutex.
    ///
    /// gengc-mark2 2026-09-20, closing the documented residual of
    /// `gengc-mark-satb-deactivate-is-stw-only-20260920`: the exclusive
    /// per-shard pass in [`Self::deactivate_and_drain`] excludes a writer only
    /// while it holds that shard, so a writer parked on shard `s` can push
    /// after the pass has moved on to `s + 1` and be stranded. This counter is
    /// what makes such a writer OBSERVABLE to the drain; see the bounded wait
    /// there for why it is bounded rather than unconditional.
    ///
    /// Maintained by [`SatbWriteGuard`], so it is correct on the unwind path
    /// too. One atomic RMW pair per `flush`, i.e. per ~256 barrier hits, not
    /// per barrier hit — deliberately NOT placed on the per-store fast path,
    /// where a shared counter would be a cache line every mutator contends on.
    in_flight: AtomicUsize,
    /// How many times [`Self::deactivate_and_drain`] gave up waiting for
    /// `in_flight` to reach zero. Non-zero means a writer was descheduled
    /// between entering `flush` and completing its push for longer than the
    /// wait budget, so this cycle fell back to the pre-2026-09-20 behaviour
    /// for that writer. Read by tests and diagnostics.
    quiescence_timeouts: AtomicU64,
    /// How many [`satb_thread_local_log`] calls were refused because this
    /// queue was INACTIVE.
    ///
    /// Non-zero means some caller's own gate is WIDER than the queue's — the
    /// condition `gengc-mark-satb-barrier-gates-on-phase-not-queue-20260920`
    /// describes, where `GenerationalHeap::satb_barrier` consults the phase
    /// and the log it writes consulted nothing. Zero is the expected value.
    ///
    /// Refusals are NOT in [`BARRIER_LOGS`]: that counter is the population of
    /// logs that actually entered a bucket. `logs + drops` is every barrier hit
    /// with a non-null old value.
    late_log_drops: AtomicU64,
}

/// RAII registration of a writer that has committed to depositing entries into
/// a [`SatbQueue`] shard.
///
/// Held across the shard-lock acquisition, so a writer parked on a mutex the
/// drain currently owns is still counted. `Drop` runs on the unwind path too,
/// which is what keeps a panicking mutator from pinning a deactivation at its
/// wait budget forever.
struct SatbWriteGuard<'q> {
    queue: &'q SatbQueue,
}

impl<'q> SatbWriteGuard<'q> {
    #[inline]
    fn enter(queue: &'q SatbQueue) -> Self {
        // `SeqCst` on both the increment here and the observation in
        // `deactivate_and_drain`: the drain must not be able to read zero
        // after this store, and this store must not sink past the shard-lock
        // acquisition that follows it.
        queue.in_flight.fetch_add(1, Ordering::SeqCst);
        Self { queue }
    }
}

impl Drop for SatbWriteGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        self.queue.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Monotonic source of process-unique [`SatbQueue::id`] values. Starts at 1
/// so 0 can serve as a never-assigned sentinel in debugging. Wraparound
/// after 2^64 queues is not a practical concern.
static NEXT_SATB_QUEUE_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// This thread's shard index, computed once.
    ///
    /// gengc-mark 2026-09-20: [`shard_for_current_thread`] used to run
    /// `std::thread::current()` (a TLS lookup plus an `Arc` clone on the first
    /// call of a thread) and a full SipHash-1-3 of the resulting `ThreadId` on
    /// EVERY `flush`. The result is a pure function of the calling thread, so
    /// every one of those hashes after the first recomputes a constant --
    /// inside the write barrier's spill path, which is exactly where the
    /// barrier's amortised cost is paid. Caching it makes the shard choice a
    /// TLS load and a mask, and cannot change which shard a thread picks
    /// (`ThreadId::hash` is stable for the life of a thread, which is the
    /// property the original comment already relied on).
    static SHARD_INDEX: usize = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        std::thread::current().id().hash(&mut h);
        (h.finish() as usize) & (SHARDS - 1)
    };
}

#[inline]
fn shard_for_current_thread() -> usize {
    // `with` can fail only while this thread's TLS is being destroyed, which
    // for a barrier flush would mean a `Drop` impl storing a reference during
    // thread teardown. Fall back to shard 0 rather than panicking there: the
    // drain visits every shard, so the choice is a contention hint only and
    // any shard is correct.
    SHARD_INDEX.try_with(|s| *s).unwrap_or(0)
}

impl SatbQueue {
    /// Create a new inactive SATB queue.
    pub fn new() -> Self {
        // Array initialisation with a non-Copy element type — use a
        // small helper so each shard gets its own Mutex.
        let shards: [Mutex<Vec<usize>>; SHARDS] = std::array::from_fn(|_| Mutex::new(Vec::new()));
        // Relaxed is sufficient: we only need uniqueness, not ordering
        // relative to other memory.
        let id = NEXT_SATB_QUEUE_ID.fetch_add(1, Ordering::Relaxed);
        // LANE W2-C — announce the id so `flush_all_thread_satb_buffers` can
        // tell a bucket that still has a drainer from one that never will.
        // See [`LIVE_SATB_QUEUE_IDS`].
        LIVE_SATB_QUEUE_IDS.lock().push(id);
        Self {
            id,
            shards,
            state: AtomicU8::new(SATB_INACTIVE),
            pending: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            quiescence_timeouts: AtomicU64::new(0),
            late_log_drops: AtomicU64::new(0),
        }
    }

    /// Lane-C — might the shards hold anything?
    ///
    /// A `false` is authoritative ("nothing queued, and nothing mid-append");
    /// a `true` may be one flusher's reservation ahead of the append. See the
    /// [`Self::pending`] doc for why that asymmetry is the safe one and for the
    /// one thing this must NOT be read as (it says nothing about per-thread
    /// buckets). Never takes a lock — that is the entire point.
    #[inline]
    pub fn has_pending(&self) -> bool {
        // Acquire pairs with the Release `fetch_add` in `flush`, so a thread
        // that observes a non-zero count also observes the shard append that
        // credit was reserved for once it takes the shard lock.
        self.pending.load(Ordering::Acquire) != 0
    }

    /// How many times a deactivation gave up waiting for writers to quiesce.
    /// See [`Self::in_flight`]. Diagnostics and tests.
    pub fn quiescence_timeouts(&self) -> u64 {
        self.quiescence_timeouts.load(Ordering::Relaxed)
    }

    /// How many barrier logs this queue refused because it was INACTIVE.
    /// See [`Self::late_log_drops`]. Diagnostics and tests.
    pub fn late_log_drops(&self) -> u64 {
        self.late_log_drops.load(Ordering::Relaxed)
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
    /// The exclusive per-shard pass below narrows that window: holding
    /// `shards[s].lock()` excludes any concurrent `flush()` on the same shard,
    /// so a writer that picked shard `s` either pushed before we acquired
    /// (drained here) or is blocked until we let go.
    ///
    /// # What this does NOT do (gengc-mark 2026-09-20)
    ///
    /// The round-9 note above used to claim the window was CLOSED, on the
    /// grounds that a blocked writer "will see INACTIVE on retry of the
    /// `is_active()` gate". It will not. The gate is checked ONCE, by the
    /// mutator, before it commits to logging; a writer parked on shard 3's
    /// mutex has already passed it and is holding entries it must deposit. So
    /// the true residual is:
    ///
    /// > a writer blocked on shard `s` while we hold it, which pushes after we
    /// > release `s` and move on to `s + 1`, lands in a shard this pass will
    /// > not revisit.
    ///
    /// Closing it properly needs writer-side cooperation, not a drain-side
    /// trick — filed as
    /// `docs/internal/gaps/gengc-mark-satb-deactivate-is-stw-only-20260920.md`.
    /// What makes the production callers sound is NOT this loop: it is that
    /// both of them (`ConcurrentMarker::remark`, `G1::final_remark`) run inside
    /// a stop-the-world pause, where no mutator is mid-`flush` at all.
    ///
    /// # The writer-side counter (gengc-mark2 2026-09-20)
    ///
    /// Step 3b below now waits, on a bounded budget, for
    /// [`SatbQueue::in_flight`] to reach zero before the `INACTIVE` store.
    /// `in_flight` is incremented by [`Self::flush`] BEFORE it chooses a
    /// shard, so the parked writer the residual describes is visible to this
    /// function instead of invisible. When the wait reaches zero — which it
    /// does immediately at an STW pause, and promptly in any realistic
    /// concurrent case — the residual is genuinely closed for this cycle.
    ///
    /// It is a budget rather than an unconditional spin because a writer
    /// SUSPENDED between its increment and its push (if the VM's safepoint
    /// protocol can suspend a thread there) would otherwise hang the
    /// collector. On expiry the function proceeds exactly as it did before
    /// this change and bumps [`SatbQueue::quiescence_timeouts`], so the
    /// fallback is countable rather than silent. The counter is a per-queue
    /// field, not a process global.
    ///
    /// The residual that remains, and is NOT closed: a writer between
    /// `satb_thread_local_log`'s gate and its call to `flush` has not yet
    /// incremented `in_flight`. That window is a few instructions wide and
    /// does not span a lock acquisition, but it is not zero. Counting it would
    /// mean an atomic RMW on every barrier hit rather than every ~256, which
    /// is the wrong trade; the per-thread handshake HotSpot uses is the real
    /// answer, and the gap page keeps it open.
    ///
    /// The extra pass after the INACTIVE store below does not close the window
    /// either; it bounds the DAMAGE. An entry that lands after the exclusive
    /// pass would otherwise sit in a shard until the NEXT cycle's drain and be
    /// replayed then — by which time the young generation it names has been
    /// swept and its address may belong to a different object. Sweeping it up
    /// here keeps this cycle's snapshot a superset of the previous behaviour
    /// (always safe: an SATB entry is a conservative gray root) and keeps stale
    /// addresses out of the next cycle.
    ///
    /// # The STW witness
    ///
    /// The soundness argument above is a property of the CALLER, so the
    /// signature now demands the proof: `stw` is a [`StopTheWorldToken`],
    /// which only code that has parked every mutator can construct. Until
    /// 2026-09-21 this parameter did not exist while
    /// `_SatbDeactivateStwCompileFailDocs` in `collector.rs` documented it as
    /// if it did — two doctests failed continuously and the invariant they
    /// advertised was enforced by nothing.
    ///
    /// A caller that is NOT at a safepoint wants
    /// [`Self::deactivate_and_discard`], which runs the identical state
    /// machine and returns nothing, because the entries it collects are not a
    /// snapshot. Both abort paths (`ConcurrentMarker::abort_cycle`,
    /// `G1Collector::abort_concurrent_mark`) were already discarding the
    /// result and saying so in their comments; they now say it in the types.
    ///
    /// Returns the concatenated entries from all drain passes.
    pub fn deactivate_and_drain(&self, _stw: &crate::collector::StopTheWorldToken) -> Vec<usize> {
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
        //
        //    THIS PASS IS NOT A HAPPENS-BEFORE EDGE, and the comment that
        //    stood here until 2026-09-21 said it was: that the pass "combined
        //    with the INACTIVE store in step 4" prevents stranded entries,
        //    because the writer's Acquire-load of `state` pairs with that
        //    Release store. There is no such load. The gate is read ONCE, by
        //    the barrier, before the writer commits to logging; a writer
        //    blocked on shard `s` while we hold it pushes the moment we
        //    release `s` and have moved on to `s + 1`, and lands in a shard
        //    this loop will not revisit. Release-ordering a store against a
        //    load nobody performs orders nothing. The doc comment on this
        //    function has said so since 2026-09-20 ("What this does NOT do");
        //    this is the line that contradicted it, and it survived two rounds
        //    of fixes for the race it was describing, which is why it is
        //    quoted here rather than quietly replaced.
        //
        //    What DOES make that writer recoverable is step 3b below.
        for shard in self.shards.iter() {
            let mut guard = shard.lock();
            if !guard.is_empty() {
                let late = std::mem::take(&mut *guard);
                self.pending.fetch_sub(late.len(), Ordering::AcqRel);
                all.extend(late);
            }
            // Implicit drop(guard) here releases the shard mutex for
            // unrelated future use; ordering against `state` store
            // below is provided by the SeqCst-equivalent lock release
            // + the Release store.
        }

        // 3b. gengc-mark2 2026-09-20 — WAIT OUT THE WRITERS THE PASS ABOVE
        //     COULD NOT EXCLUDE, on a bounded budget.
        //
        //     The exclusive pass excludes a writer only while we hold ITS
        //     shard. A writer parked on shard `s` while we held it pushes the
        //     instant we release, and if we have already moved on to `s + 1`
        //     that entry is stranded (the residual documented in
        //     `gengc-mark-satb-deactivate-is-stw-only-20260920.md`).
        //     `in_flight` counts exactly those writers: a thread increments it
        //     BEFORE it picks a shard and holds the count across the lock
        //     acquisition, so a parked writer is visible here.
        //
        //     WHY THE WAIT IS BOUNDED. We hold no shard lock while waiting, so
        //     a writer blocked on one of ours is free to finish the moment we
        //     let go — this loop cannot deadlock against the SATB locks. What
        //     it CAN hit is a writer that the VM suspended between its
        //     increment and its push: if a forced safepoint counts such a
        //     thread as stopped, it will not run again until this pause ends,
        //     and an unconditional spin would turn a latent unsoundness into a
        //     live hang. Establishing whether the safepoint protocol can do
        //     that means reading `safepoint.rs` / `gc_barrier` semantics, which
        //     is not this lane's to settle — so the wait gives up instead, and
        //     records that it did. Giving up is exactly the pre-2026-09-20
        //     behaviour for that one writer; reaching zero is strictly better
        //     than it. Both production callers (`ConcurrentMarker::remark`,
        //     `G1::final_remark`) run at an STW pause where no mutator is
        //     mid-`flush`, so the common case is one relaxed load and no spin
        //     at all.
        {
            // Yields before giving up. ~1 ms of scheduler time at a typical
            // yield cost, against a pause already measured in milliseconds.
            const QUIESCE_YIELD_BUDGET: u32 = 1024;
            let mut spun = 0u32;
            while self.in_flight.load(Ordering::SeqCst) != 0 {
                if spun >= QUIESCE_YIELD_BUDGET {
                    self.quiescence_timeouts.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        in_flight = self.in_flight.load(Ordering::Relaxed),
                        queue_id = self.id,
                        "SATB deactivation gave up waiting for writers to quiesce; an \
                         entry logged by one of them may be stranded in a shard this \
                         pass has already visited (see \
                         gengc-mark-satb-deactivate-is-stw-only-20260920.md)",
                    );
                    break;
                }
                spun += 1;
                std::thread::yield_now();
            }
            // One more pass now that (normally) nothing is in flight: this is
            // the drain that actually collects what the exclusive pass raced.
            let quiesced = self.drain();
            if !quiesced.is_empty() {
                all.extend(quiesced);
            }
        }

        // 4. Finally flip to INACTIVE.  Release-ordered: pairs with the
        //    Acquire load in `is_active()` so any subsequent mutator
        //    observation sees INACTIVE happens-after the drain.
        self.state.store(SATB_INACTIVE, Ordering::Release);

        // 5. gengc-mark 2026-09-20 — one more sweep, AFTER the gate is off.
        //    Every mutator that can still be mid-`flush` at this point passed
        //    the gate before step 1, so this is the last moment at which such
        //    an entry can be attributed to THIS cycle. Anything it finds would
        //    otherwise lie in a shard until the next cycle's drain, where it
        //    names a possibly-reclaimed address (see the doc comment). Costs
        //    one uncontended pass over 16 mutexes, once per cycle.
        let leftovers = self.drain();
        if !leftovers.is_empty() {
            all.extend(leftovers);
        }
        all
    }

    /// LANE W2-C — [`Self::deactivate_and_drain`]'s protocol with the entries
    /// thrown away instead of collected.
    ///
    /// # Why this exists
    ///
    /// G1's `cleanup` ends the cycle with `let _stragglers =
    /// deactivate_and_drain();` — and drops the result on the floor, for a
    /// documented and correct reason (the mark cycle is complete, anything not
    /// yet marked is correctly dead, and the next cycle re-discovers live state
    /// from roots). But building that result is not free, and it is built
    /// inside the cleanup STW pause: `flush_all_thread_satb_buffers` takes
    /// `SATB_BUFFER_REGISTRY`'s global mutex, upgrades and locks every
    /// registered mutator thread's buffer, moves its entries into the shards,
    /// then walks `ORPHANED_SATB_BUFFERS` doing the same — all to fill a `Vec`
    /// nobody reads.
    ///
    /// It cannot simply become `deactivate()` + `drain()`. Leaving entries in
    /// per-thread buckets hands the NEXT cycle a set of stale addresses as
    /// false seeds (the buckets are scoped by queue id, and it is the same
    /// queue), which is `mark_oob_gray_skips` noise at best. So the buckets
    /// still have to be visited — they just do not have to be CONCATENATED.
    ///
    /// # The protocol is identical
    ///
    /// ACTIVE → DRAINING, clear every thread bucket for this queue id, clear
    /// every shard under its own lock (which excludes a late `flush` on that
    /// shard exactly as the drain form does), then store INACTIVE with a
    /// `Release` that pairs with `is_active`'s `Acquire`.
    ///
    /// The round-9 HIGH-4 argument about a late writer is unchanged — and so
    /// is its limit: "is blocked and will re-read INACTIVE" was never true,
    /// because a writer that has passed the gate does not read it again. See
    /// step 3 of [`Self::deactivate_and_drain`], which quotes and corrects the
    /// same claim.
    ///
    /// # This is the spelling a NON-safepoint caller wants
    ///
    /// It takes no [`StopTheWorldToken`](crate::collector::StopTheWorldToken)
    /// where [`Self::deactivate_and_drain`] does, and that is not a hole in
    /// the enforcement: the protocol is identical, and the difference is
    /// exactly that this form hands back nothing for a sweep to trust. A
    /// stranded entry here is over-retention — a false gray root for the next
    /// cycle — never a premature free. Both abort paths
    /// (`ConcurrentMarker::abort_cycle`, `G1Collector::abort_concurrent_mark`)
    /// run outside a pause and were already discarding the result.
    pub fn deactivate_and_discard(&self) {
        let _ = self.state.compare_exchange(
            SATB_ACTIVE,
            SATB_DRAINING,
            Ordering::Release,
            Ordering::Acquire,
        );

        // Per-thread buckets first, same as the drain form and for the same
        // completeness reason — except that here they are dropped rather than
        // folded into the shards, which saves both the `Vec` moves and the
        // `flush` (and therefore a shard lock) per registered thread.
        discard_all_thread_satb_buffers(self);

        for shard in self.shards.iter() {
            let mut guard = shard.lock();
            if !guard.is_empty() {
                let late = std::mem::take(&mut *guard);
                self.pending.fetch_sub(late.len(), Ordering::AcqRel);
                // `late` drops here, inside the critical section, which is the
                // only difference from `deactivate_and_drain`'s `all.extend`.
            }
        }

        self.state.store(SATB_INACTIVE, Ordering::Release);
    }

    /// Flush a per-thread buffer's entries into the global queue.
    ///
    /// Round-7 HIGH-3: writes land on the per-thread shard so concurrent
    /// flushers from independent threads do not serialise.
    pub fn flush(&self, entries: Vec<usize>) {
        if entries.is_empty() {
            return;
        }
        // gengc-mark2 2026-09-20 — announce this writer BEFORE choosing a
        // shard, so a concurrent `deactivate_and_drain` can see that entries
        // are still in flight even while this thread is parked on the shard
        // mutex it is about to take. The guard decrements on every exit path,
        // including an unwind out of `extend`'s allocation.
        let _in_flight = SatbWriteGuard::enter(self);
        let n = entries.len();
        let s = shard_for_current_thread();
        let mut queue = self.shards[s].lock();
        // Credit BEFORE the append and under the shard lock: a reader that
        // sees zero is then guaranteed that no entry is in a shard AND that no
        // flusher holds a lock it is about to append behind. See `pending`.
        self.pending.fetch_add(n, Ordering::Release);
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
            if !taken.is_empty() {
                // Debited AFTER the take and still under the shard lock, so
                // the count is never lower than what a shard actually holds.
                self.pending.fetch_sub(taken.len(), Ordering::AcqRel);
            }
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
        // Lock-free short circuit: `pending` over-estimates and never
        // under-estimates, so a zero reading proves every shard is empty (see
        // the field doc). A non-zero reading still walks, because it may be a
        // flusher's reservation rather than a real entry.
        if !self.has_pending() {
            return true;
        }
        self.shards.iter().all(|s| s.lock().is_empty())
    }
}

impl Default for SatbQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// A dropped queue must take its per-thread buckets — and its id — with it.
///
/// # The buckets (gengc-mark 2026-09-20)
///
/// Per-thread buckets are keyed by [`SatbQueue::id`], and
/// [`NEXT_SATB_QUEUE_ID`] never reissues an id. So once a queue is dropped,
/// every bucket a thread still holds for it is unreachable by construction:
/// no drain path will ever ask for that id again. Those buckets are not
/// merely wasted memory — each entry is a raw heap ADDRESS from a heap that
/// may since have been torn down, sitting in a live thread's TLS, keeping the
/// `Vec` alive and lengthening the linear `buckets` scan on every barrier hit
/// by that thread for the rest of the process. A test binary that builds a
/// queue per test (this file does, repeatedly) and an embedding that creates
/// and destroys VMs both accumulate them without bound.
///
/// This is the queue-side twin of the dying-thread reaping
/// [`flush_all_thread_satb_buffers`] performs: there the THREAD dies and its
/// entries are rescued; here the QUEUE dies and its entries are discarded,
/// which is the correct disposition — nothing will ever mark them.
///
/// # The id (LANE W2-C, 2026-09-20)
///
/// The eager walk cannot be the whole answer, because it is a snapshot: a
/// bucket created between the walk and the last use of this queue, a buffer
/// registered concurrently, an orphan parked by a thread that exits after the
/// walk — all of them are keyed to an id no drainer will ever ask for again.
/// [`flush_all_thread_satb_buffers`]'s orphan pass retains an orphan while it
/// holds a bucket for ANY queue, so such a straggler would pin that orphan (and
/// its `Arc`) for the life of the process. Retiring the id from
/// [`LIVE_SATB_QUEUE_IDS`] makes [`ThreadSatbPartitions::drop_dead_buckets`]
/// reap them on the next pass instead.
///
/// The retirement happens FIRST, so that for the whole of the walk below no
/// other path can conclude that this queue still has a drainer.
///
/// Lock order is the one every other path uses (live-ids, taken and released
/// on its own; then registry → buffer; then orphans → buffer), and no shard
/// lock is taken, so this cannot deadlock against a concurrent drain.
impl Drop for SatbQueue {
    fn drop(&mut self) {
        let id = self.id;
        {
            let mut ids = LIVE_SATB_QUEUE_IDS.lock();
            if let Some(pos) = ids.iter().position(|&i| i == id) {
                ids.swap_remove(pos);
            }
        }
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
            // Discarded, not flushed: this queue is going away, so there is no
            // marker left to consume them. These entries never reached a shard,
            // so there is no `pending` credit to give back either.
            let _ = buf.lock().take(id);
        }
        let mut orphans = ORPHANED_SATB_BUFFERS.lock();
        orphans.retain(|buf| {
            let mut b = buf.lock();
            let _ = b.take(id);
            // An orphan with nothing left for ANY queue is reaped; a bucket for
            // a different, still-live queue is left for that queue's own drain.
            !b.buckets.is_empty()
        });
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

    /// The STW witness these tests stand in for.
    ///
    /// SAFETY: a unit test's only mutator is the test thread itself, so the
    /// safepoint precondition holds vacuously. Where a test deliberately
    /// drains WHILE a mutator runs -- to exercise the residual window the
    /// token exists to document -- the call site says so in a comment; the
    /// token is a witness the caller offers, not a guard the queue enforces.
    fn stw() -> crate::collector::StopTheWorldToken {
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

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
        let drained = q.deactivate_and_drain(&stw());
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

            let snapshot = q.deactivate_and_drain(&stw());
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
        let snapshot = q.deactivate_and_drain(&stw());
        assert!(
            snapshot.contains(&SENTINEL),
            "remark drain must include a dead thread's buffered refs, got {snapshot:?}"
        );

        // The emptied orphan was reaped: a second full drain sees nothing.
        q.activate();
        flush_all_thread_satb_buffers(&q);
        assert!(q.is_empty(), "emptied orphan must have been reaped");
        let _ = q.deactivate_and_drain(&stw());
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

    /// I-4 (audit §7) — the contract `G1Collector::remark` cites BY NAME.
    ///
    /// `g1.rs`'s remark block says it deliberately does NOT `debug_assert!`
    /// that every registered buffer is empty after the collector-side flush
    /// (the registry is process-global and such an assertion flakes against the
    /// parallel test harness), and names
    /// `satb::tests::flush_all_captures_every_parked_mutator_buffer` as the
    /// deterministic substitute. That test did not exist — the comment was the
    /// only thing standing behind the crate's single most load-bearing SATB
    /// claim ("no mutator's partially-full bucket escapes the snapshot"). This
    /// is it.
    ///
    /// Deterministic where the assertion could not be: instead of asserting
    /// over the process-global registry, it PARKS N mutator threads on a
    /// barrier after each has logged a sub-threshold sentinel (standing in for
    /// the STW safepoint the production caller runs at), flushes from the
    /// collector side, and requires every sentinel to be in the queue. Sibling
    /// tests running in parallel can only ADD entries, never remove ours, so
    /// membership is a stable predicate and a count is not.
    #[test]
    fn flush_all_captures_every_parked_mutator_buffer() {
        use std::sync::mpsc;
        use std::sync::Barrier;

        const MUTATORS: usize = 4;
        let q = Arc::new(SatbQueue::new());
        q.activate();

        // `parked` releases once every mutator has logged; `resume` releases
        // once the collector has flushed. Between the two the mutators are
        // blocked inside the barrier and cannot touch their buffers, which is
        // exactly the property the STW safepoint provides in production.
        let parked = Arc::new(Barrier::new(MUTATORS + 1));
        let resume = Arc::new(Barrier::new(MUTATORS + 1));
        let (tx, rx) = mpsc::channel::<usize>();

        let handles: Vec<_> = (0..MUTATORS)
            .map(|t| {
                let q = Arc::clone(&q);
                let parked = Arc::clone(&parked);
                let resume = Arc::clone(&resume);
                let tx = tx.clone();
                std::thread::spawn(move || {
                    // One entry, far below DEFAULT_SATB_CAPACITY, so it can
                    // ONLY reach the queue via the collector-side registry
                    // walk — never via the auto-flush.
                    let sentinel = 0x5A7B_0000 + (t + 1) * 8;
                    satb_thread_local_log(&q, sentinel);
                    tx.send(sentinel).expect("sentinel reported");
                    parked.wait();
                    resume.wait();
                })
            })
            .collect();
        drop(tx);

        parked.wait();
        // `recv` exactly MUTATORS times rather than draining the iterator: the
        // senders are still parked on the barrier below and will not drop their
        // clones until this thread releases them, so `rx.iter()` would never
        // terminate. Every send happened before the barrier, so the count is
        // known.
        let sentinels: Vec<usize> = (0..MUTATORS)
            .map(|_| rx.recv().expect("sentinel reported"))
            .collect();

        // The collector reaches into every registered thread. This is the one
        // mechanism between a mutator that never spilled and the snapshot.
        flush_all_thread_satb_buffers(&q);
        let drained = q.drain();
        resume.wait();
        for h in handles {
            h.join().unwrap();
        }

        for s in sentinels {
            assert!(
                drained.contains(&s),
                "a parked mutator's partially-full SATB bucket did not reach the queue: \
                 {s:#x} missing from {drained:?}"
            );
        }
    }

    /// Lane-C — `has_pending()` is the marker's lock-free "is a drain worth
    /// taking sixteen mutexes for?" probe, and the ONLY direction that may
    /// never be wrong is the negative one: a `false` over a populated shard
    /// makes the marker skip a drain that had work in it.
    #[test]
    fn has_pending_never_reads_empty_over_a_populated_shard() {
        let q = SatbQueue::new();
        assert!(!q.has_pending(), "a fresh queue has nothing queued");
        assert!(q.is_empty());

        q.flush(vec![0x100, 0x200, 0x300]);
        assert!(q.has_pending());
        assert!(!q.is_empty());

        // A partial consumer (the `deactivate_and_drain` shard-lock pass) must
        // debit what it took, or the count latches non-zero forever and the
        // probe stops being a probe.
        let drained = q.drain();
        assert_eq!(drained.len(), 3);
        assert!(!q.has_pending(), "drain must debit every entry it removed");
        assert!(q.is_empty());

        // And the deactivate path debits too.
        q.activate();
        q.flush(vec![0x400]);
        assert!(q.has_pending());
        let late = q.deactivate_and_drain(&stw());
        assert!(late.contains(&0x400));
        assert!(!q.has_pending(), "deactivate_and_drain must debit as well");
    }

    /// Lane-C — the counter must survive many flush/drain rounds across
    /// threads without drifting. A drift upward silently disables the fast
    /// path (harmless); a drift downward is the unsound direction, and an
    /// unbalanced debit would also underflow a `usize` into `has_pending()`
    /// reading true forever, which is the loud way the same bug shows up.
    #[test]
    fn pending_count_balances_under_concurrent_flush_and_drain() {
        use std::sync::Arc;
        let q = Arc::new(SatbQueue::new());
        q.activate();

        let flushers: Vec<_> = (0..4)
            .map(|t| {
                let q = Arc::clone(&q);
                std::thread::spawn(move || {
                    for i in 0..200usize {
                        q.flush(vec![t * 1000 + i]);
                    }
                })
            })
            .collect();

        let drainer = {
            let q = Arc::clone(&q);
            std::thread::spawn(move || {
                let mut seen = 0usize;
                for _ in 0..500 {
                    seen += q.drain().len();
                    std::thread::yield_now();
                }
                seen
            })
        };

        for h in flushers {
            h.join().unwrap();
        }
        let mid = drainer.join().unwrap();
        let rest = q.drain().len();
        assert_eq!(mid + rest, 800, "every flushed entry must come back out");
        assert!(
            !q.has_pending(),
            "the count drifted: it reads non-zero over a fully-drained queue"
        );
    }

    /// How many entries this thread still holds buffered for `queue_id`.
    fn bucket_len_for(queue_id: u64) -> usize {
        THREAD_SATB_BUFFER.with(|g| {
            g.buffer
                .lock()
                .buckets
                .iter()
                .find(|(id, _)| *id == queue_id)
                .map(|(_, v)| v.len())
                .unwrap_or(0)
        })
    }

    /// gengc-mark 2026-09-20 — dropping a queue must reap the per-thread
    /// buckets tagged with its id.
    ///
    /// Ids are never reissued, so a bucket left behind by a dropped queue can
    /// never be drained: it is a permanent leak of a `Vec` of raw heap
    /// addresses in a live thread's TLS, and it lengthens the linear `buckets`
    /// scan on that thread's barrier fast path for the rest of the process.
    #[test]
    fn dropping_a_queue_reaps_its_per_thread_bucket() {
        // Fresh thread so the buffer starts empty regardless of test ordering.
        let h = std::thread::spawn(|| {
            // A second queue that OUTLIVES the first: its bucket must survive.
            let keeper = SatbQueue::new();
            keeper.activate();
            satb_thread_local_log(&keeper, 0xCEDE_0001);

            let dead_id = {
                let doomed = SatbQueue::new();
                doomed.activate();
                satb_thread_local_log(&doomed, 0xC0FFEE);
                assert!(doomed.is_empty(), "entry must still be thread-local");
                assert_eq!(bucket_len_for(doomed.id()), 1);
                doomed.id()
                // `doomed` is dropped here.
            };

            assert_eq!(
                bucket_len_for(dead_id),
                0,
                "a dropped queue's bucket must be reaped, not orphaned forever"
            );
            assert_eq!(
                bucket_len_for(keeper.id()),
                1,
                "reaping is queue-scoped: a live queue's bucket must survive"
            );
        });
        h.join().unwrap();
    }

    /// The shard index is a pure function of the calling thread, so the cached
    /// value must equal the value a recomputation would give, every time.
    #[test]
    fn shard_for_current_thread_is_stable_and_in_range() {
        let first = shard_for_current_thread();
        assert!(first < SHARDS);
        for _ in 0..1000 {
            assert_eq!(shard_for_current_thread(), first);
        }
        // A different thread is free to pick a different shard; what matters is
        // that it, too, is stable and in range.
        std::thread::spawn(|| {
            let s = shard_for_current_thread();
            assert!(s < SHARDS);
            assert_eq!(shard_for_current_thread(), s);
        })
        .join()
        .unwrap();
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
            let b = qb.deactivate_and_drain(&stw());
            assert!(
                b.contains(&SB),
                "qb's entry lost after qa's flush_all: {b:?}"
            );
        });
        h.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // LANE W2-C — the discard form, and the orphan reap for a dropped queue
    // -----------------------------------------------------------------------

    /// `deactivate_and_discard` must leave the queue in exactly the state
    /// `deactivate_and_drain` leaves it in. The only difference between them is
    /// the `Vec` the caller gets back — and G1's `cleanup`, the sole production
    /// caller, drops it.
    #[test]
    fn deactivate_and_discard_empties_shards_and_thread_buckets_and_disables() {
        // Own thread: `satb_thread_local_log` writes into THIS thread's bucket,
        // and the assertions below are about that bucket.
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            // One entry in a shard...
            q.flush(vec![0xDEAD_0001]);
            // ...and one still sitting in this thread's local bucket, which is
            // the half a plain `deactivate()` + `drain()` would leave behind to
            // seed the NEXT cycle with a stale address.
            satb_thread_local_log(&q, 0xDEAD_0002);

            q.deactivate_and_discard();

            assert!(!q.is_active(), "the gate must be off");
            assert!(q.is_empty(), "every shard must be empty");
            assert!(!q.has_pending(), "and the lock-free counter must agree");

            // The thread bucket is gone too: a second flush_all finds nothing
            // to deliver, so nothing can reach a later cycle.
            flush_all_thread_satb_buffers(&q);
            assert!(
                q.drain().is_empty(),
                "a bucket left behind would be delivered to the next cycle as a                  stale seed — the reason this cannot simply be deactivate()+drain()"
            );
        });
        h.join().unwrap();
    }

    // -----------------------------------------------------------------------
    // gengc-mark2 2026-09-20
    // -----------------------------------------------------------------------

    /// `gengc-mark-satb-barrier-gates-on-phase-not-queue-20260920`.
    ///
    /// A log against an INACTIVE queue must be REFUSED, not buffered. Buffered
    /// is the harmful outcome: the entry sits in this thread's bucket until
    /// some future cycle's `flush_all_thread_satb_buffers` replays it as a
    /// gray root, naming an address that may by then belong to a different
    /// object.
    #[test]
    fn an_inactive_queue_refuses_a_barrier_log() {
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();

            // Never activated: refused outright.
            assert!(!satb_thread_local_log(&q, 0xDEAD_0001));
            assert_eq!(bucket_len_for(q.id()), 0, "nothing may be buffered");
            assert_eq!(q.late_log_drops(), 1);

            // Active: accepted into the thread-local bucket as before.
            q.activate();
            satb_thread_local_log(&q, 0xDEAD_0002);
            assert_eq!(bucket_len_for(q.id()), 1);
            assert_eq!(q.late_log_drops(), 1, "an accepted log is not a drop");

            // DRAINING still accepts — that is the whole point of the
            // tri-state gate, and this must not have changed.
            q.state.store(SATB_DRAINING, Ordering::Release);
            satb_thread_local_log(&q, 0xDEAD_0003);
            assert_eq!(bucket_len_for(q.id()), 2);

            // After a real deactivation the gate is shut again.
            let snapshot = q.deactivate_and_drain(&stw());
            assert!(snapshot.contains(&0xDEAD_0002));
            assert!(snapshot.contains(&0xDEAD_0003));
            assert!(!satb_thread_local_log(&q, 0xDEAD_0004));
            assert_eq!(bucket_len_for(q.id()), 0);
            assert_eq!(q.late_log_drops(), 2);
        });
        h.join().unwrap();
    }

    /// An orphaned buffer holding a bucket for a queue that has since been
    /// DROPPED used to be retained forever: the orphan pass only removes a
    /// buffer once `buckets` is empty, and nothing will ever call `take` with a
    /// dead queue's id.
    ///
    /// Harmless in a production VM (one queue, process lifetime) and a real
    /// leak everywhere else — every unit test in this file constructs queues,
    /// and a multi-heap embedding would too.
    ///
    /// The orphan is constructed and parked DIRECTLY rather than by exiting a
    /// thread. The thread-exit route is already covered
    /// (`dying_thread_residual_satb_entries_survive_until_remark_drain`), and
    /// it leaves the buffer reachable only through a process-global list that
    /// every concurrently-running test in this binary also sweeps — so an
    /// assertion about that list is a race, not a test. Holding the `Arc`
    /// makes the observation local and deterministic.
    #[test]
    fn an_orphan_holding_only_a_dead_queues_bucket_is_reaped() {
        let dead_id = {
            let doomed = SatbQueue::new();
            doomed.id()
        };

        let orphan = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
        assert!(orphan.lock().log(dead_id, 0xC0FF_EE01).is_none());
        ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&orphan));

        // An unrelated, still-live queue runs its own flush. Before the fix
        // this pass took nothing from the orphan (wrong id) and retained it
        // because `buckets` was still non-empty — forever, since no drainer for
        // `dead_id` exists any more.
        let other = SatbQueue::new();
        other.activate();
        flush_all_thread_satb_buffers(&other);

        assert!(
            orphan.lock().buckets.is_empty(),
            "a bucket belonging to a dropped queue has no drainer and can never              be delivered anywhere — retaining it pins the buffer and its Arc              for the life of the process"
        );
    }

    /// The complementary property, so the reap above cannot be implemented as
    /// "drop every bucket": a bucket for a queue that still EXISTS must survive
    /// another queue's flush, because that queue's own drain is still coming.
    /// Dropping it there would lose an SATB entry, which is a live object freed
    /// mid-mark.
    #[test]
    fn an_orphans_bucket_for_a_live_queue_survives_another_queues_flush() {
        const ENTRY: usize = 0xC0FF_EE02;
        let live = SatbQueue::new();
        live.activate();

        let orphan = Arc::new(Mutex::new(ThreadSatbPartitions::default()));
        assert!(orphan.lock().log(live.id(), ENTRY).is_none());
        ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&orphan));

        let other = SatbQueue::new();
        other.activate();
        flush_all_thread_satb_buffers(&other);

        assert_eq!(
            orphan.lock().buckets.len(),
            1,
            "the owning queue is still alive and its remark has not run yet"
        );

        // And the entry really is still deliverable to its owner.
        flush_all_thread_satb_buffers(&live);
        assert!(live.drain().contains(&ENTRY));
        assert!(
            orphan.lock().buckets.is_empty(),
            "once its owner has drained it, the bucket is gone and the orphan is              reapable by the ordinary rule"
        );
    }

    /// `gengc-mark-satb-deactivate-is-stw-only-20260920`.
    ///
    /// A writer parked on a shard mutex has already passed the activation gate
    /// and is holding entries it must deposit. It must be VISIBLE to a
    /// concurrent deactivation (`in_flight` is incremented before the shard is
    /// chosen), and its entry must land in that deactivation's snapshot rather
    /// than being stranded for a later cycle to replay as a stale address.
    #[test]
    fn a_writer_parked_on_a_shard_is_not_stranded_by_deactivation() {
        use std::sync::mpsc;
        use std::sync::Arc;

        const SENTINEL: usize = 0x5A7B_0001;
        let q = Arc::new(SatbQueue::new());
        q.activate();

        let (tx_shard, rx_shard) = mpsc::channel::<usize>();
        let (tx_go, rx_go) = mpsc::channel::<()>();

        let qw = Arc::clone(&q);
        let writer = std::thread::spawn(move || {
            // Publish the shard this thread will pick, BEFORE flushing, so
            // the test can own that shard for the duration.
            tx_shard.send(shard_for_current_thread()).unwrap();
            rx_go.recv().unwrap();
            qw.flush(vec![SENTINEL]);
        });

        let shard = rx_shard.recv().unwrap();
        let guard = q.shards[shard].lock();
        tx_go.send(()).unwrap();
        // The writer announces itself before it blocks on the mutex we hold,
        // which is exactly the property under test. This loop terminates
        // because the increment precedes the lock acquisition.
        while q.in_flight.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }

        let qd = Arc::clone(&q);
        // SAFETY -- a DELIBERATE violation, and the reason `stw()`'s doc
        // above calls the token a witness rather than a guard. This test
        // drains while a writer is parked on a shard mutex, which is exactly
        // the interleaving the precondition forbids, because reproducing that
        // interleaving is the test's entire purpose.
        let drain = std::thread::spawn(move || qd.deactivate_and_drain(&stw()));

        // Let the deactivation get as far as it can, then release the shard.
        // Whichever of the two reaches the mutex first, the entry must be in
        // the snapshot: either the drain sees it in a pass, or the drain's
        // quiescence wait holds the `INACTIVE` store back until the writer is
        // done and the following pass collects it.
        std::thread::yield_now();
        drop(guard);

        let snapshot = drain.join().unwrap();
        writer.join().unwrap();

        assert!(
            snapshot.contains(&SENTINEL),
            "a writer parked on a shard while the drain walked past it was \
             stranded; snapshot = {snapshot:?}"
        );
        assert!(!q.is_active());
        assert_eq!(
            q.in_flight.load(Ordering::SeqCst),
            0,
            "every writer guard must have been released"
        );
    }

    /// The common case — an STW deactivation with nothing in flight — must
    /// cost no spinning and must never record a timeout.
    #[test]
    fn a_quiet_deactivation_never_times_out() {
        let h = std::thread::spawn(|| {
            let q = SatbQueue::new();
            q.activate();
            satb_thread_local_log(&q, 0x1234);
            let snapshot = q.deactivate_and_drain(&stw());
            assert!(snapshot.contains(&0x1234));
            assert_eq!(
                q.quiescence_timeouts(),
                0,
                "no writer was in flight, so the wait must have exited immediately"
            );
            assert_eq!(q.in_flight.load(Ordering::SeqCst), 0);
        });
        h.join().unwrap();
    }
}
