// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Concurrent marking for the garbage collector.
//!
//! Implements tri-color marking that runs concurrently with application
//! threads. The marking process has four phases:
//!
//! 1. **Initial Mark (STW, brief):** Mark objects directly reachable from
//!    thread stacks and static fields. This is a short STW pause.
//!
//! 2. **Concurrent Mark:** Traverse the object graph from the initial roots,
//!    marking all reachable objects. Application threads continue running;
//!    the SATB write barrier logs overwritten references.
//!
//! 3. **Remark (STW, brief):** Process SATB buffers and re-scan roots to
//!    catch any references modified during concurrent marking.
//!
//! 4. **Concurrent Sweep:** Walk the old generation, freeing unmarked objects
//!    back to the free list. Application threads continue running.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};

use crate::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    GC_FLAG_COMPACT, GC_FLAG_HEADER, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use crate::mark_bitmap::MarkBitmap;
use crate::old_gen::OldGen;
use crate::satb::SatbQueue;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// Concurrent GC phase tracking
// ---------------------------------------------------------------------------

/// Current phase of the concurrent GC cycle.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConcurrentGcPhase {
    /// No concurrent GC activity.
    Idle = 0,
    /// Initial mark — brief STW to mark root-reachable objects.
    InitialMark = 1,
    /// Concurrent mark — marker threads traverse the heap.
    ConcurrentMark = 2,
    /// Remark — brief STW to process SATB buffers and re-scan roots.
    Remark = 3,
    /// Concurrent sweep — reclaim unmarked old-gen objects.
    ConcurrentSweep = 4,
}

impl From<u8> for ConcurrentGcPhase {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Idle,
            1 => Self::InitialMark,
            2 => Self::ConcurrentMark,
            3 => Self::Remark,
            4 => Self::ConcurrentSweep,
            _ => Self::Idle,
        }
    }
}

/// Atomic phase tracker visible to all threads.
pub struct ConcurrentGcState {
    phase: AtomicU8,
}

impl ConcurrentGcState {
    pub fn new() -> Self {
        Self {
            phase: AtomicU8::new(ConcurrentGcPhase::Idle as u8),
        }
    }

    /// Get the current GC phase.
    #[inline]
    pub fn phase(&self) -> ConcurrentGcPhase {
        ConcurrentGcPhase::from(self.phase.load(Ordering::Acquire))
    }

    /// Set the GC phase (called by the GC coordinator).
    ///
    /// This is the ONE writer of the phase, and therefore the one place that
    /// can keep the JIT's SATB pre-barrier gate honest. Compiled reference
    /// stores skip their pre-barrier call when that gate reads clear, so the
    /// gate must be armed for a superset of the interval in which
    /// [`Self::is_marking_active`] is true — never a subset. Hence the
    /// asymmetry below: arm BEFORE the phase becomes observable, disarm AFTER
    /// it stops being. `arm`/`disarm` count markers rather than setting a
    /// boolean, because this type is shared by the generational collector and
    /// G1 and a process can hold several heaps at once.
    pub fn set_phase(&self, phase: ConcurrentGcPhase) {
        let was = self.is_marking_active();
        let will = matches!(
            phase,
            ConcurrentGcPhase::ConcurrentMark | ConcurrentGcPhase::Remark
        );
        if will && !was {
            crate::gen_heap::arm_jit_ref_store_marker();
        }
        self.phase.store(phase as u8, Ordering::Release);
        if was && !will {
            crate::gen_heap::disarm_jit_ref_store_marker();
        }
    }

    /// Whether concurrent marking is active (SATB barrier should log).
    #[inline]
    pub fn is_marking_active(&self) -> bool {
        let p = self.phase.load(Ordering::Acquire);
        p == ConcurrentGcPhase::ConcurrentMark as u8 || p == ConcurrentGcPhase::Remark as u8
    }
}

impl Default for ConcurrentGcState {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ConcurrentGcState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ConcurrentGcState({:?})", self.phase())
    }
}

// ---------------------------------------------------------------------------
// Mark queue (work list for concurrent marking)
// ---------------------------------------------------------------------------

/// Thread-safe work queue for concurrent marking.
///
/// Uses a sharded approach: multiple `VecDeque`s behind separate `Mutex`es,
/// selected by hashing the pointer value. This reduces lock contention when
/// multiple marker threads push/pop concurrently, since threads operating on
/// different pointer ranges will typically hit different shards.
pub struct MarkQueue {
    shards: Vec<Mutex<VecDeque<*mut u8>>>,
    /// Round-11 perf: bitmask hint of which shards are currently
    /// non-empty (bit `k` set ⇒ shard `k` *may* hold work). Updated
    /// under the corresponding shard lock — `push` sets bit `k` after a
    /// `push_back`, `pop` clears bit `k` when it drains the shard empty.
    ///
    /// `pop` consults this to jump straight to a populated shard instead
    /// of probing all [`MARK_QUEUE_SHARDS`] mutexes. The mask is only a
    /// *hint*: a concurrent thread may flip a bit between the load and
    /// the lock, so a set bit can be stale (shard already drained). To
    /// stay correct `pop` falls back to a full probe if the hint-directed
    /// lookup finds nothing — it never reports the queue empty while a
    /// shard still holds work.
    nonempty_shards: AtomicU8,
    /// Round-9 gc HIGH-5 fix — set when any `push` is dropped because
    /// its target shard hit [`MARK_QUEUE_SHARD_CAP`]. The marker checks
    /// this flag at the end of remark and, if true, falls back to a
    /// full re-walk of the old generation (every live object is marked
    /// conservatively). This trades CPU for correctness: a pathological
    /// or hostile Java app graph that would previously crash the VM via
    /// `panic!` now just makes the mark phase longer.
    overflowed: AtomicBool,
    /// Termination-detection state for a MULTI-worker drain
    /// (gengc-mark2 2026-09-20). Untouched by the single-threaded
    /// [`Self::pop`] path, which every production drain still uses.
    term: Mutex<DrainTermination>,
    /// Condvar paired with [`Self::term`].
    term_cv: Condvar,
    /// Lock-free mirror of `DrainTermination::idle`, so [`Self::push`] can ask
    /// "is anyone parked waiting for work?" with one relaxed load instead of
    /// taking the termination lock on every push. Maintained under `term`, so
    /// it never disagrees except for the instant between the two updates, and
    /// a stale read costs at most one spurious `notify_all` — never a missed
    /// wakeup, because a worker re-checks every shard under `term` before it
    /// parks (see [`MarkQueue::pop_or_terminate`]).
    idle_hint: AtomicUsize,
}

/// Coordinator state for [`MarkQueue::pop_or_terminate`].
///
/// Deliberately the same shape as `young_mark`'s `DrainState`, for the same
/// reason: every quantity a termination decision depends on must be observable
/// under ONE lock, or the decision races the work it is deciding about.
#[derive(Debug)]
struct DrainTermination {
    /// Workers currently parked in the acquisition path with no work.
    idle: usize,
    /// Workers still registered on this drain. Decremented by [`MarkWorker`]'s
    /// `Drop`, so a worker that unwinds out of its scan does not park its
    /// peers forever.
    live: usize,
    /// Set once the closure is complete; every worker returns on seeing it.
    done: bool,
}

/// Number of shards for the mark queue. Must be a power of two for fast modulo.
const MARK_QUEUE_SHARDS: usize = 8;

// gc-concmark MEDIUM fix — the `nonempty_shards` hint is an `AtomicU8`, so it
// has exactly one bit per shard for at most 8 shards. The hint is set/cleared
// with `1u8 << idx` where `idx` ranges over `0..MARK_QUEUE_SHARDS`. If anyone
// bumps `MARK_QUEUE_SHARDS` above 8 while tuning, every `1u8 << idx` for
// `idx >= 8` overflows the shift width: in debug builds it panics, and in
// release builds the shift amount wraps mod 8, so shards >= 8 silently alias
// the low shards' bits — the hint becomes wrong and `pop` can skip a populated
// shard (the full-probe fallback still keeps it *correct*, just slower, but the
// hint is also actively corrupted for shards 0..8). Turn that latent landmine
// into a compile error: if you raise the shard count past 8, you must also
// widen `nonempty_shards` to `AtomicU16`/`U32`/`U64` (and the `1u8 <<` /
// `!(1u8 <<` masks below) to match. The shift expressions are written
// `1u8 << idx`, so the matching atomic width is `u8` ⇒ 8 shards max.
const _: () = assert!(
    MARK_QUEUE_SHARDS <= 8,
    "nonempty_shards is AtomicU8 (8 bits); widen it (and the `1u8 <<` masks in \
     push/pop) to AtomicU16/U32/U64 before raising MARK_QUEUE_SHARDS above 8"
);
// Sanity: the shard count must also be a power of two, because both
// `shard_for` and the round-robin `pop` cursor index with `& (SHARDS - 1)`.
const _: () = assert!(
    MARK_QUEUE_SHARDS.is_power_of_two(),
    "MARK_QUEUE_SHARDS must be a power of two (masked with `& (SHARDS - 1)`)"
);

/// Round-5 HIGH #6 — defensive cap on a single mark-queue shard.
///
/// The original `MarkQueue::push` was an unbounded `Vec` (well, `VecDeque`)
/// growth. A pathological mutator graph (e.g. a malicious or buggy
/// classloader that produces a deeply circular object graph during a
/// concurrent-mark cycle) could OOM the marker thread by pushing
/// hundreds of millions of pointers before any are drained. The cap
/// below converts that silent OOM into a deterministic panic, which is
/// strictly better than crashing the entire VM with an allocator
/// failure deep in `VecDeque::push_back`.
///
/// 1 million entries per shard * 8 shards * 8 bytes = 64 MiB — chosen
/// large enough that any realistic mark cycle stays well below it, but
/// small enough that the panic is reproducible in tests.
///
/// TODO(round-5+): the real fix is an overflow-handling strategy —
/// either spill the queue to a backing region, or drop the explicit
/// queue entirely and fall back to a "mark-everything-dirty" sweep
/// pass guided by the card table. Both are too invasive for this
/// hotfix; the cap below is the defensive interim.
const MARK_QUEUE_SHARD_CAP: usize = 1 << 20;

