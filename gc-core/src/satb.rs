// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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
//! to process. (A standalone `SatbBuffer` type exists for the unit tests only.)

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
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
//      `docs/feature-designs/jit-barrier-lowering.md`.
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
pub const SATB_INACTIVE: u8 = 0;
pub const SATB_ACTIVE: u8 = 1;
pub const SATB_DRAINING: u8 = 2;

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
        let mut b = self.buffer.lock();
        // gc-common w3-e — retire this thread's share of the barrier-log
        // census into the process total (see [`BARRIER_LOGS`]). Zeroed under
        // the same lock so an orphaned buffer is never counted twice by
        // [`barrier_log_census`]'s registry walk.
        let logs = std::mem::take(&mut b.logs);
        if logs != 0 {
            BARRIER_LOGS.fetch_add(logs, Ordering::Relaxed);
        }
        let keep = !b.buckets.is_empty();
        drop(b);
        if keep {
            ORPHANED_SATB_BUFFERS.lock().push(Arc::clone(&self.buffer));
            // The orphan list's strong `Arc` keeps this buffer's registry
            // `Weak` upgrading forever, so without this the registry pass and
            // the orphan pass would BOTH walk the buffer (two lock
            // acquisitions per exited thread per remark / cleanup pause) and
            // the slot would never be pruned. Unregister AFTER parking, so a
            // concurrent walk sees the buffer in the registry, the orphan
            // list or both (the second visit finds it empty), never neither.
            // The orphan list is locked and released before the registry is
            // taken: no lock-order edge between the two.
            let me = Arc::downgrade(&self.buffer);
            SATB_BUFFER_REGISTRY.lock().retain(|w| !w.ptr_eq(&me));
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
    buckets: Vec<SatbBucket>,
    /// This thread's share of [`BARRIER_LOGS`]: logs accepted into a bucket,
    /// not yet folded into the process total (the fold happens when the
    /// thread exits, in `Drop for SatbBufferGuard`). Counted HERE, under the
    /// thread's own uncontended lock, instead of by a process-global
    /// `fetch_add` on every logged store -- gc-common w3-e, the first bullet
    /// of `common-g-proposal-satb-owner-local-buffer-REJECTED-20260928`.
    logs: u64,
    /// One emptied bucket `Vec`, kept for the next bucket this thread opens.
    ///
    /// gc-common w5-e, the third bullet of
    /// `common-g-proposal-satb-owner-local-buffer-REJECTED-20260928`: a full bucket used to be
    /// handed to [`SatbQueue::flush`], which COPIED its entries into a shard
    /// (`extend`) and freed the 2 KiB buffer, and the very next log allocated
    /// a fresh one — one allocator round trip per 256 marking-time reference
    /// stores, per thread. The spill now empties the bucket in place
    /// ([`SatbQueue::flush_recycling`], same copy) and parks it here. Not
    /// counted as a bucket anywhere: `SatbBufferGuard::drop`, the walks and
    /// `drop_dead_buckets` look only at `buckets`, so an idle spare never
    /// orphans a buffer or delays a reap.
    spare: Option<Vec<usize>>,
    /// gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in,
    /// Generational): `(queue id, activation epoch)` for which this thread is
    /// already on that queue's dirty-thread list ([`SatbQueue::dirty_threads`]),
    /// one row per queue, so a thread joins the list once per marking cycle.
    noted: Vec<(u64, u64)>,
}

/// One thread's pending entries for one [`SatbQueue`].
///
/// # The activation epoch (gc-common w7-e, 2026-09-24)
///
/// `epoch` is the queue's [`SatbQueue::current_epoch`] -- its count of INACTIVE ->
/// ACTIVE transitions -- when the entries were logged. Every consumer compares
/// it with the queue's current epoch and treats a mismatch as a bucket left
/// over from a CLOSED cycle: its entries are discarded and counted into
/// [`SatbQueue::stale_discards`], never replayed into the open snapshot.
///
/// Why a bucket can outlive its cycle: `deactivate_and_discard` (both abort
/// paths, `ConcurrentMarker::abort_cycle` and
/// `G1Collector::abort_concurrent_mark`) runs OUTSIDE a pause. A mutator that
/// passed [`satb_thread_local_log`]'s gate before the abort and takes its own
/// bucket lock after the discard walk visited it appends into a bucket nobody
/// will visit again this cycle; the test/debug `deactivate()` walks nothing at
/// all. gc-common w1-g made [`SatbQueue::activate`] discard such leftovers from
/// the SHARDS; the per-thread bucket is the other place they live, and without
/// the epoch the next cycle's first spill or `flush_all_thread_satb_buffers`
/// replayed them as gray roots, naming addresses the intervening collections
/// may have freed and reissued (on G1 a torn-header verdict there trips the
/// retain-everything fail-safe for the whole cycle).
///
/// The premise is the one `activate`'s shard discard already rests on: the
/// INACTIVE -> ACTIVE transition runs in the initial-mark stop-the-world
/// pause, so no writer is mid-barrier across it, and a writer that read the
/// new ACTIVE state also reads the new epoch (it is bumped before the Release
/// store of ACTIVE).
struct SatbBucket {
    queue_id: u64,
    epoch: u64,
    entries: Vec<usize>,
}

