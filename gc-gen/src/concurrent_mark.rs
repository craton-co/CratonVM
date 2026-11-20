// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

use crate::old_gen::OldGen;
pub use cratonvm_gc_core::concurrent_mark::*;
use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex, RwLock};
use rustc_hash::FxHashMap;

use cratonvm_gc_core::heap::{
    array_data_size, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    GC_FLAG_COMPACT, GC_FLAG_HEADER, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use cratonvm_gc_core::mark_bitmap::MarkBitmap;
use cratonvm_gc_core::satb::SatbQueue;
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::Value;

// ---------------------------------------------------------------------------
// Concurrent GC phase tracking
// ---------------------------------------------------------------------------

















// ---------------------------------------------------------------------------
// gen r4w4/concmark4 — the old-generation collection policy
// ---------------------------------------------------------------------------








/// gci i42/mk (2026-10-16, `gci-i42mk-gen-remark-judges-death-before-thread-satb-buckets-FIXED-20261016`)
/// — `CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH` (DEFAULT ON, `=0` kills;
/// token `gen-conc-remark-thread-satb-flush`): [`ConcurrentMarker::remark_begin`]
/// pulls every registered thread's partially-full SATB bucket into the shards
/// BEFORE its first closure, as G1's `remark` does (finding #18), so the
/// dead-finalizer retention and the reference-processing seam -- both of which
/// judge "unmarked and eligible" as dead at that first fixed point -- see the
/// whole snapshot. Off: the buckets reach the bitmap only through
/// `remark_finish`'s `deactivate_and_drain`, AFTER both verdicts. Read once
/// per remark.
pub const GEN_CONC_REMARK_THREAD_SATB_FLUSH_FLAG: &str =
    "CRATONVM_GEN_CONC_REMARK_THREAD_SATB_FLUSH";

/// Is [`GEN_CONC_REMARK_THREAD_SATB_FLUSH_FLAG`] on?
pub fn gen_conc_remark_thread_satb_flush_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on(GEN_CONC_REMARK_THREAD_SATB_FLUSH_FLAG)
}


/// gen r4w4/concmark4 (2026-09-24) — which old-generation collection policy
/// the generational heap runs. Read from the process flags at each decision
/// (`CRATONVM_GC_NO_CONCURRENT_FIRST`; a tuning knob, not compatibility state).
///
/// # `Legacy` — the pre-2026-09-24 behaviour, kept as the A/B arm
///
/// One trigger, `OldGen::major_trigger_verdict` (the 75 % floor, with the
/// growth hysteresis only when `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` is set),
/// asked by both the STW old-gen collection inside the young pause and the
/// concurrent cycle right after it. Above the floor the two duplicate each
/// other, and with several mutators the next pause's STW collection makes the
/// open cycle stale
/// (`docs/feature-designs/generative-gc-architecture.md`).
///
/// # `ConcurrentFirst` — the default
///
/// Two thresholds and a deferral, HotSpot G1's shape (IHOP below the
/// full-collection point):
///
/// * The concurrent cycle starts at an initiating occupancy `T` BELOW the STW
///   floor `F = 75 % of C` ([`ConcurrentStartPolicy::threshold`]):
///
///   ```text
///   T = clamp(F − (5/4 · G + C/32),  C · 20 %,  F)
///   ```
///
///   where `G` is the old-gen growth one cycle is predicted to see: promotion
///   rate × cycle duration, MEASURED as one number (old-gen bytes allocated
///   between a cycle's initial mark and its sweep's end), smoothed by an EWMA
///   (α = ½). Until the first cycle is measured `T = 45 % of C`. The product
///   is measured rather than composed from a rate and a duration because the
///   cycle runs on a mutator thread: the promotion rate DURING a cycle is not
///   the rate between cycles (single-threaded, it is zero), and a composed
///   `rate × duration` would start every single-threaded cycle far too early.
/// * The STW old-gen collection DEFERS to an open cycle unless it was
///   requested (`System.gc()`), an old-gen allocation has failed since the
///   last collection (the OOM ladder must never wait), or the generation is at
///   [`CONC_DEFER_CEILING_PCT`]. When it runs anyway the cycle could not
///   finish in time, and the predicted growth is widened to
///   `max(G, 2 · growth so far, C/8)` for the next start.
/// * The growth hysteresis is folded in: the concurrent START always uses it
///   (a cycle does not reopen over a generation its last collection left
///   above `T` until it has grown, [`ConcurrentStartPolicy::start_due`]), and
///   the STW arm's default is one match arm in [`Self::hysteresis`] — still
///   OFF, pending the orchestrator's A/B.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OldGenPolicy {
    Legacy,
    ConcurrentFirst,
}

impl OldGenPolicy {
    /// This process's policy.
    pub fn current() -> Self {
        Self::from_opt_out(cratonvm_gc_core::gc_flags().old_no_concurrent_first)
    }

    /// `CRATONVM_GC_NO_CONCURRENT_FIRST` set → [`Self::Legacy`].
    pub fn from_opt_out(no_concurrent_first: bool) -> Self {
        if no_concurrent_first {
            Self::Legacy
        } else {
            Self::ConcurrentFirst
        }
    }

    /// The STW trigger's effective growth hysteresis. An explicit
    /// `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` (`=1` on, `=0` off) wins; unset,
    /// each policy has its own default: OFF under `Legacy`, ON under
    /// `ConcurrentFirst` since gen r4w5 (2026-09-24).
    ///
    /// The flip's evidence (count-based, so host load cannot move it):
    /// `GenR4W5MajorCadenceProbe`, `--nojit -Xmx256m`, 545 young decisions.
    /// Hysteresis OFF: 518 STW majors (95 per 100 young), 517 back-to-back,
    /// all 518 freeing < 5 % of old gen, 2 concurrent cycles. Hysteresis ON:
    /// 0 STW majors, 89 concurrent cycles. Same checksum as HotSpot in both.
    ///
    /// **The hysteresis page's default flip is the `ConcurrentFirst` arm of
    /// this match** (`gengc-r4-oldgen-major-trigger-has-no-hysteresis`): the
    /// orchestrator measures the opt-in arms this wave, and a positive A/B
    /// makes that arm `true` — the legacy arm stays the historical OFF, an
    /// explicit `=0` still switches it off, and the same commit gives the
    /// `old-trigger-hysteresis` row in `types/src/flag_groups.rs` its
    /// `off_word: Some("0")` (so `CRATONVM_GC=-old-trigger-hysteresis` writes
    /// the explicit off instead of unsetting the key). It was NOT flipped on intuition: it
    /// changes when a generation parked above 75 % is collected, and
    /// `a_finalizable_promoted_by_resurrection_survives_the_same_cycle_major`
    /// (gen_heap) stages exactly the back-to-back major it would suppress —
    /// that test has to set `=0` when the flip lands.
    pub fn hysteresis(self, explicit: Option<bool>) -> bool {
        explicit.unwrap_or(match self {
            Self::Legacy => false,
            Self::ConcurrentFirst => true,
        })
    }

    /// Short name, for the census line.
    pub fn name(self) -> &'static str {
        match self {
            Self::Legacy => "legacy",
            Self::ConcurrentFirst => "concurrent-first",
        }
    }
}



/// gen r4w4/concmark4 — bytes ever allocated in `old_gen`, as far as its
/// trigger can tell: live `used()` plus everything old-gen collections
/// reported reclaiming (`OldGenTriggerStats::freed_bytes`). Its growth between
/// two instants is the old-gen allocation (promotion plus direct) between
/// them. A free no collection reports (a promotion buffer's unused tail) only
/// makes a sample smaller; callers `saturating_sub`.
pub fn old_gen_allocated_total(old_gen: &OldGen) -> u64 {
    (old_gen.used() as u64).saturating_add(old_gen.trigger_stats().freed_bytes)
}







// ---------------------------------------------------------------------------
// gen r4w5/concmark5 — the concurrent-cycle service thread
// ---------------------------------------------------------------------------

/// gen r4w5/concmark5 (2026-09-24) — how long the service thread sleeps
/// between two of its OWN trigger checks when nothing wakes it (milliseconds).
///
/// The period is the backstop, not the mechanism: a young collection's
/// epilogue hands a due cycle off at once ([`ConcurrentGcState::hand_off_to_service`])
/// and a direct old-gen allocation that crosses the start threshold signals at
/// once ([`ConcurrentGcState::signal_old_gen_growth`]). The period catches old-gen
/// growth through any path that has no poll site. One check is the old-gen
/// lock plus the policy lock, so 50 checks a second cost nothing measurable.
pub const GEN_CONC_SERVICE_PERIOD_MS: u64 = 20;










// ---------------------------------------------------------------------------
// gen r4w5/concmark5 — the STW old-gen trigger's cadence, counted
// ---------------------------------------------------------------------------









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
    #[cfg(test)]
    term: Mutex<DrainTermination>,
    /// Condvar paired with [`Self::term`].
    #[cfg(test)]
    term_cv: Condvar,
    /// Lock-free mirror of `DrainTermination::idle`, so [`Self::push`] can ask
    /// "is anyone parked waiting for work?" with one relaxed load instead of
    /// taking the termination lock on every push. Maintained under `term`, so
    /// it never disagrees except for the instant between the two updates, and
    /// a stale read costs at most one spurious `notify_all` — never a missed
    /// wakeup, because a worker re-checks every shard under `term` before it
    /// parks (see [`MarkQueue::pop_or_terminate`]).
    #[cfg(test)]
    idle_hint: AtomicUsize,
}

/// Coordinator state for [`MarkQueue::pop_or_terminate`].
///
/// Deliberately the same shape as `young_mark`'s `DrainState`, for the same
/// reason: every quantity a termination decision depends on must be observable
/// under ONE lock, or the decision races the work it is deciding about.
#[derive(Debug)]
#[cfg(test)]
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
/// `log2(MARK_QUEUE_SHARDS)`: how many top bits of the hash `shard_for` keeps.
const MARK_QUEUE_SHARD_BITS: u32 = MARK_QUEUE_SHARDS.trailing_zeros();
// `shard_for` shifts right by `64 - SHARD_BITS`; a single shard would make that
// a 64-bit shift, which overflows.
const _: () = assert!(
    MARK_QUEUE_SHARD_BITS >= 1,
    "MARK_QUEUE_SHARDS must be at least 2 (shard_for shifts by 64 - log2(SHARDS))"
);

/// Round-5 HIGH #6 — defensive cap on a single mark-queue shard.
///
/// The original `MarkQueue::push` was an unbounded `Vec` (well, `VecDeque`)
/// growth. A pathological mutator graph (e.g. a malicious or buggy
/// classloader that produces a deeply circular object graph during a
/// concurrent-mark cycle) could OOM the marker thread by pushing
/// hundreds of millions of pointers before any are drained.
///
/// 1 million entries per shard * 8 shards * 8 bytes = 64 MiB.
///
/// gcf f1/c (doc only): this used to say the cap PANICS and that an
/// overflow strategy was a TODO. Neither is true any more: a push past the
/// cap is DROPPED and sets the queue's overflow flag (`MarkQueue::push`), and
/// the marker recovers with HotSpot's own answer to a mark-stack overflow, a
/// rescan of the marked-but-unscanned objects: concurrently in Phase 2 slices
/// (`overflow_rescan_due`, at most `CONCURRENT_OVERFLOW_RESCAN_PASSES` passes
/// per cycle, counted in `concpol_overflow_rescan_passes`), then at remark.
/// The marker-local stack (`drain_local`, depth-first) keeps the queue far
/// below the cap on ordinary graphs.
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
            #[cfg(test)]
            term: Mutex::new(DrainTermination {
                idle: 0,
                live: 0,
                done: false,
            }),
            #[cfg(test)]
            term_cv: Condvar::new(),
            #[cfg(test)]
            idle_hint: AtomicUsize::new(0),
        }
    }

    /// Select shard index from a pointer value.
    ///
    /// gen r4w2/concmark (2026-09-23): a Fibonacci hash of the 8-byte granule,
    /// taking the TOP bits. It used to be `(ptr >> 3) & (SHARDS - 1)`, the
    /// LOW bits — and legacy objects are `HEADER_SIZE (16) + n * SLOT_SIZE
    /// (16)` bytes, so in a region of them every start is 16-aligned, `ptr >> 3`
    /// is even, and only shards 0, 2, 4, 6 were ever used: half the sharding,
    /// and half the effective `MARK_QUEUE_SHARD_CAP` before the O(old gen)
    /// overflow rescan fires
    /// (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 1).
    /// The multiply mixes every address bit into the top bits, so any stride
    /// spreads. Computed in `u64` so the constant fits on every target.
    #[inline]
    fn shard_for(ptr: *mut u8) -> usize {
        let granule = (ptr as usize as u64) >> 3;
        let mixed = granule.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        (mixed >> (64 - MARK_QUEUE_SHARD_BITS)) as usize
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
        // This coordination exists only in test multi-worker drains. The
        // production single-worker path pays neither the load nor the wakeup.
        #[cfg(test)]
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
    // (gengc-mark-markqueue-has-no-termination-detection-FIXED-20260923)
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
    #[cfg(test)]
    pub fn begin_drain(&self, workers: usize) {
        let mut g = self.term.lock();
        g.idle = 0;
        g.live = workers;
        g.done = false;
        self.idle_hint.store(0, Ordering::Relaxed);
    }

    /// Register the calling thread as one of the workers [`Self::begin_drain`]
    /// counted. Exactly one per worker; hold it for the whole drain loop.
    #[cfg(test)]
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
    #[cfg(test)]
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
    #[cfg(test)]
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
#[cfg(test)]
pub struct MarkWorker<'q> {
    queue: &'q MarkQueue,
}

#[cfg(test)]
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
/// gen r5w3/unload7 (2026-09-26) — the class-loader side tables a
/// generational concurrent cycle follows when it may UNLOAD classes
/// (`CRATONVM_GEN_CONC_CLASS_UNLOAD`,
/// `docs/feature-designs/generative-gc-architecture.md`).
///
/// CratonVM keeps three class-loader relationships in VM side tables instead
/// of traceable Java fields:
///
/// * `loader_pin`: a live instance of a user-loader class keeps its defining
///   loader live (HotSpot: instance → klass → class loader data);
/// * `mirror_pin`: a live user loader keeps the mirrors of the classes it
///   defined live (HotSpot: the CLD holds its mirrors);
/// * `metadata_pin`: a live user loader keeps its classes' statics, class
///   locks, condy values, `ClassValue` results and the other
///   loader-conditional roots live — the rows `vm::memory::roots::collect_roots`
///   DEFERRED instead of rooting.
///
/// A root set taken under `gc_quiescence::with_class_unload_marking` leaves
/// user-loader mirrors and those roots to the tables, so the marker must follow
/// them: [`ConcurrentMarker::scan_object_into`] adds, for every scanned object,
/// the `loader_pin` row of its class and — when the object is a loader — its
/// `mirror_pin` and `metadata_pin` rows.
///
/// # A SNAPSHOT, taken in the initial-mark pause after the root scan
///
/// Not the live registries: `collect_roots` REBUILDS this VM's
/// `metadata_pin` rows at every pause (`replace_metadata_pins(.., &[])` first),
/// and the young pauses that run between the initial mark and the remark
/// record nothing for a cycle's loaders (hazard 1 on the page). A marker that
/// looked the rows up live in Phase 2 would find the table a young pause left
/// behind, not the rows the initial mark deferred, and the deferred values
/// would be swept. So the cycle follows the rows AS DEFERRED — the snapshot is
/// part of the cycle's snapshot-at-the-beginning, like the roots. A row added
/// later names an object allocated later (not sweep-eligible) or an owner the
/// remark re-captures ([`ConcurrentMarker::add_class_unload_tables`]).
///
/// The values are addresses: an OLD one does not move during a cycle (a free
/// or a compaction moves `reclaim_epoch` and the cycle is abandoned); a YOUNG
/// one may, but the marker drops young addresses (`markable_old_object`) and
/// every young object's old referents are already roots
/// (`collect_young_to_old_roots`), so a stale young entry costs nothing.
#[derive(Debug, Default)]
pub struct ClassUnloadTables {
    /// `class_id -> defining loader address`, the row inside THIS old
    /// generation when several VMs share the id (`loader_pin::snapshot_where`).
    /// `None` when the registry is empty.
    loaders: Option<FxHashMap<u32, usize>>,
    /// `loader address -> class mirror addresses`.
    mirrors: Option<FxHashMap<usize, Vec<usize>>>,
    /// `loader address -> deferred metadata object addresses`.
    metadata: Option<FxHashMap<usize, Vec<usize>>>,
}

/// gcj j2/og2 (C3) — the row length up to which [`ClassUnloadTables::absorb`]
/// keeps its linear `contains` dedup; longer rows go through a seen-set.
const ABSORB_LINEAR_DEDUP_MAX: usize = 32;

impl ClassUnloadTables {
    /// Read the three registries once. Call inside a pause, after the root
    /// scan whose deferrals the marker must follow.
    pub fn capture(old_gen_base: usize, old_gen_size: usize) -> Self {
        let end = old_gen_base.saturating_add(old_gen_size);
        Self {
            loaders: cratonvm_types::loader_pin::snapshot_where(|a| a >= old_gen_base && a < end),
            mirrors: cratonvm_types::mirror_pin::snapshot(),
            metadata: cratonvm_types::metadata_pin::snapshot(),
        }
    }

    /// True when every registry was empty: nothing to follow.
    pub fn is_empty(&self) -> bool {
        self.loaders.as_ref().is_none_or(|m| m.is_empty())
            && self.mirrors.as_ref().is_none_or(|m| m.is_empty())
            && self.metadata.as_ref().is_none_or(|m| m.is_empty())
    }

    /// The defining loader of `class_id` in this snapshot.
    #[inline]
    pub fn loader_of(&self, class_id: u32) -> Option<usize> {
        self.loaders.as_ref()?.get(&class_id).copied()
    }

    /// gen r5w6/conc10 — every loader this snapshot names (deduplicated): the
    /// fail-safe root set when the young classes cannot be enumerated
    /// completely (a young walk that stepped over an unparseable stretch),
    /// so no loader is judged dead under an instance nobody could see.
    pub fn all_loaders(&self) -> Vec<usize> {
        let mut loaders: Vec<usize> = self
            .loaders
            .as_ref()
            .map(|m| m.values().copied().collect())
            .unwrap_or_default();
        loaders.sort_unstable();
        loaders.dedup();
        loaders
    }

    /// Fold a LATER capture into this one: its loader rows win (a row can only
    /// have been re-pointed by a collection that moved a young loader), and
    /// its mirror / metadata rows are added to this snapshot's, so the result
    /// holds every row either capture saw.
    fn absorb(&mut self, later: ClassUnloadTables) {
        if let Some(loaders) = later.loaders {
            self.loaders
                .get_or_insert_with(FxHashMap::default)
                .extend(loaders);
        }
        for (mine, theirs) in [
            (&mut self.mirrors, later.mirrors),
            (&mut self.metadata, later.metadata),
        ] {
            let Some(theirs) = theirs else {
                continue;
            };
            let mine = mine.get_or_insert_with(FxHashMap::default);
            for (owner, values) in theirs {
                let row = mine.entry(owner).or_default();
                // gcj j2/og2 (C3 of `gcj-j1og-concurrent-cycle-and-old-gen-review-residuals`):
                // a row past a handful of entries dedups through a seen-set,
                // not a `contains` per value (O(n^2) inside the remark pause).
                // Same rows in the same order: first occurrence kept.
                if row.len() + values.len() <= ABSORB_LINEAR_DEDUP_MAX {
                    for v in values {
                        if !row.contains(&v) {
                            row.push(v);
                        }
                    }
                } else {
                    let mut seen: rustc_hash::FxHashSet<usize> = row.iter().copied().collect();
                    for v in values {
                        if seen.insert(v) {
                            row.push(v);
                        }
                    }
                }
            }
        }
    }