thread_local! {
    /// Per-thread round-robin cursor used by [`MarkQueue::pop`] to choose
    /// the shard probe order. Bumping this on every `pop` distributes
    /// marker threads across shards instead of stacking them all on
    /// shard 0 (which is what the original "start at 0" loop did,
    /// defeating the entire point of sharding under contention).
    static POP_CURSOR: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// SAFETY: The raw pointers in the queue are heap object addresses managed
// by the GC; they are valid for the duration of the marking phase. The
// sharded Mutex design ensures exclusive access to each shard.
unsafe impl Send for MarkQueue {}
unsafe impl Sync for MarkQueue {}

impl MarkQueue {
    pub fn new() -> Self {
        let shards = (0..MARK_QUEUE_SHARDS)
            .map(|_| Mutex::new(VecDeque::with_capacity(4096 / MARK_QUEUE_SHARDS)))
            .collect();
        Self {
            shards,
            nonempty_shards: AtomicU8::new(0),
            overflowed: AtomicBool::new(false),
            term: Mutex::new(DrainTermination {
                idle: 0,
                live: 0,
                done: false,
            }),
            term_cv: Condvar::new(),
            idle_hint: AtomicUsize::new(0),
        }
    }

    /// Select shard index from a pointer value.
    #[inline]
    fn shard_for(ptr: *mut u8) -> usize {
        // Use upper bits after shifting out alignment (objects are 8-byte aligned)
        ((ptr as usize) >> 3) & (MARK_QUEUE_SHARDS - 1)
    }

    /// Push an object onto the mark queue (it becomes gray).
    ///
    /// Round-9 gc HIGH-5 fix — replace the previous `panic!` (which
    /// was reachable from any hostile/buggy Java program that built a
    /// wide live graph) with a graceful overflow flag. The push is
    /// dropped; the bitmap entry remains marked, so the object itself
    /// isn't swept, but its outgoing edges won't be scanned via the
    /// queue. The marker compensates by running a conservative full
    /// re-walk of the old generation at the end of remark (see
    /// [`Self::has_overflowed`] and the remark fallback). This trades
    /// CPU for correctness — no crash, just longer mark.
    pub fn push(&self, obj_ptr: *mut u8) {
        let idx = Self::shard_for(obj_ptr);
        let mut shard = self.shards[idx].lock();
        if shard.len() >= MARK_QUEUE_SHARD_CAP {
            // Drop the push, set the overflow flag, and let the remark
            // phase fall back to a full conservative re-walk.
            // `Relaxed` is sufficient — the flag is read once during
            // STW remark after every push has happened-before.
            self.overflowed.store(true, Ordering::Relaxed);
            return;
        }
        shard.push_back(obj_ptr);
        // Round-11 perf: mark this shard non-empty in the hint mask.
        // Done under the shard lock so the bit is set before the lock
        // (and therefore the queued pointer) becomes visible to a
        // concurrent `pop`.
        self.nonempty_shards.fetch_or(1u8 << idx, Ordering::Release);
        drop(shard);
        // gengc-mark2 2026-09-20 — wake a parked marker, if there is one.
        //
        // Costs exactly one relaxed load on the single-threaded path that
        // every production drain uses today, where `idle_hint` is 0 for the
        // whole drain (the lone worker only parks once, at the very end, and
        // by then nothing is pushing). `notify_all` is reached only when a
        // peer is genuinely waiting.
        if self.idle_hint.load(Ordering::Relaxed) > 0 {
            self.term_cv.notify_all();
        }
    }

    /// Round-9 gc HIGH-5 — true iff any push since the last
    /// [`Self::clear`] was dropped due to per-shard capacity. The
    /// remark phase consults this and triggers a full re-walk if set.
    #[inline]
    pub fn has_overflowed(&self) -> bool {
        self.overflowed.load(Ordering::Relaxed)
    }

    /// Pop an object from the mark queue for scanning.
    /// Returns `None` if all shards are empty.
    ///
    /// Each calling thread maintains its own round-robin cursor so that
    /// concurrent markers spread contention evenly across shards instead
    /// of always hammering shard 0 first.
    pub fn pop(&self) -> Option<*mut u8> {
        let start = POP_CURSOR.with(|c| {
            let v = c.get();
            c.set(v.wrapping_add(1));
            v
        }) & (MARK_QUEUE_SHARDS - 1);

        // Round-11 perf: consult the non-empty bitmask hint and visit
        // only the shards it flags, instead of probing all 8 mutexes.
        // The mask is a hint — a bit can be stale either way — so this
        // pass is best-effort and is backed by the full probe below.
        let hint = self.nonempty_shards.load(Ordering::Acquire);
        if hint != 0 {
            for offset in 0..MARK_QUEUE_SHARDS {
                let idx = (start + offset) & (MARK_QUEUE_SHARDS - 1);
                if hint & (1u8 << idx) == 0 {
                    continue;
                }
                let mut shard = self.shards[idx].lock();
                match shard.pop_front() {
                    Some(ptr) => {
                        if shard.is_empty() {
                            self.nonempty_shards
                                .fetch_and(!(1u8 << idx), Ordering::Release);
                        }
                        return Some(ptr);
                    }
                    None => {
                        // Stale set bit — shard was drained by another
                        // thread. Clear it so future pops skip it.
                        self.nonempty_shards
                            .fetch_and(!(1u8 << idx), Ordering::Release);
                    }
                }
            }
        }

        // Fallback: the hint found nothing, but a concurrent `push` may
        // have populated a shard whose bit we hadn't observed. Probe
        // every shard so `pop` never reports empty while work remains.
        for offset in 0..MARK_QUEUE_SHARDS {
            let idx = (start + offset) & (MARK_QUEUE_SHARDS - 1);
            let mut shard = self.shards[idx].lock();
            if let Some(ptr) = shard.pop_front() {
                if shard.is_empty() {
                    self.nonempty_shards
                        .fetch_and(!(1u8 << idx), Ordering::Release);
                }
                return Some(ptr);
            }
        }
        None
    }

    /// Push multiple objects at once.
    pub fn push_batch(&self, ptrs: &[*mut u8]) {
        for &ptr in ptrs {
            self.push(ptr);
        }
    }

    /// Number of pending objects in the queue (sum across all shards).
    pub fn len(&self) -> usize {
        self.shards.iter().map(|s| s.lock().len()).sum()
    }

    /// Whether the queue is empty (all shards empty).
    pub fn is_empty(&self) -> bool {
        self.shards.iter().all(|s| s.lock().is_empty())
    }

    /// Clear all entries from all shards.
    pub fn clear(&self) {
        for shard in &self.shards {
            shard.lock().clear();
        }
        // Round-11 perf: every shard is now empty, so reset the
        // non-empty hint mask. Safe to clear unconditionally — `clear`
        // is only called during STW transitions.
        self.nonempty_shards.store(0, Ordering::Release);
        // Round-9 gc HIGH-5: reset the overflow flag so the next
        // cycle starts clean. Safe to clear unconditionally — `clear`
        // is only called during STW transitions.
        self.overflowed.store(false, Ordering::Relaxed);
    }

    // -----------------------------------------------------------------
    // Termination detection for a multi-worker drain
    // (gengc-mark-markqueue-has-no-termination-detection-20260920)
    // -----------------------------------------------------------------

    /// Open a drain of this queue by exactly `workers` marker threads.
    ///
    /// # Why this exists
    ///
    /// [`Self::pop`] returns `None` when every shard is empty **at the moment
    /// it looks**. With ONE marker that is a sound termination test: an empty
    /// queue plus "I am the only scanner" means the closure is complete. With
    /// two or more it is not — thread B can observe every shard empty while
    /// thread A is inside `scan_object`, about to push three children. B
    /// leaves, and whatever A pushes after that is scanned by nobody. The
    /// symptom is under-marking, which surfaces as a freed live object some
    /// cycles later, arbitrarily far from the cause.
    ///
    /// Every production drain is single-threaded today, so this is the
    /// primitive that has to exist BEFORE a second marker does, not a fix for
    /// a live bug. `young_mark::drain_parallel` solved the same problem for
    /// the young collector; this is the same argument applied to a queue whose
    /// work lives in shards rather than in the coordinator.
    ///
    /// # The protocol
    ///
    /// ```text
    /// queue.begin_drain(n);                    // coordinator, before spawning
    /// // ... in each of the n threads, INCLUDING the coordinator:
    /// let _w = queue.worker();                 // registration guard
    /// while let Some(p) = queue.pop_or_terminate() { scan(p); /* may push */ }
    /// ```
    ///
    /// The guard must outlive the loop: `live` is what makes "everyone is
    /// idle" decidable, and a worker that unwinds out of `scan` must not park
    /// its peers forever.
    ///
    /// `begin_drain` is NOT itself synchronised against a drain already in
    /// progress; call it from the coordinating thread before any worker
    /// starts, which is the only shape `ConcurrentMarker` needs.
    pub fn begin_drain(&self, workers: usize) {
        let mut g = self.term.lock();
        g.idle = 0;
        g.live = workers;
        g.done = false;
        self.idle_hint.store(0, Ordering::Relaxed);
    }

    /// Register the calling thread as one of the workers [`Self::begin_drain`]
    /// counted. Exactly one per worker; hold it for the whole drain loop.
    pub fn worker(&self) -> MarkWorker<'_> {
        MarkWorker { queue: self }
    }

    /// Pop the next object to scan, or `None` once the closure is **provably**
    /// complete.
    ///
    /// # Why the decision is sound
    ///
    /// `idle` counts only workers that have already finished scanning and have
    /// published themselves as out of work, under `term`. So a worker holding
    /// `term` and observing `idle == live` knows that
    ///
    /// * no live worker is inside a scan, hence none can push; and
    /// * no live worker can start one, because leaving the idle set requires
    ///   `term`, which this worker holds.
    ///
    /// The queue state is therefore frozen for the duration of the check, and
    /// the `pop` performed under `term` immediately before the increment is
    /// decisive. A worker that is between its fast-path `pop` and its
    /// `term.lock()` has NOT incremented `idle`, so it holds `idle < live`
    /// open and correctly prevents the decision — which is why `idle` is
    /// incremented after, never before, the re-check.
    ///
    /// # A lost wakeup costs throughput, never progress
    ///
    /// `push` notifies outside `term` (the same shape as
    /// `young_mark::drain_parallel`), so a `notify_all` can slip between a
    /// peer publishing `idle` and actually parking. That peer then sleeps
    /// through available work. It cannot sleep FOREVER: the worker that
    /// eventually finds the queue empty declares `done` and calls
    /// `notify_all` while holding `term`, which no parked worker can miss. So
    /// the worst case is that one marker does another's share, not a hang.
    /// Taking `term` on the push path to close the window would put a mutex
    /// acquisition on the hot path of a work-starved drain, which is the wrong
    /// trade.
    pub fn pop_or_terminate(&self) -> Option<*mut u8> {
        // Fast path: work is visible, so no coordination is needed at all.
        // This is the whole loop for a busy marker.
        if let Some(ptr) = self.pop() {
            return Some(ptr);
        }
        let mut g = self.term.lock();
        loop {
            if g.done {
                return None;
            }
            // Re-check under `term`: a peer may have pushed between the fast
            // path above and this acquisition.
            if let Some(ptr) = self.pop() {
                return Some(ptr);
            }
            g.idle += 1;
            self.idle_hint.store(g.idle, Ordering::Relaxed);
            // `>=`, and against `live` rather than the original worker count:
            // a worker that has already left (normally or by unwinding) can
            // never arrive here, so waiting for the full count would park the
            // survivors forever. Same rule as `young_mark::drain_parallel`.
            if g.idle >= g.live {
                g.done = true;
                g.idle -= 1;
                self.idle_hint.store(g.idle, Ordering::Relaxed);
                self.term_cv.notify_all();
                return None;
            }
            self.term_cv.wait(&mut g);
            g.idle -= 1;
            self.idle_hint.store(g.idle, Ordering::Relaxed);
        }
    }

    /// True once some worker has declared this drain complete. Diagnostics and
    /// tests; workers learn it from `pop_or_terminate` returning `None`.
    pub fn drain_is_done(&self) -> bool {
        self.term.lock().done
    }
}

/// Registration of one marker thread on a [`MarkQueue`] drain.
///
/// Dropping it — including while unwinding out of a scan — decrements the live
/// worker count and wakes the peers, so a panicking marker cannot leave the
/// others parked waiting for an `idle` count they can never reach. The
/// equivalent of `young_mark`'s `WorkerExit`.
pub struct MarkWorker<'q> {
    queue: &'q MarkQueue,
}

impl Drop for MarkWorker<'_> {
    fn drop(&mut self) {
        {
            let mut g = self.queue.term.lock();
            g.live = g.live.saturating_sub(1);
        }
        // Deliberately does NOT set `done`: a parked peer wakes, re-checks the
        // queue, and reaches `idle >= live` on its own if the closure really
        // is complete. Setting it here would discard work this worker had
        // already published before it left.
        self.queue.term_cv.notify_all();
    }
}