impl ThreadSatbPartitions {
    /// Append `addr` to `queue_id`'s bucket for activation `epoch`, creating
    /// it on first use. When the bucket reaches the auto-flush threshold it is
    /// removed and returned so the caller can spill it into the owning queue's
    /// shards.
    ///
    /// The second value is how many entries of an OLDER epoch the bucket held
    /// and were discarded to make room for this one (see [`SatbBucket`]); the
    /// caller credits them to the queue's `stale_discards`.
    #[cfg(test)]
    fn log(&mut self, queue_id: u64, epoch: u64, addr: usize) -> (Option<Vec<usize>>, usize) {
        let (spilled, stale, _) = self.log_opening(queue_id, epoch, addr);
        (spilled, stale)
    }

    /// [`Self::log`], plus whether this log OPENED the thread's bucket for
    /// `epoch` -- created it, or found one of an older epoch and reset it.
    /// gcj j2/cm2: a thread holds entries for an epoch only through a bucket
    /// some log opened at that epoch, so the opening is the one hook the
    /// dirty-thread list needs ([`SatbQueue::dirty_threads`]).
    fn log_opening(
        &mut self,
        queue_id: u64,
        epoch: u64,
        addr: usize,
    ) -> (Option<Vec<usize>>, usize, bool) {
        self.logs += 1;
        for i in 0..self.buckets.len() {
            let bucket = &mut self.buckets[i];
            if bucket.queue_id == queue_id {
                let mut stale = 0;
                let opened = bucket.epoch != epoch;
                if opened {
                    stale = bucket.entries.len();
                    bucket.entries.clear();
                    bucket.epoch = epoch;
                }
                bucket.entries.push(addr);
                if bucket.entries.len() >= DEFAULT_SATB_CAPACITY {
                    return (Some(self.buckets.swap_remove(i).entries), stale, opened);
                }
                return (None, stale, opened);
            }
        }
        let mut fresh = self
            .spare
            .take()
            .unwrap_or_else(|| Vec::with_capacity(DEFAULT_SATB_CAPACITY));
        debug_assert!(fresh.is_empty(), "a recycled SATB bucket must be empty");
        fresh.push(addr);
        self.buckets.push(SatbBucket {
            queue_id,
            epoch,
            entries: fresh,
        });
        (None, 0, true)
    }

    /// gcj j2/cm2: record that this thread is on `queue_id`'s dirty-thread
    /// list for `epoch`. `true` when it was not yet (the caller then adds it),
    /// `false` when it already is.
    fn note_dirty(&mut self, queue_id: u64, epoch: u64) -> bool {
        match self.noted.iter_mut().find(|(q, _)| *q == queue_id) {
            Some((_, e)) if *e == epoch => false,
            Some((_, e)) => {
                *e = epoch;
                true
            }
            None => {
                self.noted.push((queue_id, epoch));
                true
            }
        }
    }

    /// Keep an emptied bucket `Vec` for the next bucket this thread opens (see
    /// [`Self::spare`]). One is enough: a production thread logs against one
    /// queue. A buffer that is not empty, or whose capacity is not a bucket's,
    /// is simply dropped — the spare is an optimisation, never a store.
    fn recycle(&mut self, v: Vec<usize>) {
        if self.spare.is_none()
            && v.is_empty()
            && (DEFAULT_SATB_CAPACITY..=2 * DEFAULT_SATB_CAPACITY).contains(&v.capacity())
        {
            self.spare = Some(v);
        }
    }