    /// gcf f4/c — the miss filter of [`ClassUnloadView`]: the inclusive
    /// `(lo, hi)` bounds of the class ids the loader table names and of the
    /// owners the mirror / metadata tables name (`None`: that side is empty).
    /// O(rows), and the rows are the user loaders' classes and the loaders
    /// themselves, so a handful; computed once per drain, not per object.
    fn miss_bounds(&self) -> UnloadMissBounds {
        let mut ids: Option<(u32, u32)> = None;
        for &id in self.loaders.iter().flat_map(|m| m.keys()) {
            ids = Some(ids.map_or((id, id), |(lo, hi)| (lo.min(id), hi.max(id))));
        }
        let mut owners: Option<(usize, usize)> = None;
        for (owner, _) in self.owner_rows() {
            owners = Some(owners.map_or((owner, owner), |(lo, hi)| (lo.min(owner), hi.max(owner))));
        }
        (ids, owners)
    }

    /// Every `(owner, values)` row of the mirror and metadata tables.
    fn owner_rows(&self) -> impl Iterator<Item = (usize, &[usize])> + '_ {
        self.mirrors
            .iter()
            .flatten()
            .chain(self.metadata.iter().flatten())
            .map(|(&owner, values)| (owner, values.as_slice()))
    }
}

/// gcf f4/c — the class-unload side tables as one drain sees them
/// (`gengc-r5w3-unload7-proposal-side-table-lookups-per-drain-not-per-object`):
/// the tables behind a read guard the DRAIN holds (taken once by
/// [`ConcurrentMarker::drain_local`], not once per scanned object), with the
/// miss filter of [`ClassUnloadTables::miss_bounds`], so an object whose class
/// id and address lie outside every row's bounds costs two compares instead of
/// three hash probes. Answers exactly as the tables do (the bounds only skip
/// probes that would miss).
/// gcf f4/c / gcg g1/c — [`ClassUnloadTables::miss_bounds`]: the inclusive
/// class-id bounds of the loader table and the owner-address bounds of the
/// mirror / metadata tables (`None`: that side is empty).
type UnloadMissBounds = (Option<(u32, u32)>, Option<(usize, usize)>);

struct ClassUnloadView<'a> {
    tables: &'a ClassUnloadTables,
    ids: Option<(u32, u32)>,
    owners: Option<(usize, usize)>,
}

impl<'a> ClassUnloadView<'a> {
    /// A view with freshly computed bounds (tests; the drain uses the
    /// marker's cached ones, [`Self::with_bounds`]).
    #[cfg(test)]
    fn new(tables: &'a ClassUnloadTables) -> Self {
        Self::with_bounds(tables, tables.miss_bounds())
    }

    /// gcg g1/c: a view over bounds already computed for exactly these
    /// tables ([`ConcurrentMarker::class_unload_bounds`], refreshed at each
    /// pause that writes the tables), so a drain does not re-walk every row.
    fn with_bounds(tables: &'a ClassUnloadTables, bounds: UnloadMissBounds) -> Self {
        let (ids, owners) = bounds;
        Self {
            tables,
            ids,
            owners,
        }
    }

    /// A view that filters nothing (every probe is made), for a single
    /// object, where building the bounds would cost more than it saves.
    fn unfiltered(tables: &'a ClassUnloadTables) -> Self {
        Self {
            tables,
            ids: Some((0, u32::MAX)),
            owners: Some((0, usize::MAX)),
        }
    }

    /// The defining loader of `class_id` (`ClassUnloadTables::loader_of`).
    #[inline]
    fn loader_of(&self, class_id: u32) -> Option<usize> {
        match self.ids {
            Some((lo, hi)) if class_id >= lo && class_id <= hi => self.tables.loader_of(class_id),
            _ => None,
        }
    }

    /// Whether `owner` may have a mirror / metadata row (`false` = it has none).
    #[inline]
    fn may_own_rows(&self, owner: usize) -> bool {
        matches!(self.owners, Some((lo, hi)) if owner >= lo && owner <= hi)
    }
}

/// gcf f4/c — how one scan reaches the class-unload side tables.
enum UnloadEdgeAccess<'a> {
    /// Take the read guard for this object (`scan_object` and the rare
    /// callers, unchanged).
    PerObject,
    /// The caller holds the guard for its whole drain: the view (`None` = the
    /// cycle was not armed when the drain began; the tables are armed and
    /// disarmed only inside pauses, never during a drain), and the new edges,
    /// counted once when the drain ends.
    Held {
        view: Option<ClassUnloadView<'a>>,
        newly: u64,
    },
}

impl UnloadEdgeAccess<'_> {
    /// Diagnostic-only membership check for a class-unload loader snapshot.
    /// Used only while `CRATONVM_DBG_MIRRORPIN_WHY` is enabled.
    fn is_loader_target(&self, addr: usize) -> bool {
        let Self::Held {
            view: Some(view), ..
        } = self
        else {
            return false;
        };
        view.tables
            .loaders
            .as_ref()
            .is_some_and(|loaders| loaders.values().any(|&loader| loader == addr))
    }
}

pub struct ConcurrentMarker {
    /// Mark bitmap over the span the driver sized it for (gcj j2/og2: the old
    /// generation's growth limit, `GenerationalHeap::old_gen_mark_span`; a
    /// test's marker, whatever it was built with). The remark refuses a sweep
    /// when the generation no longer fits it (`bitmap_covers_generation`).
    pub bitmap: MarkBitmap,
    /// Work queue of gray objects to scan.
    pub queue: MarkQueue,
    /// Reuse allocation capacity between slices; addresses are drained/spilled
    /// before releasing the guard, so no grey object hides between slices.
    local_stack: Mutex<Vec<usize>>,
    /// Global SATB queue for write barrier entries.
    ///
    /// fork6 GC_STRESS fix — MUST be the same instance the heap's
    /// `satb_barrier` logs into (`GenerationalHeap::enable_concurrent_gc`),
    /// or the write barrier feeds a queue nobody drains while `remark`
    /// drains a queue nobody fills. A marker built via [`Self::new`] gets a
    /// private queue (tests); production cycles use [`Self::with_shared`]
    /// with the heap's handles.
    pub satb_queue: Arc<SatbQueue>,
    /// Phase tracker. Same sharing requirement as `satb_queue`: the VM's
    /// cycle drivers and root builders (`generational_concurrent_mark_open`)
    /// read `is_marking_active()` of the instance attached via
    /// `enable_concurrent_gc`. The heap's `satb_barrier` gates on the QUEUE
    /// (gc-common w5-e), not on this.
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
    sweep_eligible: Mutex<Option<Arc<OldGenObjectStarts>>>,
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
    /// gen r4w2/concmark (2026-09-23) — the last old-gen object-start walk,
    /// keyed on the generation's identity and [`OldGen::free_list_seq`].
    ///
    /// Phase 2 used to rebuild the set at the top of EVERY slice and again at
    /// remark — an O(old gen) walk each time, which is what forced slices to be
    /// large. The rebuild exists so an object promoted between two slices is
    /// markable in the next one (see `concurrent_mark_budget`), and that is
    /// exactly what the key captures: a walk is a function of the free list
    /// (which blocks are allocated) and of the allocated objects' headers
    /// (kind and size, which nothing rewrites while the object is allocated),
    /// and every free-list mutation — `alloc`, `free`, `compact`'s rebuild,
    /// `release_unused_tail`, a coalesce — bumps `free_list_seq`. So an
    /// unchanged key proves the rebuild would reproduce this set exactly; a
    /// changed one rebuilds. Mutators allocate old-gen objects only under the
    /// old-gen lock with the header written before the lock is released
    /// (`try_alloc_*_old`), and every walk here runs under that lock, so no
    /// walk can observe a block between its `alloc` and its header.
    starts_cache: Mutex<Option<ObjectStartsCache>>,
    /// gen r4w2/concmark (2026-09-23) — set when any object-start walk this
    /// cycle did not cover every allocated byte (`OldGen::scan_region` broke on
    /// an implausible header). Objects behind the break are not in the set, so
    /// references to them are neither marked nor TRACED, and an older,
    /// eligible object reachable only through one of them would be swept live.
    /// `remark` refuses to authorise a sweep for such a cycle; see
    /// `gengc-r4-mark-walk-desync-tail-is-untraced-FIXED-20260923.md`.
    walk_desynced: AtomicBool,
    /// gen r4w3/oldgen3 — a sliced concurrent sweep between two slices; `None`
    /// before the first slice and after the last. See
    /// [`Self::concurrent_sweep_budget`].
    sweep_progress: Mutex<Option<SweepProgress>>,
    /// gen r4w3/oldgen3 — a mark-queue overflow rescan being run in Phase-2
    /// slices. See [`Self::concurrent_mark_budget`].
    overflow_rescan: Mutex<OverflowRescan>,
    /// gen r5w1/refs5 — the REFERENT-SLOT SKIP SET: one bit per old-gen
    /// `java.lang.ref.Reference` object whose slot 0 (the referent) this
    /// cycle's trace must not follow. See [`Self::set_reference_skip`].
    ///
    /// A second [`MarkBitmap`] over the same span as `bitmap`, built on first
    /// use only (a cycle that never publishes a set allocates nothing), and
    /// lock-free to query: [`Self::scan_object_into`] asks it once per scanned
    /// OBJECT, only while [`Self::reference_skip_armed`] says a set is live.
    reference_skip: std::sync::OnceLock<MarkBitmap>,
    /// `true` from [`Self::set_reference_skip`] (with at least one bit set)
    /// until [`Self::abort_cycle`] or the sweep's end. The one load the scan
    /// pays when no set is published.
    reference_skip_armed: AtomicBool,
    /// The span `bitmap` covers, kept so [`Self::reference_skip`] can be built
    /// over exactly the same one (`MarkBitmap` no longer exposes it).
    old_gen_base: usize,
    old_gen_size: usize,
    /// gen r5w3/unload7 — the class-loader side tables this cycle follows
    /// (see [`ClassUnloadTables`]); `None` unless the driver armed concurrent
    /// class unloading. Written only inside the cycle's pauses (by the thread
    /// that runs the cycle, which is also the only scanner), read per scanned
    /// object while [`Self::class_unload_armed`] is set.
    class_unload: RwLock<Option<ClassUnloadTables>>,
    /// gcg g1/c — [`ClassUnloadTables::miss_bounds`] of `class_unload`,
    /// recomputed under its WRITE guard by every writer
    /// ([`Self::set_class_unload_tables`], [`Self::add_class_unload_tables`],
    /// [`Self::disarm_class_unload`]), so it always describes the tables a
    /// reader sees. Before this a drain recomputed the bounds -- a walk of
    /// every loader-table and owner row -- on EACH `drain_local` call, i.e.
    /// once per Phase-2 slice, which on a program with many user-loader
    /// classes (Tomcat, Spring) is O(rows) per slice against O(budget)
    /// objects scanned. Lock order: `class_unload`, then this.
    class_unload_bounds: Mutex<UnloadMissBounds>,
    /// The one load [`Self::scan_object_into`] pays when no tables are armed.
    class_unload_armed: AtomicBool,
    /// gen r5w4/conc8 — the retained-layout census candidates the driver
    /// handed this cycle in its initial-mark pause
    /// ([`Self::set_layout_census`]); taken by the sweep's first slice.
    layout_census: Mutex<Option<Vec<u32>>>,
    /// gen r5w5/conc9 — set by a driver whose remark runs reference
    /// processing ([`Self::record_finalizer_retention_closure`]): the remark's
    /// dead-finalizer retention then RECORDS every object it marks, so the
    /// reference pass can treat them as marked only for `finalize()` (see
    /// [`Self::take_finalizer_retention_closure`]). `false` (every other
    /// caller) is the retention exactly as before.
    record_retention_closure: AtomicBool,
    /// gen r5w5/conc9 — what the last recording retention marked: the retained
    /// finalizables and every object their drain newly marked. Written and
    /// taken inside the same remark pause.
    retention_closure: Mutex<Vec<usize>>,
    /// gcd d9/e — the INITIAL MARK's object-start set (the walk that also
    /// seeded `starts_cache`), kept for Phase-2 slices under
    /// `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` ([`Self::phase2_object_starts`]).
    /// Set by `initial_mark`, dropped by `abort_cycle` and at the sweep's
    /// start; `None` otherwise.
    tams_starts: Mutex<Option<Arc<OldGenObjectStarts>>>,
    /// gci i13/cm (`gci-i13cm-generational-concurrent-cycle-under-the-user-flips-review-findings-FIXED-20261016`
    /// item 1) — `true` once [`Self::finish_sweep`] has stored `Idle`: the
    /// HAND-OFF of the shared phase and SATB queue to whichever driver opens
    /// the next cycle. From then on nothing on this marker may write the
    /// shared state again: a [`ConcurrentCycle`] dropped after the hand-off
    /// (an unwind between the sweep's end and `complete`) used to run
    /// [`Self::abort_cycle`], whose blind `Idle` store, SATB deactivation and
    /// policy reset would land on the NEXT cycle. Reset by `initial_mark`.
    handed_off: AtomicBool,
}

/// gen r5w4/conc8 — the process-wide count of old-generation walk breaks
/// (`WALK_DESYNC_HITS` + `SCAN_REGION_BREAK_HITS`). A walk that breaks leaves
/// the rest of its region unvisited without telling its caller, so a census
/// that must have seen EVERY object compares this before and after: unchanged
/// means no walk anywhere broke in between (a break in another VM's walk only
/// makes the census inconclusive, the safe direction).
fn walk_break_hits() -> u64 {
    crate::old_gen::WALK_DESYNC_HITS
        .load(Ordering::Relaxed)
        .wrapping_add(crate::old_gen::SCAN_REGION_BREAK_HITS.load(Ordering::Relaxed))
}

/// gen r5w4/conc8 — one sweep's retained-layout census (see
/// [`ConcurrentGcState::note_layouts_retained`]).
struct LayoutCensus {
    /// Pending retained ids with no young instance at this cycle's initial mark.
    candidates: rustc_hash::FxHashSet<u32>,
    /// Candidates the sweep found a SURVIVING old-generation instance of.
    survivors: rustc_hash::FxHashSet<u32>,
    /// Repeated instances of the same classes need no repeated hash probes.
    /// Collisions simply evict an entry and perform the exact set checks again.
    observed_classes: [Option<u32>; 64],
    /// [`walk_break_hits`] when the sweep began.
    breaks_at_start: u64,
}

impl LayoutCensus {
    /// A surviving object (marked, or not sweep-eligible) the sweep walked.
    #[inline]
    fn observe_survivor(&mut self, obj_ptr: *mut u8) {
        // SAFETY: `walk_objects_from` yields only object starts whose header
        // tags it validated and whose size it derived from this header, under
        // the old-gen lock the sweep slice holds; `class_id` is a plain `u32`
        // field of that header.
        let class_id = unsafe { (*(obj_ptr as *const ObjectHeader)).class_id.as_u32() };
        let cache_slot = class_id as usize % self.observed_classes.len();
        if self.observed_classes[cache_slot] == Some(class_id) {
            return;
        }
        self.observed_classes[cache_slot] = Some(class_id);
        if self.candidates.contains(&class_id) {
            self.survivors.insert(class_id);
        }
    }

    /// The candidates to release if the census was complete: no walk broke
    /// since it began. `None` (release nothing) otherwise.
    fn released(self) -> Option<Vec<u32>> {
        if walk_break_hits() != self.breaks_at_start {
            return None;
        }
        let survivors = self.survivors;
        Some(
            self.candidates
                .into_iter()
                .filter(|id| !survivors.contains(id))
                .collect(),
        )
    }
}

/// gen r4w3/oldgen3 — what a sliced concurrent sweep carries from one slice
/// to the next. See [`ConcurrentMarker::concurrent_sweep_budget`].
struct SweepProgress {
    /// The TAMS-narrowed eligibility snapshot remark authorised.
    eligible: Arc<OldGenObjectStarts>,
    /// `OldGen::reclaim_epoch` after the previous slice's OWN frees. Anything
    /// else that frees or slides old-gen storage moves it, and then neither
    /// the bitmap nor the resume offset can be trusted.
    epoch: u64,
    /// Where the next slice resumes — an offset `OldGen::walk_objects_from`
    /// returned, i.e. the start of an object that was allocated then.
    next: usize,
    freed_objects: usize,
    freed_bytes: usize,
    /// gen r5w4/conc8 — `Some` while retained layouts are pending and the
    /// driver handed this cycle census candidates.
    layout_census: Option<LayoutCensus>,
}

/// gen r4w3/oldgen3 — a mark-queue overflow rescan in progress across Phase-2
/// slices, and how many passes this cycle has started.
#[derive(Default)]
struct OverflowRescan {
    /// `Some(resume offset)` while a pass is part-way through the generation.
    cursor: Option<usize>,
    passes: u32,
}

/// gen r5w1/refs5 — the remark between its two halves
/// ([`ConcurrentMarker::remark_begin`] → [`ConcurrentMarker::remark_finish`]),
/// with the old-gen guard possibly released in between. Dropping it without
/// `remark_finish` leaves the phase at `Remark` with the SATB queue active; the
/// driver's [`ConcurrentCycle`] guard then abandons the cycle on its way out,
/// as for any other early exit.
#[must_use = "a remark begun must be finished (remark_finish) inside the same pause"]
pub struct RemarkProgress {
    authorised: bool,
    scanned: usize,
    object_starts: Arc<OldGenObjectStarts>,
}

impl RemarkProgress {
    /// Whether this remark authorised a sweep. The reference-processing
    /// callback must not run on a refused remark: nothing can be judged dead.
    pub fn sweep_authorised(&self) -> bool {
        self.authorised
    }
}

/// gen r4w3/oldgen3 — at most this many overflow rescan passes run
/// concurrently per cycle; after that the flag is left set and remark's
/// `drain_closure` applies its own bounded fallback, exactly as before. The
/// same bound `drain_closure` uses.
const CONCURRENT_OVERFLOW_RESCAN_PASSES: u32 = 8;

/// gen r4w4/concmark4 — entries the marker-local stack holds before a newly
/// greyed child goes to the shared queue instead (512 KiB of pointers). Far
/// below the queue's own `MARK_QUEUE_SHARDS * MARK_QUEUE_SHARD_CAP`, so the
/// local stack never hides an overflow the queue would have reported.
const LOCAL_MARK_STACK_CAP: usize = 64 * 1024;

/// One cached [`OldGenObjectStarts`] and the identity it is valid for. See
/// [`ConcurrentMarker::starts_cache`].
struct ObjectStartsCache {
    /// `OldGen::base_ptr`, so a marker handed a different generation (tests do)
    /// can never reuse another generation's set.
    base: usize,
    /// `OldGen::free_list_seq` when the walk ran.
    seq: u64,
    starts: Arc<OldGenObjectStarts>,
}