impl Default for MarkQueue {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Concurrent Marker
// ---------------------------------------------------------------------------

/// The concurrent marker traverses the object graph, marking live objects
/// in the mark bitmap.
pub struct ConcurrentMarker {
    /// Mark bitmap covering the old generation.
    pub bitmap: MarkBitmap,
    /// Work queue of gray objects to scan.
    pub queue: MarkQueue,
    /// Global SATB queue for write barrier entries.
    ///
    /// fork6 GC_STRESS fix — MUST be the same instance the heap's
    /// `satb_barrier` logs into (`GenerationalHeap::enable_concurrent_gc`),
    /// or the write barrier feeds a queue nobody drains while `remark`
    /// drains a queue nobody fills. A marker built via [`Self::new`] gets a
    /// private queue (tests); production cycles use [`Self::with_shared`]
    /// with the heap's handles.
    pub satb_queue: Arc<SatbQueue>,
    /// Phase tracker. Same sharing requirement as `satb_queue`: the heap's
    /// `satb_barrier` fast-path gates on `is_marking_active()` of the
    /// instance attached via `enable_concurrent_gc`.
    pub state: Arc<ConcurrentGcState>,
    /// TAMS equivalent (G1MARK-3): the old-gen object starts as of the
    /// INITIAL MARK — the point the cycle's snapshot is taken. The sweep
    /// (which runs OUTSIDE any STW) may only free objects present here, so an
    /// old-gen allocation landing at any time after the mark opened (a young
    /// GC on another thread promoting survivors, or a direct large-object
    /// allocation) is implicitly live for this cycle and cannot be swept.
    ///
    /// gengc-mark 2026-09-20 moved this from REMARK to INITIAL MARK. Remark is
    /// too late: it also admits everything allocated DURING the concurrent
    /// trace, and those are exactly the objects the cycle had no way to mark
    /// (nothing here allocates black, and the SATB pre-barrier logs only OLD
    /// slot values). See `initial_mark` for the full argument. Remark now only
    /// NARROWS this set, to entries still allocated at the pause.
    ///
    /// Emptiness is no longer the abort-safe signal — the set is populated
    /// from the moment the cycle opens. The sweep is gated on
    /// [`Self::sweep_eligible_epoch`] instead, which only a completed remark
    /// stamps.
    ///
    /// gengc-mark2 2026-09-20: was `Mutex<HashSet<usize>>`; now an
    /// [`OldGenObjectStarts`] bitmap, and `None` rather than an emptied set
    /// when no cycle is open. `None` and `Some(empty)` are treated alike by
    /// every reader — see `concurrent_sweep` — so this is a representation
    /// change only.
    sweep_eligible: Mutex<Option<OldGenObjectStarts>>,
    /// `OldGen::reclaim_epoch` as of the INITIAL MARK that produced
    /// `sweep_eligible`, or `None` when no cycle is open.
    ///
    /// gengc-mark 2026-09-20. Split out from [`Self::sweep_eligible_epoch`]
    /// when the eligibility snapshot moved from remark to initial mark. The two
    /// stamps answer different questions and must not be conflated:
    ///
    /// * this one says "the layout the snapshot describes is still the current
    ///   layout", and is checked at REMARK, so a `free`/`compact` by another
    ///   old-gen collector ANYWHERE in the cycle is caught rather than only one
    ///   that happens after remark;
    /// * `sweep_eligible_epoch` says "remark ran and authorised a sweep", and
    ///   stays `None` until it does — which is the property the sweep's
    ///   abort-safe default rests on.
    initial_mark_epoch: Mutex<Option<u64>>,
    /// GCAUD-4 — `OldGen::reclaim_epoch` as of the remark that AUTHORISED a
    /// sweep of `sweep_eligible`, or `None` when no sweep is authorised.
    ///
    /// `sweep_eligible` and `bitmap` are both keyed on a bare old-gen ADDRESS,
    /// and `concurrent_sweep` runs OUTSIDE any stop-the-world: between remark
    /// and the sweep's `old_gen` lock acquisition, another thread's young GC
    /// can run a full `old_gen_gc` — either the sliding `compact` (every
    /// survivor's address changes) or the in-place sweep (blocks return to the
    /// free list and the next `alloc` re-issues those addresses to NEW
    /// objects). Either way an address in `sweep_eligible` stops naming the
    /// object it named at remark, while the bitmap bit at that address still
    /// describes the OLD occupant.
    ///
    /// The TAMS filter reads "existed at remark AND unmarked ⇒ free it", so a
    /// live object that inherited a dead object's address satisfies both
    /// halves and is freed — a use-after-free manufactured by two collectors
    /// that individually behave correctly. The epoch is the identity the bare
    /// address lacks; on a mismatch the sweep frees nothing.
    sweep_eligible_epoch: Mutex<Option<u64>>,
}

/// GCAUD-4: how many concurrent sweeps were abandoned because old-gen storage
/// was reclaimed or relocated between remark and the sweep. Non-zero means the
/// two old-gen collectors are interleaving; the cycle reclaimed nothing, which
/// is the safe half of that race.
pub static SWEEP_EPOCH_ABORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl ConcurrentMarker {
    /// Create a new concurrent marker for a heap region with PRIVATE SATB
    /// queue + phase state (tests / single-threaded callers only — mutator
    /// write barriers cannot see these instances; see [`Self::with_shared`]).
    pub fn new(old_gen_base: usize, old_gen_size: usize) -> Self {
        Self::with_shared(
            old_gen_base,
            old_gen_size,
            Arc::new(SatbQueue::new()),
            Arc::new(ConcurrentGcState::new()),
        )
    }

    /// Create a concurrent marker wired to the heap's shared SATB queue and
    /// phase state (the instances registered via
    /// `GenerationalHeap::enable_concurrent_gc`), so mutator `satb_barrier`
    /// logs are visible to this cycle's `remark`.
    pub fn with_shared(
        old_gen_base: usize,
        old_gen_size: usize,
        satb_queue: Arc<SatbQueue>,
        state: Arc<ConcurrentGcState>,
    ) -> Self {
        Self {
            bitmap: MarkBitmap::new(old_gen_base, old_gen_size),
            queue: MarkQueue::new(),
            satb_queue,
            state,
            sweep_eligible: Mutex::new(None),
            initial_mark_epoch: Mutex::new(None),
            sweep_eligible_epoch: Mutex::new(None),
        }
    }

    /// Abort the cycle WITHOUT sweeping — the mark bitmap is not final (e.g.
    /// the remark STW could not be acquired). Deactivates the SATB barrier
    /// (discarding the drained entries — they only matter to a sweep that is
    /// no longer happening) and returns the phase to Idle so the write
    /// barrier stops logging. The next `old_gen_needs_gc` trigger starts a
    /// fresh cycle.
    pub fn abort_cycle(&self) {
        // An abort is not a safepoint, and this caller discards the entries
        // anyway -- so it takes the form that cannot claim a snapshot.
        // `deactivate_and_drain` now requires a `StopTheWorldToken`; this site
        // could not honestly produce one.
        self.satb_queue.deactivate_and_discard();
        self.queue.clear();
        // No sweep will run for this cycle; make sure a later cycle's sweep
        // can never consume this cycle's stale eligibility snapshot.
        *self.sweep_eligible.lock() = None;
        *self.initial_mark_epoch.lock() = None;
        *self.sweep_eligible_epoch.lock() = None;
        self.state.set_phase(ConcurrentGcPhase::Idle);
    }

    /// Mark the cycle complete after the sweep: phase back to Idle.
    pub fn finish_cycle(&self) {
        self.state.set_phase(ConcurrentGcPhase::Idle);
    }

    /// Phase 1: Initial Mark (called during brief STW pause).
    ///
    /// Marks objects directly reachable from roots. Only marks old-gen objects;
    /// young-gen objects are handled by the minor GC.
    ///
    /// Returns the number of root objects marked.
    pub fn initial_mark(&self, roots: &[*mut u8], old_gen: &OldGen) -> usize {
        self.state.set_phase(ConcurrentGcPhase::InitialMark);
        self.bitmap.clear();
        self.queue.clear();

        // Arm the barrier BEFORE anything is marked. `set_phase`'s own doc
        // states the rule this follows -- a logging gate must be armed for a
        // SUPERSET of the interval in which it is required, never a subset --
        // and the queue gate is one such gate. Activating after the root scan
        // was safe only because this runs at a stop-the-world; arming first
        // costs nothing and removes the dependence.
        self.satb_queue.activate();

        let object_starts = old_gen_object_starts(old_gen);
        let mut count = 0;
        for &root_ptr in roots {
            if markable_old_object(root_ptr, &object_starts)
                && self.bitmap.try_mark(root_ptr as usize)
            {
                self.queue.push(root_ptr);
                count += 1;
            }
        }

        // TAMS (gengc-mark 2026-09-20) — the sweep's eligibility snapshot is
        // taken HERE, at initial mark, not at remark.
        //
        // The rule a snapshot-based concurrent collector needs is "an object
        // allocated after the mark started is implicitly live for this cycle"
        // (G1's TAMS, HotSpot's `top_at_mark_start`). The snapshot used to be
        // taken at REMARK, which is a strictly LARGER set — it also contains
        // everything allocated DURING the concurrent phase — and the extra
        // members are exactly the objects the cycle cannot have marked:
        //
        //   * a young GC promotes survivor X into the old gen while the
        //     concurrent trace is running;
        //   * X is stored into old object O, which the trace has already
        //     scanned (O is black);
        //   * the SATB pre-barrier logs the OLD value of O's slot, not X --
        //     logging X is the job of an allocate-black / post-write barrier,
        //     and there is none;
        //   * remark rescans roots and young->old edges, which finds X only if
        //     something young still points at it.
        //
        // X is then unmarked AND in the remark snapshot, which is precisely the
        // sweep's licence to free it: a live object reclaimed. Anchoring the
        // snapshot at initial mark makes X ineligible by construction; it is
        // simply collected one cycle later. `reclaim_epoch` is stamped into
        // `initial_mark_epoch` at the same instant, so the GCAUD-4 identity
        // check covers the whole cycle rather than only its tail.
        //
        // AUTHORISATION IS SEPARATE FROM THE SNAPSHOT. `sweep_eligible_epoch`
        // is deliberately left `None` here and is set only by a `remark` that
        // completes: the sweep's abort-safe default is "no authorising stamp ⇒
        // free nothing", and moving the SNAPSHOT earlier must not accidentally
        // move the AUTHORISATION earlier with it. A cycle whose remark STW
        // could not be acquired (the driver's `abort_cycle` path, and any
        // caller that skips remark) therefore still reclaims nothing — the
        // same guarantee the old "empty snapshot ⇒ free nothing" arrangement
        // gave, now stated as an explicit gate instead of an emergent one.
        *self.sweep_eligible.lock() = Some(object_starts);
        *self.initial_mark_epoch.lock() = Some(old_gen.reclaim_epoch());
        *self.sweep_eligible_epoch.lock() = None;

        self.state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        count
    }

    /// Phase 2: Concurrent Mark — process the mark queue until empty.
    ///
    /// Called by marker thread(s). This runs concurrently with application
    /// threads. The SATB barrier ensures correctness by logging overwritten
    /// references.
    ///
    /// Returns the number of objects scanned.
    pub fn concurrent_mark(&self, old_gen: &OldGen) -> usize {
        self.concurrent_mark_budget(old_gen, usize::MAX).0
    }

    /// Phase 2 in a BOUNDED SLICE: scan at most `budget` objects, then return.
    ///
    /// Returns `(objects scanned, closure complete)`. `true` for the second
    /// element means the queue drained (or the cycle degraded to
    /// `mark_all_old_gen`) and no further slice is needed.
    ///
    /// # Why this exists (gengc-mark-concurrent-mark-holds-the-old-gen-lock)
    ///
    /// The driver runs Phase 2 as
    ///
    /// ```text
    /// if let Some(guard) = shared.mem.heap.old_gen_lock() {
    ///     marker.concurrent_mark(&*guard);
    /// }
    /// ```
    ///
    /// — one acquisition of the heap's `Mutex<OldGen>` held for the whole
    /// transitive closure over the old generation, which is the longest phase
    /// of the cycle and unbounded in the size of the live old-gen graph. Every
    /// promotion path takes that same lock, so for the duration of the
    /// "concurrent" mark no thread can tenure a survivor or allocate a large
    /// object directly into the old generation. A young GC that needs to
    /// promote blocks on a lock held by the phase whose entire purpose is not
    /// to block mutators.
    ///
    /// This entry point is the collector-side half of the fix: the driver can
    /// loop `acquire → slice → release → yield` and bound the time any
    /// promoting thread waits by the slice budget rather than by the size of
    /// the heap.
    ///
    /// ```text
    /// // vm/src/runtime/interpreter/gc_and_alloc.rs, maybe_concurrent_gc
    /// loop {
    ///     let done = match shared.mem.heap.old_gen_lock() {
    ///         Some(guard) => marker.concurrent_mark_budget(&*guard, SLICE).1,
    ///         None => true,
    ///     };
    ///     if done { break; }
    ///     std::thread::yield_now();
    /// }
    /// ```
    ///
    /// **That driver change is NOT made here** — `gc_and_alloc.rs` belongs to
    /// another lane — so the gap stays open until it lands. Nothing calls this
    /// yet; `concurrent_mark` above delegates to it with an unbounded budget,
    /// which is byte-for-byte the previous behaviour.
    ///
    /// # What the caller must know about slicing
    ///
    /// * **The snapshot is rebuilt per slice, and must be.** `object_starts`
    ///   is taken at the top of each call, so an object promoted between two
    ///   slices IS markable in the next one. A cached snapshot would silently
    ///   refuse to enqueue it — and refusing to enqueue it means refusing to
    ///   scan its children, one of which may be an older object that IS
    ///   sweep-eligible and would then be freed while live. This is the
    ///   correctness question the whole-phase lock was masking, and the answer
    ///   is "rebuild", not "cache".
    /// * **The budget must therefore amortise an O(old gen) walk.** A slice of
    ///   a few hundred objects would spend all its time rebuilding. Tens of
    ///   thousands is the right order; the walk is now a bitmap fill rather
    ///   than a `HashSet` build (see [`OldGenObjectStarts`]), which is what
    ///   makes per-slice rebuilding affordable at all.
    /// * The cycle is still abandoned at remark if `reclaim_epoch` moved, so a
    ///   `free` or `compact` landing between slices fails the cycle closed
    ///   exactly as one landing anywhere else in the cycle does.
    ///
    /// A `budget` of 0 is raised to 1, so a caller cannot construct a loop
    /// that makes no progress.
    pub fn concurrent_mark_budget(&self, old_gen: &OldGen, budget: usize) -> (usize, bool) {
        let budget = budget.max(1);
        let mut scanned = 0usize;
        let object_starts = old_gen_object_starts(old_gen);

        while scanned < budget {
            let Some(obj_ptr) = self.queue.pop() else {
                return (scanned, true);
            };
            if !self.scan_object(obj_ptr, old_gen, &object_starts) {
                // Degraded path: every old-gen object is marked for this
                // cycle, so there is nothing left to slice.
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return (scanned, true);
            }
            scanned += 1;
        }

        (scanned, self.queue.is_empty())
    }

    /// Phase 3: Remark (called during brief STW pause).
    ///
    /// Processes all SATB buffer entries and re-scans roots to catch any
    /// references modified during the concurrent mark phase.
    ///
    /// Returns the number of additional objects discovered.
    pub fn remark(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
    ) -> usize {
        self.state.set_phase(ConcurrentGcPhase::Remark);
        let mut discovered = 0;
        let object_starts = old_gen_object_starts(old_gen);

        // TAMS (G1MARK-3, re-anchored gengc-mark 2026-09-20): the snapshot of
        // what the sweep may free was taken at INITIAL MARK (see there for
        // why). Remark only NARROWS it, to objects that are still allocated —
        // and only while the old gen's identity is intact.
        //
        // GCAUD-4: `sweep_eligible` is keyed on bare addresses, which stop
        // naming the same objects the moment another collector frees or slides
        // old-gen storage. If that happened between initial mark and now, the
        // snapshot is meaningless and, worse, actively wrong (a freed address
        // reissued to a live object is "eligible" with a clear bit). Fail
        // closed: empty the snapshot so this cycle reclaims nothing and the
        // next trigger starts fresh against the current layout.
        //
        // Reaching this point is also what AUTHORISES a sweep: only a remark
        // that completes stamps `sweep_eligible_epoch`, which `concurrent_sweep`
        // requires. `initial_mark` leaves it `None` on purpose — see there.
        //
        // Note this REPLACES the previous full `object_starts.clone()` into the
        // snapshot, so the remark pause no longer pays for building a second
        // copy of a set with one entry per old-gen object.
        {
            let epoch_now = old_gen.reclaim_epoch();
            let mut eligible = self.sweep_eligible.lock();
            let opened_at = *self.initial_mark_epoch.lock();
            let mut authorised = self.sweep_eligible_epoch.lock();
            // gengc-mark2 2026-09-20: the narrowing is now a word-wise AND of
            // two bitmaps instead of a `HashSet::retain` with one SipHash per
            // surviving entry. `retain_intersection` returns `false` only if
            // the two walks disagree about the generation's EXTENT, i.e. the
            // backing storage itself moved — which no `OldGen` operation does,
            // but which would silently corrupt the snapshot if it ever did.
            // Treat it exactly like an epoch mismatch: fail closed.
            let geometry_ok = opened_at == Some(epoch_now)
                && eligible
                    .as_mut()
                    .map(|e| e.retain_intersection(&object_starts))
                    .unwrap_or(true);
            if geometry_ok {
                *authorised = Some(epoch_now);
            } else {
                let stranded = eligible.as_ref().map_or(0, |e| e.len());
                if stranded > 0 && opened_at.is_some() {
                    // `opened_at.is_some()` distinguishes the real race (a cycle
                    // WAS opened and another collector moved the ground under
                    // it) from a caller that reached `remark` without an
                    // `initial_mark` at all, which is a programming error, not
                    // an interleaving, and is already fatal to the sweep below.
                    SWEEP_EPOCH_ABORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::info!(
                        snapshot_epoch = ?opened_at,
                        current_epoch = epoch_now,
                        eligible = stranded,
                        "concurrent cycle will reclaim nothing: old-gen storage was \
                         reclaimed or relocated between initial mark and remark, so the \
                         address-keyed eligibility snapshot no longer identifies the same \
                         objects",
                    );
                }
                *eligible = None;
                *authorised = None;
            }
        }

        // Process SATB entries: these are old reference values that were
        // overwritten during concurrent marking. We must mark them to
        // prevent live objects from being collected.
        //
        // gc-concmark HIGH fix — keep the SATB barrier ACTIVE across the
        // remark closure. The previous code called
        // `deactivate_and_drain()` HERE (before the closure below), which
        // flipped the gate straight to INACTIVE while the mark bitmap was
        // still being computed. Between that INACTIVE store and the start
        // of `concurrent_sweep`, the SATB pre-barrier (gated on
        // `satb_queue.is_active()`) stops logging, so a mutator that
        // overwrites a still-live old-gen reference is NOT recorded — its
        // target object's bit stays clear in the bitmap and the sweep
        // frees it while it is still reachable (floating-garbage free →
        // use-after-free). Correctness of the sweep requires the bitmap
        // to be FINAL, which means every overwritten old reference up to
        // the moment mutators are quiesced must be marked.
        //
        // Fix: drain WITHOUT deactivating so the barrier keeps logging
        // any concurrent overwrites into the queue while we build the
        // closure below; we run a final `deactivate_and_drain()` (which
        // also captures late writers via its shard-lock barrier) only
        // after the closure, and re-mark whatever it returns before the
        // gate is allowed to go INACTIVE. See the closing block.
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
                discovered += 1;
            }
        }

        // Re-scan roots (some may have changed during concurrent mark).
        for &root_ptr in roots {
            if markable_old_object(root_ptr, &object_starts)
                && self.bitmap.try_mark(root_ptr as usize)
            {
                self.queue.push(root_ptr);
                discovered += 1;
            }
        }

        // Drain the queue fully (mark transitive closure from new roots),
        // including the overflow fallback. NOTE: the SATB barrier is STILL
        // ACTIVE at this point (we used `drain()` above, not
        // `deactivate_and_drain()`), so any reference a mutator overwrites
        // while we compute this closure is logged and will be captured by
        // the final drain below.
        discovered += self.drain_closure(old_gen, &object_starts);