    /// Take (remove and return) all buffered entries for `queue_id`'s
    /// activation `epoch`, leaving other queues' buckets untouched. Returns an
    /// empty `Vec` when this thread has nothing buffered for `queue_id`.
    ///
    /// A bucket of an older epoch is removed too, but its entries belong to a
    /// closed cycle (see [`SatbBucket`]): they are dropped, and their number is
    /// the second value, for the caller's `stale_discards`.
    fn take(&mut self, queue_id: u64, epoch: u64) -> (Vec<usize>, usize) {
        for i in 0..self.buckets.len() {
            if self.buckets[i].queue_id == queue_id {
                let bucket = self.buckets.swap_remove(i);
                if bucket.epoch == epoch {
                    return (bucket.entries, 0);
                }
                return (Vec::new(), bucket.entries.len());
            }
        }
        (Vec::new(), 0)
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
            if self.buckets[i].queue_id == queue_id {
                return self.buckets.swap_remove(i).entries.len();
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
        self.buckets.retain(|b| live.contains(&b.queue_id));
        self.noted.retain(|(q, _)| live.contains(q));
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
        SATB_BUFFER_CREATED.with(|c| c.set(true));
        SatbBufferGuard { buffer: buf }
    };

    /// Whether [`THREAD_SATB_BUFFER`] has been created on this thread.
    /// `const`-initialised and destructor-free, so reading it never creates
    /// the buffer: [`flush_thread_satb_buffer`] consults it so that a thread
    /// that never logged is not registered by the act of flushing.
    static SATB_BUFFER_CREATED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
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
    // gc-common w5-e: and now for the generational barrier too — it gates on
    // this queue (`common-g-proposal-one-satb-gate`, finished), so the
    // phase-open/queue-shut window above no longer exists on any caller. The
    // check stays as the backstop for a future caller that brings its own
    // gate; `late_log_drops` still counts it.
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
    // gc-common w3-e (2026-09-23): the count is now taken INSIDE the
    // thread's own bucket (`ThreadSatbPartitions::log`, under the lock this
    // path takes anyway) and summed at report time by `barrier_log_census`.
    // It used to be a process-global relaxed `fetch_add` right here -- one
    // cache line written by every marking-time reference store on every
    // mutator thread, i.e. cross-core ping-pong that grew with the mutator
    // count (`common-g-proposal-satb-owner-local-buffer-REJECTED-20260928`, first bullet). Same
    // numbers, no shared line on the barrier. The teardown fallback below,
    // which has no bucket, still counts into the global directly.
    //
    // gc-common w1-g (2026-09-23) — `try_with`, not `with`. The buffer is a
    // `thread_local!` WITH a destructor (`SatbBufferGuard`). A reference store
    // made from another thread-local's destructor that runs after this one
    // (thread-exit hooks that clear Java-visible state) made `with` panic on a
    // destroyed key -- inside a TLS destructor, i.e. a process abort. There is
    // no bucket left to append to, so publish the single entry straight to the
    // queue, which is what the bucket would eventually have done with it.
    // `shard_for_current_thread` already tolerates teardown the same way.
    // ZGC's twin (`zgc/barrier.rs`, `Z_MARK_BUFFER.try_with`) was written this
    // way from the start.
    //
    // gc-common w7-e — the bucket is tagged with the queue's activation epoch,
    // read AFTER the gate above (the epoch is bumped before the Release store
    // of ACTIVE, so this Acquire-ordered read sees the open cycle's value). A
    // bucket still holding a closed cycle's entries is emptied here instead of
    // being spilled into this one; see [`SatbBucket`].
    let epoch = queue.current_epoch();
    // gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in, Generational):
    // the log that OPENS this thread's bucket for the epoch -- at most once
    // per 256 logs -- puts the thread on the queue's dirty-thread list, once
    // per cycle, after its own buffer lock is released. With the walk off
    // (every G1 queue, and Generational by default) that is one predicted
    // branch on the opening log.
    let (to_flush, stale) = match THREAD_SATB_BUFFER.try_with(|g| {
        let mut buf = g.buffer.lock();
        let (to_flush, stale, opened) = buf.log_opening(queue.id(), epoch, old_ref_addr);
        if opened && queue.dirty_walk_at(epoch) && buf.note_dirty(queue.id(), epoch) {
            drop(buf);
            queue.note_dirty_thread(&g.buffer);
        }
        (to_flush, stale)
    }) {
        Ok(outcome) => outcome,
        Err(_) => {
            BARRIER_LOGS.fetch_add(1, Ordering::Relaxed);
            queue.flush(vec![old_ref_addr]);
            return true;
        }
    };
    if stale != 0 {
        queue.note_stale_bucket_entries(stale);
    }
    // Returns whether this log SPILLED the thread's bucket into the shared
    // queue — the moment new gray work became visible to a marker (item 9b:
    // the caller wakes a parked worker on it).
    match to_flush {
        Some(mut entries) => {
            BARRIER_SPILLS.fetch_add(1, Ordering::Relaxed);
            queue.flush_recycling(&mut entries);
            // gc-common w5-e — hand the emptied bucket back for this thread's
            // next one (`ThreadSatbPartitions::spare`). The thread's own lock
            // again, uncontended, once per 256 logs; it replaces a 2 KiB free
            // here and a 2 KiB allocation on the next log. `try_with`: at
            // teardown there is nothing to give it back to, and it just drops.
            let _ = THREAD_SATB_BUFFER.try_with(|g| g.buffer.lock().recycle(entries));
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
///
/// # Where the count lives (gc-common w3-e, 2026-09-23)
///
/// This static holds only the RETIRED part of the census: the logs of threads
/// that have exited (folded in by `Drop for SatbBufferGuard`) plus the rare
/// teardown-fallback log that has no bucket. A live thread's logs are in its
/// own `ThreadSatbPartitions::logs`, and [`barrier_log_census`] adds them up
/// by walking [`SATB_BUFFER_REGISTRY`]. Read the census through that function,
/// never through this static.
pub static BARRIER_LOGS: AtomicU64 = AtomicU64::new(0);
/// Of [`BARRIER_LOGS`], those that spilled a full bucket into a shard.
///
/// Still a process global: it moves once per `DEFAULT_SATB_CAPACITY` logs, on
/// a path that goes on to take a shard mutex, so it is not the per-store
/// shared line the logs counter was.
pub static BARRIER_SPILLS: AtomicU64 = AtomicU64::new(0);

/// `(logs, spills)` — see [`BARRIER_LOGS`].
///
/// `logs` is the retired total plus every live thread's own count, gathered by
/// one walk of [`SATB_BUFFER_REGISTRY`] taking each thread's buffer lock once.
/// Meant for reports (it is read on the shutdown arms of
/// [`crate::gc_metrics::collector_decision_report`]), not for a hot path. A
/// thread exiting DURING the walk can be counted twice or not at all for that
/// one reading; the census is a diagnostic ratio, not an invariant.
pub fn barrier_log_census() -> (u64, u64) {
    let live: Vec<Arc<Mutex<ThreadSatbPartitions>>> = SATB_BUFFER_REGISTRY
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    let mut logs = 0u64;
    for buf in live {
        logs = logs.wrapping_add(buf.lock().logs);
    }
    (
        logs.wrapping_add(BARRIER_LOGS.load(Ordering::Relaxed)),
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
    // gc-common w1-g — `try_with`: during thread teardown the buffer may
    // already be destroyed, in which case its guard has parked any residue in
    // `ORPHANED_SATB_BUFFERS` and there is nothing left to flush here. `with`
    // would panic inside a TLS destructor (see `satb_thread_local_log`).
    //
    // A thread that never logged has no buffer, and creating one here only
    // to find it empty would register it in `SATB_BUFFER_REGISTRY` for every
    // later walk to lock: check the const flag first.
    if !SATB_BUFFER_CREATED.try_with(|c| c.get()).unwrap_or(false) {
        return;
    }
    let (entries, stale) = THREAD_SATB_BUFFER
        .try_with(|g| g.buffer.lock().take(queue.id(), queue.current_epoch()))
        .unwrap_or_default();
    if stale != 0 {
        queue.note_stale_bucket_entries(stale);
    }
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

/// The live per-thread buffers a walk of `queue` at activation `epoch` visits:
/// every registered one (pruning exited threads' slots), or -- gcj j2/cm2,
/// under the dirty-thread walk ([`SatbQueue::enable_dirty_thread_walk`]) --
/// only the threads whose log opened a bucket for `queue` at `epoch`
/// ([`SatbQueue::dirty_threads`]). Both lists are snapshotted under their own
/// lock and released before any buffer lock is taken.
///
/// The dirty list is complete for `epoch`: a thread holds entries for it only
/// in a bucket that some log OPENED at it (created, or reset from an older
/// epoch: `ThreadSatbPartitions::log_opening`), and that log put the thread
/// on the list unless an earlier opening this cycle already had; the list is
/// emptied only by the next activation. A bucket of an older epoch that it
/// leaves unvisited is never replayed (the epoch rule of [`SatbBucket`]).
fn buffers_to_walk(queue: &SatbQueue, epoch: u64) -> Vec<Arc<Mutex<ThreadSatbPartitions>>> {
    if queue.dirty_walk_at(epoch) {
        return queue.dirty_threads_snapshot();
    }
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
    // gcj j2/cm2: or, under the dirty-thread walk, only the threads that
    // opened a bucket for this queue this cycle (`buffers_to_walk`).
    let epoch = queue.current_epoch();
    let live = buffers_to_walk(queue, epoch);

    let mut stale_total = 0usize;
    for buf in live {
        // Counted HERE rather than from `live.len()`: this is the loop that
        // takes one `Mutex` per registered thread, and the lock acquisitions
        // are what the page is about. `live.len()` would also be right today,
        // but a future `continue` above this point would silently make the
        // count stop meaning "buffers locked".
        visited += 1;
        let (entries, stale) = buf.lock().take(queue.id(), epoch);
        stale_total += stale;
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
            let (entries, stale) = b.take(queue.id(), epoch);
            stale_total += stale;
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
    if stale_total != 0 {
        queue.note_stale_bucket_entries(stale_total);
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
    // gcj j2/cm2: the same buffer set as the flush form. A bucket of an older
    // epoch that the dirty-thread list leaves unvisited is never replayed
    // (`SatbBucket`'s epoch rule) and goes at its owner's next log.
    let live = buffers_to_walk(queue, queue.current_epoch());
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
///
/// **Test-only** (#[cfg(test)]): no production code uses it. The production
/// barrier logs into the per-thread, per-queue buckets of
/// [satb_thread_local_log] (ThreadSatbPartitions); this type is kept as the
/// minimal model the unit tests of the buffering rules run against.
#[cfg(test)]
pub struct SatbBuffer {
    /// Buffer of overwritten reference pointers. Each entry is a raw object
    /// address that was about to be overwritten.
    entries: Vec<usize>,
    /// Maximum entries before auto-flush.
    capacity: usize,
}

#[cfg(test)]
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
    /// Uses `std::mem::take` so that `self.entries` becomes an empty Vec and
    /// the caller receives the old Vec with its data; the empty Vec is then
    /// given `capacity` again at once (one allocation per drain), so the next
    /// fill cycle does not grow it in small steps.
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

#[cfg(test)]
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
/// Application threads flush their per-thread buckets (`ThreadSatbPartitions`) into this queue.
/// Marker threads drain it to discover references that were overwritten
/// during concurrent marking.
///
/// Round-7 HIGH-3 fix: the queue is sharded across `SHARDS` independent
/// `Mutex<Vec<usize>>` buckets, one chosen per thread (round-robin on its
/// first flush since gc-common w1-g, see `SHARD_INDEX`; it was
/// `thread_id % SHARDS`, then a hash of the `ThreadId`).  This
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
    /// `gengc-mark-satb-deactivate-is-stw-only-RETIRED-20260923`: the exclusive
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
    /// How many `yield_now`s [`Self::deactivate_and_drain`] spends waiting for
    /// `in_flight` to reach zero before it gives up (and bumps
    /// [`Self::quiescence_timeouts`]). [`QUIESCE_YIELD_BUDGET`] in every
    /// production queue; only `#[cfg(test)]` code changes it, through
    /// [`Self::with_quiesce_yield_budget`].
    ///
    /// gcg g3/k (2026-10-01): the field exists because
    /// `a_writer_parked_on_a_shard_is_not_stranded_by_deactivation` failed 3
    /// of 6 host runs under load. That test parks a writer on a shard mutex
    /// and drains anyway (the forbidden interleaving), so what it measured was
    /// whether the parked writer got a CPU within 1024 yields -- a scheduler
    /// fact, not a property of the protocol. The test now asks for an
    /// effectively unbounded budget (the property: the wait recovers the
    /// writer) and a second test pins the expiry path with a zero budget (the
    /// property: giving up is counted, never silent).
    quiesce_yield_budget: u32,
    /// How many [`satb_thread_local_log`] calls were refused because this
    /// queue was INACTIVE.
    ///
    /// Non-zero means some caller's own gate is WIDER than the queue's — the
    /// condition `gengc-mark-satb-barrier-gates-on-phase-not-queue-FIXED-20260923`
    /// describes, where `GenerationalHeap::satb_barrier` consults the phase
    /// and the log it writes consulted nothing. Zero is the expected value.
    ///
    /// Refusals are NOT in [`BARRIER_LOGS`]: that counter is the population of
    /// logs that actually entered a bucket. `logs + drops` is every barrier hit
    /// with a non-null old value.
    late_log_drops: AtomicU64,
    /// How many entries were discarded because they were left over from a
    /// closed cycle: shard entries [`Self::activate`] found (gc-common w1-g),
    /// and per-thread bucket entries of an older [`Self::current_epoch`] (gc-common
    /// w7-e, see [`SatbBucket`]). Per-queue, not a process global. Zero is the
    /// expected value.
    stale_discards: AtomicU64,
    /// Activation epoch: the number of INACTIVE -> ACTIVE transitions this
    /// queue has made. Bumped by [`Self::activate`] BEFORE its Release store of
    /// ACTIVE; per-thread buckets are tagged with it (gc-common w7-e, see
    /// [`SatbBucket`]).
    epoch: AtomicU64,
    /// Does THIS queue currently hold one count of the compiled-code SATB
    /// pre-gate (`gen_heap::arm_jit_ref_store_marker`)?
    ///
    /// gc-common w4-e, step 2 of `common-g-proposal-one-satb-gate`: the queue
    /// arms the JIT gate BEFORE it stores ACTIVE and disarms it AFTER it stores
    /// INACTIVE (and on `Drop`, for a queue that dies active), so "the JIT gate
    /// is armed whenever this queue accepts logs" holds by construction rather
    /// than by each collector's call-site order. Since gc-common w5-e this is
    /// the ONLY Generational/G1 source of that count
    /// (`ConcurrentGcState::set_phase` no longer arms one), and the Rust
    /// barriers of both collectors gate on [`Self::is_active`] too, so the
    /// compiled-code gate and the Rust gate are the same fact seen twice.
    /// The gate is "armed while ANY count is held" (other queues, ZGC's
    /// boolean), and a wider gate is safe: compiled code then calls the
    /// collector's own barrier, which decides.
    /// The swap makes arm/disarm exactly-once per transition, so a repeated
    /// `activate` or a second deactivation cannot unbalance the process-wide
    /// counter.
    jit_gate_armed: AtomicBool,
    /// gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in, set on the
    /// Generational heap's queue only, by `VmHeap::enable_concurrent_gc`;
    /// `gcj-j1cm-proposal-dirty-thread-satb-walk`): the first activation epoch
    /// whose walks visit only [`Self::dirty_threads`] instead of every
    /// registered thread. `u64::MAX` (the default, and every G1 queue): never.
    dirty_walk_from: AtomicU64,
    /// gcj j2/cm2: the per-thread buffers that opened a bucket for this queue
    /// in the current activation epoch, each once (`ThreadSatbPartitions::
    /// note_dirty`); emptied by [`Self::activate`]. Maintained only while
    /// [`Self::dirty_walk_from`] is reached. `Weak`, as the registry: an
    /// exited thread's residue is in [`ORPHANED_SATB_BUFFERS`], walked anyway.
    dirty_threads: Mutex<Vec<Weak<Mutex<ThreadSatbPartitions>>>>,
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
    ///
    /// gc-common w1-g (2026-09-23): a `const`-initialised, `Drop`-free `Cell`
    /// filled on first use from a round-robin ticket, instead of a lazy
    /// initialiser that hashed `std::thread::current().id()`. Two reasons:
    ///
    /// * the lazy initialiser could run for the FIRST time from
    ///   `satb_thread_local_log`'s teardown fallback (a store made from a TLS
    ///   destructor, whose `flush` is this thread's first), and
    ///   `thread::current()` is not usable once the thread's TLS is being torn
    ///   down -- a panic there is a process abort. A const `Cell` with no
    ///   destructor has neither a lazy-init path nor a destroyed state;
    /// * round-robin spreads the first `SHARDS` threads over `SHARDS`
    ///   different shard locks, where a hash lets busy threads collide.
    ///
    /// Which shard a thread uses has never been part of any contract -- every
    /// drain visits all of them -- and a thread's choice is still stable for
    /// its lifetime.
    static SHARD_INDEX: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
}

/// Yields [`SatbQueue::deactivate_and_drain`] spends waiting for in-flight
/// writers before it gives up. ~1 ms of scheduler time at a typical yield
/// cost, against a pause already measured in milliseconds. Per-queue copy:
/// `SatbQueue::quiesce_yield_budget`.
const QUIESCE_YIELD_BUDGET: u32 = 1024;

/// Round-robin source for [`SHARD_INDEX`].
static NEXT_SATB_SHARD: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn shard_for_current_thread() -> usize {
    // `try_with` cannot fail for a const, drop-free key; kept so the fallback
    // (shard 0 -- any shard is correct, the drain visits all of them) stays
    // explicit if the key ever grows a destructor.
    SHARD_INDEX
        .try_with(|c| {
            let s = c.get();
            if s != usize::MAX {
                return s;
            }
            let s = NEXT_SATB_SHARD.fetch_add(1, Ordering::Relaxed) & (SHARDS - 1);
            c.set(s);
            s
        })
        .unwrap_or(0)
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
            quiesce_yield_budget: QUIESCE_YIELD_BUDGET,
            late_log_drops: AtomicU64::new(0),
            stale_discards: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            jit_gate_armed: AtomicBool::new(false),
            dirty_walk_from: AtomicU64::new(u64::MAX),
            dirty_threads: Mutex::new(Vec::new()),
        }
    }

    /// Test-only: replace [`Self::quiesce_yield_budget`]. See that field for
    /// why. Called on a fresh queue before it is shared.
    #[cfg(test)]
    pub fn with_quiesce_yield_budget(mut self, yields: u32) -> Self {
        self.quiesce_yield_budget = yields;
        self
    }

    /// The current activation epoch (the `epoch` field). See [`SatbBucket`].
    #[inline]
    fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// Credit `n` per-thread bucket entries of a closed cycle, discarded by a
    /// consumer, to [`Self::stale_discards`]. They never reached a shard, so
    /// there is no `pending` credit to give back.
    #[cold]
    fn note_stale_bucket_entries(&self, n: usize) {
        self.stale_discards.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// gcj j2/cm2 (`CRATONVM_GEN_SATB_DIRTY_THREAD_WALK`, opt-in): from the
    /// activation after the next one on, the remark / cleanup walks of this
    /// queue visit only the threads that logged into it during the cycle
    /// ([`Self::dirty_threads`]) instead of every registered thread. Called
    /// once, before the queue's first activation (`VmHeap::enable_concurrent_gc`,
    /// the Generational arm); the two-epoch margin keeps a cycle whose
    /// activation raced this call on the full walk, since its buckets may have
    /// opened before the list was kept. Idempotent.
    pub fn enable_dirty_thread_walk(&self) {
        let from = self.current_epoch().saturating_add(2);
        let _ = self.dirty_walk_from.compare_exchange(
            u64::MAX,
            from,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    /// gcj j2/cm2: does a walk of activation `epoch` use the dirty-thread list?
    #[inline]
    fn dirty_walk_at(&self, epoch: u64) -> bool {
        epoch >= self.dirty_walk_from.load(Ordering::Acquire)
    }

    /// gcj j2/cm2: put a thread's buffer on [`Self::dirty_threads`] (its first
    /// bucket opening of the cycle; the caller holds no buffer lock).
    #[cold]
    fn note_dirty_thread(&self, buffer: &Arc<Mutex<ThreadSatbPartitions>>) {
        self.dirty_threads.lock().push(Arc::downgrade(buffer));
    }

    /// gcj j2/cm2: the live buffers on [`Self::dirty_threads`], pruning the
    /// slots of exited threads. The list is kept (a second walk in the same
    /// pause, `ConcurrentMarker`'s remark then `remark_finish`, sees it whole).
    fn dirty_threads_snapshot(&self) -> Vec<Arc<Mutex<ThreadSatbPartitions>>> {
        let mut dirty = self.dirty_threads.lock();
        let mut live = Vec::with_capacity(dirty.len());
        dirty.retain(|w| match w.upgrade() {
            Some(arc) => {
                live.push(arc);
                true
            }
            None => false,
        });
        live
    }

    /// gcj j2/cm2: how many threads are on the dirty-thread list now. Tests
    /// and diagnostics.
    pub fn dirty_thread_count(&self) -> usize {
        self.dirty_threads.lock().len()
    }

    /// Take this queue's count of the JIT SATB pre-gate, once. Called BEFORE
    /// the ACTIVE store in [`Self::activate`]. See [`Self::jit_gate_armed`].
    #[inline]
    fn arm_jit_gate(&self) {
        if !self.jit_gate_armed.swap(true, Ordering::AcqRel) {
            crate::jit_bounds::arm_jit_ref_store_marker();
        }
    }

    /// Give this queue's count back, once. Called AFTER every INACTIVE store
    /// and from `Drop`. See [`Self::jit_gate_armed`].
    #[inline]
    fn disarm_jit_gate(&self) {
        if self.jit_gate_armed.swap(false, Ordering::AcqRel) {
            crate::jit_bounds::disarm_jit_ref_store_marker();
        }
    }

    /// Does this queue currently hold a count of the JIT SATB pre-gate?
    /// Diagnostics and tests.
    pub fn holds_jit_gate(&self) -> bool {
        self.jit_gate_armed.load(Ordering::Acquire)
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
    ///
    /// # A fresh cycle starts from EMPTY shards (gc-common w1-g, 2026-09-23)
    ///
    /// On an INACTIVE -> ACTIVE transition, whatever is still in the shards is
    /// discarded and counted into [`Self::stale_discards()`]. Such an entry
    /// belongs to no snapshot. `deactivate_and_drain`'s step 5 sweeps up
    /// writers that were still mid-`flush` when the gate went off, but its own
    /// doc names the residual it cannot see (a writer between the barrier's
    /// gate and `flush`'s `in_flight` increment). `deactivate_and_discard` --
    /// which both abort paths use, and `ConcurrentMarker::abort_cycle` calls
    /// OUTSIDE a safepoint, with mutators running -- has no post-INACTIVE sweep
    /// at all, so a writer that passed the gate before the discard deposits
    /// after it. And `deactivate()` -- the test/debug form -- drains nothing. Either way the entry would be
    /// replayed as a gray root by the next cycle's first drain, naming an
    /// address the intervening collections may have freed and reissued. This
    /// is the one point in the protocol where "anything queued is stale" is
    /// true by construction, so it is where the residual is closed.
    ///
    /// The same leftovers can sit in a mutator's per-thread bucket instead of
    /// a shard; this function cannot reach those without an O(threads) walk,
    /// so it bumps the queue's activation epoch instead and every bucket
    /// consumer drops a bucket of an older epoch (gc-common w7-e, see
    /// `SatbBucket`). Both discards land in [`Self::stale_discards()`].
    ///
    /// Re-activating a queue that is already ACTIVE or DRAINING keeps its
    /// entries: they belong to the snapshot that is still open.
    ///
    /// Uses [`Self::drain`], so the [`Self::pending`] credit is debited
    /// exactly as for any other drain.
    pub fn activate(&self) {
        if self.state.load(Ordering::Acquire) == SATB_INACTIVE {
            // gc-common w7-e — a new snapshot opens: every per-thread bucket
            // still tagged with the previous epoch now reads as stale to its
            // next consumer (see [`SatbBucket`]). Bumped BEFORE the Release
            // store of ACTIVE below, so a writer that observes ACTIVE observes
            // this epoch too.
            self.epoch.fetch_add(1, Ordering::AcqRel);
            // gcj j2/cm2: the dirty-thread list is per cycle (a thread joins it
            // at its first bucket opening of the new epoch).
            self.dirty_threads.lock().clear();
            if self.has_pending() {
                let stale = self.drain();
                if !stale.is_empty() {
                    self.stale_discards
                        .fetch_add(stale.len() as u64, Ordering::Relaxed);
                }
            }
        }
        // gc-common w4-e: arm the compiled-code pre-gate BEFORE the queue
        // accepts logs, so the gate is a superset of the queue's window by
        // construction. See [`Self::jit_gate_armed`].
        self.arm_jit_gate();
        self.state.store(SATB_ACTIVE, Ordering::Release);
    }

    /// Entries [`Self::activate`] found left over in the shards from a
    /// previous cycle and discarded. Expected 0; non-zero names the residual
    /// window documented on [`Self::deactivate_and_drain`]. Diagnostics and
    /// tests.
    pub fn stale_discards(&self) -> u64 {
        self.stale_discards.load(Ordering::Relaxed)
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
        self.disarm_jit_gate();
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
    /// `docs/feature-designs/jit-barrier-lowering.md`.
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
        // 1. Transition ACTIVE -> DRAINING. The result is not looked at: a
        //    double stop (the gate already INACTIVE) runs every step below
        //    again -- the thread-buffer walk, the three shard passes and the
        //    in-flight wait -- and returns what they find (normally nothing).
        //    gcj j2/cm2: this said "just drain once for safety and return",
        //    which the code never did (`gcj-j1cm-gc-core-files-review-residuals`
        //    item 12); harmless, it over-retains nothing.
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
        //     `gengc-mark-satb-deactivate-is-stw-only-RETIRED-20260923.md`).
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
            // Yields before giving up: `QUIESCE_YIELD_BUDGET` (~1 ms of
            // scheduler time) in production; see `quiesce_yield_budget`.
            let budget = self.quiesce_yield_budget;
            let mut spun = 0u32;
            while self.in_flight.load(Ordering::SeqCst) != 0 {
                if spun >= budget {
                    self.quiescence_timeouts.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        in_flight = self.in_flight.load(Ordering::Relaxed),
                        queue_id = self.id,
                        "SATB deactivation gave up waiting for writers to quiesce; an \
                         entry logged by one of them may be stranded in a shard this \
                         pass has already visited (see \
                         gengc-mark-satb-deactivate-is-stw-only-RETIRED-20260923.md)",
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
        // gc-common w4-e: the JIT pre-gate count goes back only AFTER the
        // queue stopped accepting logs. See [`Self::jit_gate_armed`].
        self.disarm_jit_gate();

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
        // gc-common w4-e: AFTER the gate stops accepting logs.
        self.disarm_jit_gate();
    }

    /// Flush a per-thread buffer's entries into the global queue.
    ///
    /// Round-7 HIGH-3: writes land on the per-thread shard so concurrent
    /// flushers from independent threads do not serialise.
    pub fn flush(&self, mut entries: Vec<usize>) {
        self.flush_recycling(&mut entries);
    }

    /// [`Self::flush`] that leaves `entries` EMPTY with its capacity intact,
    /// so the caller can reuse the buffer (gc-common w5-e: the spill path of
    /// [`satb_thread_local_log`], see `ThreadSatbPartitions::spare`). The
    /// entries are copied into the shard exactly as `flush`'s `extend` copied
    /// them; only the allocation's fate differs.
    pub fn flush_recycling(&self, entries: &mut Vec<usize>) {
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
        // `append`, not `extend`: the same element copy, but it leaves
        // `entries` empty with its capacity for the caller to reuse.
        queue.append(entries);
    }

    /// Drain all accumulated entries for the marker to process.
    ///
    /// Visits every shard in order, atomically swapping each Vec out
    /// and concatenating into a single result.  The marker is the
    /// single consumer so per-shard ordering across shards is not
    /// meaningful — entries from shard 0 simply precede shard 1's.
    pub fn drain(&self) -> Vec<usize> {
        let mut total = 0usize;
        let mut non_empty = 0usize;
        let mut buckets: [Vec<usize>; SHARDS] = std::array::from_fn(|_| Vec::new());
        for (i, shard) in self.shards.iter().enumerate() {
            let mut guard = shard.lock();
            let taken = std::mem::take(&mut *guard);
            if !taken.is_empty() {
                // Debited AFTER the take and still under the shard lock, so
                // the count is never lower than what a shard actually holds.
                self.pending.fetch_sub(taken.len(), Ordering::AcqRel);
                non_empty += 1;
            }
            total += taken.len();
            buckets[i] = taken;
        }
        // gc-common w3-e: with at most one populated shard -- the common shape
        // for G1's per-step drain and the generational slice drain, where one
        // or two mutators spilled since the last look -- hand that shard's
        // `Vec` back as is instead of copying it into a fresh allocation.
        if non_empty <= 1 {
            return buckets
                .into_iter()
                .find(|b| !b.is_empty())
                .unwrap_or_default();
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
        // gc-common w4-e: a queue that dies ACTIVE (the unit tests build and
        // drop them constantly) must not leave its count on the process-wide
        // JIT pre-gate, or every later compiled store takes the slow path.
        self.disarm_jit_gate();
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
            let _ = buf.lock().discard(id);
        }
        let mut orphans = ORPHANED_SATB_BUFFERS.lock();
        orphans.retain(|buf| {
            let mut b = buf.lock();
            let _ = b.discard(id);
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
mod tests;