/// gen r4w2/concmark — object-start walks the marker REUSED because
/// `free_list_seq` had not moved (see `ConcurrentMarker::starts_cache`).
pub static CONC_MARK_WALKS_REUSED: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — object-start walks the marker actually ran (initial
/// mark, a slice or remark whose cache key had moved).
pub static CONC_MARK_WALKS_BUILT: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles whose remark refused to authorise a sweep because
/// an object-start walk desynced (see `ConcurrentMarker::walk_desynced`).
/// Non-zero means `WALK_DESYNC_HITS`/`SCAN_REGION_BREAK_HITS` are non-zero too.
pub static CONC_MARK_WALK_DESYNC_ABORTS: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — `ConcurrentMarker::try_open_cycle` refusals: a driver
/// found another cycle already owning the shared phase.
pub static CONC_CYCLE_OPEN_REFUSED: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — Phase-2 slices the generational driver ran (bumped by
/// `maybe_concurrent_gc_at`). Slices per completed cycle is the number to read
/// against `CRATONVM_GEN_CONC_MARK_SLICE`.
pub static CONC_MARK_SLICES: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — remark pauses the driver lost to another thread's pause
/// and then RETRIED after joining it (the sliced driver only).
pub static CONC_REMARK_RETRIES: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles abandoned because every remark attempt was lost
/// (in the legacy `CRATONVM_GEN_CONC_MARK_SLICE=0` arm, the first loss).
pub static CONC_REMARK_ABANDONS: AtomicU64 = AtomicU64::new(0);
/// gen r4w2/concmark — cycles the driver abandoned BEFORE the remark pause
/// because another old-gen collector had freed or slid storage since the
/// initial mark (`ConcurrentMarker::cycle_is_stale`). Slicing lets young
/// pauses — including ones that reclaim old-gen blocks in place — run inside
/// Phase 2, so this is the counter that says whether they are starving cycles.
pub static CONC_CYCLE_STALE_ABANDONS: AtomicU64 = AtomicU64::new(0);

/// gen r4w2/concmark — objects per Phase-2 slice when the flag is unset.
///
/// The slice bounds how long the marker holds the old-gen lock (every
/// promotion and every direct old-gen allocation waits for it) and how long it
/// goes without a safepoint poll (every other thread's pause waits for it).
/// Scanning one object is a header decode plus one load per slot, so 32 Ki
/// objects is on the order of a millisecond on ordinary objects; with the
/// object-start cache a slice no longer pays for a walk unless the old
/// generation changed since the previous one.
pub const DEFAULT_GEN_CONC_MARK_SLICE: usize = 32 * 1024;

/// `CRATONVM_GEN_CONC_MARK_SLICE=<n>` — objects scanned per Phase-2 slice of
/// the generational concurrent old-gen cycle (gen r4w2/concmark, 2026-09-23).
///
/// * unset → [`DEFAULT_GEN_CONC_MARK_SLICE`];
/// * `n > 0` → slices of `n` objects, with the old-gen lock released and a
///   safepoint polled between slices, and a lost remark pause retried;
/// * `0` → the pre-2026-09-23 driver: one unbounded `concurrent_mark` under a
///   single old-gen lock hold, no polls, and a lost remark aborts the cycle.
///   Kept as the A/B arm and the kill switch.
///
/// gen r4w3/oldgen3: the same knob sizes Phase-4 SWEEP slices (objects walked
/// per slice, `ConcurrentMarker::concurrent_sweep_budget`), and `0` likewise
/// restores the single-hold sweep.
///
/// A value that does not parse keeps the default. Read once per process: it
/// is a tuning knob, not compatibility state.
pub fn gen_conc_mark_slice() -> usize {
    static SLICE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SLICE.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<usize>().ok()))
            .unwrap_or(DEFAULT_GEN_CONC_MARK_SLICE)
    })
}

/// Ownership of one open generational concurrent cycle, from
/// [`ConcurrentMarker::try_open_cycle`] until [`Self::complete`].
///
/// gen r4w2/concmark (2026-09-23). Dropping it without `complete` — an early
/// return on a lost pause, a stale epoch, or an unwind — ABANDONS the cycle:
/// the shared phase goes back to `Idle` and the SATB queue is deactivated, so
/// no exit path can leave the barrier armed and every later driver locked out.
///
/// `complete` is for the one exit where the sweep itself has already returned
/// the phase to `Idle`. That store is the hand-off: from that instant another
/// driver may open the NEXT cycle, so nothing on this side may write the
/// phase again — which is why `ConcurrentMarker::finish_cycle` is now a
/// compare-and-swap from `ConcurrentSweep` rather than a blind `Idle` store.
#[must_use = "dropping the ownership abandons the cycle"]
pub struct ConcurrentCycle<'m> {
    marker: &'m ConcurrentMarker,
    open: bool,
}

impl ConcurrentCycle<'_> {
    /// The sweep ran and returned the phase to `Idle`: release ownership
    /// WITHOUT touching the shared state again.
    pub fn complete(mut self) {
        self.open = false;
        self.marker.finish_cycle();
    }

    /// Abandon the cycle now. Same as dropping it; named for call sites.
    pub fn abort(self) {
        drop(self);
    }
}

impl Drop for ConcurrentCycle<'_> {
    fn drop(&mut self) {
        if !self.open {
            return;
        }
        self.open = false;
        let m = self.marker;
        // gci i13/cm item 1: the sweep already handed the shared state to the
        // next cycle (`finish_sweep` stored `Idle`); this marker no longer owns
        // the phase, the SATB queue or the policy baseline, so an unwind
        // between that hand-off and `complete` must not abandon anything --
        // there is nothing of this cycle left to abandon, and what is there
        // may be the next cycle's.
        if m.handed_off.load(Ordering::Acquire) {
            return;
        }
        if m.state.phase() == ConcurrentGcPhase::InitialMark && !m.satb_queue.is_active() {
            // `initial_mark` never ran (the initial-mark pause was lost): there
            // is no barrier to take down and no snapshot to drop, and the
            // thread-bucket walk `deactivate_and_discard` does would be pure
            // cost on the path that allocation storms take most often.
            let _ = m
                .state
                .cas_quiet_phase(ConcurrentGcPhase::InitialMark, ConcurrentGcPhase::Idle);
            // gcd d9/e: `[GC] conc_cycles: conccyc_open_lost`.
            m.state.note_open_lost();
        } else {
            m.abort_cycle();
        }
    }
}