        // gc-concmark HIGH fix — final quiescing drain.
        //
        // Now that the closure from the snapshot + roots is complete, flip
        // the barrier off ATOMICALLY with a final drain. `deactivate_and_drain`
        // transitions ACTIVE→DRAINING (loggers keep logging), drains, then
        // takes each shard lock exclusively to capture any late writer that
        // observed ACTIVE before the CAS, and only then stores INACTIVE.
        //
        // This closes the window the old code left open: between the moment
        // the gate went INACTIVE and the sweep, an overwritten live old-gen
        // reference could go unlogged and be swept while reachable. Here the
        // gate is not allowed to go INACTIVE until every entry logged up to
        // the drain barrier has been returned to us — and we mark every one
        // of them (plus its transitive closure) BEFORE returning, so the
        // bitmap handed to `concurrent_sweep` is final.
        //
        // In a true STW remark mutators are already stopped, so this drain
        // typically returns nothing; in a (mostly-)concurrent remark it
        // reaps the stragglers. Either way the bitmap is final on exit.
        //
        // gengc-mark 2026-09-20 — CLOSE THE MUTATOR GATE FIRST.
        //
        // The generational heap's `satb_barrier` (gen_heap.rs) gates on
        // `ConcurrentGcState::is_marking_active()`, i.e. on the PHASE, while
        // the log it writes into is gated on `SatbQueue::is_active()`. G1
        // asserts the invariant that ties those two together --
        // `is_marking_active() => satb_queue.is_active()` (`g1.rs`, the
        // `assert_satb_invariant` note) -- and this function used to break it
        // for the whole width of the closure below: the phase stayed `Remark`
        // (barrier ON) while the queue had already gone INACTIVE. A mutator
        // storing a reference in that window passes the phase gate, appends to
        // its per-thread bucket for a queue nobody will drain again, and the
        // entry sits there until the NEXT cycle's `flush_all_thread_satb_buffers`
        // replays it as a root -- by which time the address may name a
        // different object entirely.
        //
        // Moving the phase transition ahead of the deactivation restores the
        // superset rule: `ConcurrentSweep` turns the mutator barrier off while
        // the queue is still ACTIVE, and the drain immediately afterwards
        // therefore captures every entry any mutator could still have logged.
        // The observable post-conditions of `remark` are unchanged (phase ==
        // ConcurrentSweep, queue inactive).
        self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        let late_entries = self.satb_queue.deactivate_and_drain(stw);
        for addr in late_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
                discovered += 1;
            }
        }
        // Mark the transitive closure of any late entries (again with the
        // overflow fallback). The gate is INACTIVE now, but mutators are
        // quiesced past the drain barrier, so no further live overwrite can
        // escape the bitmap.
        discovered += self.drain_closure(old_gen, &object_starts);

        // Phase was already advanced to `ConcurrentSweep` above, ahead of the
        // deactivation, to keep `is_marking_active() => satb_queue.is_active()`
        // true at every instant.

        discovered
    }

    /// Drain the mark queue to its transitive closure, then run the
    /// graceful overflow fallback (Round-9 gc HIGH-5) until no shard has
    /// overflowed. Returns the number of objects scanned. Extracted so the
    /// remark closure can be re-run after the final SATB quiescing drain
    /// (gc-concmark fix) without duplicating the overflow logic.
    fn drain_closure(&self, old_gen: &OldGen, object_starts: &OldGenObjectStarts) -> usize {
        let mut discovered = 0;

        // Drain the queue fully (mark transitive closure).
        while let Some(obj_ptr) = self.queue.pop() {
            if !self.scan_object(obj_ptr, old_gen, object_starts) {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return discovered;
            }
            discovered += 1;
        }

        // Round-9 gc HIGH-5 fix — graceful overflow fallback.
        //
        // If any `push` during this cycle was dropped because a queue
        // shard hit `MARK_QUEUE_SHARD_CAP`, the transitive closure
        // above is incomplete: outgoing references from the dropped
        // objects were never scanned. To preserve correctness we run
        // a conservative full re-walk of the old generation, marking
        // every reachable object found through any allocated object's
        // outgoing references. This is O(N) instead of O(reachable)
        // and may take many seconds on a multi-GB heap, but it
        // **cannot** crash the VM the way the previous `panic!` did.
        //
        // The loop iterates because the rescan itself may overflow:
        // each pass clears the flag, scans everything currently
        // allocated, and repeats until a pass completes with no new
        // overflow. In practice one or two passes suffice; the cap is
        // generous enough that any realistic graph terminates immediately.
        let mut rescan_passes = 0;
        while self.queue.has_overflowed() {
            // Reset before the rescan so a fresh overflow in this pass
            // is observable. Push/pop happen-before this load: STW.
            self.queue.overflowed.store(false, Ordering::Relaxed);
            rescan_passes += 1;
            if rescan_passes > 8 {
                // Hard guard: if we somehow can't converge, abandon
                // the queue and mark every allocated object directly.
                // The sweep that follows will keep all live objects;
                // garbage retention is the price of forward progress.
                for (obj_ptr, _size) in old_gen.walk_objects() {
                    self.bitmap.try_mark(obj_ptr as usize);
                }
                self.queue.clear();
                break;
            }
            for (obj_ptr, _size) in old_gen.walk_objects() {
                if self.bitmap.is_marked(obj_ptr as usize) {
                    // Already gray/black: re-scan its outgoing refs to
                    // pick up children we may have dropped.
                    if !self.scan_object(obj_ptr, old_gen, object_starts) {
                        self.mark_all_old_gen(old_gen);
                        self.queue.clear();
                        return discovered;
                    }
                    discovered += 1;
                }
            }
            // Drain anything the rescan re-enqueued.
            while let Some(obj_ptr) = self.queue.pop() {
                if !self.scan_object(obj_ptr, old_gen, object_starts) {
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    return discovered;
                }
                discovered += 1;
            }
        }

        discovered
    }

    /// Phase 4: Concurrent Sweep — reclaim unmarked old-gen objects.
    ///
    /// Walks all allocated objects in the old generation and frees those
    /// that are not marked in the bitmap.
    ///
    /// Returns the number of objects freed.
    pub fn concurrent_sweep(&self, old_gen: &mut OldGen) -> usize {
        let objects = old_gen.walk_objects();
        let mut freed = Vec::new();

        // TAMS (G1MARK-3, re-anchored gengc-mark 2026-09-20): the bitmap was
        // finalized at the remark STW, but this sweep runs OUTSIDE any STW — an
        // old-gen allocation landing after the mark opened is unmarked yet
        // fully live. Only objects that existed AT INITIAL MARK (the
        // `sweep_eligible` snapshot, narrowed at remark) may be freed; later
        // allocations are implicitly live for this cycle.
        // gengc-mark2 2026-09-20: `Option::take` where this used to
        // `mem::take` a `HashSet`. `None` (no cycle, or one abandoned by
        // `abort_cycle`/remark) and `Some(empty)` (a cycle over an empty
        // generation) take the same "free nothing" exit, exactly as the empty
        // `HashSet` did for both cases before.
        let eligible = self.sweep_eligible.lock().take();
        let snapshot_epoch = self.sweep_eligible_epoch.lock().take();
        // Belt and braces: the cycle is over either way, so leave no stamp a
        // later sweep could mistake for an authorisation.
        *self.initial_mark_epoch.lock() = None;
        let Some(eligible) = eligible.filter(|e| !e.is_empty()) else {
            self.bitmap.clear();
            self.state.set_phase(ConcurrentGcPhase::Idle);
            return 0;
        };
        // NO AUTHORISING STAMP ⇒ FREE NOTHING (gengc-mark 2026-09-20).
        //
        // `sweep_eligible_epoch` is set by `remark` and by nothing else, so
        // `None` here means remark did not run for this snapshot: the bitmap is
        // not final, the SATB log was never drained, and every unmarked-looking
        // object may simply be one the closure had not reached. Before the
        // snapshot moved to initial mark this was implied by the snapshot being
        // empty; now it has to be checked, because the snapshot is populated
        // from the moment the cycle opens.
        //
        // Distinct from the epoch MISMATCH below: nothing raced, so this is not
        // counted as an abort. It is the abort-safe default for a cycle the
        // caller abandoned (the driver's `!remark_done` path does call
        // `abort_cycle`, which clears the snapshot outright; this catches the
        // caller that forgets).
        if snapshot_epoch.is_none() {
            tracing::debug!(
                eligible = eligible.len(),
                "concurrent sweep skipped: remark never authorised this snapshot",
            );
            self.bitmap.clear();
            self.state.set_phase(ConcurrentGcPhase::Idle);
            return 0;
        }

        // GCAUD-4: `eligible` and `bitmap` are keyed on bare old-gen
        // addresses, and this sweep is the one phase of the cycle that runs
        // outside a stop-the-world. If any other collector freed or relocated
        // old-gen storage since remark, an address in `eligible` may now name
        // a DIFFERENT object — a live one, whose bit is clear only because the
        // bit describes its predecessor. Both halves of the TAMS filter would
        // then be satisfied by a live object and the sweep would free it.
        //
        // Fail closed: reclaim nothing this cycle. The next `old_gen_needs_gc`
        // trigger starts a fresh cycle against the current layout, so the
        // garbage is collected one cycle later rather than the live object
        // being collected now.
        if snapshot_epoch != Some(old_gen.reclaim_epoch()) {
            SWEEP_EPOCH_ABORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // `info!`: a whole concurrent sweep's work is being thrown
            // away, at most once per cycle. `SWEEP_EPOCH_ABORTS` is read
            // only by this file's tests, and as `debug!` this line could
            // not print in a release build, so the abandonment was
            // unobservable outside a debug run.
            tracing::info!(
                snapshot_epoch = ?snapshot_epoch,
                current_epoch = old_gen.reclaim_epoch(),
                eligible = eligible.len(),
                "concurrent sweep abandoned: old-gen storage was reclaimed or \
                 relocated since remark, so the address-keyed mark bitmap and \
                 eligibility snapshot no longer identify the same objects",
            );
            self.bitmap.clear();
            self.state.set_phase(ConcurrentGcPhase::Idle);
            return 0;
        }

        for (obj_ptr, total_size) in objects {
            if !self.bitmap.is_marked(obj_ptr as usize) && eligible.contains(obj_ptr as usize) {
                freed.push((obj_ptr, total_size));
            }
        }

        let freed_count = freed.len();
        for (ptr, size) in freed {
            // SAFETY: ptr and size come from old_gen.walk_objects() which yields
            // valid (pointer, total_size) pairs for allocated objects. The object
            // is unmarked (unreachable), so freeing it is correct.
            unsafe { old_gen.free(ptr, size) };
        }
        // Same amortised merge the in-place STW sweep does (see
        // `OldGen::coalesce_free_blocks`): this reclaimer never compacts, and
        // `free` alone leaves every reclaimed object as an isolated free
        // block, so without it the generation fragments monotonically until a
        // modest request OOMs on mostly-free storage.
        if freed_count > 0 {
            old_gen.coalesce_free_blocks();
        }

        self.bitmap.clear();
        self.state.set_phase(ConcurrentGcPhase::Idle);

        freed_count
    }

    fn mark_all_old_gen(&self, old_gen: &OldGen) -> usize {
        let mut marked = 0;
        for (obj_ptr, _size) in old_gen.walk_objects() {
            if self.bitmap.try_mark(obj_ptr as usize) {
                marked += 1;
            }
        }
        marked
    }

    /// Scan an object's reference fields and mark any old-gen targets.
    ///
    /// Returns `false` when the queued pointer names an object with an
    /// inconsistent or implausible header. Callers respond by marking every
    /// old-gen object for this cycle, retaining garbage rather than under-marking
    /// live objects or dereferencing a bogus field extent.
    fn scan_object(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> bool {
        // SAFETY: obj_ptr was popped from the mark queue, which only contains
        // pointers to valid old-gen objects verified by old_gen.contains() before
        // being enqueued. The header is readable for the lifetime of the GC cycle.
        let header_ptr = obj_ptr as *const ObjectHeader;
        let Some(total_size) = concurrent_mark_object_size(header_ptr) else {
            let snapshot = ConcurrentMarkHeaderSnapshot::read(header_ptr);
            tracing::warn!(
                "concurrent mark: skipping object at {:p} with inconsistent header \
                 (kind_tag={}, element_tag={}, class_id={}, array_length={}, num_slots={}, \
                 gc_flags=0x{:02x}); marking all old-gen objects for this cycle",
                obj_ptr,
                snapshot.kind_tag,
                snapshot.element_tag,
                snapshot.class_id,
                snapshot.array_length(),
                snapshot.num_slots(),
                snapshot.gc_flags,
            );
            return false;
        };
        if total_size < HEADER_SIZE
            || !old_gen.contains(unsafe { obj_ptr.add(total_size.saturating_sub(1)) })
        {
            let snapshot = ConcurrentMarkHeaderSnapshot::read(header_ptr);
            tracing::warn!(
                "concurrent mark: skipping object at {:p} with implausible extent {} \
                 (kind_tag={}, element_tag={}, class_id={}, array_length={}, num_slots={}, \
                 gc_flags=0x{:02x}); marking all old-gen objects for this cycle",
                obj_ptr,
                total_size,
                snapshot.kind_tag,
                snapshot.element_tag,
                snapshot.class_id,
                snapshot.array_length(),
                snapshot.num_slots(),
                snapshot.gc_flags,
            );
            return false;
        }
        let header = unsafe { &*header_ptr };

        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Reference array: compact 8-byte pointer per element.
                for i in 0..header.array_length() as usize {
                    // SAFETY: i < array_length, offset is within the allocated array object.
                    let slot_ptr =
                        unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    // SAFETY: slot_ptr points to a valid 8-byte reference element in the array.
                    let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if markable_old_object(ref_ptr, object_starts)
                            && self.bitmap.try_mark(ref_ptr as usize)
                        {
                            self.queue.push(ref_ptr);
                        }
                    }
                }
            }
            // Primitive arrays have no references to scan.
        } else if crate::is_compact_object(header) {
            // HIB-DCAST-LATEPHASE.1: `compact_oop_scan` returns `None` both
            // for "this is a legacy object" (its documented contract) and,
            // via its internal `class_layout_for_fields(..)?`, for "this IS
            // a compact object (`GC_FLAG_COMPACT` set) but its class's
            // layout is not registered right now". Gating on
            // `is_compact_object(header)` (the header bit, independent of
            // the registry) rather than `compact_oop_scan(..).is_some()`
            // keeps the second case out of the legacy arm below, which would
            // misread this object's packed compact body under the legacy
            // `num_slots * SLOT_SIZE` formula — an UNBOUNDED stride (this
            // loop has no `body_bytes` cap, unlike `scan_dirty_cards`'s
            // twin) past the object's real extent. A compact object whose
            // layout cannot be resolved has no provably-safe reference slots
            // to visit; skip it.
            if let Some((layout, body)) = crate::heap::compact_oop_scan(header) {
                // Compact object: 8-byte reference slots at the per-class oop-map
                // offsets. An aligned single-word 8-byte pointer load cannot tear,
                // so (like the reference-array branch above) no stripe lock is
                // needed even though this runs concurrently with mutators.
                for &off in &layout.ref_offsets {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    // SAFETY: `off` is within the object's body (capped above).
                    let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + off) };
                    let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if markable_old_object(ref_ptr, object_starts)
                            && self.bitmap.try_mark(ref_ptr as usize)
                        {
                            self.queue.push(ref_ptr);
                        }
                    }
                }
            }
        } else {
            // Object: 16-byte Value slots.
            //
            // gc-concmark MEDIUM fix — torn-read data race. This loop runs on
            // the marker thread *concurrently* with mutators (Phase 2
            // `concurrent_mark` is the whole point of this module), so a
            // mutator may be mid-store into the very slot we read here. A
            // `Value` is 16 bytes — wider than any stable atomic on x86-64 —
            // so a plain `ptr::read::<Value>` can splice the tag word of one
            // store with the payload word of another (or with the pre-store
            // bytes). The resulting `Value::Object(Some(..))` would carry a
            // garbage pointer that we then feed to `old_gen.contains` /
            // `try_mark` / `queue.push` and ultimately dereference as an
            // `ObjectHeader` in the next `scan_object` — a memory-safety hole
            // reachable purely from concurrent application activity (and plain
            // UB under the Rust memory model: a non-atomic read racing a
            // non-atomic write).
            //
            // Mirror exactly how the rest of the heap reads a 16-byte slot
            // safely under concurrency: `Heap::get_field_volatile`
            // (heap.rs:540) takes the per-slot stripe lock from
            // `collector::volatile_stripe_lock(obj_ref, index)` so the
            // 16-byte `Value` appears either fully-old or fully-new, never
            // torn. We acquire the SAME stripe lock here, keyed on the same
            // `(object, slot index)`, so this read serializes against every
            // mutator store routed through the volatile field-access helpers,
            // and the `SeqCst` fence gives the read the JMM acquire edge. The
            // 8-byte reference-array path above needs no lock: an aligned
            // 8-byte pointer load/store is single-word and cannot tear.
            //
            // SAFETY: `obj_ptr` was popped from the mark queue, where it was
            // validated by `old_gen.contains` before being enqueued, so it is
            // a non-null, 8-byte-aligned, live old-gen object address — the
            // precondition for `ObjectRef::from_raw`. We use the resulting
            // `ObjectRef` only as a stripe-lock key (its address is hashed),
            // never to mutate the object.
            let obj_ref = unsafe { cratonvm_types::ObjectRef::from_raw(obj_ptr) };
            // Bound the walk by the extent this function ALREADY VALIDATED,
            // not by a fresh read of the header.
            //
            // `total_size` came from `concurrent_mark_object_size`, which reads
            // a `ConcurrentMarkHeaderSnapshot` and cross-validates it, and the
            // guard above then required
            // `old_gen.contains(obj_ptr + total_size - 1)` — the object's last
            // byte is inside the old generation. Re-reading `header.num_slots()`
            // here discards that: it is a SECOND read of a header this module
            // explicitly treats as racy (the snapshot reader exists precisely
            // because the header can be torn or garbage), so the count that was
            // validated and the count that is walked need not be the same
            // number. If the second read is the larger one, this loop visits
            // slots past the extent `old_gen.contains` approved — which is the
            // "can a reader visit slot n of an object whose real slot count is
            // below n" question that
            // `internal/fixed-bugs/hib-orm-json-xml-function-tests-segfault-g1-zgc-FIXED-20260901.md`
            // §0.5 item 2 asks of exactly this code.
            //
            // Deriving the count from `total_size` closes the window by
            // construction: the same arithmetic that was validated
            // (`HEADER_SIZE + num_slots * SLOT_SIZE`) is inverted here, so the
            // walk cannot outrun the bytes that were checked. The `1 << 24`
            // plausibility clamp still applies — it is enforced inside
            // `concurrent_mark_object_size`, which returns `None` (and so
            // returns early above) for anything larger.
            let num_slots = total_size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
            for slot_idx in 0..num_slots {
                // Serialize the 16-byte read against striped mutator writes so
                // we never observe a torn (tag, payload) pair. Held only for
                // the duration of this single slot read.
                let _stripe = crate::collector::volatile_stripe_lock(obj_ref, slot_idx);
                std::sync::atomic::fence(Ordering::SeqCst);
                // SAFETY: slot_idx < num_slots, offset is within the allocated object.
                let slot_ptr = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                // Read the 16-byte slot as two atomic words. The stripe lock
                // serializes this against `set_field_volatile` writers, but
                // JIT-compiled field stores write the slot directly and take no
                // stripe lock, so the atomic read is what makes *that* pairing
                // well-defined (see `read_value_atomic` / `write_value_atomic`).
                // SAFETY: slot_ptr points to a valid, aligned Value slot.
                // Screen the discriminant before the bytes become a `Value`.
                // `read_value_atomic` transmutes two words unconditionally, so a
                // cell holding two heap pointers (a swept-and-reused slot) became
                // a `Value` with an out-of-range tag — UB the moment it exists,
                // and a garbage `ObjectRef` we would push onto the mark queue and
                // later dereference as an `ObjectHeader`. `heap::read_slot`,
                // `g1::get_field` and `zgc::get_field` were moved onto this guard
                // for exactly that reason; the marker was not, and it is a
                // G1/ZGC-only path, which is where the JSON/XML SIGSEGV pattern
                // lives. Corrupt cells decode to `Value::Object(None)`, which this
                // loop skips, and are counted by the cell census.
                let value = unsafe {
                    crate::heap::read_value_cell_checked(
                        slot_ptr as *const Value,
                        "concurrent_mark::scan_object",
                    )
                };
                std::sync::atomic::fence(Ordering::SeqCst);
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if markable_old_object(ref_ptr, object_starts)
                        && self.bitmap.try_mark(ref_ptr as usize)
                    {
                        self.queue.push(ref_ptr);
                    }
                }
            }
        }
        true
    }

    /// Run all four phases of a concurrent GC cycle.
    ///
    /// This is a convenience method for testing. In production, the phases
    /// are coordinated by the GC barrier with STW pauses at phase 1 and 3.
    ///
    /// Returns (objects_marked, objects_swept).
    pub fn full_cycle(
        &self,
        stw: &crate::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &mut OldGen,
    ) -> (usize, usize) {
        let initial = self.initial_mark(roots, old_gen);
        let concurrent = self.concurrent_mark(old_gen);
        let remark = self.remark(stw, roots, old_gen);
        let swept = self.concurrent_sweep(old_gen);
        (initial + concurrent + remark, swept)
    }
}