impl ConcurrentMarker {
    /// GCAUD-4: count one cycle that reclaims nothing (more) because old-gen
    /// storage was reclaimed or relocated under it. Per VM, on the shared
    /// census ([`ConcurrentCycleCensus::sweep_epoch_aborts`]).
    ///
    /// gen r5w1/refs5 (`gengc-r4-mark-counters-and-comments-that-lie`): this
    /// was the process-global `pub static SWEEP_EPOCH_ABORTS`, read only by
    /// one test, documented as "between remark and the sweep" although remark
    /// also counted a reclaim between initial mark and remark, and summed
    /// across every VM in the process.
    fn note_sweep_epoch_abort(&self) {
        self.state
            .census
            .sweep_epoch_aborts
            .fetch_add(1, Ordering::Relaxed);
    }

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
        // gen r5w1/refs5 (`gengc-r4-mark-counters-and-comments-that-lie`): the
        // mark bitmap is anchored at the RAW old-gen base, while
        // `OldGenObjectStarts` anchors at `base & !7` because an `OldGen`'s
        // `Vec<u8>` base carries no alignment guarantee, and the bitmap's
        // `locate` refuses any address whose offset from ITS base is not
        // 8-aligned. On a misaligned base every `try_mark` of a real (8-aligned)
        // object would answer `false`: nothing marked, everything eligible
        // swept. The allocator has always returned an aligned base, so this
        // has never fired; it is stated here, where the disagreement lives,
        // and `remark_begin` refuses to authorise a sweep over such a bitmap
        // rather than trusting it (fail closed, never a crash in release).
        debug_assert!(
            old_gen_base % 8 == 0,
            "ConcurrentMarker: old-gen base {old_gen_base:#x} is not 8-aligned; \
             the mark bitmap could mark nothing"
        );
        Self {
            bitmap: MarkBitmap::new(old_gen_base, old_gen_size),
            queue: MarkQueue::new(),
            local_stack: Mutex::new(Vec::new()),
            satb_queue,
            state,
            sweep_eligible: Mutex::new(None),
            initial_mark_epoch: Mutex::new(None),
            sweep_eligible_epoch: Mutex::new(None),
            starts_cache: Mutex::new(None),
            walk_desynced: AtomicBool::new(false),
            sweep_progress: Mutex::new(None),
            overflow_rescan: Mutex::new(OverflowRescan::default()),
            reference_skip: std::sync::OnceLock::new(),
            reference_skip_armed: AtomicBool::new(false),
            old_gen_base,
            old_gen_size,
            class_unload: RwLock::new(None),
            class_unload_bounds: Mutex::new((None, None)),
            class_unload_armed: AtomicBool::new(false),
            layout_census: Mutex::new(None),
            record_retention_closure: AtomicBool::new(false),
            retention_closure: Mutex::new(Vec::new()),
            tams_starts: Mutex::new(None),
            handed_off: AtomicBool::new(false),
        }
    }

    /// gen r5w5/conc9 — this cycle's remark runs reference processing
    /// (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`), so its dead-finalizer
    /// retention must record what it marks. Call before the remark.
    ///
    /// # Why (`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`)
    ///
    /// The retention ([`Self::retain_dead_finalizer_candidates`]) runs in
    /// [`Self::remark_begin`], BEFORE the reference-processing seam, and marks
    /// every dead finalizable plus everything reachable only through it. The
    /// seam's verdict (`remark_is_marked`) then calls all of them live, so a
    /// `WeakReference` to a dead finalizable, or to an object reachable only
    /// through one, was NOT cleared at the remark: `finalize()` could resurrect
    /// the object while the weak reference still named it. HotSpot clears
    /// soft and weak references before it discovers finalizable objects. The
    /// post-collection path gets that from the collectors' resurrection
    /// closure (`ReferenceProcessor::note_resurrected_finalizables`); this is
    /// the same closure for the concurrent remark.
    pub fn record_finalizer_retention_closure(&self) {
        self.record_retention_closure.store(true, Ordering::Release);
    }

    /// gen r5w5/conc9 — the objects the last recording remark retention
    /// marked (retained finalizables first, then what their drain newly
    /// marked), taken once. Empty unless
    /// [`Self::record_finalizer_retention_closure`] was called and the remark
    /// found a dead finalizable. Every entry was UNMARKED at the first fixed
    /// point (the strong closure) and is marked now, which is exactly the
    /// processor's "marked only by the resurrection pass" contract. The
    /// closure can be SHORT (a scan that failed closed marked the whole
    /// generation without recording it): that is the depth-1 answer for what
    /// is missing, never a strongly reachable object in the set.
    pub fn take_finalizer_retention_closure(&self) -> Vec<usize> {
        std::mem::take(&mut *self.retention_closure.lock())
    }

    /// gen r5w4/conc8 — hand this cycle the retained-layout census candidates:
    /// the pending retained class ids ([`ConcurrentGcState::retained_layouts_pending`])
    /// of which the young generation holds no instance. Must be called inside
    /// the initial-mark pause, where the young walk that proved that ran. The
    /// sweep's walk then notes which of them still have an old-generation
    /// survivor, and a complete sweep releases the rest (see
    /// [`ConcurrentGcState::note_layouts_retained`]). An empty list arms
    /// nothing.
    pub fn set_layout_census(&self, candidates: Vec<u32>) {
        *self.layout_census.lock() = (!candidates.is_empty()).then_some(candidates);
    }

    /// gen r5w3/unload7 — arm concurrent class unloading for this cycle with
    /// the side tables its initial-mark root scan deferred to (see
    /// [`ClassUnloadTables`]). Must be called inside the initial-mark pause,
    /// after that root scan and BEFORE [`Self::initial_mark_with_frozen`], so
    /// no object is scanned without the edges. An empty capture arms nothing
    /// (no user loader has pinned anything, so the root scan deferred
    /// nothing).
    pub fn set_class_unload_tables(&self, tables: ClassUnloadTables) {
        let armed = !tables.is_empty();
        let mut guard = self.class_unload.write();
        *self.class_unload_bounds.lock() = if armed {
            tables.miss_bounds()
        } else {
            (None, None)
        };
        *guard = armed.then_some(tables);
        drop(guard);
        self.class_unload_armed.store(armed, Ordering::Release);
    }

    /// gen r5w3/unload7 — fold the REMARK's capture into the cycle's tables.
    /// Call inside the remark pause, after its root scan and BEFORE
    /// [`Self::remark_begin`], which then marks the rows of every owner that
    /// is already live (see [`Self::mark_rows_of_live_owners`]). A remark
    /// root set taken under `with_class_unload_marking` deferred the CURRENT
    /// static values (and the other rows) again; an owner that turned black
    /// before this pause will not be scanned again, so its fresh rows are
    /// marked by that pre-pass rather than by a scan.
    pub fn add_class_unload_tables(&self, later: ClassUnloadTables) {
        let mut guard = self.class_unload.write();
        match guard.as_mut() {
            Some(tables) => tables.absorb(later),
            None => {
                if later.is_empty() {
                    return;
                }
                *guard = Some(later);
            }
        }
        // gcg g1/c: the bounds of the merged tables, under the write guard.
        if let Some(tables) = guard.as_ref() {
            *self.class_unload_bounds.lock() = tables.miss_bounds();
        }
        drop(guard);
        self.class_unload_armed.store(true, Ordering::Release);
    }

    /// gen r5w3/unload7 — is this cycle following the class-loader side
    /// tables?
    pub fn class_unload_armed(&self) -> bool {
        self.class_unload_armed.load(Ordering::Acquire)
    }

    /// gen r5w3/unload7 — drop the tables (the mark is over, or the cycle).
    fn disarm_class_unload(&self) {
        self.class_unload_armed.store(false, Ordering::Release);
        let mut guard = self.class_unload.write();
        *guard = None;
        *self.class_unload_bounds.lock() = (None, None);
    }

    /// gen r5w3/unload7 — the side-table edges of one scanned object: the
    /// `loader_pin` row of `class_id`, and the `mirror_pin` / `metadata_pin`
    /// rows `obj_ptr` owns. Each target is marked and handed to `push` exactly
    /// as a field referent is. Only called while the tables are armed.
    fn push_class_unload_edges<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        class_id: u32,
        object_starts: &OldGenObjectStarts,
        push: &mut F,
    ) {
        let guard = self.class_unload.read();
        let Some(tables) = guard.as_ref() else {
            return;
        };
        // One object: no filter worth building (it is O(rows)).
        let view = ClassUnloadView::unfiltered(tables);
        let newly =
            self.push_class_unload_edges_with(&view, obj_ptr, class_id, object_starts, push);
        drop(guard);
        self.state.note_class_unload_edges(newly);
    }

    /// gcf f4/c — [`Self::push_class_unload_edges`]' body over a view the
    /// caller holds; returns how many objects it newly marked (the caller
    /// notes them). Takes no lock: a drain that holds the guard must never
    /// read `class_unload` again (a parking_lot read behind a queued writer
    /// would deadlock).
    #[inline]
    fn push_class_unload_edges_with<F: FnMut(*mut u8)>(
        &self,
        view: &ClassUnloadView<'_>,
        obj_ptr: *mut u8,
        class_id: u32,
        object_starts: &OldGenObjectStarts,
        push: &mut F,
    ) -> u64 {
        let mut newly = 0u64;
        let trace_loader_marks =
            false;
        let mut edge = |addr: usize| {
            let p = addr as *mut u8;
            if markable_old_object(p, object_starts) && self.bitmap.try_mark(addr) {
                if trace_loader_marks && view.loader_of(class_id) == Some(addr) {
                    eprintln!(
                        "[conc-unload-class-loader-mark] source={obj_ptr:#p} class_id={class_id} target={addr:#x}"
                    );
                }
                newly += 1;
                push(p);
            }
        };
        if let Some(loader) = view.loader_of(class_id) {
            edge(loader);
        }
        let owner = obj_ptr as usize;
        if view.may_own_rows(owner) {
            for table in [&view.tables.mirrors, &view.tables.metadata] {
                if let Some(values) = table.as_ref().and_then(|m| m.get(&owner)) {
                    for &v in values {
                        edge(v);
                    }
                }
            }
        }
        newly
    }

    /// gen r5w3/unload7 — mark (and queue) the mirror / metadata rows of every
    /// owner this cycle cannot judge dead: one already marked, or one that is
    /// not sweep-eligible (young, allocated after the initial mark, or not an
    /// old-gen object start at all — `remark_is_marked`'s verdict). Returns
    /// how many objects it marked.
    ///
    /// Two callers, one rule:
    ///
    /// * the initial mark (`initial_mark_with_frozen`): the owners that are
    ///   YOUNG. The concurrent trace never scans a young object, so a young
    ///   loader's rows would otherwise never be followed (hazard 2 on the
    ///   page, its loader half; the instance half is the driver's young walk);
    /// * the remark (`remark_begin`): every owner already live, so the rows
    ///   the REMARK's root scan deferred are followed for a loader that turned
    ///   black before the pause. An owner marked later in the remark is
    ///   scanned, and its scan follows the merged tables.
    ///
    /// Over-approximate in the safe direction: a stale young key (a loader
    /// that died young, whose row no reconcile pruned yet) keeps its rows one
    /// more cycle.
    fn mark_rows_of_live_owners(&self, object_starts: &OldGenObjectStarts) -> usize {
        if !self.class_unload_armed() {
            return 0;
        }
        let mut targets: Vec<usize> = Vec::new();
        {
            let guard = self.class_unload.read();
            let Some(tables) = guard.as_ref() else {
                return 0;
            };
            let eligible = self.sweep_eligible.lock();
            for (owner, values) in tables.owner_rows() {
                let live = self.bitmap.is_marked(owner)
                    || !eligible.as_ref().is_some_and(|e| e.contains(owner));
                if live {
                    targets.extend_from_slice(values);
                }
            }
        }
        let mut marked = 0usize;
        for addr in targets {
            let p = addr as *mut u8;
            if markable_old_object(p, object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(p);
                marked += 1;
            }
        }
        self.state.note_class_unload_edges(marked as u64);
        marked
    }

    /// gen r5w1/refs5 — publish this cycle's REFERENT-SLOT SKIP SET: the
    /// addresses of the old-gen `java.lang.ref.Reference` objects (weak, soft
    /// and phantom; never a `Finalizer` row, whose object IS the finalizable,
    /// nor a `Cleaner` row, whose layout is not referent-at-slot-0) whose
    /// slot 0 the trace must NOT follow. Must be called inside the
    /// initial-mark pause, BEFORE [`Self::initial_mark`] /
    /// [`Self::initial_mark_with_frozen`] (which scans frozen objects), so no
    /// object is scanned without the skip in force. Returns how many addresses
    /// landed in the set (addresses outside the old generation are ignored:
    /// a young `Reference`'s referent is kept by `collect_young_to_old_roots`,
    /// over-retention, unchanged).
    ///
    /// # Why (`gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing`)
    ///
    /// The marker traces every reference slot, a `Reference`'s referent
    /// included, so every referent of a live `Reference` is marked and a
    /// remark-time reference pass would be told "alive" for all of them.
    /// Hiding slot 0 — G1's INT-8 (`G1Collector::set_reference_skip_set`) —
    /// leaves the bitmap with an untainted verdict for each referent.
    ///
    /// # The one-commit rule
    ///
    /// Hiding without processing is a use-after-free: a referent the trace no
    /// longer reaches is unmarked while its `Reference` still points at it, and
    /// the sweep frees it. So a caller that publishes a set MUST run
    /// [`Self::remark_with_reference_processing`] (or the
    /// [`Self::remark_begin`] / [`Self::remark_finish`] pair) with a callback
    /// that clears or resurrects every unmarked referent of a live `Reference`
    /// in the set. The VM driver does both behind one flag
    /// (`CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK`).
    ///
    /// # No re-keying
    ///
    /// Old-gen objects do not move during a cycle: a compaction or a free by
    /// another collector moves `reclaim_epoch` and the cycle is abandoned (and
    /// with it the sweep that could have freed a hidden referent). A
    /// `Reference` promoted mid-cycle is not in the set, so its referent is
    /// traced — over-retention, safe.
    pub fn set_reference_skip(&self, refs: &[usize]) -> usize {
        let skip = self
            .reference_skip
            .get_or_init(|| MarkBitmap::new(self.old_gen_base, self.old_gen_size));
        // No scan can be running: this is the initial-mark pause, before the
        // cycle's first scan (the `clear` exclusivity contract).
        self.reference_skip_armed.store(false, Ordering::Release);
        skip.clear();
        let mut published = 0usize;
        for &addr in refs {
            // `try_mark` answers `false` for an address outside the span (a
            // young `Reference`), which is exactly the filter wanted.
            if skip.try_mark(addr) {
                published += 1;
            }
        }
        if published > 0 {
            self.reference_skip_armed.store(true, Ordering::Release);
            self.state
                .census
                .reference_skip_published
                .fetch_add(published as u64, Ordering::Relaxed);
        }
        published
    }

    /// gen r5w1/refs5 — whether this cycle hides slot 0 of the object at
    /// `obj`. One relaxed load when no set is published.
    #[inline]
    fn referent_slot_hidden(&self, obj: *mut u8) -> bool {
        self.reference_skip_armed.load(Ordering::Acquire)
            && self
                .reference_skip
                .get()
                .is_some_and(|skip| skip.is_marked(obj as usize))
    }

    /// gen r5w1/refs5 — withdraw the skip set (abort, or the sweep's end).
    /// Only the gate: the bits are cleared by the next
    /// [`Self::set_reference_skip`], which runs inside a pause.
    fn disarm_reference_skip(&self) {
        self.reference_skip_armed.store(false, Ordering::Release);
    }

    /// gen r4w2/concmark — open a cycle on the shared phase, or `None` when
    /// another driver already owns one (see
    /// [`ConcurrentGcState::try_open_cycle`] for why this must be exclusive).
    pub fn try_open_cycle(&self) -> Option<ConcurrentCycle<'_>> {
        if self.state.try_open_cycle() {
            Some(ConcurrentCycle {
                marker: self,
                open: true,
            })
        } else {
            CONC_CYCLE_OPEN_REFUSED.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// gen r4w2/concmark — `true` once another old-gen collector has freed or
    /// slid storage since this cycle's initial mark. `remark` is then certain
    /// to refuse the sweep, so a driver can abandon the cycle without paying
    /// for the remark pause. `false` for a cycle that never ran `initial_mark`.
    pub fn cycle_is_stale(&self, old_gen: &OldGen) -> bool {
        matches!(
            *self.initial_mark_epoch.lock(),
            Some(opened) if opened != old_gen.reclaim_epoch()
        )
    }

    /// The object-start set for `old_gen` as it is NOW: the cached walk when
    /// `free_list_seq` has not moved since it was taken, a fresh walk (which
    /// then becomes the cache) otherwise. See [`Self::starts_cache`].
    fn object_starts_for(&self, old_gen: &OldGen) -> Arc<OldGenObjectStarts> {
        self.object_starts_for_noting(old_gen).0
    }

    /// [`Self::object_starts_for`], also saying how long a fresh walk took
    /// (`None`: the cached walk was reused). gcd d9/e: for the Phase-2 walk
    /// census (`[GC] conc_cycles: conccyc_phase2_start_walks`).
    fn object_starts_for_noting(
        &self,
        old_gen: &OldGen,
    ) -> (Arc<OldGenObjectStarts>, Option<std::time::Duration>) {
        let base = old_gen.base_ptr() as usize;
        let seq = old_gen.free_list_seq();
        let mut cache = self.starts_cache.lock();
        if let Some(c) = cache.as_ref() {
            if c.base == base && c.seq == seq {
                CONC_MARK_WALKS_REUSED.fetch_add(1, Ordering::Relaxed);
                return (Arc::clone(&c.starts), None);
            }
        }
        let t0 = std::time::Instant::now();
        let starts = Arc::new(old_gen_object_starts(old_gen));
        let took = t0.elapsed();
        self.note_walk(&starts);
        *cache = Some(ObjectStartsCache {
            base,
            seq,
            starts: Arc::clone(&starts),
        });
        (starts, Some(took))
    }

    /// gcd d9/e (2026-09-28) — the object-start set a PHASE-2 slice judges
    /// markability against.
    ///
    /// # Default: the generation as it is now
    ///
    /// [`Self::object_starts_for`]: the cached walk while `free_list_seq`
    /// stands still, a fresh O(old gen) walk the moment it moves. Every
    /// promotion moves it, so under steady promotion EVERY young collection
    /// between two slices costs the next slice a whole-generation walk — the
    /// cycle's marking time then scales with (young collections during the
    /// cycle) × (old-gen size), and a slow cycle is what lets the STW
    /// collection pre-empt it (`gengc-r4w3-oldgen3-stw-major-and-concurrent-
    /// cycle-share-one-trigger`). The walks and their time are counted on
    /// `[GC] conc_cycles:` (`conccyc_phase2_start_walks`,
    /// `conccyc_phase2_walk_ms`).
    ///
    /// # `CRATONVM_GEN_CONC_MARK_TAMS_STARTS`: the initial mark's snapshot
    ///
    /// Decided once per cycle, by `initial_mark` (which keeps the set in
    /// `tams_starts` only when the flag is on then). Phase 2 then uses the set
    /// `initial_mark` walked (G1's rule: objects above
    /// TAMS are implicitly live and are not traced by concurrent marking). An
    /// address outside it — an object allocated after the initial mark — is
    /// then not markable in Phase 2, so the slice neither marks nor traces it.
    /// Why that loses nothing the snapshot-at-the-beginning invariant needs:
    ///
    /// * such an object is not sweep-eligible (the eligibility snapshot IS the
    ///   initial mark's walk, only narrowed at remark), so it survives this
    ///   cycle whether it is marked or not;
    /// * every object that existed at the initial mark and was reachable then
    ///   is marked through the snapshot's own edges: from a root, from the
    ///   young→old seeds (every young object's old referents at the pause —
    ///   a young holder promoted later was seeded while it was young), or,
    ///   when a mutator later removes the old edge it was reached by, through
    ///   the SATB log of the overwritten value;
    /// * the remark still walks the CURRENT generation (`remark_begin` calls
    ///   [`Self::object_starts_for`]), re-marks the roots and the young→old
    ///   seeds against it and drains inside the pause, so an object reachable
    ///   only through a post-snapshot holder that a root or a young object
    ///   still names is traced there exactly as today.
    ///
    /// What it gives up is defence in depth for one shape: an old object
    /// reachable ONLY through a post-snapshot holder P that an already-scanned
    /// (black) old object names and no root or young object does, whose
    /// snapshot-time path was removed WITHOUT an SATB log. Today a slice
    /// after the promotion marks P and traces it; with the flag only a sound
    /// barrier covers it. Opt-in until the gce triage (2026-09-29) flipped it on after clean batteries; `=0` restores the per-promotion re-walk.
    ///
    /// The epoch check at the top of every slice is unchanged: a free or a
    /// compaction since the initial mark still abandons the cycle, so no
    /// snapshot address can name a recycled block.
    fn phase2_object_starts(&self, old_gen: &OldGen) -> Arc<OldGenObjectStarts> {
        // `Some` only when the flag was on at this cycle's initial mark.
        if let Some(starts) = self.tams_starts.lock().as_ref() {
            self.state
                .census
                .phase2_tams_slices
                .fetch_add(1, Ordering::Relaxed);
            return Arc::clone(starts);
        }
        let (starts, walked) = self.object_starts_for_noting(old_gen);
        if let Some(took) = walked {
            let c = &self.state.census;
            c.phase2_start_walks.fetch_add(1, Ordering::Relaxed);
            c.phase2_walk_us.fetch_add(
                u64::try_from(took.as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        starts
    }

    /// Record a fresh walk: count it, and latch `walk_desynced` if it did not
    /// cover every allocated byte.
    fn note_walk(&self, starts: &OldGenObjectStarts) {
        CONC_MARK_WALKS_BUILT.fetch_add(1, Ordering::Relaxed);
        if !starts.walk_complete() {
            self.walk_desynced.store(true, Ordering::Relaxed);
        }
    }

    /// Abort the cycle WITHOUT sweeping — the mark bitmap is not final (e.g.
    /// the remark STW could not be acquired). Deactivates the SATB barrier
    /// (discarding the drained entries — they only matter to a sweep that is
    /// no longer happening) and returns the phase to Idle so the write
    /// barrier stops logging. The next `old_gen_needs_gc` trigger starts a
    /// fresh cycle.
    pub fn abort_cycle(&self) {
        // gen r4/mark (2026-09-23) — CLOSE THE MUTATOR GATE FIRST, exactly as
        // `remark` was fixed to do on 2026-09-20. This used to deactivate the
        // queue first and return the phase to Idle last, and an abort runs
        // OUTSIDE any pause: for that whole window mutators passed the phase
        // gate (`is_marking_active()`, and the still-armed JIT gate) and had
        // their log refused by the INACTIVE queue. Harmless since round 2 made
        // the refusal a drop, but it breaks `is_marking_active() =>
        // satb_queue.is_active()` and bumps `SatbQueue::late_log_drops`, whose
        // contract is "zero unless some caller's gate is WIDER than the
        // queue's" — which is the one signal that would catch a real ordering
        // regression. The post-conditions are unchanged.
        // gcd d9/e: an OPENED cycle (past its initial mark) being abandoned,
        // for `[GC] conc_cycles: conccyc_abandoned`.
        if self.state.cycle_in_progress() {
            self.state.note_cycle_abandoned();
        }
        self.state.set_phase(ConcurrentGcPhase::Idle);
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
        // gen r4w2/concmark: and the cached walk, which is up to 1/64th of the
        // old generation and describes a layout no one will ask about again.
        *self.starts_cache.lock() = None;
        *self.tams_starts.lock() = None;
        // gen r4w3/oldgen3: a sweep abandoned between two slices, and an
        // overflow rescan abandoned part-way. Neither may be resumed by
        // anything: the resume offsets describe this cycle's layout.
        *self.sweep_progress.lock() = None;
        *self.overflow_rescan.lock() = OverflowRescan::default();
        // gen r5w1/refs5: no sweep will run, so no hidden referent can be
        // freed; the skip set describes nothing any more.
        self.disarm_reference_skip();
        // gen r5w3/unload7: nor will anything be scanned again.
        self.disarm_class_unload();
        // gen r4w4/concmark4: an abandoned cycle measured nothing the start
        // policy can use (its growth sample would stop at an arbitrary point).
        self.state.note_cycle_dropped();
        // No other cycle can have opened on the shared state yet, although the
        // phase is already `Idle`: the next cycle's `initial_mark` (which is
        // what re-activates the queue this call just deactivated) runs inside a
        // stop-the-world pause, and that pause cannot complete until THIS
        // thread reaches a safepoint — which it does not do inside this call.
        // A driver that opened in between (`try_open_cycle`) is still waiting
        // for that pause.
    }

    /// Mark the cycle complete after the sweep: phase back to Idle.
    ///
    /// gen r4w2/concmark (2026-09-23): only FROM `ConcurrentSweep`. The sweep
    /// already returns the phase to `Idle` on every path, and that store is
    /// the hand-off to the next cycle (see [`ConcurrentCycle`]); a blind
    /// `Idle` store here used to be able to land after another driver had
    /// opened the next cycle, reopening the shared state under it so that a
    /// THIRD driver could open concurrently with it. Now it can only complete
    /// a cycle that is still this one.
    pub fn finish_cycle(&self) {
        // gci i13/cm item 1: after the hand-off the phase is not ours to move
        // (the CAS would already fail unless the NEXT cycle sat in
        // `ConcurrentSweep`, which it must not be closed out of by us).
        if self.handed_off.load(Ordering::Acquire) {
            return;
        }
        let _ = self
            .state
            .cas_quiet_phase(ConcurrentGcPhase::ConcurrentSweep, ConcurrentGcPhase::Idle);
    }

    /// Phase 1: Initial Mark (called during brief STW pause).
    ///
    /// Marks objects directly reachable from roots. Only marks old-gen objects;
    /// young-gen objects are handled by the minor GC.
    ///
    /// Returns the number of root objects marked.
    pub fn initial_mark(&self, roots: &[*mut u8], old_gen: &OldGen) -> usize {
        // gci i13/cm item 1: a (test-)reused marker owns the state again.
        self.handed_off.store(false, Ordering::Release);
        self.state.set_phase(ConcurrentGcPhase::InitialMark);
        self.bitmap.clear();
        self.queue.clear();
        // gen r4w3/oldgen3: a fresh cycle starts with no rescan in flight and
        // its full budget of concurrent overflow passes; any sweep progress
        // left over belongs to a cycle that is over.
        *self.overflow_rescan.lock() = OverflowRescan::default();
        *self.sweep_progress.lock() = None;

        // Arm the barrier BEFORE anything is marked. `set_phase`'s own doc
        // states the rule this follows -- a logging gate must be armed for a
        // SUPERSET of the interval in which it is required, never a subset --
        // and the queue gate is one such gate. Activating after the root scan
        // was safe only because this runs at a stop-the-world; arming first
        // costs nothing and removes the dependence.
        self.satb_queue.activate();

        // gen r4w2/concmark: ONE walk, two sets. `object_starts` becomes the
        // sweep-eligibility snapshot below and is narrowed at remark. An
        // independent word copy seeds the immutable slice cache, so the
        // first Phase-2 slice (which usually follows this pause directly) does
        // not walk the generation again.
        self.walk_desynced.store(false, Ordering::Relaxed);
        let object_starts = OldGenObjectStarts::build(old_gen);
        self.note_walk(&object_starts);
        let cached = Arc::new(object_starts.duplicate());
        // gcd d9/e: under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS` (decided once
        // per cycle, here) the same immutable set is the TAMS snapshot Phase 2
        // judges against (`phase2_object_starts`); no extra walk.
        *self.tams_starts.lock() = gen_conc_mark_tams_starts_enabled().then(|| Arc::clone(&cached));
        *self.starts_cache.lock() = Some(ObjectStartsCache {
            base: old_gen.base_ptr() as usize,
            seq: old_gen.free_list_seq(),
            starts: cached,
        });
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
        *self.sweep_eligible.lock() = Some(Arc::new(object_starts));
        *self.initial_mark_epoch.lock() = Some(old_gen.reclaim_epoch());
        *self.sweep_eligible_epoch.lock() = None;

        // gen r4w4/concmark4: the start policy's growth sample is measured
        // from here to the sweep's end (see `OldGenPolicy`).
        self.state
            .note_cycle_open(old_gen_allocated_total(old_gen), old_gen.used());

        self.state.set_phase(ConcurrentGcPhase::ConcurrentMark);
        count
    }

    /// [`Self::initial_mark`], then — still inside the initial-mark pause —
    /// SCAN every old-gen object in `frozen`: the objects held in the
    /// registers and stacks of threads the xt takeover FROZE for this pause.
    ///
    /// gen r4w4/concmark4 (2026-09-24) — closes
    /// `docs/feature-designs/jit-ir-optimizations.md`.
    ///
    /// # The window
    ///
    /// Compiled reference stores test the JIT SATB gate once and skip the
    /// pre-barrier when it reads clear; nothing between that test and the
    /// store is a safepoint, so a thread that ARRIVES at a pause can never be
    /// between them. A thread the takeover FREEZES can: it tested the gate
    /// before this pause armed it, and after the pause it completes
    /// `O.f = B` without logging `A`, the value `O.f` held at the snapshot. If
    /// a second mutator has meanwhile copied `A` into an object the trace has
    /// already scanned, `A` is reachable, unmarked, and swept.
    ///
    /// # Why scanning `O` inside the pause closes it
    ///
    /// SATB's obligation is to mark everything reachable AT THE SNAPSHOT, and
    /// the one value the frozen store can hide is `O.f`'s value at the
    /// snapshot — which is still in `O` while the world is stopped. Scanning
    /// `O` here greys `A` (and every other reference `O` holds) before the
    /// frozen thread can overwrite anything, so the unlogged store loses
    /// nothing. `O` is in `frozen`: both gated emitters
    /// (`emit_gated_compact_ref_putfield`, the gated `aastore`) load the
    /// receiver into RAX from its frame slot BEFORE the gate test and store
    /// through it, so the receiver sits in a register and in the frame for the
    /// whole window, and the takeover's conservative scan reports both (a
    /// derived pointer resolved to its base, `CRATONVM_XT_TAKEOVER_INTERIOR`).
    /// gci i13/cm item 4: the optimizing tier's gated stores keep the same
    /// contract and are covered by the same argument -- `jit/src/ir_lower.rs`
    /// `emit_gated_ir_ref_putfield` and `emit_gated_ir_aastore` load the
    /// receiver / array into RAX before the `pre` byte test and store through
    /// it, and their inline card arms (`emit_ir_putfield_card_arm`,
    /// `emit_ir_aastore_card_arm`, which re-test `pre` right before the store)
    /// keep RAX on every edge. A new gated emitter must do the same, or this
    /// scan no longer closes its window.
    /// A YOUNG `O` needs nothing: its old-gen referents are already initial-mark
    /// roots (`collect_young_to_old_roots`; under `CRATONVM_GEN_Y2O_LIVE_SEED`
    /// the live young set, which contains `O` because the frozen words that
    /// name it are in the pause's root list).
    ///
    /// No cycle is refused and no thread is waited for, so a JIT-heavy run
    /// that freezes someone at every pause (the starvation the page feared for
    /// its "refuse on takeover" option) costs only the scan of the few objects
    /// the frozen threads hold. `CRATONVM_GEN_CONC_NO_FROZEN_EAGER_SCAN` turns
    /// it off (bisection only). Returns [`Self::initial_mark`]'s count.
    pub fn initial_mark_with_frozen(
        &self,
        roots: &[*mut u8],
        frozen: &[*mut u8],
        old_gen: &OldGen,
    ) -> usize {
        let count = self.initial_mark(roots, old_gen);
        // gen r5w3/unload7: with class unloading armed, the rows of the owners
        // the trace will never scan (young loaders) are marked now, inside the
        // pause; see `mark_rows_of_live_owners`. One load when unarmed.
        if self.class_unload_armed() {
            let object_starts = self.object_starts_for(old_gen);
            self.mark_rows_of_live_owners(&object_starts);
        }
        if frozen.is_empty() || cratonvm_gc_core::gc_flags().gen_conc_no_frozen_eager_scan {
            return count;
        }
        // The walk `initial_mark` just cached: same lock hold, no allocation
        // in between, so this is a cache hit, not a second walk.
        let object_starts = self.object_starts_for(old_gen);
        let mut scanned = 0u64;
        for &obj_ptr in frozen {
            if !markable_old_object(obj_ptr, &object_starts) {
                continue;
            }
            // Marked (it is a root, so usually already by `initial_mark`) and
            // scanned NOW. If it was not yet marked it is black after this
            // scan and need not be queued; if it was, the queued copy is
            // scanned again later, which only re-marks what is marked.
            let _ = self.bitmap.try_mark(obj_ptr as usize);
            if !self.scan_object(obj_ptr, old_gen, &object_starts, true) {
                // Fail closed exactly as the trace does.
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            scanned += 1;
        }
        self.state.note_frozen_objects_scanned(scanned);
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
    /// This entry point is the collector-side half of the fix. The driver
    /// (`maybe_concurrent_gc_at` in `vm/src/runtime/interpreter/gc_and_alloc.rs`,
    /// gen r4w2/concmark 2026-09-23) now loops
    /// `lock → slice → unlock → safepoint poll` and so bounds, by the slice
    /// budget rather than by the size of the heap, both the time any promoting
    /// thread waits for the old-gen lock and the time any other thread's pause
    /// waits for the marker to arrive. `CRATONVM_GEN_CONC_MARK_SLICE=0`
    /// restores the single unbounded call.
    ///
    /// # What the caller must know about slicing
    ///
    /// * **The object-start set must describe the generation as it is at THIS
    ///   slice.** An object promoted between two slices has to be markable in
    ///   the next one: refusing to enqueue it means refusing to scan its
    ///   children, one of which may be an older object that IS sweep-eligible
    ///   and would then be freed while live. This is the correctness question
    ///   the whole-phase lock was masking.
    /// * **It is cached, not rebuilt, while that is provably the same set**
    ///   (gen r4w2/concmark): keyed on `OldGen::free_list_seq`, which every
    ///   promotion and every other allocation or free moves. A slice after a
    ///   young GC that promoted anything therefore still rebuilds; a slice
    ///   after one that did not, or after no pause at all, reuses the previous
    ///   walk instead of paying O(old gen) again. See `starts_cache`.
    /// * The cycle is still abandoned at remark if `reclaim_epoch` moved, so a
    ///   `free` or `compact` landing between slices fails the cycle closed
    ///   exactly as one landing anywhere else in the cycle does; and a slice
    ///   that finds the epoch moved drops the queue and reports the closure
    ///   complete (see [`Self::cycle_is_stale`]).
    ///
    /// A `budget` of 0 is raised to 1, so a caller cannot construct a loop
    /// that makes no progress.
    pub fn concurrent_mark_budget(&self, old_gen: &OldGen, budget: usize) -> (usize, bool) {
        let _phase = cratonvm_jfr::phase::enter(cratonvm_jfr::phase::Category::GcConcurrent);
        let budget = budget.max(1);
        // gen r4/mark (2026-09-23): stop tracing a cycle that is already dead.
        //
        // The queue holds bare addresses pushed by earlier slices (and by
        // `initial_mark`). If another old-gen collector freed or slid storage
        // since the cycle opened, `reclaim_epoch` has moved, `remark` is
        // guaranteed to fail the cycle closed, and every queued address may now
        // name a recycled block or a stale free-list header. Scanning them is
        // pure waste at best; at worst an incoherent stale header trips
        // `scan_object`'s refusal and degrades the slice to `mark_all_old_gen`
        // — an O(old gen) pass for a cycle nobody will sweep. A cycle that
        // never went through `initial_mark` (tests that seed the queue by hand)
        // has no epoch and is left alone.
        if self.cycle_is_stale(old_gen) {
            self.queue.clear();
            // gen r4w3/oldgen3: a rescan cursor is an offset into a layout
            // that no longer exists.
            self.overflow_rescan.lock().cursor = None;
            return (0, true);
        }
        let mut rescan = self.overflow_rescan.lock();
        // gen r4w3/oldgen3 — drain the SATB log in Phase 2, not only at remark
        // (`docs/feature-designs/jit-barrier-lowering.md`,
        // whose proposed fix names this file's owner). The shards are
        // unbounded, so a mutation-heavy cycle grew them by eight bytes per
        // logged store for its whole length and remark then marked all of it
        // inside the pause. Draining with `drain()` while the queue stays
        // ACTIVE is exactly remark's first pass, run earlier: every entry is
        // an old value some mutator overwrote since initial mark, marking it
        // is conservative, and whatever is logged after this drain is still
        // in the queue for remark. Only while the queue is active (a cycle is
        // marking) — `has_pending` is one lock-free load.
        let satb_pending = self.satb_queue.is_active() && self.satb_queue.has_pending();
        // gen r4w2/concmark: an empty queue needs no object-start set, and a
        // driver that re-enters Phase 2 after losing its remark pause usually
        // finds exactly that. Same result as the pop below returning `None`.
        // gen r4w3/oldgen3: unless an overflow rescan or the SATB log still
        // has work for this slice.
        if self.queue.is_empty()
            && !satb_pending
            && !Self::overflow_rescan_due(&self.queue, &rescan)
        {
            return (0, true);
        }
        let mut scanned = 0usize;
        // gen r4w2/concmark: cached across slices while `free_list_seq` is
        // unchanged; rebuilt (and so still covering anything promoted since the
        // previous slice) the moment it moves. See `starts_cache`.
        // gcd d9/e: through `phase2_object_starts`, which counts those walks
        // and, under `CRATONVM_GEN_CONC_MARK_TAMS_STARTS`, uses the initial
        // mark's snapshot instead.
        let object_starts = self.phase2_object_starts(old_gen);
        if satb_pending {
            // The same admission `remark` applies to its SATB entries. The
            // epoch was checked above, so every old-gen address in the log
            // still names the object it named when it was logged.
            let entries = self.satb_queue.drain();
            self.state
                .census
                .satb_drained_in_mark
                .fetch_add(entries.len() as u64, Ordering::Relaxed);
            // gen r4w6/concsvc6: how many of them the barrier need never have
            // logged (outside the old generation; `markable_old_object`
            // rejects every one). A range test, counted only.
            let mut outside = 0u64;
            for addr in entries {
                let ptr = addr as *mut u8;
                outside += u64::from(!old_gen.contains(ptr as *const u8));
                if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                    self.queue.push(ptr);
                }
            }
            if outside != 0 {
                self.state
                    .census
                    .satb_outside_old_gen
                    .fetch_add(outside, Ordering::Relaxed);
            }
        }

        // gen r4w4/concmark4 — the marker-local stack (costs page item 4): the
        // slice pops and pushes through a private LIFO `Vec`, touching the
        // sharded queue only to refill an empty stack or to spill. Whatever is
        // left when the budget runs out is spilled back below, so between
        // slices (and for the stale check, remark and `abort_cycle`) the queue
        // is again the whole grey set.
        let use_local = !cratonvm_gc_core::gc_flags().gen_conc_mark_no_local_stack;
        let mut local = self.local_stack.lock();
        debug_assert!(local.is_empty());
        while scanned < budget {
            if use_local {
                let (n, degraded) =
                    self.drain_local(old_gen, &object_starts, &mut local, budget - scanned, false);
                scanned += n;
                if degraded {
                    // Degraded path: every old-gen object is marked for this
                    // cycle, so there is nothing left to slice.
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    rescan.cursor = None;
                    return (scanned, true);
                }
                if scanned >= budget {
                    break;
                }
                // Both the stack and the queue are empty: fall through to the
                // overflow rescan, exactly where the queue arm's `pop` found
                // nothing.
            } else if let Some(obj_ptr) = self.queue.pop() {
                if !self.scan_object(obj_ptr, old_gen, &object_starts, false) {
                    // Degraded path: every old-gen object is marked for this
                    // cycle, so there is nothing left to slice.
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    rescan.cursor = None;
                    return (scanned, true);
                }
                scanned += 1;
                continue;
            }

            // gen r4w3/oldgen3 — MARK-STACK OVERFLOW IS RECOVERED HERE, NOT
            // ONLY IN THE REMARK PAUSE (item 2 of
            // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`).
            //
            // A `push` dropped at `MARK_QUEUE_SHARD_CAP` leaves an object MARKED
            // but never scanned, and `drain_closure` repairs that with up to
            // eight O(old gen) rescans of every marked object — inside the
            // remark pause, the one place this cycle is supposed to be brief.
            // The same rescan is sound here: rescanning a marked object while
            // mutators run can only find more to mark, and anything a mutator
            // hides from it by overwriting a slot is in the SATB log remark
            // drains. So when a slice empties the queue with the flag set,
            // clear the flag and walk the generation in budgeted pieces
            // (`OldGen::walk_objects_from`), rescanning marked objects and
            // draining what they push, and only report the closure complete
            // once a whole pass has run with no new overflow.
            //
            // Why a pass started after the flag was cleared is enough: every
            // object dropped BEFORE it is marked and does not move (the cycle
            // aborts above if anything frees or slides old-gen storage), so the
            // pass reaches it; every object dropped DURING it sets the flag
            // again, which starts another pass here or — once
            // `CONCURRENT_OVERFLOW_RESCAN_PASSES` are spent — is left set for
            // remark's own fallback, exactly as before. Remark also re-arms the
            // flag if it ever finds a pass part-way (see
            // `remark_with_reference_processing`).
            if rescan.cursor.is_none() {
                if !Self::overflow_rescan_due(&self.queue, &rescan) {
                    return (scanned, true);
                }
                // Reset before the pass so an overflow during it is observable
                // — the same protocol `drain_closure` follows.
                self.queue.overflowed.store(false, Ordering::Relaxed);
                rescan.passes += 1;
                rescan.cursor = Some(0);
                self.state
                    .census
                    .overflow_rescan_passes
                    .fetch_add(1, Ordering::Relaxed);
            }
            let from = rescan.cursor.unwrap_or(0);
            let mut visited = 0usize;
            let mut degraded = false;
            let next = old_gen.walk_objects_from(from, budget - scanned, |obj_ptr, _size| {
                visited += 1;
                if !degraded
                    && self.bitmap.is_marked(obj_ptr as usize)
                    && !self.scan_object(obj_ptr, old_gen, &object_starts, false)
                {
                    degraded = true;
                }
            });
            // Every object walked is work done, marked or not.
            scanned += visited.max(1);
            if degraded {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                rescan.cursor = None;
                return (scanned, true);
            }
            // `Some` = the budget ran out part-way (the loop ends); `None` =
            // the pass finished, and the loop drains what it pushed before
            // deciding whether another pass is due.
            rescan.cursor = next;
        }

        // gen r4w4/concmark4: the budget ran out with grey objects still on
        // the local stack — hand them back to the shared queue.
        self.spill_local(&mut local);
        let done = self.queue.is_empty()
            && rescan.cursor.is_none()
            && !Self::overflow_rescan_due(&self.queue, &rescan);
        (scanned, done)
    }

    /// gen r4w3/oldgen3 — should a Phase-2 slice start (or continue) a
    /// concurrent overflow rescan? A pass part-way through always continues;
    /// a new one starts while the flag is set and the per-cycle pass budget
    /// is not spent.
    fn overflow_rescan_due(queue: &MarkQueue, rescan: &OverflowRescan) -> bool {
        rescan.cursor.is_some()
            || (queue.has_overflowed() && rescan.passes < CONCURRENT_OVERFLOW_RESCAN_PASSES)
    }

    /// gen r4w4/concmark4 — scan grey objects through the marker-local stack
    /// `local`, refilled one object at a time from the shared queue when it is
    /// empty, until both are empty or `budget` objects have been scanned.
    /// Returns `(objects scanned, degraded)`; `degraded` means a scan refused
    /// an object (the caller marks everything, as with the queue).
    ///
    /// # Why a local stack (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 4)
    ///
    /// Every `MarkQueue::push` takes a shard mutex and a `fetch_or` on the
    /// non-empty mask, and every `pop` a mutex, a `fetch_and` and a
    /// thread-local cursor bump — per grey object, on a marker that is single-
    /// threaded in every production drain. Here a child goes on a `Vec` and
    /// comes off it: no lock, no RMW. The queue stays the spill space, so its
    /// cap, its overflow flag and the overflow rescan keep their meaning:
    /// past [`LOCAL_MARK_STACK_CAP`] entries a child is pushed to the queue
    /// instead (counted in `mark_local_spills`), and a caller that stops with
    /// entries left calls [`Self::spill_local`].
    ///
    /// LIFO (depth-first) rather than the queue's FIFO: the frontier — and so
    /// the memory, and the overflow risk the round-2 note worried about — is
    /// bounded by the graph's depth × fan-out instead of its breadth. The
    /// fixed point is the same: a child is marked before it is pushed, on
    /// either path, so the set of objects scanned does not depend on order.
    ///
    /// `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK` restores the queue-only drain.
    fn drain_local(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        local: &mut Vec<usize>,
        budget: usize,
        in_pause: bool,
    ) -> (usize, bool) {
        let mut scanned = 0usize;
        let mut spills = 0u64;
        let mut degraded = false;
        // gcf f4/c (`gengc-r5w3-unload7-proposal-side-table-lookups-per-drain-not-per-object`):
        // the class-unload tables' read guard once per drain, not once per
        // scanned object, with the view's miss filter. The tables are written
        // only inside the cycle's pauses (`set_class_unload_tables`,
        // `add_class_unload_tables`, `disarm_class_unload`), and a pause cannot
        // begin while this thread drains (no safepoint poll in this loop), so
        // the guard blocks no writer that could otherwise run. Nothing below
        // reads `class_unload` again (`push_class_unload_edges_with` takes no
        // lock). `CRATONVM_GEN_CONC_UNLOAD_PER_OBJECT_LOOKUP=1` restores the
        // per-object read (the A/B arm).
        let guard = (self.class_unload_armed()
            && false)
        .then(|| self.class_unload.read());
        let mut unload = match guard.as_ref() {
            // gcg g1/c: the bounds cached by the writers, not a fresh walk of
            // every row per drain (`class_unload_bounds`).
            Some(g) => UnloadEdgeAccess::Held {
                view: g
                    .as_ref()
                    .map(|t| ClassUnloadView::with_bounds(t, *self.class_unload_bounds.lock())),
                newly: 0,
            },
            None => UnloadEdgeAccess::PerObject,
        };
        while scanned < budget {
            let Some(obj_ptr) = local.pop().map(|p| p as *mut u8).or_else(|| self.queue.pop()) else {
                break;
            };
            let queue = &self.queue;
            let ok = self.scan_object_into_with(
                obj_ptr,
                old_gen,
                object_starts,
                &mut unload,
                in_pause,
                &mut |p| {
                    if local.len() < LOCAL_MARK_STACK_CAP {
                        local.push(p as usize);
                    } else {
                        spills += 1;
                        queue.push(p);
                    }
                },
            );
            if !ok {
                local.clear();
                degraded = true;
                break;
            }
            scanned += 1;
        }
        if let UnloadEdgeAccess::Held { newly, .. } = unload {
            self.state.note_class_unload_edges(newly);
        }
        drop(guard);
        self.state.note_mark_local_spills(spills);
        (scanned, degraded)
    }

    /// gen r4w4/concmark4 — hand every entry left on a marker-local stack
    /// back to the shared queue (each is marked and unscanned; a push the
    /// queue drops at its cap sets the overflow flag, as for any push).
    fn spill_local(&self, local: &mut Vec<usize>) {
        let n = local.len() as u64;
        for p in local.drain(..) {
            self.queue.push(p as *mut u8);
        }
        self.state.note_mark_local_spills(n);
    }

    /// gen r4w4/concmark4 — drain the queue to its closure with no budget,
    /// through the local stack unless `CRATONVM_GEN_CONC_MARK_NO_LOCAL_STACK`.
    /// `None` = a scan refused an object (the caller degrades). For the remark
    /// pause, where the per-object queue cost is pause time.
    fn drain_unbounded(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> Option<usize> {
        if !cratonvm_gc_core::gc_flags().gen_conc_mark_no_local_stack {
            let mut local = self.local_stack.lock();
            debug_assert!(local.is_empty());
            let (n, degraded) = self.drain_local(old_gen, object_starts, &mut local, usize::MAX, true);
            return (!degraded).then_some(n);
        }
        let mut n = 0usize;
        while let Some(obj_ptr) = self.queue.pop() {
            if !self.scan_object(obj_ptr, old_gen, object_starts, true) {
                return None;
            }
            n += 1;
        }
        Some(n)
    }

    /// Phase 3: Remark (called during brief STW pause).
    ///
    /// Processes all SATB buffer entries and re-scans roots to catch any
    /// references modified during the concurrent mark phase.
    ///
    /// Returns the number of objects the remark scanned — no longer "the
    /// additional objects discovered", which it double-counted (see
    /// [`Self::remark_with_reference_processing`]).
    ///
    /// gen r4w5/concmark5 (2026-09-24): the Generational-side hook for
    /// remark-time reference processing
    /// (`docs/feature-designs/generative-gc-architecture.md`).
    /// With `CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK` set, this remark goes
    /// through the reference-processing seam with a NO-OP callback: it is
    /// called at the seam's place in the pause (first fixed point, barrier
    /// active, phase `Remark`), counts itself (`remark_refproc_hook_calls`),
    /// clears nothing and keeps nothing; the bitmap is exactly the unhooked
    /// one. Unset — the default — this is the plain remark.
    ///
    /// gen r5w1/refs5: this entry point is now tests-only. The VM driver
    /// (`gen_concurrent_remark_pause`) has called the seam directly since
    /// gc-common w8-c (its JNI weak-global callback), so this no-op hook never
    /// ran in a real VM and the wave-5 probe's `concdrv_remark_refproc_hook_calls`
    /// stayed 0. Under the same flag the driver now runs the REAL remark-time
    /// reference processing (skip set at initial mark, callback at remark),
    /// which counts itself through [`ConcurrentGcState::note_remark_refproc_retired`].
    pub fn remark(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
    ) -> usize {
        if cratonvm_gc_core::gc_flags().gen_conc_remark_refproc_hook {
            let state = &self.state;
            let mut hook = |_is_marked: &dyn Fn(usize) -> bool| -> Vec<usize> {
                state.note_remark_refproc_hook();
                Vec::new()
            };
            let hook_dyn: &mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize> = &mut hook;
            return self.remark_with_reference_processing(stw, roots, old_gen, Some(hook_dyn));
        }
        self.remark_with_reference_processing(stw, roots, old_gen, None)
    }

    /// [`Self::remark`] with a remark-time reference-processing seam
    /// (gen r4w2/concmark, 2026-09-23; step 1 of
    /// `gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md`).
    ///
    /// `process`, when given, is called ONCE, inside the pause, after the
    /// closure over the SATB log and the fresh roots has reached its first
    /// fixed point and while the SATB barrier is still ACTIVE, with the cycle's
    /// liveness verdict: `is_marked(addr)` is "the bit is set in this cycle's
    /// bitmap, OR `addr` is not sweep-eligible" (young, allocated after the
    /// initial mark, or outside the old generation — none of which this cycle
    /// may judge dead). It is the same shape as G1's
    /// `VmHeap::g1_final_remark_and_cleanup` callback. The addresses it returns
    /// (policy-retained soft referents, finalizable objects about to be
    /// resurrected, submitted cleaner actions) are marked and their closure is
    /// drained before the phase advances, so they are in the bitmap
    /// `concurrent_sweep` is handed. A referent clear the callback performs
    /// therefore still runs under an active barrier; it should use the
    /// SATB-suppressed store, exactly as G1's does.
    ///
    /// Not invoked when this remark refuses the sweep (epoch moved, walk
    /// desynced): nothing can be judged dead on such a cycle.
    ///
    /// # Lock order
    ///
    /// The caller's callback will typically hold the VM's reference-processor
    /// lock while it queries `is_marked`. An immutable eligibility snapshot
    /// is taken before calling the processor; no marker lock is held across
    /// its call and each query needs no eligibility lock.
    ///
    /// The CALLER's old-gen guard (`old_gen`) is held across the callback
    /// here. A callback that may take the old-gen lock itself (any heap
    /// accessor that locks the generation, e.g. `is_heap_addr` or the soft
    /// policy's `soft_ref_policy_free_mb`) must use the
    /// [`Self::remark_begin`] / [`Self::remark_finish`] pair instead and run
    /// with the guard released (gen r5w1/refs5; the design page's option (b)).
    ///
    /// # Return value
    ///
    /// The objects this remark SCANNED (each grey object once; an overflow
    /// rescan pass counts every marked object it rescans), plus the walk-gap
    /// seeds a recovering remark marked. gen r5w1/refs5: it used to add one
    /// per newly marked SATB entry, root and keep address ON TOP of the scan
    /// count, which scans those same objects again — a double count
    /// (`gengc-r4-mark-counters-and-comments-that-lie`).
    pub fn remark_with_reference_processing(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
        process: Option<&mut dyn FnMut(&dyn Fn(usize) -> bool) -> Vec<usize>>,
    ) -> usize {
        let progress = self.remark_begin(stw, roots, old_gen);
        let keep = match process {
            Some(process) if progress.sweep_authorised() => {
                let is_marked = self.remark_liveness();
                Some(process(&is_marked))
            }
            _ => None,
        };
        self.remark_finish(stw, progress, keep.as_deref(), old_gen)
    }

    /// gen r5w1/refs5 — the seam's liveness verdict, for a callback that runs
    /// between [`Self::remark_begin`] and [`Self::remark_finish`]: the bit is
    /// set in this cycle's bitmap, OR `addr` is not sweep-eligible (young,
    /// allocated after the initial mark, or outside the old generation — none
    /// of which this cycle may judge dead). For a batch of queries use
    /// `remark_liveness` to acquire the immutable eligibility view once.
    /// Needs no old-gen lock: only the pause's driver changes the mark bitmap.
    pub fn remark_is_marked(&self, addr: usize) -> bool {
        self.remark_liveness()(addr)
    }

    /// One immutable eligibility view for a batch of liveness queries between
    /// remark_begin and remark_finish, inside the same pause. Drops the mutex
    /// before any caller takes the reference-processor/old-gen locks. The live
    /// mark bitmap remains shared, so newly retained referents are visible.
    pub fn remark_liveness(&self) -> impl Fn(usize) -> bool + '_ {
        let eligible = self.sweep_eligible.lock().clone();
        move |addr| self.bitmap.is_marked(addr)
            || !eligible.as_ref().is_some_and(|e| e.contains(addr))
    }

    /// gen r5w1/refs5 — the FIRST half of the remark: everything
    /// [`Self::remark_with_reference_processing`] does before its callback
    /// (phase `Remark`, the TAMS narrowing and the sweep authorisation, the
    /// SATB drain with the queue left ACTIVE, the root rescan, the first
    /// closure, the dead-finalizer retention). Hand the result to
    /// [`Self::remark_finish`] — inside the same pause; the barrier stays
    /// active in between.
    ///
    /// # Why the remark can be split around the callback
    ///
    /// So the VM can run remark-time reference processing with the old-gen
    /// guard RELEASED: its consumers call heap accessors that take the
    /// old-gen lock (a non-reentrant mutex), and it adds an `old-gen →
    /// ref_processor` edge the lock ranking never had. Releasing the guard
    /// inside the pause is sound: every counted mutator is parked, so only an
    /// uncounted thread (in the GC-blocked region) could reach the old
    /// generation in the gap, and what it could allocate lies above the
    /// initial-mark snapshot (not sweep-eligible, so implicitly live); nothing
    /// frees old-gen storage outside a collection, and a free would move
    /// `reclaim_epoch`, which the sweep re-checks. The object-start set taken
    /// here is reused by the second half for the same reason.
    pub fn remark_begin(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        roots: &[*mut u8],
        old_gen: &OldGen,
    ) -> RemarkProgress {
        self.state.set_phase(ConcurrentGcPhase::Remark);
        // gcd d9/e: the marking phase's length (`[GC] conc_cycles:`).
        self.state.note_remark_reached();
        // gen r4w3/oldgen3: a concurrent overflow rescan that did not finish
        // (the driver reached remark without a slice reporting the closure
        // complete — no production path does, but a caller may) cleared the
        // overflow flag at its start, so `drain_closure` below would not know
        // objects may still be marked-but-unscanned. Re-arm the flag: the
        // pause then runs its own full rescan, as it did before rescans could
        // run concurrently.
        {
            let mut rescan = self.overflow_rescan.lock();
            if rescan.cursor.take().is_some() {
                self.queue.overflowed.store(true, Ordering::Relaxed);
            }
        }
        let mut scanned = 0;
        // gen r4w2/concmark: the last slice's walk when nothing was allocated
        // or freed in the old generation since it (see `starts_cache`), which
        // takes an O(old gen) walk out of the pause in the common case.
        let mut object_starts = self.object_starts_for(old_gen);
        // gen r4w6/oldpin6: over a desynced walk, an opt-in plan to sweep
        // anyway (`remark_walk_gap_plan`); its fresh grid replaces the cached
        // one for the narrowing and the closure below.
        let gap_plan = if self.walk_desynced.load(Ordering::Relaxed) {
            self.remark_walk_gap_plan(old_gen)
        } else {
            None
        };
        if let Some(plan) = gap_plan.as_ref() {
            object_starts = Arc::clone(&plan.starts);
        }
        let mut sweep_authorised = false;

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
                    .map(|e| Arc::make_mut(e).retain_intersection(&object_starts))
                    .unwrap_or(true);
            // gen r4w2/concmark — a desynced walk anywhere in this cycle means
            // some allocated objects were never in the object-start set, so
            // references to them were neither marked nor traced, and an
            // eligible object reachable only through one of them has a clear
            // bit while live. Refuse the sweep, exactly as `OldGen::compact`
            // refuses to slide over a walk that does not cover `used()`.
            // gen r4w6/oldpin6: ...unless the gap plan accounts for every
            // byte the walk missed; the closure below then seeds the gap words
            // and rescans every marked object (see `seed_remark_walk_gaps`).
            let walk_ok = !self.walk_desynced.load(Ordering::Relaxed) || gap_plan.is_some();
            if geometry_ok && !walk_ok && opened_at.is_some() {
                CONC_MARK_WALK_DESYNC_ABORTS.fetch_add(1, Ordering::Relaxed);
                tracing::info!(
                    eligible = eligible.as_ref().map_or(0, |e| e.len()),
                    "concurrent cycle will reclaim nothing: an old-gen object walk \
                     desynced this cycle (see WALK_DESYNC_HITS / SCAN_REGION_BREAK_HITS), \
                     so objects behind the break were never traced",
                );
            }
            // gen r5w1/refs5: a mark bitmap anchored at a base that is not
            // 8-aligned cannot hold a mark for any 8-aligned object (see
            // `with_shared`), so its "unmarked" means nothing. Never observed;
            // refused rather than trusted.
            let base_ok = self.old_gen_base % 8 == 0;
            if !base_ok {
                tracing::warn!(
                    base = self.old_gen_base,
                    "concurrent cycle will reclaim nothing: the mark bitmap's old-gen \
                     base is not 8-aligned, so no object could be marked",
                );
            }
            // gen r5w6/conc10 (old9's cross-lane request,
            // `gengc-r5w5-old9-concurrent-mark-bitmap-does-not-follow-old-gen-growth`):
            // the bitmap was sized from `old_gen_info()` BEFORE the initial-mark
            // pause took the lock, and nothing re-checked it. An in-place growth
            // in between (`OldGen::grow_after_refusal`, reachable only under
            // `CRATONVM_GC_OLD_BORROW_YOUNG`) leaves the grown span unmarkable
            // (`MarkBitmap::try_mark` refuses an address past its span) while
            // its objects are in the eligible snapshot: swept while live. Fail
            // closed, as for a misaligned base; the next cycle is built at the
            // new size. `remark_is_marked` answers from the same refusal (no
            // eligible set ⇒ everything live).
            let covers_ok = bitmap_covers_generation(
                self.old_gen_base,
                self.old_gen_size,
                old_gen.base_ptr() as usize,
                old_gen.capacity(),
            );
            if !covers_ok {
                self.state
                    .census
                    .bitmap_cover_refusals
                    .fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    bitmap_base = self.old_gen_base,
                    bitmap_size = self.old_gen_size,
                    old_gen_base = old_gen.base_ptr() as usize,
                    old_gen_capacity = old_gen.capacity(),
                    "concurrent cycle will reclaim nothing: the old generation grew (or \
                     moved) after this cycle's mark bitmap was sized, so objects past the \
                     bitmap's span could not be marked",
                );
            }
            let base_ok = base_ok && covers_ok;
            if geometry_ok && walk_ok && base_ok {
                *authorised = Some(epoch_now);
                sweep_authorised = true;
            } else {
                let stranded = eligible.as_ref().map_or(0, |e| e.len());
                // `!geometry_ok`: a desync refusal was counted above under its
                // own name and is not an epoch race.
                if !geometry_ok && stranded > 0 && opened_at.is_some() {
                    // `opened_at.is_some()` distinguishes the real race (a cycle
                    // WAS opened and another collector moved the ground under
                    // it) from a caller that reached `remark` without an
                    // `initial_mark` at all, which is a programming error, not
                    // an interleaving, and is already fatal to the sweep below.
                    self.note_sweep_epoch_abort();
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

        // gen r4w2/concmark — a refused remark does no marking
        // (`gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md` item 6).
        //
        // With no authorising stamp the sweep frees nothing and clears the
        // bitmap, so both closures below — the SATB drain and the
        // `drain_closure`s, which can be O(live old gen) and all run inside the
        // pause — computed a result nobody reads. Keep only the post-conditions
        // a completed remark establishes: phase `ConcurrentSweep` (mutator gate
        // closed FIRST, as below), SATB queue inactive and empty, mark queue
        // empty.
        if !sweep_authorised {
            self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
            let _ = self.satb_queue.deactivate_and_drain(stw);
            self.queue.clear();
            return RemarkProgress {
                authorised: false,
                scanned: 0,
                object_starts,
            };
        }
        // gen r4w6/oldpin6: the walk-gap recovery's marks, before any closure.
        if let Some(plan) = gap_plan.as_ref() {
            scanned += self.seed_remark_walk_gaps(old_gen, plan);
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
        //
        // gci i42/mk — the per-thread buckets FIRST. `drain()` sees only the
        // shards; a thread that did not pass `safepoint_check` on its way
        // into this pause (a peer the take-over FROZE, or one parked in the
        // GC-blocked region) still holds up to 255 overwritten references in
        // its own bucket. They used to reach the bitmap only through
        // `remark_finish`'s `deactivate_and_drain`, i.e. AFTER the two
        // verdicts this half hands out at its first fixed point: the
        // dead-finalizer retention below (a finalizable kept alive by a
        // bucket entry was reported for `finalize()` while reachable) and the
        // caller's reference-processing seam (a weak referent kept alive by
        // one was cleared). G1's `remark` flushes here for the same reason
        // (finding #18). Sound at this STW pause, the precondition the token
        // witnesses; `remark_finish`'s walk then finds the buckets empty.
        if gen_conc_remark_thread_satb_flush_enabled() {
            cratonvm_gc_core::satb::flush_all_thread_satb_buffers(&self.satb_queue);
        }
        let satb_entries = self.satb_queue.drain();
        for addr in satb_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }

        // Re-scan roots (some may have changed during concurrent mark).
        for &root_ptr in roots {
            if markable_old_object(root_ptr, &object_starts)
                && self.bitmap.try_mark(root_ptr as usize)
            {
                self.queue.push(root_ptr);
            }
        }
        // gen r5w3/unload7: the rows the remark's own root scan deferred, for
        // every owner already live (see `mark_rows_of_live_owners`); an owner
        // the closure below marks is scanned, and its scan follows them. No-op
        // unless class unloading is armed.
        self.mark_rows_of_live_owners(&object_starts);

        // Drain the queue fully (mark transitive closure from new roots),
        // including the overflow fallback. NOTE: the SATB barrier is STILL
        // ACTIVE at this point (we used `drain()` above, not
        // `deactivate_and_drain()`), so any reference a mutator overwrites
        // while we compute this closure is logged and will be captured by
        // the final drain below.
        scanned += self.drain_closure(old_gen, &object_starts);

        // gc-common w36-d — RETAIN THE DEAD FINALIZABLES. At this first fixed
        // point "unmarked and sweep-eligible" is "unreachable", so a published
        // finalizer candidate in that state died old; the sweep would free it
        // without `finalize()`. Mark every such candidate BEFORE tracing any
        // (two that reach only each other are both kept and both reported),
        // drain their closure, and hand them to the next stop-the-world
        // collection to report (`ConcurrentGcState::publish_finalizer_candidates`).
        // Before the reference-processing seam below, so what it judges sees
        // them kept, as HotSpot's JNI weak processing sees the final-reachable
        // set. Nothing to do, and no lock beyond one leaf read, without
        // candidates.
        scanned += self.retain_dead_finalizer_candidates(old_gen, &object_starts);

        // gen r4w2/concmark — the remark-time reference-processing seam sits
        // HERE, at the FIRST fixed point, with the barrier still active and
        // the phase still `Remark`: the caller runs it between this half and
        // `remark_finish` (gen r5w1/refs5 split the function at this point).
        RemarkProgress {
            authorised: true,
            scanned,
            object_starts,
        }
    }

    /// gen r5w1/refs5 — the SECOND half of the remark. `keep` is what the
    /// reference-processing callback returned (policy-retained soft
    /// referents, finalizables about to be resurrected, submitted cleaner
    /// actions): each is marked and its closure drained, then the barrier goes
    /// off with the final quiescing drain and the phase becomes
    /// `ConcurrentSweep`. `None` — no callback ran — skips the keep closure
    /// entirely (byte-for-byte the pre-seam remark). A refused remark
    /// (`!progress.sweep_authorised()`) has already established its
    /// post-conditions in [`Self::remark_begin`]; this returns 0 for it.
    ///
    /// The caller must hold the old-gen guard again (`old_gen`) and still be
    /// inside the pause `remark_begin` ran in. Returns the whole remark's
    /// count (see [`Self::remark_with_reference_processing`]).
    pub fn remark_finish(
        &self,
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
        progress: RemarkProgress,
        keep: Option<&[usize]>,
        old_gen: &OldGen,
    ) -> usize {
        let RemarkProgress {
            authorised,
            mut scanned,
            object_starts,
        } = progress;
        if !authorised {
            return scanned;
        }
        if let Some(keep) = keep {
            for &addr in keep {
                let ptr = addr as *mut u8;
                if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                    self.queue.push(ptr);
                }
            }
            scanned += self.drain_closure(old_gen, &object_starts);
        }

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
        //
        // gc-common w5-e — history now: `satb_barrier` gates on the QUEUE
        // (`common-g-proposal-one-satb-gate`), so the half-open window above
        // can no longer be expressed and this order is not load-bearing for
        // the barrier. It is kept so the phase (which the VM's root builders
        // still read) never claims marking with a closed queue.
        self.state.set_phase(ConcurrentGcPhase::ConcurrentSweep);
        let late_entries = self.satb_queue.deactivate_and_drain(stw);
        for addr in late_entries {
            let ptr = addr as *mut u8;
            if markable_old_object(ptr, &object_starts) && self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }
        // Mark the transitive closure of any late entries (again with the
        // overflow fallback). The gate is INACTIVE now, but mutators are
        // quiesced past the drain barrier, so no further live overwrite can
        // escape the bitmap.
        scanned += self.drain_closure(old_gen, &object_starts);

        // Phase was already advanced to `ConcurrentSweep` above, ahead of the
        // deactivation, to keep `is_marking_active() => satb_queue.is_active()`
        // true at every instant.

        // gen r5w3/unload7: the bitmap is final; nothing scans again this
        // cycle, so the side tables (one row per user-loader class, mirror
        // and deferred root) are released now rather than at the sweep's end.
        if self.class_unload_armed() {
            self.disarm_class_unload();
        }

        scanned
    }

    /// gc-common w36-d — the remark's finalizer retention (see the call site
    /// and [`ConcurrentGcState::publish_finalizer_candidates`]). Marks every
    /// published candidate that is an old-gen object start, unmarked, and
    /// sweep-eligible (so this cycle would free it); drains their closure;
    /// records them for the next stop-the-world collection to report. Returns
    /// the objects the drain scanned.
    fn retain_dead_finalizer_candidates(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
    ) -> usize {
        let candidates = self.state.finalizer_candidates();
        if candidates.is_empty() {
            return 0;
        }
        let mut retained: Vec<usize> = Vec::new();
        {
            let eligible = self.sweep_eligible.lock();
            let Some(eligible) = eligible.as_ref() else {
                return 0;
            };
            for &addr in &candidates {
                if markable_old_object(addr as *mut u8, object_starts)
                    && !self.bitmap.is_marked(addr)
                    && eligible.contains(addr)
                {
                    retained.push(addr);
                }
            }
        }
        if retained.is_empty() {
            return 0;
        }
        // gen r5w5/conc9: a remark that runs reference processing records
        // the closure (see `record_finalizer_retention_closure`).
        if self.record_retention_closure.load(Ordering::Acquire) {
            let scanned = self.retain_and_record_closure(old_gen, object_starts, &retained);
            self.state.note_remark_retained_finalizers(&retained);
            return scanned;
        }
        // Every candidate first, then one drain: the mutually-reachable rule.
        // gen r5w1/refs5: counted once, by the drain that scans them (the
        // remark's count is "objects scanned").
        for &addr in &retained {
            if self.bitmap.try_mark(addr) {
                self.queue.push(addr as *mut u8);
            }
        }
        let scanned = self.drain_closure(old_gen, object_starts);
        self.state.note_remark_retained_finalizers(&retained);
        scanned
    }

    /// gen r5w5/conc9 — [`Self::retain_dead_finalizer_candidates`]'s marking,
    /// RECORDING what it marks into [`Self::retention_closure`]. Every
    /// candidate is marked before any is traced (the mutually-reachable rule,
    /// unchanged), then one marker-local depth-first drain scans them and
    /// everything it newly marks; each child `scan_object_into` hands to `push`
    /// was marked by that very call, i.e. was unmarked at the strong closure's
    /// fixed point. Runs inside the remark pause on the driver thread, so the
    /// local stack needs no termination protocol; the shared queue is empty
    /// on entry (the strong closure just drained it) and is left empty.
    ///
    /// A scan that fails closed marks the whole generation, exactly as the
    /// shared drain does ([`Self::drain_closure`]); the objects that marks are
    /// not recorded (the set is then short, never wrong). Returns the objects
    /// scanned.
    fn retain_and_record_closure(
        &self,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        retained: &[usize],
    ) -> usize {
        let mut closure: Vec<usize> = Vec::with_capacity(retained.len());
        let mut stack: Vec<*mut u8> = Vec::with_capacity(retained.len());
        for &addr in retained {
            if self.bitmap.try_mark(addr) {
                closure.push(addr);
                stack.push(addr as *mut u8);
            }
        }
        let mut scanned = 0usize;
        let mut children: Vec<*mut u8> = Vec::new();
        while let Some(obj_ptr) = stack.pop() {
            children.clear();
            let ok = self.scan_object_into(obj_ptr, old_gen, object_starts, true, &mut |child| {
                children.push(child)
            });
            if !ok {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            scanned += 1;
            closure.extend(children.iter().map(|&c| c as usize));
            stack.extend_from_slice(&children);
        }
        *self.retention_closure.lock() = closure;
        scanned
    }

    /// Drain the mark queue to its transitive closure, then run the
    /// graceful overflow fallback (Round-9 gc HIGH-5) until no shard has
    /// overflowed. Returns the number of objects scanned. Extracted so the
    /// remark closure can be re-run after the final SATB quiescing drain
    /// (gc-concmark fix) without duplicating the overflow logic.
    fn drain_closure(&self, old_gen: &OldGen, object_starts: &OldGenObjectStarts) -> usize {
        let mut discovered = 0;

        // Drain the queue fully (mark transitive closure). gen r4w4/concmark4:
        // through the marker-local stack (`drain_unbounded`).
        match self.drain_unbounded(old_gen, object_starts) {
            Some(n) => discovered += n,
            None => {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return discovered;
            }
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
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                break;
            }
            // gen r4w3/oldgen3: `for_each_object`, not `walk_objects()` — the
            // same walk without a `Vec` of every old-gen object built inside
            // the remark pause, once per pass.
            let mut degraded = false;
            old_gen.for_each_object(|obj_ptr, _size| {
                if degraded || !self.bitmap.is_marked(obj_ptr as usize) {
                    return;
                }
                // Already gray/black: re-scan its outgoing refs to
                // pick up children we may have dropped.
                if self.scan_object(obj_ptr, old_gen, object_starts, true) {
                    discovered += 1;                } else {
                    degraded = true;
                }
            });
            if degraded {
                self.mark_all_old_gen(old_gen);
                self.queue.clear();
                return discovered;
            }
            // Drain anything the rescan re-enqueued.
            match self.drain_unbounded(old_gen, object_starts) {
                Some(n) => discovered += n,
                None => {
                    self.mark_all_old_gen(old_gen);
                    self.queue.clear();
                    return discovered;
                }
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
    ///
    /// gen r4w3/oldgen3 (2026-09-23): the whole sweep in one call — one
    /// unbounded [`Self::concurrent_sweep_budget`] slice, which walks to the
    /// end of the generation and finishes the cycle. Kept for the
    /// `CRATONVM_GEN_CONC_MARK_SLICE=0` driver arm and the tests; the sliced
    /// driver calls the budgeted form directly.
    pub fn concurrent_sweep(&self, old_gen: &mut OldGen) -> usize {
        let mut freed = 0usize;
        loop {
            let (n, done) = self.concurrent_sweep_budget(old_gen, usize::MAX);
            freed += n;
            if done {
                return freed;
            }
        }
    }

    /// Phase 4 in a BOUNDED SLICE: walk at most `budget` old-gen objects,
    /// free the eligible unmarked ones among them, and return `(objects freed
    /// by this slice, sweep finished)`.
    ///
    /// gen r4w3/oldgen3 (2026-09-23),
    /// `gengc-r4w2-concmark-concurrent-sweep-holds-the-old-gen-lock-FIXED-20260923.md`.
    /// The driver used to run the whole sweep — an O(old gen) walk, one `free`
    /// per dead object and a coalesce — under ONE old-gen lock hold with no
    /// safepoint poll, so every promotion and every other thread's pause
    /// waited for all of it: Phase 2's problem, moved one phase later. The
    /// driver now loops `lock → slice → unlock → safepoint poll` here, as it
    /// does for Phase 2.
    ///
    /// # What has to hold between two slices
    ///
    /// * **The epoch.** The bitmap and the eligibility snapshot are keyed on
    ///   bare addresses. A `free` or `compact` by ANOTHER collector between
    ///   slices (a young pause's in-place old-gen sweep, say) may hand an
    ///   address to a new object, so a slice that finds
    ///   `OldGen::reclaim_epoch` moved stops the sweep — reclaiming nothing
    ///   more this cycle, the same fail-closed answer the first slice gives.
    ///   This sweep's own frees bump the epoch too, so the stamp is re-taken
    ///   after each slice's frees, under the same lock hold.
    /// * **The resume offset.** Each slice starts where
    ///   `OldGen::walk_objects_from` said the previous one stopped: the start
    ///   of an object that was allocated then. With the epoch unchanged
    ///   nothing but this sweep has freed anything, and this sweep frees only
    ///   BELOW that offset, so the object is still there. An allocation in
    ///   between (a promotion into a hole this sweep freed, a buffer carved
    ///   and its tail released, a coalesce on an allocation failure) fills
    ///   free space, which never contains the offset — the walk returns the
    ///   first object of the NEXT region, never a free-block start that a
    ///   later allocation could straddle.
    /// * **TAMS.** An object allocated between slices is not in the snapshot
    ///   (its block was free at remark, or it was freed after — which moved
    ///   the epoch), so it is walked past and never freed. The same rule that
    ///   protects an object promoted between remark and a one-shot sweep.
    ///
    /// The free list is coalesced once, when the sweep finishes, and the
    /// generation's trigger bookkeeping is told what the cycle reclaimed
    /// (`OldGen::note_concurrent_collection_end`) — both on a normal finish
    /// and on an epoch stop after some slices had already freed.
    ///
    /// A `budget` of 0 is raised to 1.
    pub fn concurrent_sweep_budget(&self, old_gen: &mut OldGen, budget: usize) -> (usize, bool) {
        self.concurrent_sweep_budget_into(old_gen, budget, None)
    }

    /// [`Self::concurrent_sweep_budget`], also appending every span this slice
    /// returned to the free list to `freed` as `(start, len)` — ascending,
    /// non-overlapping; with `no_oldgen_coalesce` one span per object.
    ///
    /// For the VM's address-keyed side tables, which must drop rows keyed
    /// inside a freed span BEFORE the span can be handed out again: a direct
    /// old-gen allocation (a humongous array) between two slices would
    /// otherwise land on the span and make the dead row look live to every
    /// later `survived_in_place` sweep (gc-common w5-g,
    /// `common-w3g-concurrent-sweep-direct-old-alloc-window`). `None` is
    /// exactly [`Self::concurrent_sweep_budget`].
    pub fn concurrent_sweep_budget_into(
        &self,
        old_gen: &mut OldGen,
        budget: usize,
        mut freed: Option<&mut Vec<(usize, usize)>>,
    ) -> (usize, bool) {
        let budget = budget.max(1);
        let mut progress = self.sweep_progress.lock();
        if progress.is_none() {
            match self.begin_sweep(old_gen) {
                Some(p) => *progress = Some(p),
                // Nothing authorised, or the snapshot was already stale:
                // `begin_sweep` has finished the cycle.
                None => return (0, true),
            }
        }
        let Some(p) = progress.as_mut() else {
            return (0, true);
        };
        self.state
            .census
            .sweep_slices
            .fetch_add(1, Ordering::Relaxed);

        // GCAUD-4, between slices: see the doc above.
        if p.epoch != old_gen.reclaim_epoch() {
            self.note_sweep_epoch_abort();
            self.state
                .census
                .sweep_epoch_stops
                .fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                snapshot_epoch = p.epoch,
                current_epoch = old_gen.reclaim_epoch(),
                freed_so_far = p.freed_objects,
                "concurrent sweep stopped between slices: old-gen storage was reclaimed or \
                 relocated by another collector, so the address-keyed mark bitmap and \
                 eligibility snapshot no longer identify the same objects",
            );
            let finished = progress.take();
            // gci i13/cm item 7: part of the stop-the-world collection's
            // episode, not a second collection nor a completed cycle.
            self.finish_sweep_as(old_gen, finished, true);
            return (0, true);
        }

        // One slice: collect the dead RUNS (adjacent eligible unmarked
        // objects), then free them. The walk borrows the generation, so the
        // frees wait for it; nothing between the two touches old gen.
        //
        // Runs, not objects — the in-place STW sweep's argument
        // (`gen_heap::old_gen_gc`): `walk_objects_from` yields back-to-back
        // `(base, size)` pairs inside a region, so "the next dead object
        // starts where the run ends" means their union is one contiguous span
        // of freed objects; a region boundary or a desync break leaves a gap
        // and ends the run. The trailing coalesce would have merged the pieces
        // anyway. Not under `no_oldgen_coalesce`, whose point is unmerged
        // blocks.
        let batch = !cratonvm_gc_core::gc_flags().no_oldgen_coalesce;
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut freed_objects = 0usize;
        let mut freed_bytes = 0usize;
        let next = {
            let bitmap = &self.bitmap;
            let eligible = &p.eligible;
            // gen r5w4/conc8: every object this sweep does NOT free is a
            // survivor for the retained-layout census (one `Option` test per
            // object when no census runs).
            let census = &mut p.layout_census;
            old_gen.walk_objects_from(p.next, budget, |obj_ptr, total_size| {
                let addr = obj_ptr as usize;
                if bitmap.is_marked(addr) || !eligible.contains(addr) {
                    if let Some(c) = census.as_mut() {
                        c.observe_survivor(obj_ptr);
                    }
                    return;
                }
                freed_objects += 1;
                freed_bytes += total_size;
                let extends = batch && runs.last().is_some_and(|&(start, len)| start + len == addr);
                if extends {
                    if let Some((_, len)) = runs.last_mut() {
                        *len += total_size;
                    }
                } else {
                    runs.push((addr, total_size));
                }
            })
        };
        // SAFETY: these ascending runs contain exactly the dead, eligible
        // objects yielded above, under the same old-gen lock and epoch.
        unsafe { old_gen.free_runs_ascending(&runs) };
        if let Some(out) = freed {
            out.extend_from_slice(&runs);
        }
        p.freed_objects += freed_objects;
        p.freed_bytes += freed_bytes;
        // gci i13/cm item 2: what the sweep has freed so far, for a STW
        // pre-emption's growth sample before the sweep's end reports it.
        if freed_bytes > 0 {
            self.state.note_sweep_freed_so_far(p.freed_bytes);
        }
        // Re-stamp AFTER this slice's own frees, under the same lock hold, so
        // only someone ELSE's free is caught by the next slice.
        p.epoch = old_gen.reclaim_epoch();
        match next {
            Some(resume) => {
                p.next = resume;
                (freed_objects, false)
            }
            None => {
                let mut finished = progress.take();
                // gen r5w4/conc8: the walk reached the end with the epoch
                // intact — the one exit that completes a retained-layout
                // census (an epoch stop above drops it with the progress).
                if let Some(census) = finished.as_mut().and_then(|p| p.layout_census.take()) {
                    if let Some(released) = census.released() {
                        self.state.complete_layout_census(released);
                    }
                }
                self.finish_sweep(old_gen, finished);
                (freed_objects, true)
            }
        }
    }

    /// The gates the first sweep slice passes through. `Some` authorises the
    /// sweep; `None` means it frees nothing, and the cycle has been finished
    /// (bitmap cleared, phase `Idle`).
    fn begin_sweep(&self, old_gen: &mut OldGen) -> Option<SweepProgress> {
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
        // gen r4w2/concmark: the mark is over; drop the cached walk (up to
        // 1/64th of the old generation) rather than carry it to the marker's
        // drop. The sweep walks for itself.
        *self.starts_cache.lock() = None;
        *self.tams_starts.lock() = None;
        let Some(eligible) = eligible.filter(|e| !e.is_empty()) else {
            self.finish_sweep(old_gen, None);
            return None;
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
        let Some(snapshot_epoch) = snapshot_epoch else {
            tracing::debug!(
                eligible = eligible.len(),
                "concurrent sweep skipped: remark never authorised this snapshot",
            );
            self.finish_sweep(old_gen, None);
            return None;
        };

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
        if snapshot_epoch != old_gen.reclaim_epoch() {
            self.note_sweep_epoch_abort();
            // `info!`: a whole concurrent sweep's work is being thrown
            // away, at most once per cycle. As `debug!` this line could not
            // print in a release build, so the abandonment was unobservable
            // outside a debug run (the count is on the `[GC] conc_driver:`
            // line as `concdrv_sweep_epoch_aborts` since gen r5w1/refs5).
            tracing::info!(
                snapshot_epoch = ?Some(snapshot_epoch),
                current_epoch = old_gen.reclaim_epoch(),
                eligible = eligible.len(),
                "concurrent sweep abandoned: old-gen storage was reclaimed or \
                 relocated since remark, so the address-keyed mark bitmap and \
                 eligibility snapshot no longer identify the same objects",
            );
            self.finish_sweep(old_gen, None);
            return None;
        }

        // gen r5w4/conc8: the retained-layout census rides on this walk; its
        // break baseline is taken now, before the first slice walks anything.
        let layout_census = self
            .layout_census
            .lock()
            .take()
            .map(|candidates| LayoutCensus {
                candidates: candidates.into_iter().collect(),
                survivors: rustc_hash::FxHashSet::default(),
                observed_classes: [None; 64],
                breaks_at_start: walk_break_hits(),
            });
        Some(SweepProgress {
            eligible,
            epoch: snapshot_epoch,
            next: 0,
            freed_objects: 0,
            freed_bytes: 0,
            layout_census,
        })
    }

    /// End the sweep (and with it the cycle): coalesce once if anything was
    /// freed — the same amortised merge the in-place STW sweep does (see
    /// `OldGen::coalesce_free_blocks`: this reclaimer never compacts, and
    /// `free` alone leaves every reclaimed run an isolated block, so without
    /// it the generation fragments monotonically) — tell the generation's
    /// trigger what an authorised sweep reclaimed, clear the bitmap, and
    /// return the phase to `Idle` (the hand-off to the next cycle; see
    /// [`ConcurrentCycle`]).
    fn finish_sweep(&self, old_gen: &mut OldGen, progress: Option<SweepProgress>) {
        self.finish_sweep_as(old_gen, progress, false);
    }

    /// [`Self::finish_sweep`]; `stopped` = the sweep was stopped between two
    /// slices because a stop-the-world collection moved `reclaim_epoch`
    /// (gci i13/cm item 7). Such a sweep is part of THAT collection's episode:
    /// its frees and every decision input are recorded exactly as before
    /// (`OldGen::note_concurrent_sweep_stopped`, `ConcurrentGcState::note_cycle_stopped`),
    /// but it is not counted as a second old-gen collection, nor as a
    /// completed cycle with a report line.
    fn finish_sweep_as(
        &self,
        old_gen: &mut OldGen,
        progress: Option<SweepProgress>,
        stopped: bool,
    ) {
        if let Some(p) = progress {
            if p.freed_objects > 0 {
                old_gen.coalesce_free_blocks();
            }
            // gen r4w3/oldgen3 — the wave-2 `oldgen2` cross-lane request: a
            // concurrent cycle is an old-gen collection too, and the
            // hysteresis must measure growth from its end, not from the last
            // STW collection's.
            if stopped {
                old_gen.note_concurrent_sweep_stopped(p.freed_bytes);
            } else {
                old_gen.note_concurrent_collection_end(p.freed_bytes);
            }
            // gen r4w5/concmark5 — and the same figure on the state, so the
            // STW cadence census can tell the two kinds of frees apart.
            self.state.note_concurrent_freed(p.freed_bytes);
            // gen r4w4/concmark4 — the start policy's growth sample, AFTER
            // `note_concurrent_collection_end` so `freed_bytes` (half of
            // `old_gen_allocated_total`) already includes this sweep.
            if stopped {
                self.state.note_cycle_stopped(
                    old_gen_allocated_total(old_gen),
                    old_gen.used(),
                    old_gen.capacity(),
                );
            } else {
                self.state.note_cycle_end(
                    old_gen_allocated_total(old_gen),
                    old_gen.used(),
                    old_gen.capacity(),
                    p.freed_bytes,
                );
            }
        } else {
            // gcd d9/e: `conccyc_swept_nothing` — a cycle whose remark ran
            // (this is the sweep's gate) but which reclaims nothing.
            self.state.note_swept_nothing();
            self.state.note_cycle_dropped();
        }
        self.disarm_reference_skip();
        if self.class_unload_armed() {
            self.disarm_class_unload();
        }
        self.bitmap.clear();
        // gci i13/cm item 1: recorded BEFORE the `Idle` store that hands the
        // shared state over, so this marker's `ConcurrentCycle` can never see
        // `Idle`-owned-by-someone-else without also seeing the flag.
        self.handed_off.store(true, Ordering::Release);
        self.state.set_phase(ConcurrentGcPhase::Idle);
    }

    fn mark_all_old_gen(&self, old_gen: &OldGen) -> usize {
        let mut marked = 0;
        // gen r4w3/oldgen3: `for_each_object` — no `Vec` of the generation.
        old_gen.for_each_object(|obj_ptr, _size| {
            if self.bitmap.try_mark(obj_ptr as usize) {
                marked += 1;
            }
        });
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
        in_pause: bool,
    ) -> bool {
        let queue = &self.queue;
        self.scan_object_into(obj_ptr, old_gen, object_starts, in_pause, &mut |p| queue.push(p))
    }

    /// [`Self::scan_object`] with the newly greyed children handed to `push`
    /// instead of the shared [`MarkQueue`]. gen r4w4/concmark4: the
    /// marker-local stack's entry point (see [`Self::drain_local`]); every
    /// child passed to `push` has already been marked.
    fn scan_object_into<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        in_pause: bool,
        push: &mut F,
    ) -> bool {
        self.scan_object_into_with(
            obj_ptr,
            old_gen,
            object_starts,
            &mut UnloadEdgeAccess::PerObject,
            in_pause,
            push,
        )
    }

    /// [`Self::scan_object_into`] with the class-unload side tables reached
    /// through `unload` (gcf f4/c): per object as before, or through the view
    /// a drain holds.
    fn scan_object_into_with<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        unload: &mut UnloadEdgeAccess<'_>,
        in_pause: bool,
        push: &mut F,
    ) -> bool {
        // The queue admits only readable old-generation object starts; use
        // the same header snapshot for layout selection and extent validation.
        let snapshot = ConcurrentMarkHeaderSnapshot::read(obj_ptr as *const ObjectHeader);
        if snapshot.kind_tag == ObjectKind::Object as u8
            && snapshot.gc_flags & GC_FLAG_COMPACT != 0
            && snapshot.object_flags_valid()
        {
            // Keep the same borrowed layout for sizing and scanning. Besides
            // avoiding a second lookup, this prevents validating one registry
            // generation and then visiting slots from another generation.
            if let Some(scanned) = snapshot.with_compact_layout(|layout| {
                self.scan_object_into_with_layout(
                    obj_ptr,
                    old_gen,
                    object_starts,
                    unload,
                    in_pause,
                    push,
                    &snapshot,
                    Some(layout),
                )
            }) {
                return scanned;
            }
        }
        self.scan_object_into_with_layout(
            obj_ptr,
            old_gen,
            object_starts,
            unload,
            in_pause,
            push,
            &snapshot,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn scan_object_into_with_layout<F: FnMut(*mut u8)>(
        &self,
        obj_ptr: *mut u8,
        old_gen: &OldGen,
        object_starts: &OldGenObjectStarts,
        unload: &mut UnloadEdgeAccess<'_>,
        in_pause: bool,
        push: &mut F,
        snapshot: &ConcurrentMarkHeaderSnapshot,
        compact_layout: Option<&cratonvm_types::CompactLayout>,
    ) -> bool {
        let trace_loader_edges =
            false;
        // SAFETY: obj_ptr was popped from the mark queue, which only contains
        // old-gen object starts admitted by `markable_old_object` (this cycle's
        // object-start set) before being enqueued (gcj j2/og2, C8: the comment
        // named `old_gen.contains()`, which is not the admission). The header is
        // readable for the lifetime of the GC cycle.
        let header_ptr = obj_ptr as *const ObjectHeader;
        let total_size = match compact_layout {
            Some(layout) => Some(layout.total_size as usize),
            None => concurrent_mark_snapshot_size(snapshot),
        };
        let Some(total_size) = total_size else {
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
        // gen r5w6/conc10 (unload7's r5w3 request, repeated by old9): the
        // WHOLE claimed extent, not its last byte. With an interior decommit
        // (`CRATONVM_GC_OLD_INTERIOR_DECOMMIT`) both ends can be committed
        // while a granule between them is released, and the slot loops below
        // would fault reading it; `contains_range` checks every granule the
        // extent spans. Without holes it is exactly the old test (the start is
        // an old-gen object start; the end below `readable_end`).
        if total_size < HEADER_SIZE || !old_gen.contains_range(obj_ptr, total_size) {
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
                //
                // gen r4w3/oldgen3 (item 4 of
                // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`):
                // bound the walk by the extent VALIDATED above, as the object
                // arm below already does. `header.array_length()` is a second
                // read of a header this module treats as racy; if it read
                // larger than the snapshot `concurrent_mark_object_size` sized,
                // the loop would visit elements past the `old_gen.contains`
                // check. `min` keeps the re-read's count whenever it is the
                // smaller (the normal case: the two agree), so a well-formed
                // array is walked exactly as before.
                let validated = total_size.saturating_sub(ARRAY_DATA_OFFSET) / ref_element_size();
                let len = (header.array_length() as usize).min(validated);
                for i in 0..len {
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
                            if trace_loader_edges && unload.is_loader_target(ref_ptr as usize) {
                                eprintln!(
                                    "[conc-unload-heap-edge] source={obj_ptr:#p} class_id={} slot=array[{i}] target={:#x}",
                                    header.class_id.as_u32(),
                                    ref_ptr as usize,
                                );
                            }
                            push(ref_ptr);
                        }
                    }
                }
            }
            // Primitive arrays have no references to scan.
        } else if cratonvm_gc_core::is_compact_object(header) {
            // gen r5w1/refs5: a `Reference` in this cycle's skip set does not
            // trace its referent (field index 0, `Reference.referent`, first
            // in declaration order with no superclass fields ahead of it; the
            // same index G1's INT-8 skip hides). See `set_reference_skip`.
            let hide_referent = self.referent_slot_hidden(obj_ptr);
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
            //
            // gen r4w2/concmark (2026-09-23): "skip it" was an UNDER-mark. The
            // object is live (it is on the queue) and its reference slots are
            // exactly what the unresolvable layout would have told us, so
            // skipping it leaves every child it alone keeps alive unmarked and
            // sweep-eligible. Fail closed instead, like the size screen above:
            // `false` makes the caller mark every old-gen object this cycle.
            // The ordinary compact path holds one borrowed layout across
            // sizing and scanning. The fallback still refuses a missing map
            // or an inconsistent header classification rather than dropping
            // the object's outgoing edges.
            //
            // gc-common w3-e: borrowing form (`with_compact_oop_scan`) -- no
            // layout `Arc` clone/drop per scanned object; `None` is the same
            // unresolvable-layout answer and still fails the cycle closed.
            let mut visit = |layout: &cratonvm_types::CompactLayout, body: usize| {
                // gen r5w1/refs5: the displacement of field 0 when it is a
                // reference field, i.e. the referent slot to hide.
                let hidden_disp = if hide_referent && layout.is_ref.first() == Some(&true) {
                    layout.field_disps.first().map(|&d| d as usize)
                } else {
                    None
                };
                // Compact object: 8-byte reference slots at the per-class oop-map
                // offsets. An aligned single-word 8-byte pointer load cannot tear,
                // so (like the reference-array branch above) no stripe lock is
                // needed even though this runs concurrently with mutators.
                for &off in &layout.ref_disps {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    if hidden_disp == Some(off) {
                        continue;
                    }
                    // SAFETY: `off` is within the object (capped above).
                    let slot_ptr = unsafe { obj_ptr.add(off) };
                    let raw: u64 = unsafe { read_ref_slot(slot_ptr) };
                    if raw != 0 {
                        let ref_ptr = raw as usize as *mut u8;
                        if markable_old_object(ref_ptr, object_starts)
                            && self.bitmap.try_mark(ref_ptr as usize)
                        {
                            if trace_loader_edges && unload.is_loader_target(ref_ptr as usize) {
                                let class_name = cratonvm_gc_core::gc::resolve_class_info(
                                    header.class_id.as_u32(),
                                )
                                .map(|(name, _)| name)
                                .unwrap_or_else(|| "<unresolved>".to_string());
                                eprintln!(
                                    "[conc-unload-heap-edge] source={obj_ptr:#p} class_id={} class={class_name} slot=compact+{off} target={:#x}",
                                    header.class_id.as_u32(),
                                    ref_ptr as usize,
                                );
                            }
                            push(ref_ptr);
                        }
                    }
                }
            };
            let scanned = match compact_layout {
                Some(layout) => {
                    visit(layout, layout.total_size as usize);
                    Some(())
                }
                None => cratonvm_gc_core::heap::with_compact_oop_scan(header, visit),
            };
            if scanned.is_none() {
                tracing::warn!(
                    "concurrent mark: compact object at {:p} (class_id={}) has no \
                     resolvable layout; marking all old-gen objects for this cycle",
                    obj_ptr,
                    header.class_id.as_u32(),
                );
                return false;
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
            // mutator store routed through the volatile field-access helpers
            // (the lock's acquire is the edge; the per-slot `SeqCst` fences
            // that used to sit around the read were dropped by gen r4w3/oldgen3
            // — see the loop). The
            // 8-byte reference-array path above needs no lock: an aligned
            // 8-byte pointer load/store is single-word and cannot tear.
            //
            // SAFETY: `obj_ptr` was popped from the mark queue, where it was
            // admitted by `markable_old_object` before being enqueued, so it is
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
            // `old_gen.contains_range(obj_ptr, total_size)` — every granule of
            // the object's extent is inside the (committed) old generation. Re-reading `header.num_slots()`
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
            // gen r5w1/refs5: slot 0 is `Reference.referent`; a `Reference` in
            // this cycle's skip set starts at slot 1 (see `set_reference_skip`).
            let first_slot = usize::from(self.referent_slot_hidden(obj_ptr));
            for slot_idx in first_slot..num_slots {
                // Serialize the 16-byte read against striped mutator writes so
                // we never observe a torn (tag, payload) pair. Held only for
                // the duration of this single slot read.
                // JNI Set<Type>Field enters ForeignJniEntry::enter before
                // touching a slot. An in-native/foreign/downcall thread must
                // leave its blocked region (and wait out the pause) first.
                // Initial mark and remark therefore have no mutator writers.
                let _stripe =
                    (!in_pause).then(|| cratonvm_gc_core::collector::volatile_stripe_lock(obj_ref, slot_idx));
                // gen r4w3/oldgen3 (item 4 of
                // `gengc-r4-mark-old-gen-concurrent-mark-costs-FIXED-20260924.md`):
                // there used to be a `fence(SeqCst)` here and another after the
                // read, on every slot. Neither ordered anything this reader
                // uses. Against `set_field_volatile` writers the stripe lock is
                // the edge (its acquire makes the writer's stores, made under
                // the same lock, visible to the read below). Against JIT writers,
                // which take no lock, a reader-side fence pairs with nothing —
                // the two-word atomic read plus the discriminant screen is what
                // makes a torn cell harmless, and SATB, not the order of this
                // read, is what makes a racing overwrite harmless (the old value
                // is logged and marked at remark). Two full barriers per 16-byte
                // slot, primitive slots included, for no guarantee.
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
                // for exactly that reason; the marker was not. (gen r4/mark
                // 2026-09-23: this used to add "and it is a G1/ZGC-only path".
                // It is not — `ConcurrentMarker` is the GENERATIONAL old-gen
                // marker, driven from `maybe_concurrent_gc`; G1 has its own in
                // `g1_concurrent.rs` and shares only `ConcurrentGcState` and
                // `concurrent_mark_object_size` with this file.) Corrupt cells decode to `Value::Object(None)`,
                // which this loop skips, and are counted by the cell census.
                let value = unsafe {
                    cratonvm_gc_core::heap::read_value_cell_checked(
                        slot_ptr as *const Value,
                        "concurrent_mark::scan_object",
                    )
                };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr();
                    if markable_old_object(ref_ptr, object_starts)
                        && self.bitmap.try_mark(ref_ptr as usize)
                    {
                        if trace_loader_edges && unload.is_loader_target(ref_ptr as usize) {
                            eprintln!(
                                "[conc-unload-heap-edge] source={obj_ptr:#p} class_id={} slot={slot_idx} target={:#x}",
                                header.class_id.as_u32(),
                                ref_ptr as usize,
                            );
                        }
                        push(ref_ptr);
                    }
                }
            }
        }
        // gen r5w3/unload7 — the class-loader side-table edges, when this
        // cycle may unload classes (`ClassUnloadTables`). One acquire load
        // otherwise. A reference array's header carries its component's class
        // id, so a `C[]` keeps `C`'s loader, as an array klass does in HotSpot
        // (the same lookup `old_gen_gc`'s BFS makes).
        // gcf f4/c: a drain holds the guard and the view (`Held`).
        match unload {
            UnloadEdgeAccess::PerObject => {
                if self.class_unload_armed.load(Ordering::Acquire) {
                    self.push_class_unload_edges(
                        obj_ptr,
                        header.class_id.as_u32(),
                        object_starts,
                        push,
                    );
                }
            }
            UnloadEdgeAccess::Held {
                view: Some(view),
                newly,
            } => {
                *newly += self.push_class_unload_edges_with(
                    view,
                    obj_ptr,
                    header.class_id.as_u32(),
                    object_starts,
                    push,
                );
            }
            UnloadEdgeAccess::Held { view: None, .. } => {}
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
        stw: &cratonvm_gc_core::collector::StopTheWorldToken,
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

// ---------------------------------------------------------------------------
// gen r4w6/oldpin6 (2026-09-24): the remark-to-sweep hand-off over a walk gap.
// `docs/feature-designs/generative-gc-architecture.md`.
// Kept in its own block so the rest of this file (lane `concsvc6`) merges
// cleanly; the only hunks inside `remark_with_reference_processing` are the
// three marked `gen r4w6/oldpin6`.
// ---------------------------------------------------------------------------

/// Remarks that authorised a sweep over a desynced walk through
/// `ConcurrentMarker::remark_walk_gap_plan` (the counterpart of
/// `CONC_MARK_WALK_DESYNC_ABORTS`, which now counts only the refusals).
pub static CONC_MARK_WALK_GAP_RECOVERIES: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    /// Tests only: the flags are read once per process, so a test turns the
    /// opt-in on for its own thread here.
    static FORCE_CONC_WALK_GAP_RECOVERY: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn forced_conc_walk_gap_recovery() -> bool {
    FORCE_CONC_WALK_GAP_RECOVERY.with(std::cell::Cell::get)
}

#[cfg(not(test))]
fn forced_conc_walk_gap_recovery() -> bool {
    false
}

/// `CRATONVM_GC_CONC_WALK_GAP_RECOVERY` (opt-in), or the test override.
fn conc_walk_gap_recovery_enabled() -> bool {
    forced_conc_walk_gap_recovery() || cratonvm_gc_core::gc_flags().conc_walk_gap_recovery
}

/// What a remark over a desynced walk needs in order to sweep anyway: a
/// fresh grid of the objects the walk DID cover, the byte ranges it could not
/// (offsets, `OldGen::walk_objects_with_gaps`), and the object-start set of
/// the grid.
struct RemarkWalkGapPlan {
    walked: Vec<(*mut u8, usize)>,
    gaps: Vec<(usize, usize)>,
    starts: Arc<OldGenObjectStarts>,
}

impl ConcurrentMarker {
    /// gen r4w6/oldpin6 — at a remark whose cycle saw a desynced walk, the
    /// plan that lets it sweep instead of refusing, or `None` (refuse, as
    /// before). Opt-in (`CRATONVM_GC_CONC_WALK_GAP_RECOVERY`).
    ///
    /// The refusal existed because a desynced walk anywhere in the cycle
    /// drops marks: a push to an object behind the break fails
    /// `markable_old_object`, so that object is neither marked nor traced,
    /// and an eligible object reachable only through it keeps a clear bit.
    /// The stop-the-world sweep recovers from the same anomaly by treating
    /// the unwalked bytes as a conservative root range
    /// (`gen_heap::seed_old_gen_walk_gaps`). The concurrent remark can do the
    /// same, plus one more step for the marks the CONCURRENT phase dropped:
    ///
    /// 1. **A fresh walk, inside the pause.** Its gaps must account exactly
    ///    for the bytes it missed (`walked + gap == used`, the stop-the-world
    ///    arm's condition) and its objects must ascend; otherwise refuse. Its
    ///    start set replaces the cached one, so the eligibility snapshot is
    ///    narrowed to the objects walked NOW (gap bytes are never eligible,
    ///    hence never swept) and every push below is judged against the
    ///    layout the sweep will see.
    /// 2. **The gap words as marks** ([`Self::seed_remark_walk_gaps`]): every
    ///    aligned word in a gap (and both narrow halves under compressed
    ///    references) that names a walked object — base or interior — is
    ///    marked and queued. The concurrent marker follows no owner-keyed side
    ///    table for ANY owner (whatever keeps those referents alive for a
    ///    walked owner — the remark's roots — keeps them for an owner in a
    ///    gap), so the heap words are the whole of what a gap can name here.
    /// 3. **A full rescan of every marked object**, forced by raising the
    ///    queue's overflow flag before `drain_closure`: a dropped push had a
    ///    MARKED referrer (or a root, re-scanned at remark), so re-scanning
    ///    every marked object against the fresh start set re-pushes every
    ///    referent a desynced walk refused, and the drain traces it. This is
    ///    the path the overflow fallback already trusts for dropped pushes,
    ///    and it is O(old gen) inside the pause — paid only on a cycle that
    ///    desynced, which before this reclaimed nothing at all.
    ///
    /// With the remark's roots, the SATB log and the young→old roots, the
    /// final bitmap then holds every walked object reachable from the roots,
    /// and nothing in a gap is freed.
    fn remark_walk_gap_plan(&self, old_gen: &OldGen) -> Option<RemarkWalkGapPlan> {
        if !conc_walk_gap_recovery_enabled() {
            return None;
        }
        let (walked, gaps) = old_gen.walk_objects_with_gaps();
        let walked_bytes: usize = walked.iter().map(|&(_, size)| size).sum();
        let gap_bytes: usize = gaps.iter().map(|&(s, e)| e.saturating_sub(s)).sum();
        if walked_bytes.checked_add(gap_bytes) != Some(old_gen.used()) {
            return None;
        }
        if !walked
            .windows(2)
            .all(|w| (w[0].0 as usize) < (w[1].0 as usize))
        {
            return None;
        }
        let starts = Arc::new(OldGenObjectStarts::build_from(old_gen, &walked));
        Some(RemarkWalkGapPlan {
            walked,
            gaps,
            starts,
        })
    }

    /// Step 2 of [`Self::remark_walk_gap_plan`]: mark and queue every walked
    /// object a gap word names, and raise the overflow flag so the remark's
    /// `drain_closure` rescans every marked object (step 3). Returns how many
    /// objects the gap words marked. Called inside the remark pause, under the
    /// old-gen lock the remark holds.
    fn seed_remark_walk_gaps(&self, old_gen: &OldGen, plan: &RemarkWalkGapPlan) -> usize {
        let walked = &plan.walked;
        // The walked object containing `cand` (base or interior), if any.
        let resolve = |cand: usize| -> Option<usize> {
            let i = walked.partition_point(|&(p, _)| (p as usize) <= cand);
            let (p, size) = *walked.get(i.checked_sub(1)?)?;
            let b = p as usize;
            (cand < b.saturating_add(size)).then_some(b)
        };
        let mut marked = 0usize;
        let mut mark = |cand: usize| {
            if let Some(b) = resolve(cand) {
                if self.bitmap.try_mark(b) {
                    self.queue.push(b as *mut u8);
                    marked += 1;
                }
            }
        };
        let base = old_gen.base_ptr() as usize;
        let narrow = cratonvm_types::narrow_oop::narrow_oops_enabled();
        for &(start, end) in &plan.gaps {
            let mut a = base + ((start + 7) & !7);
            let hi = base + end;
            while a + 8 <= hi {
                // SAFETY: `[a, a + 8)` lies inside `[base + start, base + end)`,
                // a gap `walk_objects_with_gaps` reported: the tail of an
                // ALLOCATED region (regions are the spans between free
                // blocks), so it is below `readable_end` and committed. `a` is
                // 8-aligned (`base` is, and the offset is rounded up). The
                // world is stopped and the old-gen lock is held, so nothing
                // writes these bytes concurrently. No header is read: the
                // word is only compared against the walked grid.
                let w = unsafe { std::ptr::read(a as *const u64) };
                if w != 0 {
                    mark(w as usize);
                    if narrow {
                        for half in [w as u32, (w >> 32) as u32] {
                            if half != 0 {
                                mark(cratonvm_types::narrow_oop::decode(half) as usize);
                            }
                        }
                    }
                }
                a += 8;
            }
        }
        // Step 3: the dropped pushes' referrers are marked; rescan them all.
        self.queue.overflowed.store(true, Ordering::Relaxed);
        CONC_MARK_WALK_GAP_RECOVERIES.fetch_add(1, Ordering::Relaxed);
        marked
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
pub struct OldGenObjectStarts {
    bits: cratonvm_gc_core::young_mark::ObjectStartBits,
    unrepresentable: HashSet<usize>,
    /// gen r4w2/concmark — the walk accounted for every allocated byte
    /// (`sum of object sizes == OldGen::used()`), i.e. no `scan_region` broke
    /// early. The same test `OldGen::compact` uses (GCAUD-9) before it trusts
    /// a walk enough to slide over it.
    walk_complete: bool,
}

impl Clone for OldGenObjectStarts {
    fn clone(&self) -> Self { self.duplicate() }
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
    /// describe a handful of objects, once per walk of the cycle, where the
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
        let base = old_gen.base_ptr() as usize & !7usize;
        let span = old_gen.high_water().min(old_gen.capacity())
            + (old_gen.base_ptr() as usize - base);
        let mut bits = cratonvm_gc_core::young_mark::ObjectStartBits::new(base, span);
        let mut unrepresentable = HashSet::new();
        let mut walked_bytes = 0usize;
        let mut recorded_span = 0usize;
        old_gen.for_each_object(|ptr, size| {
            walked_bytes += size;
            recorded_span = recorded_span.max((ptr as usize - base) + 8);
            if !bits.insert(ptr as usize) {
                unrepresentable.insert(ptr as usize);
            }
        });
        // Preserve build_from's exact geometry: remark can build its set
        // from an existing walk and intersection requires identical extents.
        bits.truncate(recorded_span.min(span));
        Self { bits, unrepresentable, walk_complete: walked_bytes == old_gen.used() }
    }

    /// [`Self::build`] over a walk the caller already has, so one walk can
    /// feed more than one set (gen r4w2/concmark: `initial_mark` seeds the
    /// slice cache from the same walk as the eligibility snapshot). `objects`
    /// must be `old_gen.walk_objects()` taken under the same lock hold.
    fn build_from(old_gen: &OldGen, objects: &[(*mut u8, usize)]) -> Self {
        let (lo, _hi) = old_gen.extent();
        let base = lo & !7usize;
        let walked_bytes: usize = objects.iter().map(|&(_, size)| size).sum();
        let walk_complete = walked_bytes == old_gen.used();
        // Do not assume `walk_objects` is ordered — fold for the maximum.
        let highest = objects.iter().map(|(ptr, _)| *ptr as usize).max();
        let span = match highest {
            // `+ 8` so the highest start's own bit is inside the span.
            Some(top) => top.saturating_sub(base).saturating_add(8),
            None => 0,
        };
        let bits = cratonvm_gc_core::young_mark::ObjectStartBits::new(base, span);
        let mut unrepresentable = HashSet::new();
        for &(ptr, _size) in objects {
            let addr = ptr as usize;
            if !bits.insert(addr) {
                unrepresentable.insert(addr);
            }
        }
        Self {
            bits,
            unrepresentable,
            walk_complete,
        }
    }

    fn duplicate(&self) -> Self {
        Self {
            bits: self.bits.duplicate(),
            unrepresentable: self.unrepresentable.clone(),
            walk_complete: self.walk_complete,
        }
    }

    #[inline]
    pub fn contains(&self, addr: usize) -> bool {
        self.bits.contains(addr) || self.unrepresentable.contains(&addr)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bits.is_empty() && self.unrepresentable.is_empty()
    }

    /// Number of recorded starts. Diagnostics, logging and tests.
    pub fn len(&self) -> usize {
        self.bits.len() + self.unrepresentable.len()
    }

    /// Whether the walk this set came from covered every allocated byte (see
    /// the field). `false` means some allocated objects are missing.
    #[inline]
    pub fn walk_complete(&self) -> bool {
        self.walk_complete
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

/// gen r5w6/conc10 — does a mark bitmap built over `[bitmap_base,
/// bitmap_base + bitmap_size)` still cover the old generation now at
/// `[old_base, old_base + old_capacity)`? Same base, and a capacity no larger
/// than the span the bitmap was built for (a capacity that SHRANK is still
/// covered). See the remark's fail-closed check in `remark_begin`.
#[inline]
fn bitmap_covers_generation(
    bitmap_base: usize,
    bitmap_size: usize,
    old_base: usize,
    old_capacity: usize,
) -> bool {
    bitmap_base == old_base && old_capacity <= bitmap_size
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
mod tests;