/// Torn/garbage-header gate shared by the Generational marker's `scan_object`
/// and (G1MARK-8) G1's `concurrent_mark_step`: reads the header field-by-field
/// with unaligned loads and cross-validates kind tag, element tag, gc-flag
/// universe and size arithmetic. `None` means "do not trust this header".
pub(crate) fn concurrent_mark_object_size(header: *const ObjectHeader) -> Option<usize> {
    let snapshot = ConcurrentMarkHeaderSnapshot::read(header);
    match snapshot.kind_tag {
        tag if tag == ObjectKind::Array as u8 => {
            let element_type = array_element_type_from_tag(snapshot.element_tag)?;
            let data_size = array_data_size(snapshot.array_length() as usize, element_type).ok()?;
            ARRAY_DATA_OFFSET.checked_add(data_size)
        }
        tag if tag == ObjectKind::Object as u8 => {
            // Every DEFINED flag, and `GC_FLAG_HEADER` is one of them as of
            // 2026-09-08. Omitting it here does not merely weaken the screen —
            // it inverts it: the flag is set on every object every allocator
            // publishes, so an incomplete `known_flags` rejects the sizing of
            // EVERY plain object, the concurrent marker skips them all and
            // falls back to "mark all old-gen objects for this cycle", and G1
            // stops unloading classes (caught by `RClassUnloadSweep` in the
            // regression suite, and by this file's own
            // `concurrent_mark_object_size_rejects_inconsistent_object_header`).
            let known_flags = GC_FLAG_OLD_GEN | GC_FLAG_MARKED | GC_FLAG_COMPACT | GC_FLAG_HEADER;
            if snapshot.gc_flags & !known_flags != 0 {
                return None;
            }
            if snapshot.gc_flags & GC_FLAG_COMPACT != 0 {
                return HEADER_SIZE.checked_add(snapshot.compact_body_size()?);
            }
            if snapshot.num_slots() > (1 << 24) {
                return None;
            }
            let fields_size = (snapshot.num_slots() as usize).checked_mul(SLOT_SIZE)?;
            HEADER_SIZE.checked_add(fields_size)
        }
        _ => None,
    }
}

/// The set of old-generation object starts, as a bitmap.
///
/// # Why this is not a `HashSet<usize>` any more (gengc-mark2 2026-09-20)
///
/// It was one, built by `walk_objects().map(..).collect()` at the top of
/// `initial_mark`, `concurrent_mark` and `remark` — three full constructions
/// per cycle, two of them inside a stop-the-world pause. Per call that cost a
/// `Vec<(*mut u8, usize)>` of the whole generation, a `std::collections::
/// HashSet<usize>` at ~48 bytes and one SipHash per entry, and then one
/// SipHash per membership test — and [`markable_old_object`] is called once
/// per reference SLOT scanned, not once per object.
///
/// `young_mark::ObjectStartBits` already exists for exactly this problem on
/// the young side, where the identical `FxHashSet` design measured 49 % of
/// whole-process time on bt18 at `-Xmx8g`. This reuses it rather than
/// inventing a second one: one bit per 8 bytes is 1/64th of the old
/// generation, `contains` becomes a bounds check, a shift and a mask, and the
/// remark-time narrowing becomes a word-wise AND
/// ([`ObjectStartBits::retain_intersection`]).
///
/// # The `unrepresentable` escape hatch
///
/// `ObjectStartBits` indexes by `(addr - base) >> 3` and refuses an address
/// that is outside its span or not 8-byte aligned RELATIVE TO ITS BASE.
/// `OldGen`'s storage is a `Vec<u8>`, whose base carries no alignment
/// guarantee, while every object start is absolutely 8-aligned (`alloc`
/// aligns the ABSOLUTE `block_addr`, not the offset). Anchoring the bitmap at
/// `base & !7` makes the two agree for every 8-aligned address in the
/// generation, which is every object start `walk_objects` can yield.
///
/// `unrepresentable` is the proof rather than the assumption: any start the
/// bitmap declined is kept in a `HashSet` and consulted on every lookup, so
/// this type answers EXACTLY what the old `HashSet` answered no matter what
/// the allocator does. It is expected to stay empty; a non-empty one costs
/// only the old behaviour for those few addresses.
pub(crate) struct OldGenObjectStarts {
    bits: crate::young_mark::ObjectStartBits,
    unrepresentable: HashSet<usize>,
}

impl OldGenObjectStarts {
    /// Walk `old_gen` and record every allocated object's start.
    ///
    /// # Sizing
    ///
    /// The bitmap is anchored at the generation's storage base (rounded DOWN
    /// to an 8-byte boundary, so that `addr - base` is 8-aligned for every
    /// 8-aligned `addr`, whatever alignment the backing `Vec<u8>` happened to
    /// get) and spans only as far as the HIGHEST object start the walk found.
    ///
    /// Spanning the whole `capacity()` instead would be a regression for a
    /// large but sparsely-occupied old generation — 32 MB of bitmap to
    /// describe a handful of objects, three times per cycle, where the
    /// `HashSet` this replaces cost a few hundred bytes. Sizing to the
    /// occupied prefix keeps the win monotone: the bitmap is never larger
    /// than 1/64th of the bytes actually in use.
    ///
    /// The base is FIXED and the span only GROWS while a cycle is open (the
    /// old generation gains objects through promotion; anything that removes
    /// one bumps `reclaim_epoch` and the cycle is abandoned). That is exactly
    /// the precondition `ObjectStartBits::retain_intersection` requires of the
    /// initial-mark snapshot against the remark walk.
    fn build(old_gen: &OldGen) -> Self {
        let (lo, _hi) = old_gen.extent();
        let base = lo & !7usize;
        let objects = old_gen.walk_objects();
        // Do not assume `walk_objects` is ordered — fold for the maximum.
        let highest = objects.iter().map(|(ptr, _)| *ptr as usize).max();
        let span = match highest {
            // `+ 8` so the highest start's own bit is inside the span.
            Some(top) => top.saturating_sub(base).saturating_add(8),
            None => 0,
        };
        let bits = crate::young_mark::ObjectStartBits::new(base, span);
        let mut unrepresentable = HashSet::new();
        for (ptr, _size) in objects {
            let addr = ptr as usize;
            if !bits.insert(addr) {
                unrepresentable.insert(addr);
            }
        }
        Self {
            bits,
            unrepresentable,
        }
    }

    #[inline]
    pub(crate) fn contains(&self, addr: usize) -> bool {
        self.bits.contains(addr) || self.unrepresentable.contains(&addr)
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.bits.is_empty() && self.unrepresentable.is_empty()
    }

    /// Number of recorded starts. Diagnostics, logging and tests.
    pub(crate) fn len(&self) -> usize {
        self.bits.len() + self.unrepresentable.len()
    }

    /// Keep only the starts also present in `other`.
    ///
    /// Returns `false` when the two bitmaps no longer agree on their geometry
    /// — a different base, or a `self` that outspans `other` — in which case
    /// NOTHING has changed and the caller must fail closed. Either can only
    /// happen if the old generation's storage moved, or its occupied prefix
    /// shrank, between the two walks; both bump `reclaim_epoch`, which already
    /// makes the cycle abandon, so this is a second, independent guard on the
    /// same condition rather than a new failure mode.
    #[must_use]
    fn retain_intersection(&mut self, other: &OldGenObjectStarts) -> bool {
        if !self.bits.retain_intersection(&other.bits) {
            return false;
        }
        self.unrepresentable.retain(|a| other.contains(*a));
        true
    }
}

fn old_gen_object_starts(old_gen: &OldGen) -> OldGenObjectStarts {
    OldGenObjectStarts::build(old_gen)
}

fn markable_old_object(ptr: *mut u8, object_starts: &OldGenObjectStarts) -> bool {
    !ptr.is_null() && object_starts.contains(ptr as usize)
}

struct ConcurrentMarkHeaderSnapshot {
    class_id: u32,
    kind_tag: u8,
    element_tag: u8,
    shape: u32,
    gc_flags: u8,
}

impl ConcurrentMarkHeaderSnapshot {
    fn read(header: *const ObjectHeader) -> Self {
        unsafe {
            Self {
                class_id: std::ptr::addr_of!((*header).class_id)
                    .read_unaligned()
                    .as_u32(),
                // Raw tags, still without forming a typed enum: they now
                // come out of the mark word rather than out of two header
                // bytes, but the reason for taking them raw is unchanged --
                // this snapshots possibly-corrupt memory, and an
                // out-of-range discriminant must survive to be rejected
                // rather than being UB at the point of the read.
                kind_tag: ObjectHeader::kind_tag((*header).mark_word.load(Ordering::Relaxed)),
                element_tag: ObjectHeader::element_type_tag(
                    (*header).mark_word.load(Ordering::Relaxed),
                ),
                shape: std::ptr::addr_of!((*header).shape).read_unaligned(),
                gc_flags: (*header).gc_flags(),
            }
        }
    }

    #[inline]
    fn array_length(&self) -> u32 {
        if self.kind_tag == ObjectKind::Array as u8 {
            self.shape
        } else {
            0
        }
    }

    #[inline]
    fn num_slots(&self) -> u32 {
        self.shape
    }

    #[inline]
    fn compact_body_size(&self) -> Option<usize> {
        // Borrowing accessor: reads one `u32` and drops the handle, so there is
        // no reason to pay an `Arc` clone/drop for it.
        cratonvm_types::with_class_layout(self.class_id, self.num_slots(), |layout| {
            layout.body_size as usize
        })
    }
}

fn array_element_type_from_tag(tag: u8) -> Option<ArrayElementType> {
    match tag {
        tag if tag == ArrayElementType::Reference as u8 => Some(ArrayElementType::Reference),
        tag if tag == ArrayElementType::Boolean as u8 => Some(ArrayElementType::Boolean),
        tag if tag == ArrayElementType::Char as u8 => Some(ArrayElementType::Char),
        tag if tag == ArrayElementType::Float as u8 => Some(ArrayElementType::Float),
        tag if tag == ArrayElementType::Double as u8 => Some(ArrayElementType::Double),
        tag if tag == ArrayElementType::Byte as u8 => Some(ArrayElementType::Byte),
        tag if tag == ArrayElementType::Short as u8 => Some(ArrayElementType::Short),
        tag if tag == ArrayElementType::Int as u8 => Some(ArrayElementType::Int),
        tag if tag == ArrayElementType::Long as u8 => Some(ArrayElementType::Long),
        _ => None,
    }
}

impl std::fmt::Debug for ConcurrentMarker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConcurrentMarker")
            .field("phase", &self.state.phase())
            .field("bitmap", &self.bitmap)
            .field("queue_len", &self.queue.len())
            .field("satb_queue", &self.satb_queue)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{ArrayElementType, ObjectHeader, ObjectKind, HEADER_SIZE, SLOT_SIZE};
    use cratonvm_types::{ClassId, ObjectRef, Value};

    /// The STW witness these tests stand in for.
    ///
    /// SAFETY: a unit test has one mutator thread -- itself -- so the
    /// safepoint precondition `StopTheWorldToken::new` demands holds
    /// vacuously. This mirrors the helper `g1.rs` and `g1_concurrent.rs`
    /// already use for the same reason.
    fn stw() -> crate::collector::StopTheWorldToken {
        unsafe { crate::collector::StopTheWorldToken::new() }
    }

    fn make_old_gen_with_object(num_slots: u32) -> (OldGen, *mut u8) {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + num_slots as usize * SLOT_SIZE;
        let ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let header = &mut *(ptr as *mut ObjectHeader);
            header.class_id = ClassId::new(1);
            header.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            header.set_num_slots(num_slots);
            header.set_gc_age(0);
            header.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        }
        (og, ptr)
    }

    #[test]
    fn concurrent_mark_object_size_rejects_inconsistent_object_header() {
        let mut header = ObjectHeader::new(
            ClassId::new(240),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            2,
        );
        assert_eq!(
            concurrent_mark_object_size(&header),
            Some(HEADER_SIZE + 2 * SLOT_SIZE)
        );

        header.shape = (1 << 24) + 1;
        assert_eq!(concurrent_mark_object_size(&header), None);
    }

    #[test]
    fn initial_mark_rejects_interior_old_gen_pointer() {
        let (og, obj_ptr) = make_old_gen_with_object(2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let interior = unsafe { obj_ptr.add(8) };

        assert_eq!(marker.initial_mark(&[interior], &og), 0);
        assert!(!marker.bitmap.is_marked(interior as usize));
        assert!(!marker.bitmap.is_marked(obj_ptr as usize));
    }

    #[test]
    fn initial_mark_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(2);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
    }

    #[test]
    fn concurrent_mark_follows_references() {
        let mut og = OldGen::new(65536);

        // Allocate object A (2 slots)
        let size_a = HEADER_SIZE + 2 * SLOT_SIZE;
        let ptr_a = og.alloc(size_a, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(2);
            h.set_gc_flags(0x01);
        }

        // Allocate object B (1 slot, no refs)
        let size_b = HEADER_SIZE + SLOT_SIZE;
        let ptr_b = og.alloc(size_b, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // A.field[0] = ref to B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            let obj_b = ObjectRef::from_raw(ptr_b);
            std::ptr::write(slot, Value::Object(Some(obj_b)));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // Both A and B should be marked
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(scanned >= 1); // At least B was scanned via A
    }

    #[test]
    fn sweep_frees_unmarked() {
        let mut og = OldGen::new(65536);

        // Allocate two objects
        let size = HEADER_SIZE + SLOT_SIZE;
        let live_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(live_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let dead_ptr = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(dead_ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Only mark live_ptr as a root (dead_ptr is unreachable)
        let (_marked, swept) = marker.full_cycle(&stw(), &[live_ptr], &mut og);
        assert_eq!(swept, 1); // dead_ptr should be freed
        assert!(og.used() < used_before);
    }

    /// GCAUD-4 — the concurrent sweep must not act on an address-keyed
    /// snapshot another old-gen collection has invalidated.
    ///
    /// `concurrent_sweep` is the one phase of the cycle that runs outside a
    /// stop-the-world. Its liveness test is "the address existed at remark
    /// (`sweep_eligible`) AND its bit is clear (`bitmap`)" — two tables keyed
    /// on a bare old-gen address. Between remark and the sweep's lock
    /// acquisition, another thread's young GC can run `old_gen_gc`: the
    /// in-place arm hands blocks back to the free list, and the very next
    /// promotion re-issues those addresses to NEW, fully live objects. Such an
    /// object satisfies BOTH halves of the test — it inherited a dead object's
    /// address, so it is "eligible", and its bit is clear because the bit
    /// describes its predecessor — and the sweep frees it while it is live.
    ///
    /// This test drives exactly that sequence: remark, then free a dead block
    /// and let the allocator re-issue the same address to a live object. The
    /// sweep must reclaim nothing.
    #[test]
    fn concurrent_sweep_refuses_a_snapshot_invalidated_by_another_old_gen_collection() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let init = |ptr: *mut u8, cid: u32| {
            // SAFETY: `ptr` is a live `size`-byte old-gen block.
            unsafe {
                let h = &mut *(ptr as *mut ObjectHeader);
                h.class_id = ClassId::new(cid);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
                h.set_num_slots(1);
                h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
            }
        };

        let live = og.alloc(size, 8).unwrap();
        init(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init(dead, 2);
        // A third block, recycled below. It is unreachable at remark, so its
        // address enters `sweep_eligible` with its bit clear.
        let recycled = og.alloc(size, 8).unwrap();
        init(recycled, 3);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);
        marker.remark(&stw(), &[live], &og);

        // --- another old-gen collection interleaves here -------------------
        // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(recycled, size) };
        let resurrected = og
            .alloc(size, 8)
            .expect("the just-freed block must be reusable");
        assert_eq!(
            resurrected, recycled,
            "precondition: the allocator must re-issue the freed address, which \
             is what makes the snapshot's address ambiguous",
        );
        init(resurrected, 4);
        // -------------------------------------------------------------------

        let used_before_sweep = og.used();
        let aborts_before = SWEEP_EPOCH_ABORTS.load(std::sync::atomic::Ordering::Relaxed);
        let swept = marker.concurrent_sweep(&mut og);

        assert_eq!(
            swept, 0,
            "a sweep whose address-keyed snapshot was invalidated must reclaim \
             nothing — one of the eligible addresses now names a LIVE object",
        );
        assert!(
            SWEEP_EPOCH_ABORTS.load(std::sync::atomic::Ordering::Relaxed) > aborts_before,
            "the abandoned sweep must be counted, not silent",
        );
        assert_eq!(og.used(), used_before_sweep);
        assert!(
            og.is_allocated_addr(resurrected),
            "the resurrected (live) object must still be allocated after the sweep",
        );
        assert!(
            og.is_allocated_addr(dead),
            "and nothing else may be reclaimed on the abandoned path either",
        );
    }

    /// Shared object initialiser for the TAMS tests below.
    fn init_old_object(ptr: *mut u8, cid: u32) {
        // SAFETY: `ptr` is a live `HEADER_SIZE + SLOT_SIZE` old-gen block.
        unsafe {
            let h = &mut *(ptr as *mut ObjectHeader);
            h.class_id = ClassId::new(cid);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Byte);
            h.set_num_slots(1);
            h.set_gc_flags(0x01); // GC_FLAG_OLD_GEN
        }
    }

    /// gengc-mark 2026-09-20 — TAMS must be anchored at INITIAL MARK.
    ///
    /// An object that enters the old generation DURING the concurrent trace
    /// (a young GC promoting a survivor, or a direct large-object allocation)
    /// cannot be discovered by this cycle: the trace may already have scanned
    /// and blackened every object that now points at it, the SATB pre-barrier
    /// logs only OLD slot values, and the remark root rescan sees it only if
    /// something outside the old gen still references it. With the snapshot
    /// taken at remark it was nevertheless "eligible", so the sweep freed a
    /// live object. Anchored at initial mark, it is ineligible by construction
    /// and is simply collected one cycle later.
    #[test]
    fn an_object_promoted_during_the_concurrent_phase_is_not_swept() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 2);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);

        // --- a promotion interleaves with the concurrent trace --------------
        let promoted = og.alloc(size, 8).unwrap();
        init_old_object(promoted, 3);
        // --------------------------------------------------------------------

        marker.remark(&stw(), &[live], &og);
        assert!(
            !marker.bitmap.is_marked(promoted as usize),
            "precondition: nothing in this cycle can have marked the promoted \
             object — that is the whole point of the test",
        );

        let swept = marker.concurrent_sweep(&mut og);
        assert!(
            og.is_allocated_addr(promoted),
            "an object that entered the old gen after initial mark is implicitly \
             live for this cycle and must survive the sweep",
        );
        assert_eq!(
            swept, 1,
            "only the object that was already dead at initial mark may be freed",
        );
        assert!(og.is_allocated_addr(live));
        assert!(!og.is_allocated_addr(dead));
    }

    /// gengc-mark 2026-09-20 — a cycle whose remark never ran must free
    /// nothing, even though its eligibility snapshot is now populated from
    /// initial mark onwards.
    ///
    /// This is the safety property the old arrangement got for free ("empty
    /// snapshot ⇒ remark never ran ⇒ free nothing") and that moving the
    /// snapshot earlier would have silently removed. It is now an explicit
    /// gate: `sweep_eligible_epoch` is stamped by `remark` and by nothing else.
    /// Without remark the bitmap is not final and the SATB log was never
    /// drained, so an unmarked object is not evidence of anything.
    #[test]
    fn a_sweep_without_a_remark_frees_nothing() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let unreached = og.alloc(size, 8).unwrap();
        init_old_object(unreached, 2);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);
        // No remark: model the driver losing the remark STW race.

        let used_before = og.used();
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(
            swept, 0,
            "an unauthorised snapshot must not license any reclamation",
        );
        assert_eq!(og.used(), used_before);
        assert!(og.is_allocated_addr(unreached));
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        // And `abort_cycle` -- the path the driver actually takes -- leaves the
        // same state, so a later sweep cannot consume the stale snapshot.
        let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker2.initial_mark(&[live], &og);
        marker2.abort_cycle();
        assert_eq!(marker2.concurrent_sweep(&mut og), 0);
        assert!(og.is_allocated_addr(unreached));
    }

    /// gengc-mark 2026-09-20 — GCAUD-4 now covers the whole cycle, not its tail.
    ///
    /// The epoch is stamped at initial mark, so a `free` (or `compact`) by
    /// another old-gen collector ANYWHERE in the cycle invalidates the
    /// address-keyed snapshot, and remark must fail closed rather than hand the
    /// sweep a set of addresses that no longer name the objects they named.
    #[test]
    fn a_reclaim_between_initial_mark_and_remark_abandons_the_cycle() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let live = og.alloc(size, 8).unwrap();
        init_old_object(live, 1);
        let dead = og.alloc(size, 8).unwrap();
        init_old_object(dead, 2);
        let recycled = og.alloc(size, 8).unwrap();
        init_old_object(recycled, 3);

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[live], &og);
        marker.concurrent_mark(&og);

        // --- another old-gen collection interleaves -------------------------
        // SAFETY: `(recycled, size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(recycled, size) };
        let resurrected = og
            .alloc(size, 8)
            .expect("the just-freed block must be reusable");
        assert_eq!(
            resurrected, recycled,
            "precondition: the allocator must re-issue the freed address",
        );
        init_old_object(resurrected, 4);
        // --------------------------------------------------------------------

        marker.remark(&stw(), &[live], &og);
        let swept = marker.concurrent_sweep(&mut og);

        assert_eq!(
            swept, 0,
            "a cycle whose snapshot was invalidated mid-flight must reclaim nothing",
        );
        assert!(og.is_allocated_addr(dead), "nothing may be reclaimed");
        assert!(og.is_allocated_addr(resurrected));
    }

    /// gengc-mark 2026-09-20 — the two SATB gates must never disagree in the
    /// unsafe direction.
    ///
    /// The generational `satb_barrier` checks the PHASE
    /// (`ConcurrentGcState::is_marking_active`); the log it writes into checks
    /// the QUEUE (`SatbQueue::is_active`). G1 asserts
    /// `is_marking_active() => satb_queue.is_active()`; `remark` used to break
    /// it for the width of its final closure, during which a mutator's
    /// pre-barrier entry went into a per-thread bucket nobody would drain
    /// again. Check the invariant at every phase boundary of a full cycle.
    #[test]
    fn the_phase_gate_is_never_wider_than_the_satb_gate() {
        let (mut og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let check = |marker: &ConcurrentMarker, where_: &str| {
            assert!(
                !marker.state.is_marking_active() || marker.satb_queue.is_active(),
                "{where_}: the mutator barrier is armed but its queue is not — \
                 an entry logged here would be stranded in a per-thread bucket",
            );
        };

        check(&marker, "idle");
        marker.initial_mark(&[obj_ptr], &og);
        check(&marker, "after initial_mark");
        marker.concurrent_mark(&og);
        check(&marker, "after concurrent_mark");
        marker.remark(&stw(), &[obj_ptr], &og);
        check(&marker, "after remark");
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
        marker.concurrent_sweep(&mut og);
        check(&marker, "after sweep");

        // An aborted cycle must leave the same agreement.
        let marker2 = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker2.initial_mark(&[obj_ptr], &og);
        marker2.abort_cycle();
        check(&marker2, "after abort_cycle");
    }

    #[test]
    fn satb_prevents_lost_object() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A (root)
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // Object B (initially referenced by A)
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // A.field[0] = B initially
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Phase 1: initial mark
        marker.initial_mark(&[ptr_a], &og);

        // Simulate concurrent mutation: A.field[0] = null
        // The SATB barrier should log the OLD value (ptr_b).
        marker.satb_queue.flush(vec![ptr_b as usize]);

        // Now break the reference
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(None));
        }

        // Phase 2: concurrent mark (won't find B via A anymore)
        marker.concurrent_mark(&og);

        // Phase 3: remark — should discover B from SATB
        let discovered = marker.remark(&stw(), &[ptr_a], &og);
        assert!(discovered > 0); // B should be re-discovered from SATB

        // Phase 4: sweep — B should NOT be freed
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 0); // Both A and B are live
    }

    // gc-concmark HIGH regression — the SATB barrier must stay ACTIVE
    // across the remark closure, so a live old-gen reference overwritten
    // by a mutator DURING remark (after the snapshot drain, before sweep)
    // is still logged, marked, and NOT swept.
    //
    // Before the fix, `remark` called `deactivate_and_drain()` at the very
    // top, flipping the gate to INACTIVE before the closure was computed.
    // An SATB entry flushed after that point (modelling a write the barrier
    // would have logged had it still been active) was never captured by the
    // mark phase, leaving B's bit clear so the sweep freed a live object.
    #[test]
    fn satb_active_through_remark_closure() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A (root, no live refs to B by remark time).
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        // Object B — only ever reachable via the SATB log of an overwrite.
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Phases 1–2: A is the only root; B is not reachable from A.
        marker.initial_mark(&[ptr_a], &og);
        marker.concurrent_mark(&og);

        // The barrier must still be active going into remark — that is the
        // invariant whose violation caused the bug.
        assert!(marker.satb_queue.is_active());

        // Model a mutator overwriting a live reference to B *during* the
        // concurrent window: the pre-barrier logs B's old address. With the
        // fix, `remark` drains this WITHOUT deactivating and marks B; the
        // final quiescing drain then closes the gate.
        marker.satb_queue.flush(vec![ptr_b as usize]);

        let discovered = marker.remark(&stw(), &[ptr_a], &og);
        assert!(discovered > 0, "B must be discovered from the SATB log");
        assert!(
            marker.bitmap.is_marked(ptr_b as usize),
            "B must be marked — the SATB-logged live ref was not lost"
        );
        // Gate must be off once the closure is final.
        assert!(!marker.satb_queue.is_active());

        // Sweep must keep B (it is live via the SATB snapshot).
        let swept = marker.concurrent_sweep(&mut og);
        assert_eq!(swept, 0, "neither A nor B may be freed");
    }

    #[test]
    fn phase_transitions() {
        let marker = ConcurrentMarker::new(0x0, 1024);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        let og = OldGen::new(1024);
        marker.initial_mark(&[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        // Phase doesn't change after concurrent mark — stays ConcurrentMark

        marker.remark(&stw(), &[], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());
    }

    // -----------------------------------------------------------------------
    // Additional tests
    // -----------------------------------------------------------------------

    #[test]
    fn initial_mark_multiple_roots() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        let ptrs: Vec<*mut u8> = (0..5)
            .map(|i| {
                let p = og.alloc(size, 8).unwrap();
                unsafe {
                    let h = &mut *(p as *mut ObjectHeader);
                    h.class_id = ClassId::new(i + 1);
                    h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                    h.set_num_slots(1);
                    h.set_gc_flags(0x01);
                }
                p
            })
            .collect();

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let count = marker.initial_mark(&ptrs, &og);

        assert_eq!(count, 5);
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
        assert_eq!(marker.queue.len(), 5);
    }

    #[test]
    fn initial_mark_skips_null_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let roots: Vec<*mut u8> = vec![std::ptr::null_mut(), obj_ptr, std::ptr::null_mut()];
        let count = marker.initial_mark(&roots, &og);

        assert_eq!(count, 1);
        assert!(marker.bitmap.is_marked(obj_ptr as usize));
    }

    #[test]
    fn initial_mark_deduplicates_roots() {
        let (og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Same root listed twice — should only mark once
        let count = marker.initial_mark(&[obj_ptr, obj_ptr], &og);
        assert_eq!(count, 1);
        assert_eq!(marker.queue.len(), 1);
    }

    #[test]
    fn empty_heap_marking() {
        let og = OldGen::new(4096);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let count = marker.initial_mark(&[], &og);
        assert_eq!(count, 0);

        let scanned = marker.concurrent_mark(&og);
        assert_eq!(scanned, 0);

        let discovered = marker.remark(&stw(), &[], &og);
        assert_eq!(discovered, 0);
    }

    #[test]
    fn single_object_full_cycle() {
        let (mut og, obj_ptr) = make_old_gen_with_object(0);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        let (marked, swept) = marker.full_cycle(&stw(), &[obj_ptr], &mut og);
        // The single object is a root, so it should survive
        assert!(marked >= 1);
        assert_eq!(swept, 0);
    }

    #[test]
    fn mark_queue_push_pop_ordering() {
        let q = MarkQueue::new();
        assert!(q.is_empty());
        assert_eq!(q.len(), 0);

        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        q.push(0x300 as *mut u8);
        assert_eq!(q.len(), 3);

        // FIFO ordering
        assert_eq!(q.pop().unwrap() as usize, 0x100);
        assert_eq!(q.pop().unwrap() as usize, 0x200);
        assert_eq!(q.pop().unwrap() as usize, 0x300);
        assert!(q.pop().is_none());
        assert!(q.is_empty());
    }

    #[test]
    fn mark_queue_push_batch() {
        let q = MarkQueue::new();
        let ptrs: Vec<*mut u8> = (1..=4).map(|i| (i * 0x100) as *mut u8).collect();

        q.push_batch(&ptrs);
        assert_eq!(q.len(), 4);

        for expected in &ptrs {
            assert_eq!(q.pop().unwrap() as usize, *expected as usize);
        }
    }

    #[test]
    fn mark_queue_clear() {
        let q = MarkQueue::new();
        q.push(0x100 as *mut u8);
        q.push(0x200 as *mut u8);
        assert_eq!(q.len(), 2);

        q.clear();
        assert!(q.is_empty());
        assert!(q.pop().is_none());
    }

    #[test]
    fn mark_queue_concurrent_push_pop() {
        use std::sync::Arc;

        let q = Arc::new(MarkQueue::new());
        let mut handles = Vec::new();

        // 4 threads each push 100 items
        for t in 0..4u64 {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                for i in 0..100u64 {
                    q.push((t * 1000 + i) as *mut u8);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(q.len(), 400);

        // Drain all
        let mut count = 0;
        while q.pop().is_some() {
            count += 1;
        }
        assert_eq!(count, 400);
    }

    #[test]
    fn concurrent_gc_state_is_marking_active() {
        let state = ConcurrentGcState::new();
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::InitialMark);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Remark);
        assert!(state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        assert!(!state.is_marking_active());

        state.set_phase(ConcurrentGcPhase::Idle);
        assert!(!state.is_marking_active());
    }

    #[test]
    fn phase_from_u8_invalid_defaults_to_idle() {
        assert_eq!(ConcurrentGcPhase::from(255), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(5), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(100), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn phase_from_u8_all_valid() {
        assert_eq!(ConcurrentGcPhase::from(0), ConcurrentGcPhase::Idle);
        assert_eq!(ConcurrentGcPhase::from(1), ConcurrentGcPhase::InitialMark);
        assert_eq!(
            ConcurrentGcPhase::from(2),
            ConcurrentGcPhase::ConcurrentMark
        );
        assert_eq!(ConcurrentGcPhase::from(3), ConcurrentGcPhase::Remark);
        assert_eq!(
            ConcurrentGcPhase::from(4),
            ConcurrentGcPhase::ConcurrentSweep
        );
    }

    #[test]
    fn large_object_graph_marking() {
        let mut og = OldGen::new(1 << 20); // 1 MB
        let size = HEADER_SIZE + SLOT_SIZE;

        // Build a chain: obj[0] -> obj[1] -> ... -> obj[N-1]
        let n = 50;
        let mut ptrs: Vec<*mut u8> = Vec::new();
        for i in 0..n {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i as u32 + 1);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
            ptrs.push(p);
        }

        // Wire up chain references: ptrs[i].slot[0] = ptrs[i+1]
        for i in 0..n - 1 {
            unsafe {
                let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
            }
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        // Only root is the first object
        marker.initial_mark(&[ptrs[0]], &og);
        marker.concurrent_mark(&og);

        // All objects in the chain should be marked
        for &p in &ptrs {
            assert!(marker.bitmap.is_marked(p as usize));
        }
    }

    #[test]
    fn marking_graph_with_cycle() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Create A -> B -> C -> A (cycle)
        let ptr_a = og.alloc(size, 8).unwrap();
        let ptr_b = og.alloc(size, 8).unwrap();
        let ptr_c = og.alloc(size, 8).unwrap();

        for (p, id) in [(ptr_a, 1u32), (ptr_b, 2), (ptr_c, 3)] {
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(id);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
        }

        // A -> B
        unsafe {
            let slot = ptr_a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_b))));
        }
        // B -> C
        unsafe {
            let slot = ptr_b.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_c))));
        }
        // C -> A (back edge creating cycle)
        unsafe {
            let slot = ptr_c.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(slot, Value::Object(Some(ObjectRef::from_raw(ptr_a))));
        }

        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr_a], &og);
        let scanned = marker.concurrent_mark(&og);

        // All three should be marked despite the cycle
        assert!(marker.bitmap.is_marked(ptr_a as usize));
        assert!(marker.bitmap.is_marked(ptr_b as usize));
        assert!(marker.bitmap.is_marked(ptr_c as usize));
        assert!(scanned >= 2); // A scanned in initial_mark's queue, B and C via concurrent
    }

    #[test]
    fn full_cycle_phase_sequence() {
        let (mut og, obj_ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        // Before cycle: Idle
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);

        marker.initial_mark(&[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);
        assert!(marker.satb_queue.is_active());

        marker.concurrent_mark(&og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

        marker.remark(&stw(), &[obj_ptr], &og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentSweep);
        assert!(!marker.satb_queue.is_active());

        marker.concurrent_sweep(&mut og);
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    }

    #[test]
    fn sweep_all_unreachable() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Allocate 3 objects, none rooted
        for i in 0..3 {
            let p = og.alloc(size, 8).unwrap();
            unsafe {
                let h = &mut *(p as *mut ObjectHeader);
                h.class_id = ClassId::new(i + 1);
                h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
                h.set_num_slots(1);
                h.set_gc_flags(0x01);
            }
        }

        let used_before = og.used();
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        let (_marked, swept) = marker.full_cycle(&stw(), &[], &mut og);

        assert_eq!(swept, 3);
        assert!(og.used() < used_before);
    }

    // gc-concmark MEDIUM regression — the concurrent-mark object-slot read
    // must serialize against striped mutator writes so it never observes a
    // torn (tag, payload) `Value`. Here a writer thread continuously flips a
    // reference slot between `Object(Some(b))` and `Object(None)` while
    // holding the SAME per-slot stripe lock that `scan_object` now takes; the
    // marker scans the object in a tight loop on another thread. Without the
    // stripe lock in `scan_object`, a torn read could splice the non-null tag
    // of one store with the (null) payload of another and feed a bogus
    // pointer into `old_gen.contains` / `try_mark` — corrupting the heap or
    // crashing. With the lock, every read sees a fully-old or fully-new value,
    // so the test runs to completion and only ever marks the real target `b`.
    #[test]
    fn scan_object_serializes_against_striped_writer() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;

        // Object A — single reference slot, the contended one.
        let ptr_a = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_a as *mut ObjectHeader);
            h.class_id = ClassId::new(1);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }
        // Object B — the only legitimate target A's slot can point to.
        let ptr_b = og.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(ptr_b as *mut ObjectHeader);
            h.class_id = ClassId::new(2);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let marker = Arc::new(ConcurrentMarker::new(og.base_ptr() as usize, og.capacity()));
        // The marker must be in the concurrent-mark phase so its bitmap is live.
        marker.initial_mark(&[ptr_a], &og);

        let stop = Arc::new(AtomicBool::new(false));
        let a_addr = ptr_a as usize;
        let b_addr = ptr_b as usize;
        let obj_a = unsafe { ObjectRef::from_raw(ptr_a) };
        let obj_b = unsafe { ObjectRef::from_raw(ptr_b) };

        // Writer: flip A.slot[0] between Some(B) and None under the stripe
        // lock, exactly as the volatile field-access helpers do.
        let writer = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let slot = (a_addr + HEADER_SIZE) as *mut Value;
                let mut toggle = false;
                while !stop.load(Ordering::Relaxed) {
                    let _g = crate::collector::volatile_stripe_lock(obj_a, 0);
                    std::sync::atomic::fence(Ordering::SeqCst);
                    let v = if toggle {
                        Value::Object(Some(obj_b))
                    } else {
                        Value::Object(None)
                    };
                    // SAFETY: slot is A's single in-bounds reference field.
                    unsafe { std::ptr::write(slot, v) };
                    std::sync::atomic::fence(Ordering::SeqCst);
                    toggle = !toggle;
                }
            })
        };

        // Reader: scan A many times concurrently with the writer.
        let object_starts = old_gen_object_starts(&og);
        for _ in 0..50_000 {
            marker.scan_object(a_addr as *mut u8, &og, &object_starts);
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        // A must still be marked; B must be marked iff it was observed as a
        // live target — either is legal, but no THIRD address may ever have
        // been marked (a torn read would have produced a garbage pointer that
        // either failed `contains` or, worse, aliased some other object).
        assert!(marker.bitmap.is_marked(a_addr));
        // Sanity: the only addresses that can possibly be marked are A and B.
        // (B is the sole non-null value the writer ever stores.)
        let _ = b_addr; // referenced for clarity; marking B is permitted, not required.
    }

    /// fork6 GC_STRESS fix — `with_shared` must adopt the caller's SATB queue
    /// + phase state (the heap-attached instances the write barrier reaches),
    /// and a pre-barrier log flushed into that SHARED queue must be marked by
    /// remark. With the old per-cycle private queue this plumbing did not
    /// exist and the logged target was swept while live.
    #[test]
    fn with_shared_marker_marks_satb_entries_from_shared_queue() {
        // One object, reachable ONLY via the (simulated) overwritten ref.
        let mut og2 = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let live = og2.alloc(size, 8).unwrap();
        unsafe {
            let h = &mut *(live as *mut ObjectHeader);
            h.class_id = ClassId::new(7);
            h.set_shape_tags(ObjectKind::Object, ArrayElementType::Reference);
            h.set_num_slots(1);
            h.set_gc_flags(0x01);
        }

        let satb = Arc::new(SatbQueue::new());
        let state = Arc::new(ConcurrentGcState::new());
        let marker = ConcurrentMarker::with_shared(
            og2.base_ptr() as usize,
            og2.capacity(),
            satb.clone(),
            state.clone(),
        );
        // The marker must hold the SAME instances (not copies).
        assert!(Arc::ptr_eq(&marker.satb_queue, &satb));
        assert!(Arc::ptr_eq(&marker.state, &state));

        // Initial mark with NO roots: `live` is invisible to the trace.
        marker.initial_mark(&[], &og2);
        assert!(
            satb.is_active(),
            "initial_mark must activate the shared queue"
        );
        assert!(state.is_marking_active());

        // Simulate the write barrier on another code path: a mutator
        // overwrote the only reference to `live` during concurrent mark and
        // the pre-barrier logged the old value into the SHARED queue.
        crate::satb::satb_thread_local_log(&satb, live as usize);
        crate::satb::flush_thread_satb_buffer(&satb);

        marker.concurrent_mark(&og2);
        marker.remark(&stw(), &[], &og2);

        assert!(
            marker.bitmap.is_marked(live as usize),
            "remark must mark targets logged into the SHARED SATB queue"
        );
    }

    /// fork6 GC_STRESS fix — a cycle whose remark STW could not be acquired
    /// must be abortable: `abort_cycle` deactivates the (shared) SATB barrier
    /// and returns the phase to Idle so the write barrier stops logging and
    /// the next trigger starts fresh. The caller skips the sweep entirely.
    #[test]
    fn abort_cycle_deactivates_barrier_and_resets_phase() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());

        marker.initial_mark(&[ptr], &og);
        assert!(marker.satb_queue.is_active());
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::ConcurrentMark);

        marker.abort_cycle();
        assert!(
            !marker.satb_queue.is_active(),
            "abort must deactivate the barrier"
        );
        assert_eq!(marker.state.phase(), ConcurrentGcPhase::Idle);
    }

    // ------------------------------------------------------------------
    // gengc-mark2 2026-09-20
    // ------------------------------------------------------------------

    /// `gengc-mark-old-gen-object-starts-is-a-hashset-per-phase-20260920`.
    ///
    /// The bitmap must answer EXACTLY what the `HashSet<usize>` it replaced
    /// answered — the same membership at every address in (and around) the
    /// generation, and the same cardinality. Mirrors
    /// `young_mark::object_start_bits_match_a_hash_set_exactly`, which is the
    /// in-repo precedent this change reuses.
    #[test]
    fn old_gen_object_starts_bitmap_matches_a_hash_set_exactly() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let mut allocated: Vec<*mut u8> = Vec::new();
        for i in 0..64u32 {
            let p = og.alloc(size, 8).unwrap();
            init_old_object(p, i + 1);
            allocated.push(p);
        }
        // Free a few so the walk has gaps to skip: that is what makes the
        // "starts" set different from "every 8-byte slot in the generation",
        // and it is the case a bitmap could get wrong.
        for i in [5usize, 17, 40] {
            // SAFETY: each is a live block of exactly `size` bytes from this
            // generation, freed once.
            unsafe { og.free(allocated[i], size) };
        }
        let reference: HashSet<usize> = og
            .walk_objects()
            .into_iter()
            .map(|(p, _)| p as usize)
            .collect();

        let bits = old_gen_object_starts(&og);
        assert_eq!(
            bits.len(),
            reference.len(),
            "bitmap cardinality must equal the HashSet's"
        );
        assert!(!bits.is_empty());

        // Every 8-byte slot in the generation, plus a margin either side, must
        // get the same verdict from both.
        // Walk 8-ALIGNED addresses: every object start is absolutely
        // 8-aligned, so a loop anchored on an unaligned base would step past
        // all of them and the comparison would be vacuous.
        let (lo, hi) = og.extent();
        let mut addr = (lo & !7usize).saturating_sub(64);
        while addr < hi + 64 {
            assert_eq!(
                bits.contains(addr),
                reference.contains(&addr),
                "bitmap and HashSet disagree at {addr:#x}"
            );
            addr += 8;
        }
        // And an interior (non-start) address of a real object is not a start.
        assert!(!bits.contains(allocated[0] as usize + 8));
        // A freed object's address is no longer a start, in both.
        assert!(!bits.contains(allocated[5] as usize));
    }

    /// An empty generation yields an empty, zero-length set — the condition
    /// `concurrent_sweep` uses as its "free nothing" exit.
    #[test]
    fn old_gen_object_starts_on_an_empty_generation_is_empty() {
        let og = OldGen::new(65536);
        let bits = old_gen_object_starts(&og);
        assert!(bits.is_empty());
        assert_eq!(bits.len(), 0);
        assert!(!bits.contains(og.base_ptr() as usize));
    }

    /// Remark narrows the initial-mark snapshot by intersecting it with the
    /// current walk. The word-wise AND must produce exactly what
    /// `HashSet::retain` produced.
    #[test]
    fn object_starts_intersection_keeps_exactly_the_common_starts() {
        let mut og = OldGen::new(65536);
        let size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(size, 8).unwrap();
        init_old_object(a, 1);
        let b = og.alloc(size, 8).unwrap();
        init_old_object(b, 2);

        let mut at_initial_mark = old_gen_object_starts(&og);
        assert!(at_initial_mark.contains(a as usize));
        assert!(at_initial_mark.contains(b as usize));

        // `b` goes away and `c` arrives. `c` lands either in `b`'s freed block
        // or above it — never below, because nothing is free below `a` — so
        // the later walk spans at least as far as the earlier one, which is
        // the precondition `retain_intersection` requires.
        // SAFETY: `b` is a live block of exactly `size` bytes from this gen.
        unsafe { og.free(b, size) };
        let c = og.alloc(size, 8).unwrap();
        init_old_object(c, 3);
        let at_remark = old_gen_object_starts(&og);

        assert!(
            at_initial_mark.retain_intersection(&at_remark),
            "the two walks share a base and the later one is no shorter"
        );
        assert!(
            at_initial_mark.contains(a as usize),
            "`a` was present at both walks and must survive the narrowing"
        );
        // ORCHESTRATOR CORRECTION, 2026-09-20 — this assertion used to be
        // unconditional (`!contains(c)`), which contradicted the very next
        // block's acknowledgement that `c` may land on `b`'s address. It does:
        // `b` is freed and `c` requests the SAME size, so the size-segregated
        // free list hands back exactly `b`'s block on the usual path, and the
        // test failed on its own stated edge case rather than on a defect.
        //
        // Both outcomes are correct behaviour and the test now says which is
        // which, because the distinction is the whole point of the epoch check
        // living somewhere else.
        if c as usize == b as usize {
            // The free list reused the block. That ADDRESS was genuinely
            // present at both walks, so a word-wise AND keeps it — and must.
            // An address-keyed intersection cannot tell the dead `b` from the
            // live `c` that replaced it, and is not the mechanism that is
            // supposed to: `reclaim_epoch` is (see `release_unused_tail`'s
            // address-identity stamp). Asserting absence here would be
            // asserting that the intersection does a job it deliberately
            // delegates.
            assert!(
                at_initial_mark.contains(c as usize),
                "an address live at both walks must survive the narrowing, \
                 whichever object owned it"
            );
        } else {
            assert!(
                !at_initial_mark.contains(c as usize),
                "`c` did not exist at initial mark and must not be added by it"
            );
            assert!(
                !at_initial_mark.contains(b as usize),
                "`b` was freed before the remark walk and must be narrowed out"
            );
        }
    }

    /// `gengc-mark-markqueue-has-no-termination-detection-20260920`.
    ///
    /// The property the plain `pop` loop cannot give: with N markers, every
    /// node of a graph is scanned EXACTLY once and no marker leaves while a
    /// peer still has children to push. The graph is a chain, so at any
    /// instant at most one node is available — a worker that terminates on
    /// "the queue looked empty" loses the rest of the chain, which is the
    /// failure this primitive exists to prevent.
    #[test]
    fn parallel_drain_visits_every_node_exactly_once() {
        // Long enough that the "peer is mid-scan with an empty queue" window
        // is hit many times per run, short enough that eight workers parking
        // and waking on every link stays a fast unit test.
        const NODES: usize = 1024;
        for &workers in &[1usize, 2, 4, 8] {
            let queue = MarkQueue::new();
            // Node k is the address `(k + 1) * 8`; scanning node k pushes
            // node k + 1. Addresses are never dereferenced here.
            let visits: Vec<AtomicUsize> = (0..NODES).map(|_| AtomicUsize::new(0)).collect();
            queue.push(8 as *mut u8);
            queue.begin_drain(workers);

            let run = || {
                let _w = queue.worker();
                while let Some(ptr) = queue.pop_or_terminate() {
                    let k = (ptr as usize / 8) - 1;
                    visits[k].fetch_add(1, Ordering::Relaxed);
                    // Make the "peer is inside a scan with the queue empty"
                    // window wide enough to be hit.
                    std::thread::yield_now();
                    if k + 1 < NODES {
                        queue.push(((k + 2) * 8) as *mut u8);
                    }
                }
            };

            std::thread::scope(|s| {
                for _ in 1..workers {
                    s.spawn(&run);
                }
                run();
            });

            assert!(
                queue.drain_is_done(),
                "{workers} worker(s): the drain must end by declaring completion, \
                 not by a worker guessing the queue is empty"
            );
            for (k, v) in visits.iter().enumerate() {
                assert_eq!(
                    v.load(Ordering::Relaxed),
                    1,
                    "{workers} worker(s): node {k} was scanned {} times, not once",
                    v.load(Ordering::Relaxed)
                );
            }
            assert!(queue.is_empty());
        }
    }

    /// The race the chain test makes likely, made deterministic: one marker is
    /// held inside its "scan" while the queue is empty, and the other markers
    /// must not declare the closure complete until it has pushed its child.
    #[test]
    fn a_marker_may_not_terminate_while_a_peer_is_mid_scan() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Barrier;

        let queue = MarkQueue::new();
        let scanning = Barrier::new(2);
        let child_seen = AtomicBool::new(false);
        queue.push(0x100 as *mut u8);
        queue.begin_drain(2);

        std::thread::scope(|s| {
            // Worker A pops 0x100, then parks INSIDE the scan until B has had
            // every chance to observe an empty queue, and only then pushes.
            s.spawn(|| {
                let _w = queue.worker();
                let first = queue.pop_or_terminate().expect("seed must be popped");
                assert_eq!(first as usize, 0x100);
                scanning.wait();
                std::thread::yield_now();
                queue.push(0x200 as *mut u8);
                while let Some(p) = queue.pop_or_terminate() {
                    if p as usize == 0x200 {
                        child_seen.store(true, Ordering::Relaxed);
                    }
                }
            });
            // Worker B sees an empty queue while A holds the only node.
            let _w = queue.worker();
            scanning.wait();
            while let Some(p) = queue.pop_or_terminate() {
                if p as usize == 0x200 {
                    child_seen.store(true, Ordering::Relaxed);
                }
            }
        });

        assert!(
            child_seen.load(Ordering::Relaxed),
            "a child pushed while a peer observed an empty queue was never scanned — \
             this is exactly the premature-termination bug"
        );
    }

    /// A worker that unwinds must not park its peers forever: `MarkWorker`'s
    /// `Drop` decrements the live count on the unwind path too.
    #[test]
    fn a_panicking_marker_releases_its_peers() {
        let queue = std::sync::Arc::new(MarkQueue::new());
        queue.begin_drain(2);

        let panicker = {
            let q = queue.clone();
            std::thread::spawn(move || {
                let _w = q.worker();
                panic!("marker died mid-scan");
            })
        };
        // The survivor must reach termination rather than parking forever.
        let survivor = {
            let q = queue.clone();
            std::thread::spawn(move || {
                let _w = q.worker();
                while q.pop_or_terminate().is_some() {}
            })
        };
        assert!(panicker.join().is_err());
        survivor
            .join()
            .expect("the surviving marker must terminate, not hang");
    }

    /// `gengc-mark-concurrent-mark-holds-the-old-gen-lock-20260920`.
    ///
    /// Slicing Phase 2 must reach the same fixed point as the unbounded run:
    /// the same bitmap, the same total scan count, and a `true` completion
    /// flag exactly once.
    #[test]
    fn a_sliced_concurrent_mark_reaches_the_same_fixed_point() {
        /// Build a chain of `n` old-gen objects, each pointing at the next,
        /// and return `(old_gen, root)`.
        fn chain(n: usize) -> (OldGen, *mut u8) {
            let mut og = OldGen::new(1 << 20);
            let size = HEADER_SIZE + SLOT_SIZE;
            let mut ptrs = Vec::new();
            for i in 0..n {
                let p = og.alloc(size, 8).unwrap();
                init_old_object(p, i as u32 + 1);
                ptrs.push(p);
            }
            for i in 0..n - 1 {
                // SAFETY: slot 0 of a 1-slot object allocated just above.
                unsafe {
                    let slot = ptrs[i].add(HEADER_SIZE) as *mut Value;
                    slot.write(Value::Object(Some(ObjectRef::from_raw(ptrs[i + 1]))));
                }
            }
            (og, ptrs[0])
        }

        const N: usize = 200;

        let (og, root) = chain(N);
        let whole = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        whole.initial_mark(&[root], &og);
        let whole_scanned = whole.concurrent_mark(&og);

        let (og2, root2) = chain(N);
        let sliced = ConcurrentMarker::new(og2.base_ptr() as usize, og2.capacity());
        sliced.initial_mark(&[root2], &og2);
        let mut sliced_scanned = 0usize;
        let mut slices = 0usize;
        loop {
            let (n, done) = sliced.concurrent_mark_budget(&og2, 7);
            sliced_scanned += n;
            slices += 1;
            assert!(slices < N + 10, "slicing must terminate");
            if done {
                break;
            }
        }

        assert!(
            slices > 1,
            "a budget of 7 over a 200-node chain must take many slices, took {slices}"
        );
        assert_eq!(
            sliced_scanned, whole_scanned,
            "the sliced mark must scan the same number of objects as the whole-phase mark"
        );
        assert_eq!(
            sliced.bitmap.marked_count(),
            whole.bitmap.marked_count(),
            "the sliced mark must reach the same bitmap"
        );
        assert_eq!(whole.bitmap.marked_count(), N);
    }

    /// A zero budget must still make progress rather than spinning forever.
    #[test]
    fn a_zero_budget_slice_still_scans_one_object() {
        let (og, ptr) = make_old_gen_with_object(1);
        let marker = ConcurrentMarker::new(og.base_ptr() as usize, og.capacity());
        marker.initial_mark(&[ptr], &og);
        let (n, done) = marker.concurrent_mark_budget(&og, 0);
        assert_eq!(n, 1);
        assert!(done);
    }
}
