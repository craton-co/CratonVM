// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Parallel evacuation for the generational young (Cheney) collection.
//!
//! # What this replaces
//!
//! `GenerationalHeap::collect_garbage_inner`'s moving path copies survivors
//! with `forward_object`, which takes `&mut Arena` for young to-space and
//! `&mut OldGen` for promotions. Those two `&mut`s are what make the copy
//! phase single-threaded *by construction* — the borrow checker enforces it,
//! and the per-cycle copy tally's thread-local says so in as many words. The
//! mark closure of a young cycle already went parallel (`young_mark`), and the
//! sweep's span zeroing with it, which left the COPY as the one serial phase
//! of a moving pause. On a survivor-heavy cycle that phase (`cheney_drain`) is
//! the pause.
//!
//! This module supplies the pieces that let N workers copy at once:
//!
//! * [`ParEvac`] — the immutable shared context (from-space bounds, the exact
//!   object-start bitmap, the to-space bump region, the old-gen allocation
//!   lock, the tenuring policy).
//! * [`EvacShard`] — one worker's private outputs, merged by the driver after
//!   the completion barrier. Every hot write lands here, never behind a shared
//!   lock.
//! * [`ParEvac::evacuate`] — copy-then-CAS forwarding, so exactly one worker
//!   ever copies a given object however many workers reach it.
//! * [`ParEvac::drain`] — the work-sharing transitive closure.
//!
//! # Why it is race-free
//!
//! 1. **From-space is read-only except for the forwarding install.** The only
//!    write a worker makes to a from-space object is the forwarding CAS on its
//!    mark word, and that is an atomic RMW.
//! 2. **Exactly one worker copies each object.** The CAS on the source mark
//!    word is the claim. A loser abandons its speculative copy and adopts the
//!    winner's address, so every reference converges on one destination. This
//!    is the same protocol `g1::SharedEvac::evacuate` uses, including the
//!    lesson its two `DEFECT-2` sites paid for: a CAS loser (and a fast-path
//!    "already forwarded" hit) MUST still record `old -> new` in this cycle's
//!    forward set, or a root naming `old` never gets remapped and dangles once
//!    from-space is reset. (gen r4w4/young4: the fast path skips the record
//!    when `new` is in young TO-space — such a forward can only have been
//!    installed by this phase's CAS winner, whose own record is merged. An
//!    old-gen `new` is still recorded; see `evacuate`.)
//! 3. **Destinations never overlap.** To-space is carved into per-worker
//!    buffers by one CAS (`bump_shared`) on a shared cursor; old-gen promotions go
//!    through the old generation's own allocator under a mutex.
//! 4. **Each destination is scanned once.** An object is pushed onto a
//!    worklist only by the worker whose CAS installed its forward.
//! 5. **The driver reads nothing until every worker has returned.**
//!    [`crate::evac_pool::EvacPool::scope`]'s completion barrier supplies that
//!    edge, and it holds even when a worker unwinds.
//!
//! # The to-space grid stays walkable
//!
//! A serial Cheney copy leaves to-space densely packed, and the next cycle's
//! from-space walk (`collect_garbage_inner`'s object-start walk) depends on
//! that: it strides object-by-object with `gen_object_total_size`. Per-worker
//! buffers break density — every retired buffer leaves a tail too small for
//! the object that triggered the retirement.
//!
//! Those tails are FILLED, not merely skipped, with the same two sentinels the
//! TLAB retire path already uses and every young walk in this crate already
//! strides: a well-formed `int[]` stamped [`crate::tlab::TLAB_FILLER_CLASS_ID`]
//! for a tail of at least `HEADER_SIZE`, and the 8-byte
//! [`crate::tlab::GAP_FILLER_CLASS_ID`] sentinel (class id at +0, exact gap
//! length at +4) for the 8-byte case below it. Using the existing
//! sentinels rather than inventing a third is the point: a new filler shape
//! would need every one of the ~14 walks that special-case these to learn
//! about it, and the one that did not would desync.
//!
//! (The split point is `HEADER_SIZE`, which is 16 today. An earlier revision
//! of this note said "the 8/16/24/32-byte cases" use the `GAP_FILLER`
//! sentinel; that was true when the header was 40 bytes and has been wrong
//! since the 2026-08-06 shrink — `install_gap_filler` and `Tlab::
//! install_tail_filler` both branch on `size < HEADER_SIZE`, so only the
//! 8-byte case takes the sentinel. Corrected 2026-09-20, gengc-round1.)
//!
//! A cycle that runs BUFFERLESS (see [`ParEvac::plan`] — the common case, since
//! a young collection triggers with from-space nearly full) leaves no tails at
//! all: every object is an exact span off the shared cursor, so to-space comes
//! out as densely packed as the serial collector leaves it.
//!
//! # `CRATONVM_GC_PAR_EVAC=0` is NOT a behaviour-neutral bisection lever
//!
//! `docs/GC.md` and the matching comment in `gen_heap.rs` say the two
//! evacuators "seed from the same three sources and produce the same
//! forwarding map, so `=0` is a one-run bisection lever rather than a
//! behaviour switch". The first half is true and was re-checked in round 1:
//! `seed_roots`, `seed_overlay_roots` and `seed_dirty_card_roots` are called
//! in the same order with the same arguments on both arms, and only the
//! `forward` closure differs.
//!
//! The conclusion does not follow, and the reason is the paragraph above this
//! one. **To-space becomes the NEXT cycle's from-space**, and the parallel arm
//! leaves PLAB tail fillers in it that the serial arm never produces. So the
//! next cycle's object-start walk, its `young_object_starts`, which addresses
//! `Arena::note_object_start` anchors, and the arena occupancy that decides
//! when the collection AFTER that triggers, all differ between the two arms.
//! `PAR_EVAC_FILLER_BYTES` exists because this was once measured at 327 KiB of
//! filler for 393 KiB of survivors.
//!
//! This is not a correctness defect — every young walk in this crate strides
//! both sentinels. It is a DEBUGGING defect, and a sharp one: flipping the
//! lever to chase a suspected premature reclamation makes a filler-striding
//! desync disappear, which reads as "the parallel evacuator is at fault" when
//! the bug is in a walk the serial path simply never feeds. Worse, it is
//! intermittent, because a bufferless cycle produces no filler at all — so the
//! same binary diverges on some cycles and not others.
//!
//! Operationally: [`par_evac_census`]'s `filler_bytes` is what says whether a
//! given run had any filler to compare. A bisection report that does not quote
//! it has not established which two configurations it compared.
//!
//! gengc-round4-move (2026-09-23): the MECHANISM for the second lever that
//! splits the question now exists — `EvacPlan::into_bufferless` turns any
//! plan into a bufferless one (`plab_bytes = 0`, no abandoned-tail allowance,
//! a reservation of exactly the survivors) while leaving everything else about
//! the parallel arm intact, so to-space comes out as densely packed as the
//! serial arm leaves it:
//!
//! | | `PAR_EVAC=0` | bufferless lever | default |
//! |---|---|---|---|
//! | copy phase | serial | parallel | parallel |
//! | fillers in to-space | none | none | some |
//!
//! gen r4w2 (2026-09-23) added the switch `CRATONVM_GC_PAR_EVAC_BUFFERLESS`
//! for that second run; gce e2/o removed it (a bisection lever nobody ran in
//! two rounds, `e1-p-triage.md` table 2B). `into_bufferless` stays as a
//! unit-test helper. See
//! `docs/internal/gc/gengc-oldgen2-bufferless-par-evac-lever-FIXED-20260923.md`.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::{Condvar, Mutex};

use crate::heap::{
    array_element_type_from_tag, object_kind_from_tag, ArrayElementType, ObjectHeader, ObjectKind,
    GC_FLAG_OLD_GEN, HEADER_SIZE,
};
use crate::old_gen::OldGen;

/// Ceiling on the bytes a worker claims from the shared to-space cursor per
/// buffer.
///
/// Large enough that the `fetch_add` is amortised over hundreds of survivors —
/// the median young object is tens of bytes — without being large enough to
/// matter against a young generation measured in megabytes.
pub(crate) const PLAB_MAX_BYTES: usize = 64 * 1024;

/// Preferred floor on the same. Below this the shared cursor's `fetch_add`
/// stops being amortised over much.
const PLAB_MIN_BYTES: usize = 4 * 1024;

/// Buffer size below which a buffer is not worth having at all: the cycle runs
/// bufferless, every object taking its own exact span off the shared cursor.
///
/// A young collection triggers with from-space ~99.9% full, so on a real cycle
/// the slack is sometimes only a few KiB and there is genuinely nothing to
/// carve buffers from. Running bufferless is strictly better than falling back
/// to a single-threaded copy, which is what the alternative was.
const PLAB_FLOOR_BYTES: usize = 512;

/// Divisor relating a cycle's buffer size to what it expects to copy:
/// `from_used / (workers * PLAB_LIVE_DIVISOR)`.
///
/// A FIXED buffer size is wrong at the small end, and measurably so. Eight
/// workers each holding a 64 KiB buffer reserve 512 KiB; on a cycle whose
/// entire live set is 393 KiB, the tails they retire came to **327 KiB of
/// filler for 393 KiB of survivors**. Nothing is unsafe about that — the
/// reservation `plan` makes covers it, and the space returns at the next cycle
/// — but it doubles the arena the next collection has to walk in exchange for
/// nothing. Sizing the buffer against what there is to copy keeps the worst
/// case a bounded fraction of the live set at both ends of the range.
///
/// MEASURED 2026-09-02 (bt18, -Xmx512m, 8 workers): **inert on a real cycle**.
/// Values 1, 4 and 64 produce byte-identical plans, because `plab_bytes` is
/// `min(slack / SLACK_TO_BUFFERS_DIV / workers, from_used / workers / this)`
/// and the SLACK term always wins — `min(11983, 65536)`. It only binds when
/// slack is large relative to the live set, which is the generously-sized
/// to-space a unit test builds, never the ~99.9%-full from-space a real young
/// GC triggers on. Do not spend time tuning it against a real workload; it has
/// no effect there.
const PLAB_LIVE_DIVISOR: usize = 4;

/// Fraction of a buffer above which an object bypasses it and takes its own
/// exact span from the shared cursor: `plab_bytes / PLAB_DIRECT_SHIFT_DIV`.
///
/// Keeps one large object from displacing a buffer's worth of small ones.
/// It is NOT what bounds the wasted tail — [`ParEvac::waste_allowance`] is.
const PLAB_DIRECT_SHIFT_DIV: usize = 8;

/// Share of the to-space slack spent on in-flight buffers; the rest becomes
/// the retirement allowance. See [`ParEvac::plan`].
///
/// MEASURED 2026-09-02 (bt18, n=5 each, interleaved): `cheney_drain` medians
/// 1817 / **1788** / 1927 / 2015 ms for 1 / 2 / 4 / 8. The default has the best
/// median and 1 and 2 are indistinguishable; 4 and 8 are directionally worse
/// and much noisier (div=4 spanned 1588..3183 ms), consistent with tiny buffers
/// meaning more shared-cursor traffic.
///
/// WEAKER EVIDENCE THAN IT LOOKS, and the reason is structural rather than
/// sampling: `plab_bytes` varies BETWEEN RUNS at a fixed divisor (div=4 was
/// seen at 0, 2912 and 5984 bytes) because the trigger point moves, so a
/// value's own samples are not all from the same regime. More repetitions
/// would not fix that. Treat 1..2 as a plateau, not 2 as an optimum.
const SLACK_TO_BUFFERS_DIV: usize = 2;

/// A worker's first old-gen promotion buffer (gen-gc-five item 4).
///
/// Promotion used to be one `OldGen::alloc` per object under the old-gen
/// mutex: a walk up the size buckets, a best-fit scan inside one, the split
/// remainder re-pushed, the sorted free-list cache invalidated, and a
/// `memset` of the block that the copy then overwrote byte for byte. A
/// worker now carves a buffer with `OldGen::alloc_unzeroed` — the copy is the
/// write — and bumps promoted objects out of it, so the mutex is taken once
/// per buffer instead of once per object.
const OLD_PLAB_MIN: usize = 16 * 1024;
/// The promotion buffer's ceiling; each refill doubles up to this.
const OLD_PLAB_MAX: usize = 256 * 1024;
/// A promoted object at least this large bypasses the buffer and takes its
/// own block, so one large array cannot strand most of a buffer.
const OLD_PLAB_DIRECT_MIN: usize = 32 * 1024;

/// Batch size a worker takes from the shared worklist per acquisition.
///
/// MEASURED 2026-09-02: this is a cap that essentially never engages, so its
/// value does not matter on any workload resembling bt18. Instrumented over
/// two moving cycles: **1 of 379 and 1 of 433 acquisitions were clamped by it**,
/// with a mean share of 8 and 11. The binding term is `len.div_ceil(threads)` —
/// a transitive closure keeps a frontier of order `graph width`, and split
/// eight ways that is single digits. Sweeping 16 / 64 / 256 / 1024 moved the
/// median `cheney_drain` by 4.7% against a 19% within-value spread, i.e. not
/// resolvably, which is exactly what a cap that fires 0.25% of the time should
/// do. Keep it as the runaway guard it is; do not read the value as tuned.
const ACQUIRE_CHUNK: usize = 256;
/// Local worklist depth at which a worker publishes its surplus unprompted.
const SPILL_HIGH: usize = 2048;
/// Depth a worker keeps for itself when it spills.
const SPILL_KEEP: usize = 512;
/// Smallest local stack a worker will split in half for an IDLE peer.
///
/// Below this the hand-off costs more than the work it moves — a lock, a
/// notify, and a cold cache line at the far end for one or two objects.
///
/// MEASURED 2026-09-02, and the ONE constant here that demonstrably matters.
/// `cheney_drain` median / `helper_scans` median, bt18, 8 workers:
///
/// | SHARE_MIN | 4 | **8** | 16 | 32 | 64 | 512 |
/// |---|---|---|---|---|---|---|
/// | drain ms | 1736 | **1657** | 1703 | 2109 | 2158 | 2568 |
/// | helper_scans | 1.36M | 1.35M | 1.36M | 1.26M | 1.06M | 0.52M |
///
/// 4..16 is a flat optimum; degradation starts at 32 and 8-vs-64 is a DISJOINT
/// range separation (max 1791 against min 2049). The second row is the
/// mechanism rather than a correlation: raising the threshold makes workers
/// hoard instead of publishing, and helper participation halves. 8 sits
/// mid-plateau with ~2x headroom either side.
const SHARE_MIN: usize = 8;

/// Times a worker LOST the forwarding CAS and adopted the winner's target.
///
/// A parallel evacuator whose CAS never loses is one whose object graph never
/// shared a child between two workers — which is a statement about the
/// workload, not about the code. Published so "the loser arm is exercised" is
/// a number rather than an assumption; the equivalent G1 counter exists
/// because the loser arm carried an unrecorded forward for months.
pub static EVAC_CAS_LOSSES: AtomicU64 = AtomicU64::new(0);

/// Young collections whose copy phase took the parallel evacuator's path.
///
/// gen r5w1/young5: counted by the driver before `ParEvac::drain`, so it also
/// counts a cycle whose drain ran INLINE because the pool had no helper
/// (`drain`'s `helpers == 0` arm, which bypasses `EvacPool::scope` and so is
/// not in `pool_inline_runs` either). "Actually ran in parallel", as this used
/// to say, is `par_helper_scans > 0`.
pub static PAR_EVAC_CYCLES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// [`PAR_EVAC_CYCLES`], counted only for cycles THIS thread drove.
    ///
    /// The global is the production census; this is what a test can assert on.
    /// A test that reads the global before and after its own collection is
    /// asserting on a number every other test in the binary is also bumping,
    /// so it either passes on someone else's cycle or fails on one — and the
    /// thing it is trying to establish ("the gated path I am testing actually
    /// ran") is precisely the thing that must not be taken on faith.
    static PAR_EVAC_CYCLES_HERE: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Parallel copy phases driven by the CALLING thread. See
/// [`PAR_EVAC_CYCLES_HERE`].
pub fn par_evac_cycles_on_this_thread() -> u64 {
    PAR_EVAC_CYCLES_HERE.with(|c| c.get())
}

/// Record that this thread drove a parallel copy phase.
pub(crate) fn note_par_evac_cycle() {
    PAR_EVAC_CYCLES.fetch_add(1, Ordering::Relaxed);
    PAR_EVAC_CYCLES_HERE.with(|c| c.set(c.get() + 1));
}

/// Young collections that WANTED a parallel copy phase and could not have one
/// because to-space lacked the PLAB slack (see [`ParEvac::plan`]).
///
/// Separate from "never asked": a feature that silently declines every cycle
/// and a feature that is switched off report the same zero otherwise.
pub static PAR_EVAC_DECLINED_SLACK: AtomicU64 = AtomicU64::new(0);

/// Bytes lost to PLAB tail fillers, summed over the process.
pub static PAR_EVAC_FILLER_BYTES: AtomicU64 = AtomicU64::new(0);

/// Destinations scanned by a worker OTHER than the driver.
///
/// This is the counter that says whether the parallel evacuator is parallel.
/// "It engaged" and "it spread the work" are different claims, and the first
/// held while the second did not: a 6144-node DAG on eight workers had the
/// driver scan all 6144 because the drain only shared work once a local stack
/// passed 2048 entries, which a narrow frontier never does. Every liveness
/// assertion in the suite was green throughout. Published so the next such
/// regression is a number rather than a wall-clock mystery.
///
/// SCANNED rather than copied, deliberately. A helper that reaches a subgraph
/// the driver has already claimed copies nothing — every child comes back off
/// `evacuate`'s already-forwarded fast path — while still doing the full slot
/// walk. Counting copies would report that helper as idle.
pub static PAR_EVAC_HELPER_SCANS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// [`PAR_EVAC_HELPER_SCANS`] for cycles this thread drove. See
    /// [`PAR_EVAC_CYCLES_HERE`] for why the tests need a per-thread view.
    static PAR_EVAC_HELPER_SCANS_HERE: std::cell::Cell<u64> = const {
        std::cell::Cell::new(0)
    };
}

/// Destinations scanned by non-driver workers, for cycles the CALLING thread
/// drove.
pub fn par_evac_helper_scans_on_this_thread() -> u64 {
    PAR_EVAC_HELPER_SCANS_HERE.with(|c| c.get())
}

/// Record a merged shard's helper scans. Driver-side, after the barrier.
pub(crate) fn note_par_evac_helper_scans(n: u64) {
    if n == 0 {
        return;
    }
    PAR_EVAC_HELPER_SCANS.fetch_add(n, Ordering::Relaxed);
    PAR_EVAC_HELPER_SCANS_HERE.with(|c| c.set(c.get() + n));
}

/// Objects the parallel path promoted to old gen.
///
/// Promotion is the one allocation that takes the old generation's lock, so a
/// zero here means the mutex arm — the only shared-lock contention point in the
/// copy phase — has never been exercised by whatever was run, and any statement
/// about its cost is untested.
pub static PAR_EVAC_PROMOTIONS: AtomicU64 = AtomicU64::new(0);

/// Bytes of old gen stamped with a dead-object filler because a forwarding
/// CAS loser abandoned a copy it had already PROMOTED.
///
/// gengc-round1 2026-09-20. The loser's speculative copy is a complete object
/// carrying this cycle's from-space addresses in its reference slots, and in
/// old gen that is not inert: the dirty-card scan walks every object whose
/// header starts in a dirty card WITHOUT consulting liveness, so a neighbour's
/// deferred card re-mark is enough to make the collector read those stale
/// slots as old→young roots two cycles later (when the semispaces have flipped
/// back and the addresses land inside from-space again). Stamping a
/// reference-free `int[]` over the abandonment keeps the old-gen walk striding
/// correctly, costs the same bytes it already cost, and takes the slots out of
/// circulation until the next major GC reclaims the block.
pub static PAR_EVAC_ABANDONED_OLD_BYTES: AtomicU64 = AtomicU64::new(0);

/// Young survivors the parallel path TENURED EARLY because the to-space
/// region it reserved was exhausted (gengc-round4-move, 2026-09-23).
///
/// `ParEvac::plan`'s budget covers `from_used` plus the buffers and the
/// abandoned-tail allowance, and the serial collector's to-space can never
/// overflow because every destination byte it spends comes from a from-space
/// object. The parallel arm has one consumer the budget does not count: a
/// forwarding-CAS LOSER's abandoned speculative copy, which is a whole object's
/// worth of to-space spent on nothing. On a cycle whose survival is near 100 %
/// (a startup phase building a long-lived structure, the first cycle after a
/// big cache load) those can push the region past its end, and the only
/// answer this path used to have was `process::abort()`. It now promotes the
/// object into old gen instead — exactly the mirror image of the promotion
/// arm's own fallback to to-space when old gen is full. Expected to be ZERO; a
/// non-zero reading is a cycle that would have killed the process.
pub static PAR_EVAC_OVERFLOW_PROMOTIONS: AtomicU64 = AtomicU64::new(0);

/// Per-worker cap on the forward-refusal records a shard carries back to the
/// driver. The driver's own ledger (the heap's `gen_heap::ForwardRefusalLedger`,
/// per heap since gcd d3/m) is capped too, so this only bounds the transient
/// per-shard vector.
const REFUSALS_PER_SHARD: usize = 64;

/// Old-gen holders the parallel path deferred a card re-mark for.
///
/// The old→young fixup is the arm whose omission does not fail now: a missed
/// card is a remembered-set entry lost for one cycle, and the object it should
/// have kept alive is reclaimed later, somewhere else. A zero here means the
/// arm never ran, which is a different thing from it being right.
pub static PAR_EVAC_DEFERRED_CARDS: AtomicU64 = AtomicU64::new(0);

/// Record a cycle's promotions and deferred cards. Driver-side, after the
/// barrier.
pub(crate) fn note_par_evac_arms(promotions: u64, deferred_cards: u64) {
    if promotions > 0 {
        PAR_EVAC_PROMOTIONS.fetch_add(promotions, Ordering::Relaxed);
    }
    if deferred_cards > 0 {
        PAR_EVAC_DEFERRED_CARDS.fetch_add(deferred_cards, Ordering::Relaxed);
    }
}

/// Everything the parallel copy phase publishes about itself.
///
/// A struct rather than a tuple because the fields that matter are the ones
/// that answer "did this arm ever run?", and those keep being added as arms
/// turn out to be reachable-in-principle but unexercised-in-fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParEvacCensus {
    /// Copy phases that ran in parallel. Read this first: zero means the path
    /// never engaged, which is not the same as engaging and doing nothing.
    pub cycles: u64,
    /// Destinations scanned by non-driver workers. Zero beside a non-zero
    /// `cycles` is a load-balancing regression every correctness test passes.
    pub helper_scans: u64,
    /// Forwarding CAS losses — the two-workers-on-one-object arm.
    pub cas_losses: u64,
    /// Cycles that asked for a parallel copy and could not have one.
    pub declined_for_slack: u64,
    /// Bytes stamped with a filler over a retired buffer tail.
    pub filler_bytes: u64,
    /// Objects promoted to old gen (the old-gen mutex arm).
    pub promotions: u64,
    /// Old-gen holders whose card was deferred for re-marking.
    pub deferred_cards: u64,
    /// Bytes of old gen filled over an abandoned promotion (CAS loser).
    /// See [`PAR_EVAC_ABANDONED_OLD_BYTES`].
    pub abandoned_old_bytes: u64,
    /// Young survivors tenured early because the reserved to-space region was
    /// exhausted. See [`PAR_EVAC_OVERFLOW_PROMOTIONS`].
    pub overflow_promotions: u64,
}

/// Snapshot of the parallel copy phase's counters.
pub fn par_evac_census() -> ParEvacCensus {
    ParEvacCensus {
        cycles: PAR_EVAC_CYCLES.load(Ordering::Relaxed),
        helper_scans: PAR_EVAC_HELPER_SCANS.load(Ordering::Relaxed),
        cas_losses: EVAC_CAS_LOSSES.load(Ordering::Relaxed),
        declined_for_slack: PAR_EVAC_DECLINED_SLACK.load(Ordering::Relaxed),
        filler_bytes: PAR_EVAC_FILLER_BYTES.load(Ordering::Relaxed),
        promotions: PAR_EVAC_PROMOTIONS.load(Ordering::Relaxed),
        deferred_cards: PAR_EVAC_DEFERRED_CARDS.load(Ordering::Relaxed),
        abandoned_old_bytes: PAR_EVAC_ABANDONED_OLD_BYTES.load(Ordering::Relaxed),
        overflow_promotions: PAR_EVAC_OVERFLOW_PROMOTIONS.load(Ordering::Relaxed),
    }
}

// ---------------------------------------------------------------------------
// Test seam for the forwarding-CAS loser arm
// ---------------------------------------------------------------------------

#[cfg(test)]
thread_local! {
    /// Run between the object copy and the forwarding CAS, so a test can
    /// install a competing forward and drive the CAS-LOSER arm on demand.
    ///
    /// That arm is otherwise reachable only by a genuine two-worker race on
    /// one object, which no test can schedule — a wide fan-in gets the
    /// already-forwarded FAST path instead, and measuring it confirmed as
    /// much: `EVAC_CAS_LOSSES` stayed at 0 across the whole young-GC suite.
    /// A seam is the price of covering it, and it is worth paying: the
    /// equivalent arm in `g1::SharedEvac::evacuate` carried an unrecorded
    /// forward — a root left pointing into reclaimed from-space — for months,
    /// found only by a 1-in-5 stall under load.
    static RACE_HOOK: std::cell::RefCell<Option<Box<dyn Fn(*mut u8)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Removes the installed [`RACE_HOOK`] on drop.
#[cfg(test)]
pub(crate) struct RaceHookGuard;

#[cfg(test)]
impl Drop for RaceHookGuard {
    fn drop(&mut self) {
        RACE_HOOK.with(|h| *h.borrow_mut() = None);
    }
}

/// Install a [`RACE_HOOK`] for the lifetime of the returned guard.
#[cfg(test)]
pub(crate) fn set_race_hook(f: Box<dyn Fn(*mut u8)>) -> RaceHookGuard {
    RACE_HOOK.with(|h| *h.borrow_mut() = Some(f));
    RaceHookGuard
}

/// Run the installed hook, if any. Compiled out entirely outside `cfg(test)`.
#[inline(always)]
fn run_race_hook(_src: *mut u8) {
    #[cfg(test)]
    {
        // Take the hook out for the call: a hook that re-entered `evacuate`
        // would otherwise recurse forever.
        let hook = RACE_HOOK.with(|h| h.borrow_mut().take());
        if let Some(f) = hook {
            f(_src);
            RACE_HOOK.with(|h| *h.borrow_mut() = Some(f));
        }
    }
}

/// One worker's private to-space bump buffer.
#[derive(Default)]
struct Plab {
    /// Next free address, or 0 when the worker holds no PLAB.
    cursor: usize,
    /// One past the last usable address of the PLAB.
    end: usize,
}

/// One evacuation worker's private results.
///
/// Merged by the driver after the completion barrier. Kept per-worker rather
/// than behind a shared lock because `forwards` is appended to for every
/// object copied — the hottest write of the pause.
#[derive(Default)]
pub(crate) struct EvacShard {
    /// `(from_addr, to_addr)` for every forward this worker OBSERVED, not only
    /// the ones it performed: a fast-path hit and a CAS loss record too. See
    /// the module note — an unrecorded forward is a root that never gets
    /// remapped.
    pub(crate) forwards: Vec<(usize, usize)>,
    /// Old-gen object addresses that must be re-marked dirty because a
    /// reference they hold was forwarded and stayed in young to-space, as
    /// `(holder, offset)` (gen r5w2/roots6, cards3 item 2): the card re-marked
    /// is `holder + offset` after the holder's possible compaction. This
    /// worker scans whole objects, so its offset is always 0 (the header card,
    /// "scan the whole object").
    pub(crate) deferred_dirty_cards: Vec<(usize, usize)>,
    /// Objects this worker copied (CAS winner only).
    pub(crate) objects_copied: usize,
    /// Destinations this worker SCANNED. Distinct from `objects_copied` and
    /// the better measure of participation: a helper that arrives after the
    /// driver has already claimed a subgraph still scans every destination it
    /// takes, and copies none of them.
    pub(crate) objects_scanned: usize,
    /// Copy tally in `gen_heap`'s `COPY_TALLY` layout, so the driver can fold
    /// it straight in: `[bytes_promoted, objects_promoted, bytes_copied_young,
    /// objects_copied_young, re_encounters, copies]`.
    ///
    /// The last pair is the ratio that says whether a slow `cheney_drain` is a
    /// COPYING cost or a LOOKUP cost, and they are counted here unconditionally
    /// rather than behind the `gcpause` flag the serial `copy_tally_arm` checks:
    /// a per-worker `u64` increment is not worth a branch, and leaving the pair
    /// unfed on the parallel path would have printed a breakdown claiming the
    /// drain performed no lookups at all.
    pub(crate) tally: [u64; 6],
    /// `(addr, size)` PLAB tails this worker retired. Filled by the driver.
    pub(crate) plab_gaps: Vec<(usize, usize)>,
    /// `(addr, reason)` for every address this worker REFUSED to forward, in
    /// the serial evacuator's reason vocabulary (`not-an-object-start`,
    /// `invalid-kind-or-element-tag`, `suspect-header`). The driver feeds them
    /// to the same per-cycle ledger `forward_object_impl` writes
    /// (`GenerationalHeap::forward_refusal_reason`), after the barrier.
    ///
    /// gengc-round4-move (2026-09-23): the parallel arm used to refuse
    /// silently, so on every cycle that copied in parallel — the default on a
    /// multi-core host with a young generation past `CRATONVM_GC_PAR_MIN_BYTES`
    /// — the VM's post-GC root audit (`vm/src/memory/gc.rs`) asked the ledger
    /// "was this address refused?" and was told "no" for an address the
    /// collector had in fact declined to move. Capped at
    /// [`REFUSALS_PER_SHARD`].
    pub(crate) refusals: Vec<(usize, &'static str)>,
    /// Local scan worklist (to-space / promoted-object addresses).
    work: Vec<usize>,
    /// This worker's young to-space PLAB.
    plab: Plab,
    /// This worker's OLD-gen promotion buffer (gen-gc-five item 4), carved
    /// unzeroed from the old generation and bump-allocated; see
    /// [`ParEvac::promote_alloc`].
    old_plab: Plab,
    /// Size of the next promotion buffer this worker carves: 0 means
    /// [`OLD_PLAB_MIN`], then doubling to [`OLD_PLAB_MAX`] while the worker
    /// keeps promoting, so a worker that promotes little wastes little.
    next_old_plab: usize,
    // ---- gen r5w3/evac7 (L1): a PARALLEL pinned in-place cycle only -------
    // (`CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`; see [`ParInPlace`]). All
    // empty / zero on every other cycle.
    /// This worker's buffer inside the from-space destination spans. Kept
    /// apart from [`Self::plab`] because its unused tail must NOT get a
    /// filler: it was free (zero) when the cycle began and the driver's
    /// rebuild frees whatever no survivor occupies.
    ip_plab: Plab,
    /// `(addr, size)` of every object this worker left in from-space for the
    /// rebuild: copies into a destination span and survivors kept in place.
    /// (Pinned objects are already in the driver's `live` list.)
    pub(crate) in_place_live: Vec<(usize, usize)>,
    /// `(addr, saved mark)` of every PINNED object this worker claimed. The
    /// claim is a self-forward (`ObjectHeader::try_self_forward`), which the
    /// driver undoes with `restore_self_forwarded_mark` after the drain.
    pub(crate) in_place_pinned: Vec<(usize, u32)>,
    /// `(addr, saved mark)` of every survivor this worker KEPT in place
    /// because neither a destination span nor old gen could take it; claimed
    /// and restored the same way.
    pub(crate) in_place_kept: Vec<(usize, u32)>,
    /// Survivors this worker copied into a destination span.
    pub(crate) in_place_copies: u64,
    /// Survivors too young to tenure that this worker promoted because the
    /// destination spans could not take them.
    pub(crate) in_place_overflow: u64,
}

// gce e2/o: `EvacShard::seed_first_old_plab` / `promoted_bytes` (proposal W4,
// `CRATONVM_GC_PAR_EVAC_OLD_PLAB_HINT`) are removed with their switch.

/// The plan a driver commits to before opening the parallel phase.
pub(crate) struct EvacPlan {
    /// Absolute address of the first byte workers may allocate.
    pub(crate) region_start: usize,
    /// One past the last byte workers may allocate.
    pub(crate) region_end: usize,
    /// Bytes each worker claims per to-space buffer, sized against what this
    /// cycle has to copy. See [`PLAB_LIVE_DIVISOR`].
    pub(crate) plab_bytes: usize,
    /// Bytes of abandoned buffer tail this cycle may spend. See
    /// [`ParEvac::plan`].
    pub(crate) waste_allowance: usize,
    /// Workers this cycle dispatches. Today this is exactly the policy's ask
    /// (clamped to at least 1): `plan` used to scale the count DOWN to what
    /// the slack could buy buffers for, and stopped when a thin-slack cycle
    /// learned to run BUFFERLESS instead (gengc-round4-move, 2026-09-23 — the
    /// doc here still described the old narrowing). The field is kept rather
    /// than folded into the caller's number so a future plan that does narrow
    /// has one place to say so.
    pub(crate) workers: usize,
    /// Bytes of the to-space tail the workers may touch — survivors, one live
    /// buffer per worker, and the abandoned-tail allowance.
    ///
    /// The driver must COMMIT this much (`Arena::commit_parallel_evacuation_region`)
    /// before dispatching: the backing store maps lazily, so an uncommitted
    /// write faults rather than reading zero.
    pub(crate) reserved: usize,
}

impl EvacPlan {
    /// The plan of a parallel pinned in-place cycle (gen r5w3/evac7, L1): no
    /// to-space region at all (`region_start == region_end`, nothing
    /// reserved or committed), `workers` workers. Young destinations come from
    /// [`ParInPlace`]'s spans; the empty region makes any to-space allocation
    /// fail rather than write outside what was mapped.
    pub(crate) fn in_place(to_cursor_addr: usize, workers: usize) -> Self {
        Self {
            region_start: to_cursor_addr,
            region_end: to_cursor_addr,
            plab_bytes: 0,
            waste_allowance: 0,
            workers: workers.max(1),
            reserved: 0,
        }
    }

    /// The same plan with every buffer removed: `plab_bytes = 0`, no
    /// abandoned-tail allowance, and a reservation of exactly the survivors.
    ///
    /// The mechanism of the bufferless bisection lever (see the module note);
    /// `collect_garbage_inner` applies it to the plan when
    /// `CRATONVM_GC_PAR_EVAC_BUFFERLESS` is set (gen r4w2, 2026-09-23).
    /// A bufferless cycle is not a special mode: it is what `plan`
    /// already returns whenever the slack cannot pay for buffers, so this only
    /// forces a configuration the collector runs on its own. Every object
    /// takes an exact span off the shared cursor, so the region consumption is
    /// exactly the survivors, which the collection's own Cheney backstop
    /// (`to_headroom >= from_used`) already guarantees.
    ///
    /// gce e2/o: test-only since its switch (`CRATONVM_GC_PAR_EVAC_BUFFERLESS`)
    /// was removed; the plan tests still use it to describe the thin-slack
    /// plan `plan` builds on its own.
    #[cfg(test)]
    pub(crate) fn into_bufferless(self) -> Self {
        // `reserved` is `from_used + plab_bytes * workers + waste_allowance` by
        // construction in `plan`; recover `from_used` from it rather than
        // threading it through a second time. Saturating so a hand-built plan
        // that breaks the identity degrades to a smaller reservation rather
        // than a wrap.
        let from_used = self
            .reserved
            .saturating_sub(self.plab_bytes.saturating_mul(self.workers))
            .saturating_sub(self.waste_allowance);
        Self {
            region_end: self.region_start + from_used,
            plab_bytes: 0,
            waste_allowance: 0,
            reserved: from_used,
            ..self
        }
    }
}

/// Smallest remainder a destination span must keep to stay worth searching;
/// below it the span is retired for every worker (the serial twin is
/// `gen_heap::IN_PLACE_MIN_DEST_SPAN`, and the same value).
const IN_PLACE_MIN_SPAN: usize = 256;

/// Spans past the first live one a destination claim searches before it
/// gives up (the last span, the large tail window, is always tried too). The
/// serial twin is `gen_heap::IN_PLACE_DEST_SEARCH`.
const IN_PLACE_SPAN_SEARCH: usize = 32;

/// A worker's buffer inside the destination spans. Smaller than a to-space
/// PLAB because the spans are free blocks of a swept young generation, often
/// only a few KiB each.
const IN_PLACE_PLAB_BYTES: usize = 4 * 1024;

/// An object at least this large takes an exact span of its own instead of
/// retiring the worker's buffer for it.
const IN_PLACE_DIRECT_BYTES: usize = IN_PLACE_PLAB_BYTES / 4;

/// The shared state of a PARALLEL pinned in-place young cycle (gen r5w3/evac7,
/// limit L1 of `docs/known-issues/gc/gengc-r4w5-pinned5-in-place-copy-limits-20260924.md`,
/// wave 6's design; `CRATONVM_GEN_PINNED_YOUNG_COPY_PARALLEL`).
///
/// The serial pinned cycle (`gen_heap`'s `InPlaceEvac`) copies every survivor
/// that is not on a pinned page into bytes of from-space that were FREE when
/// the cycle began, and leaves the pinned objects where they are. This is the
/// same cycle on the parallel evacuator, with the four changes wave 6 named:
///
/// 1. **A span pool in place of the to-space region.** `span_*` are the
///    serial plan's destination spans (ascending, disjoint), each with a
///    shared cursor a worker advances by CAS to claim a buffer or an exact
///    span; `span_next` is the first span that may still serve anyone.
/// 2. **Pinned objects are claimed by a self-forward.** The first worker to
///    reach a pinned object CASes its mark word to FORWARDED|SELF
///    (`try_self_forward`) and scans it; every other worker reads the
///    forward and gets the object's own address back. The driver restores
///    each claimed mark word after the drain
///    (`ObjectHeader::restore_self_forwarded_mark`), before any serial phase
///    reads it, and hands the claims to the serial state as `visited`.
/// 3. **"Kept" is claimed the same way.** A survivor neither a span nor old
///    gen can take stays where it is; the self-forward is what stops a second
///    worker, whose own buffer might still fit it, from copying it as well.
/// 4. **`dest_written` over the pool.** A slot re-read after its rewrite (the
///    same card slot seeded twice) names a destination byte, not an object
///    start; [`Self::dest_written`] recognises it so it is not recorded as a
///    refusal.
///
/// Nothing here writes a filler: a destination byte no survivor occupies was
/// free (zero) at the start of the cycle and stays zero — a CAS loser ZEROES
/// its abandoned young copy — and the driver's rebuild frees the complement of
/// the live set.
pub(crate) struct ParInPlace {
    /// `(base, size)` of every pinned object, ascending and disjoint.
    pinned: Vec<(usize, usize)>,
    /// `[first pinned base, last pinned end)`, the cheap reject.
    pinned_lo: usize,
    pinned_hi: usize,
    /// Destination span starts, ascending.
    span_start: Vec<usize>,
    /// Destination span ends (exclusive), index-aligned with `span_start`.
    span_end: Vec<usize>,
    /// Destination span cursors: the next unclaimed byte of each span.
    span_cursor: Vec<AtomicUsize>,
    /// First span that may still serve a claim.
    span_next: AtomicUsize,
}

impl ParInPlace {
    /// Build from the serial plan: `pinned` as `(base, size)` ascending, and
    /// `dest` as `(start, cursor, end)` ascending (the serial `InPlaceEvac`
    /// shape; the cursor is `start` on a fresh cycle).
    pub(crate) fn new(pinned: &[(usize, usize)], dest: &[(usize, usize, usize)]) -> Self {
        let pinned_lo = pinned.first().map_or(0, |&(b, _)| b);
        let pinned_hi = pinned.iter().map(|&(b, s)| b + s).max().unwrap_or(0);
        Self {
            pinned: pinned.to_vec(),
            pinned_lo,
            pinned_hi,
            span_start: dest.iter().map(|&(s, _, _)| s).collect(),
            span_end: dest.iter().map(|&(_, _, e)| e).collect(),
            span_cursor: dest.iter().map(|&(_, c, _)| AtomicUsize::new(c)).collect(),
            span_next: AtomicUsize::new(0),
        }
    }

    /// Is `addr` the BASE of a pinned object?
    #[inline]
    fn is_pinned_base(&self, addr: usize) -> bool {
        addr >= self.pinned_lo
            && addr < self.pinned_hi
            && self.pinned.binary_search_by_key(&addr, |&(b, _)| b).is_ok()
    }

    /// Is `addr` a byte this cycle already claimed for a copy (below its
    /// span's cursor)? The serial twin is `InPlaceEvac::dest_written`.
    #[inline]
    pub(crate) fn dest_written(&self, addr: usize) -> bool {
        let i = self.span_start.partition_point(|&s| s <= addr);
        i > 0 && addr < self.span_cursor[i - 1].load(Ordering::Acquire)
    }

    /// Claim `[base, base + len)` with `size <= len <= want` from the first
    /// span that has `size` bytes left, searching at most
    /// [`IN_PLACE_SPAN_SEARCH`] spans past `span_next` plus the last one.
    /// `None` when no searched span can take `size`.
    fn claim(&self, size: usize, want: usize) -> Option<(usize, usize)> {
        let n = self.span_end.len();
        let first = self.span_next.load(Ordering::Relaxed);
        let stop = n.min(first.saturating_add(IN_PLACE_SPAN_SEARCH));
        for i in first..stop {
            if let Some(got) = self.claim_in(i, size, want) {
                return Some(got);
            }
            // A span that cannot serve even a minimal request is retired for
            // everyone. Only the worker that sees `span_next == i` advances
            // it, so a span below a still-useful one is never skipped.
            let room = self.span_end[i].saturating_sub(self.span_cursor[i].load(Ordering::Relaxed));
            if room < IN_PLACE_MIN_SPAN {
                let _ = self.span_next.compare_exchange(
                    i,
                    i + 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                );
            }
        }
        // The tail window is the one large span; try it past the window too.
        if stop < n {
            return self.claim_in(n - 1, size, want);
        }
        None
    }

    /// One CAS loop on span `i`: take `min(want, room)` bytes if `room >= size`.
    fn claim_in(&self, i: usize, size: usize, want: usize) -> Option<(usize, usize)> {
        let end = self.span_end[i];
        let cursor = &self.span_cursor[i];
        let mut cur = cursor.load(Ordering::Relaxed);
        loop {
            let room = end.saturating_sub(cur);
            if room < size {
                return None;
            }
            let take = want.max(size).min(room);
            // AcqRel with `dest_written`'s Acquire load. The slot rewrite that
            // makes a reader ask is a Relaxed store, so this is not a
            // guarantee that the reader sees the claim; a reader that misses
            // it records one spurious `not-an-object-start` refusal and still
            // returns the address unchanged, which is the right answer.
            match cursor.compare_exchange_weak(cur, cur + take, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => return Some((cur, cur + take)),
                Err(now) => cur = now,
            }
        }
    }

    /// Every span's final cursor, index-aligned with the plan's `dest`, for the
    /// driver's write-back into the serial state.
    pub(crate) fn cursors(&self) -> Vec<usize> {
        self.span_cursor
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .collect()
    }
}

struct DrainState {
    stack: Vec<usize>,
    idle: usize,
    /// Workers that UNWOUND out of [`ParEvac::run_worker`]. Counted towards the
    /// termination test beside `idle`, so a panicking worker cannot leave the
    /// others waiting for an idle count that will never be reached. See
    /// [`WorkerExit`].
    dead: usize,
    done: bool,
}

/// Registers a worker's death with the drain's termination handshake if it
/// unwinds (gengc-round4-move, 2026-09-23).
///
/// Termination is "every worker idle and the shared stack empty". A worker
/// that PANICS inside `scan_object` — the evacuator reads mutator-written
/// headers and a corrupt one can trip a bounds or overflow check — leaves
/// `run_worker` without ever registering idle, so every surviving worker
/// (including the DRIVER) waits on `cv` for a count that cannot be reached.
/// `EvacPool::scope` catches the helper's panic and would re-raise it at the
/// barrier, but the driver never gets there: it is parked inside its own
/// `run_worker`. The pause hangs with every mutator stopped and the panic that
/// caused it unreported — precisely the failure `young_mark::drain_parallel`'s
/// `WorkerExit` (gengc-mark, 2026-09-20) was written to prevent in the mark
/// phase, which this drain never received. The same shape holds with the
/// roles reversed: a DRIVER panic left every helper parked on `cv`, so the
/// pool's "a driver panic still waits for the workers" barrier waited forever.
///
/// The closure is incomplete once a worker has died, which does not matter:
/// the panic is re-raised on the driver after the pool's barrier and fails
/// the collection.
struct WorkerExit<'g, 'a> {
    evac: &'g ParEvac<'a>,
    threads: usize,
}

impl Drop for WorkerExit<'_, '_> {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        // `run_worker` never panics while holding `shared` (its locked regions
        // are Vec moves and counter updates), and its guard is declared after
        // this one, so it has already been released by the time this runs.
        let mut g = self.evac.shared.lock();
        g.dead += 1;
        if g.idle + g.dead >= self.threads {
            g.done = true;
        }
        drop(g);
        self.evac.cv.notify_all();
    }
}

/// Immutable shared state handed to every evacuation worker.
pub(crate) struct ParEvac<'a> {
    /// Young from-space `[base, base + used)` — the exact span the
    /// object-start bitmap covers.
    from_base: usize,
    from_used: usize,
    /// Young from-space `[base, base + capacity)` — the extent an object's
    /// computed footprint must fit inside.
    from_end_cap: usize,
    starts: &'a crate::young_mark::ObjectStartBits,
    /// Young to-space arena extent, for validating a forwarding target.
    to_base: usize,
    to_end_cap: usize,
    /// Shared to-space bump cursor (absolute address) and its ceiling.
    to_cursor: AtomicUsize,
    to_region_end: usize,
    /// This cycle's per-worker buffer size, and the object size at or above
    /// which an allocation bypasses the buffer. Both from [`EvacPlan`].
    plab_bytes: usize,
    plab_direct_bytes: usize,
    /// Bytes of abandoned buffer tail still affordable this cycle.
    ///
    /// This is what keeps the reservation honest without worst-casing it: a
    /// worker may retire a partly-used buffer only while the allowance covers
    /// the tail it is throwing away. Once it is spent, an object that will not
    /// fit the current buffer takes its own exact span off the shared cursor
    /// instead, so the buffer is kept and nothing further is abandoned.
    waste_allowance: AtomicUsize,
    /// The old generation's allocator. Promotion is the only path that takes
    /// this, and it is a minority of a young cycle's copies by design.
    old_gen: Mutex<&'a mut OldGen>,
    /// Old-generation extent as two plain words, so a worker can answer "did
    /// this land in old gen?" without taking the lock (`OldGen::extent`
    /// exists for exactly this).
    old_lo: usize,
    old_hi: usize,
    /// Tenure every survivor regardless of age (the semispace death-spiral
    /// break; see `forward_object_impl`).
    force_promote_all: bool,
    /// `PROMOTION_AGE` from `gen_heap`.
    promotion_age: u8,
    /// `cratonvm_types::loader_pin::loader_pinning_enabled()`, hoisted.
    loader_pin_on: bool,
    /// `CRATONVM_FWD_RESOLVE_STRICT`: refuse to forward through an object
    /// whose class id does not resolve (false-root hardening).
    fwd_resolve_strict: bool,
    /// Shared worklist for the transitive closure.
    shared: Mutex<DrainState>,
    cv: Condvar,
    /// A relaxed mirror of `DrainState::idle`, readable without the lock.
    ///
    /// The per-object sharing decision has to cost a single load, so it reads
    /// this rather than taking the mutex. Approximate by construction and
    /// harmless in both directions — see [`ParEvac::run_worker`].
    idle_hint: AtomicUsize,
    /// gen r5w3/evac7 (L1): `Some` on a parallel pinned in-place cycle, whose
    /// young destinations are from-space spans rather than to-space. See
    /// [`ParInPlace`] and [`Self::with_in_place`].
    in_place: Option<&'a ParInPlace>,
    /// gcd d2/h: the card table's base when its reference-array marks are
    /// element-precise, `None` otherwise. [`ParEvac::scan_object`] hands it to
    /// `gen_heap::HolderCardDefer`, so an old reference-array holder defers
    /// its young elements' cards, not its header's. Set by
    /// [`Self::with_element_cards`]; `None` (header-grained, the old
    /// behaviour) unless the driver asks.
    element_card_base: Option<usize>,
}

impl<'a> ParEvac<'a> {
    /// Decide whether a parallel copy phase is affordable, and reserve
    /// to-space for it.
    ///
    /// Returns `None` only when to-space cannot cover from-space at all, or the
    /// cursor is off the 8-grid — both of which mean something upstream is
    /// already wrong. It does NOT decline for want of slack; see below. The
    /// caller falls back to the serial copy, which is always correct, so every
    /// decision here is a performance choice and never a correctness one.
    ///
    /// # The budget, and why it is not a worst-case waste bound
    ///
    /// It was one, and that made the whole feature unreachable in production.
    /// The first version budgeted `from_used + from_used/7 + workers*plab`,
    /// where `from_used/7` is the provable worst case for abandoned buffer
    /// tails. MEASURED on `bench/BinT.java` at depth 18, `-Xmx512m`, on the
    /// first real moving cycle:
    ///
    /// ```text
    /// to_headroom=134217728  from_used=134096736  budget=153777700
    /// ```
    ///
    /// A young collection triggers when from-space is **99.91% full**, and the
    /// Cheney invariant only ever promises `to_headroom >= from_used` — here by
    /// 120,992 bytes. A 19 MB waste term cannot fit in 121 KB, so the parallel
    /// evacuator declined, and would have declined on every cycle of every real
    /// workload while every unit test — each sizing its to-space generously —
    /// stayed green.
    ///
    /// So the waste is BUDGETED, not worst-cased. The slack that actually
    /// exists is split: half to in-flight buffers, half to the
    /// `waste_allowance`, which `plab_alloc` spends and then stops spending,
    /// falling back to exact per-object spans off the shared cursor rather than
    /// abandoning another tail. Total region consumption is
    /// `direct_bytes + buffers_claimed * plab`, which is at most
    /// `from_used + allowance + workers * plab` — exactly `to_headroom`.
    ///
    /// Sizing the buffers from the slack was still not enough, which took a
    /// SECOND end-to-end run to see: eight workers need `8 * PLAB_MIN_BYTES` of
    /// it, and the slack at the trigger point lands either side of that from
    /// one collection to the next, so the copy phase engaged on roughly half of
    /// bt18's cycles and fell back to serial on the rest — a single run reports
    /// that as a success. Buffers are an OPTIMISATION: with `plab_bytes == 0`
    /// every object takes its own exact span, consuming exactly the survivors,
    /// which `to_headroom >= from_used` already guarantees. So a thin-slack
    /// cycle now runs BUFFERLESS rather than serial, and this function has no
    /// slack decline left at all.
    ///
    /// The region start must be 8-aligned, because the driver commits the
    /// result by advancing the arena's own cursor to wherever the workers
    /// stopped: a misaligned start would leave a sub-8-byte pad below that
    /// cursor, and a pad that small cannot hold even the smallest gap sentinel,
    /// so nothing could make it walkable. Every allocation this collector makes
    /// is an 8-multiple, so a misaligned cursor means something upstream is
    /// already wrong — decline rather than paper over it.
    pub(crate) fn plan(
        to_headroom: usize,
        to_cursor_addr: usize,
        from_used: usize,
        workers: usize,
    ) -> Option<EvacPlan> {
        if to_cursor_addr % 8 != 0 {
            tracing::warn!(
                to_cursor_addr,
                "GC: young to-space cursor is not 8-aligned — declining the parallel copy phase"
            );
            return None;
        }
        let workers = workers.max(1);
        // Everything above the survivors.
        //
        // The only thing that can make a parallel copy impossible is to-space
        // not covering from-space at all — and the caller has already refused
        // the whole collection in that case (the "Cheney invariant" backstop in
        // `collect_garbage_inner`). So this subtraction is the last decline
        // this function has, and it should never fire.
        let Some(slack) = to_headroom.checked_sub(from_used) else {
            PAR_EVAC_DECLINED_SLACK.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                to_headroom,
                from_used,
                "GC: declining the parallel copy phase — to-space does not cover \
                 from-space, which the collection's own backstop should have caught",
            );
            return None;
        };
        // Buffers are an OPTIMISATION, not a precondition — which is the whole
        // reason this no longer declines for want of slack.
        //
        // Measured on bt18: with a fixed eight workers the buffer half of the
        // slack has to cover `8 * PLAB_MIN_BYTES`, and the slack at the trigger
        // point lands either side of that from one collection to the next, so
        // the copy phase engaged on roughly half the cycles and silently fell
        // back to serial on the rest. Scaling the worker count down helped and
        // did not fix it; the slack is sometimes just a few KiB.
        //
        // With `plab_bytes == 0` every object takes its own exact span off the
        // shared cursor, so the region consumption is exactly the survivors —
        // which `to_headroom >= from_used` already guarantees. That costs one
        // CAS (`bump_shared`) per object, measured against a `memcpy` per object, and
        // it keeps every worker copying. A buffer is what removes that atomic
        // when there is room to pay for it, nothing more.
        let desired =
            (from_used / workers / PLAB_LIVE_DIVISOR).clamp(PLAB_MIN_BYTES, PLAB_MAX_BYTES);
        let plab_bytes = match (slack / SLACK_TO_BUFFERS_DIV / workers).min(desired) & !7 {
            n if n < PLAB_FLOOR_BYTES => 0,
            n => n,
        };
        // The allowance is CAPPED at one more buffer per worker rather than
        // taking all the remaining slack. Two reasons, and the second is not
        // optional: a bigger allowance buys nothing once a worker can refill
        // once, and the reservation below is COMMITTED up front — the backing
        // store maps lazily, so an allowance of "all the slack" would map the
        // whole to-space tail on every cycle and throw away exactly what the
        // lazy store is for.
        let waste_allowance = (slack - plab_bytes * workers).min(plab_bytes * workers);
        // Everything a worker can touch: the survivors themselves, one live
        // buffer each, and the tails the allowance lets them abandon.
        let reserved = from_used + plab_bytes * workers + waste_allowance;
        debug_assert!(reserved <= to_headroom);
        Some(EvacPlan {
            region_start: to_cursor_addr,
            // The workers' ceiling is the RESERVATION, not the tail: past it
            // the backing store is reserved but unmapped, and a write there
            // faults rather than reading zero.
            region_end: to_cursor_addr + reserved,
            plab_bytes,
            waste_allowance,
            workers,
            reserved,
        })
    }

    /// Build the shared context. `old_gen` is borrowed for the whole
    /// evacuation; the driver gets it back when this value is dropped.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        from_base: usize,
        from_used: usize,
        from_capacity: usize,
        starts: &'a crate::young_mark::ObjectStartBits,
        to_base: usize,
        to_capacity: usize,
        plan: &EvacPlan,
        old_gen: &'a mut OldGen,
        force_promote_all: bool,
        promotion_age: u8,
        loader_pin_on: bool,
        fwd_resolve_strict: bool,
    ) -> Self {
        let (old_lo, old_hi) = old_gen.extent();
        Self {
            from_base,
            from_used,
            from_end_cap: from_base + from_capacity,
            starts,
            to_base,
            to_end_cap: to_base + to_capacity,
            to_cursor: AtomicUsize::new(plan.region_start),
            to_region_end: plan.region_end,
            plab_bytes: plan.plab_bytes,
            plab_direct_bytes: (plan.plab_bytes / PLAB_DIRECT_SHIFT_DIV).max(8),
            waste_allowance: AtomicUsize::new(plan.waste_allowance),
            old_gen: Mutex::new(old_gen),
            old_lo,
            old_hi,
            force_promote_all,
            promotion_age,
            loader_pin_on,
            fwd_resolve_strict,
            shared: Mutex::new(DrainState {
                stack: Vec::new(),
                idle: 0,
                dead: 0,
                done: false,
            }),
            cv: Condvar::new(),
            idle_hint: AtomicUsize::new(0),
            in_place: None,
            element_card_base: None,
        }
    }

    /// Defer element-precise cards for old reference-array holders (gcd d2/h,
    /// `gengc-r4w3-cards3-precise-array-cards-residuals` item 3 producer
    /// (c)): `base` is `Some(card_table.base_addr())` when the table's marks
    /// are element-precise, `None` to keep the header-grained deferral. See
    /// `gen_heap::HolderCardDefer`.
    pub(crate) fn with_element_cards(mut self, base: Option<usize>) -> Self {
        self.element_card_base = base;
        self
    }

    /// Run this cycle as a parallel pinned in-place cycle (gen r5w3/evac7,
    /// L1): young survivors are copied into `ip`'s from-space spans instead of
    /// to-space, pinned objects are claimed in place, and a survivor nothing
    /// can take is kept. The plan's to-space region is then unused; the
    /// driver builds it empty (`region_start == region_end`).
    pub(crate) fn with_in_place(mut self, ip: &'a ParInPlace) -> Self {
        self.in_place = Some(ip);
        self
    }

    /// On a parallel pinned in-place cycle, is `addr` a destination byte this
    /// cycle already claimed? `false` on every other cycle.
    #[inline]
    fn in_place_dest_written(&self, addr: usize) -> bool {
        self.in_place.is_some_and(|ip| ip.dest_written(addr))
    }

    /// Young destination on a parallel pinned in-place cycle: the worker's
    /// span buffer, a fresh buffer claimed from the span pool, or — for an
    /// object of [`IN_PLACE_DIRECT_BYTES`] and up — an exact span of its own.
    /// `None` when the searched spans cannot take `size`.
    fn in_place_alloc(ip: &ParInPlace, shard: &mut EvacShard, size: usize) -> Option<usize> {
        if shard.ip_plab.cursor != 0 && size <= shard.ip_plab.end - shard.ip_plab.cursor {
            let addr = shard.ip_plab.cursor;
            shard.ip_plab.cursor += size;
            return Some(addr);
        }
        if size >= IN_PLACE_DIRECT_BYTES {
            // Keep the buffer; a later small object may still fit it.
            return ip.claim(size, size).map(|(base, _)| base);
        }
        // The old buffer's tail is simply abandoned: it was free (zero) and
        // nothing writes it, so the rebuild frees it with the rest.
        let (base, end) = ip.claim(size, IN_PLACE_PLAB_BYTES)?;
        shard.ip_plab.cursor = base + size;
        shard.ip_plab.end = end;
        Some(base)
    }

    /// Claim `old_ptr` IN PLACE by a self-forward (a pinned object, or a
    /// survivor nothing can take): the winner records the identity forward,
    /// queues the object for its in-place scan and remembers the mark word to
    /// restore; a loser returns whatever the winner published (the object's
    /// own address for a peer's self-forward, a copy's address if a peer
    /// copied it first).
    ///
    /// # Safety
    /// `old_ptr` must be a proved from-space object start whose mark word was
    /// `observed` (not forwarded) when the caller read it.
    unsafe fn claim_in_place(
        &self,
        shard: &mut EvacShard,
        hdr: &ObjectHeader,
        old_ptr: *mut u8,
        observed: u32,
        kept_size: Option<usize>,
    ) -> *mut u8 {
        let old_addr = old_ptr as usize;
        match hdr.try_self_forward(observed) {
            Ok(()) => {
                match kept_size {
                    Some(size) => {
                        shard.in_place_kept.push((old_addr, observed));
                        shard.in_place_live.push((old_addr, size));
                    }
                    None => shard.in_place_pinned.push((old_addr, observed)),
                }
                // The identity pair is how reference processing, finalizer
                // resurrection and the loader rescue learn the object
                // survived — the serial path inserts the same pair.
                shard.forwards.push((old_addr, old_addr));
                shard.work.push(old_addr);
                old_ptr
            }
            Err(winner) if ObjectHeader::is_forwarded_mark(winner) => {
                let fwd = wait_for_claimed_forwarding(hdr);
                if fwd == 0 {
                    Self::note_refusal(shard, old_addr, "suspect-header");
                    return old_ptr;
                }
                if fwd != old_addr && self.in_old(fwd) {
                    // A peer PROMOTED it; its own shard carries the pair, but
                    // recording it again is what the CAS-loser arm does too.
                    shard.forwards.push((old_addr, fwd));
                }
                shard.tally[4] += 1;
                fwd as *mut u8
            }
            Err(_) => {
                tracing::warn!(
                    "gen_evac::claim_in_place: self-forward lost to a non-forwarded word at {:p}",
                    old_ptr,
                );
                old_ptr
            }
        }
    }

    /// Is `addr` inside young from-space's live extent?
    #[inline]
    pub(crate) fn in_from(&self, addr: usize) -> bool {
        addr >= self.from_base && addr < self.from_base + self.from_used
    }

    /// Is `addr` anywhere inside young from-space's BACKING, `[base, base +
    /// capacity)`?
    ///
    /// The range the serial evacuator screens with (`Arena::contains` is a
    /// capacity test), so the two arms hand the same addresses to their
    /// forwarders and refuse the same ones. A reference past the allocation
    /// frontier names no object and is refused either way; what the wider
    /// screen buys is that the refusal is RECORDED, exactly as
    /// `forward_object_impl` records it (gengc-round4-move, 2026-09-23).
    #[inline]
    pub(crate) fn in_from_extent(&self, addr: usize) -> bool {
        addr >= self.from_base && addr < self.from_end_cap
    }

    /// Record one refusal for the driver's ledger. See [`EvacShard::refusals`].
    #[cold]
    fn note_refusal(shard: &mut EvacShard, addr: usize, reason: &'static str) {
        if shard.refusals.len() < REFUSALS_PER_SHARD {
            shard.refusals.push((addr, reason));
        }
    }

    /// Did `addr` land in old gen? Lock-free by construction — the old
    /// generation's backing storage does not move during a young cycle.
    #[inline]
    pub(crate) fn in_old(&self, addr: usize) -> bool {
        addr >= self.old_lo && addr < self.old_hi
    }

    /// The to-space bump cursor's final value: what the driver must publish as
    /// the arena's cursor once every worker has stopped.
    pub(crate) fn to_cursor_end(&self) -> usize {
        self.to_cursor.load(Ordering::Relaxed)
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Claim `size` bytes of young to-space for this worker.
    ///
    /// Three ways to be served, in order of preference:
    ///
    /// 1. from the worker's current buffer, which costs a compare and an add;
    /// 2. by retiring that buffer and claiming a fresh one — but ONLY while
    ///    [`Self::waste_allowance`] covers the tail being abandoned;
    /// 3. by an exact span off the shared cursor, one CAS (`bump_shared`).
    ///
    /// (3) is also the direct path for an object large enough to distort a
    /// buffer. It is the fallback rather than the failure case precisely
    /// because the allowance can run out: a young collection triggers with
    /// from-space ~99.9% full, so on a real cycle there is very little slack to
    /// spend and most of it goes to the buffers themselves. Degrading to an
    /// atomic add per object is a throughput cost paid against a memcpy; a
    /// fourth option that abandoned tails anyway would be a correctness cost.
    ///
    /// Returns `None` only when the reserved region is exhausted. [`Self::plan`]'s
    /// budget covers every byte a CAS WINNER spends, but not a CAS loser's
    /// abandoned speculative copy, so on a near-total-survival cycle this is
    /// reachable; `evacuate` then tenures the object early
    /// ([`PAR_EVAC_OVERFLOW_PROMOTIONS`]) and aborts only if old gen refuses too.
    fn plab_alloc(&self, shard: &mut EvacShard, size: usize) -> Option<usize> {
        // Bufferless cycle: the slack could not pay for buffers at all. Every
        // object takes its own exact span, which consumes exactly the
        // survivors. See `plan`.
        if self.plab_bytes == 0 || size >= self.plab_direct_bytes {
            return self.bump_shared(size);
        }
        if shard.plab.cursor + size <= shard.plab.end {
            let addr = shard.plab.cursor;
            shard.plab.cursor += size;
            return Some(addr);
        }
        let tail = shard.plab.end.saturating_sub(shard.plab.cursor);
        if !self.try_charge_waste(tail) {
            // The allowance is spent. Keep the buffer — a later, smaller object
            // may still fit it — and serve this one directly.
            return self.bump_shared(size);
        }
        // gengc-round1 2026-09-20: retire only the YOUNG buffer here. This path
        // used to call `retire_plab`, which also hands the worker's OLD-gen
        // promotion buffer back under the old generation's mutex — a lock
        // acquisition on the young to-space allocation path, throwing away a
        // perfectly good promotion buffer and forcing the next promotion to
        // take the mutex again to carve a replacement. The two buffers are
        // independent; only the driver's end-of-drain `retire_plab` needs both.
        Self::retire_young_plab(shard);
        let Some(base) = self.bump_shared(self.plab_bytes) else {
            // gengc-round1 2026-09-20: the reserved region has no room for a
            // whole BUFFER, which says nothing about whether it has room for
            // this OBJECT — and returning `None` here sends `evacuate` to its
            // `process::abort()` arm. Take an exact span instead; that is the
            // same fallback the spent-allowance branch above uses, and it is
            // what `plan`'s "buffers are an optimisation, not a precondition"
            // budget argument already assumes.
            return self.bump_shared(size);
        };
        shard.plab.cursor = base + size;
        shard.plab.end = base + self.plab_bytes;
        Some(base)
    }

    /// Charge `tail` bytes against the abandoned-tail allowance, or refuse.
    ///
    /// A CAS loop rather than `fetch_sub`, because an unconditional subtract
    /// could take the counter below zero (it is unsigned — below zero means a
    /// wrap to a huge value, i.e. an allowance that never runs out, i.e. the
    /// reservation this exists to enforce silently removed).
    fn try_charge_waste(&self, tail: usize) -> bool {
        if tail == 0 {
            return true;
        }
        self.waste_allowance
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |left| {
                left.checked_sub(tail)
            })
            .is_ok()
    }

    /// Take `size` bytes off the shared to-space cursor.
    ///
    /// A compare-exchange loop that never moves the cursor past
    /// `to_region_end`, so a `None` here is EXACT: the region really has fewer
    /// than `size` bytes left.
    ///
    /// gengc-round4-move (2026-09-23): this used to be `fetch_add`, then a
    /// bounds check, then a `fetch_sub` to undo a failure. The undo is what was
    /// wrong. Between a failing worker's add and its subtract the cursor sits
    /// past the region end, and every OTHER worker's bump in that window reads
    /// the inflated value and fails too — including a small request the region
    /// could have served. Near the end of the region (a high-survival cycle,
    /// where the reservation is consumed almost exactly) one worker's refused
    /// buffer refill therefore spuriously refused a peer's exact-span object,
    /// and a refused young copy is `evacuate`'s `process::abort()`. The CAS
    /// loop costs a retry under contention instead of an unconditional RMW,
    /// and it only runs per BUFFER on a buffered cycle; per object only on a
    /// bufferless one.
    ///
    /// `checked_add` is kept for the reason gengc-round1 added it: the cursor
    /// holds a raw address, and a wrapped `cur + size` would compare below
    /// `to_region_end`.
    fn bump_shared(&self, size: usize) -> Option<usize> {
        let mut cur = self.to_cursor.load(Ordering::Relaxed);
        loop {
            let end = cur
                .checked_add(size)
                .filter(|&end| end <= self.to_region_end)?;
            match self.to_cursor.compare_exchange_weak(
                cur,
                end,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(cur),
                Err(now) => cur = now,
            }
        }
    }

    /// Hand this worker's unused young PLAB tail to the driver's filler list.
    ///
    /// Split out of [`Self::retire_plab`] (gengc-round1 2026-09-20) so the
    /// young-allocation refill path can rotate its to-space buffer without
    /// taking the old generation's mutex to dump a promotion buffer it is
    /// still using. Associated rather than a method because the refill path
    /// holds `&mut shard` and needs no `&self`.
    fn retire_young_plab(shard: &mut EvacShard) {
        let (cur, end) = (shard.plab.cursor, shard.plab.end);
        shard.plab.cursor = 0;
        shard.plab.end = 0;
        if end > cur && cur != 0 {
            shard.plab_gaps.push((cur, end - cur));
        }
    }

    /// Hand this worker's unused PLAB tail to the driver's filler list, and
    /// its unused promotion-buffer tail back to the old generation.
    ///
    /// The driver's end-of-drain retirement: both buffers, once per worker.
    pub(crate) fn retire_plab(&self, shard: &mut EvacShard) {
        Self::retire_young_plab(shard);
        self.retire_old_plab(shard);
    }

    /// Return the never-written tail of the worker's promotion buffer to the
    /// old generation's free list.
    ///
    /// `OldGen::release_unused_tail` deliberately does NOT stamp the reclaim
    /// epoch: the tail never held an object, so no concurrent-mark remark
    /// snapshot can name an address in it, and a young cycle retiring its
    /// buffers must not invalidate an in-flight old-gen sweep. The tail is 0
    /// or at least `HEADER_SIZE` by [`Self::old_lab_alloc`]'s rule, so it is
    /// always a legal free block.
    fn retire_old_plab(&self, shard: &mut EvacShard) {
        let (cur, end) = (shard.old_plab.cursor, shard.old_plab.end);
        shard.old_plab.cursor = 0;
        shard.old_plab.end = 0;
        if end > cur && cur != 0 {
            let tail = end - cur;
            debug_assert!(
                tail >= HEADER_SIZE && tail % 8 == 0,
                "a promotion buffer tail must be a free-list-sized block (tail={tail})"
            );
            // SAFETY: `[cur, end)` is the unused remainder of a block this
            // worker carved with `alloc_unzeroed`; nothing was written there.
            unsafe {
                self.old_gen
                    .lock()
                    .release_unused_tail(cur as *mut u8, tail)
            };
        }
    }

    /// Bump `size` bytes out of the worker's promotion buffer, refusing an
    /// allocation that would leave exactly 8 bytes: an 8-byte remainder can
    /// neither go back to the old-gen free list (its minimum block is
    /// `HEADER_SIZE`) nor carry an `int[]` filler, so the buffer is retired
    /// with a tail of either nothing or at least `HEADER_SIZE` bytes instead.
    /// (The comment used to say "at least 24 bytes", which was the answer for
    /// the pre-2026-08-06 40-byte header; corrected 2026-09-20. The number is
    /// load-bearing: it is exactly what makes `retire_old_plab`'s
    /// `tail >= HEADER_SIZE` assertion and `release_unused_tail`'s
    /// legal-free-block contract hold.)
    fn old_lab_alloc(shard: &mut EvacShard, size: usize) -> Option<usize> {
        if shard.old_plab.cursor == 0 {
            return None;
        }
        let after = shard.old_plab.cursor.checked_add(size)?;
        if after > shard.old_plab.end || shard.old_plab.end - after == 8 {
            return None;
        }
        let p = shard.old_plab.cursor;
        shard.old_plab.cursor = after;
        Some(p)
    }

    /// Old-gen destination for a promoted object of `size` bytes (8-aligned):
    /// the worker's promotion buffer, a fresh buffer carved under the old-gen
    /// lock, or — when the old generation cannot spare a buffer — the object's
    /// own block. `None` means old gen is full and the caller falls back to
    /// to-space, exactly as the serial path does.
    fn promote_alloc(&self, shard: &mut EvacShard, size: usize) -> Option<usize> {
        if size >= OLD_PLAB_DIRECT_MIN {
            return self
                .old_gen
                .lock()
                .alloc_unzeroed(size, 8)
                .map(|p| p as usize);
        }
        if let Some(p) = Self::old_lab_alloc(shard, size) {
            return Some(p);
        }
        self.retire_old_plab(shard);
        let want = if shard.next_old_plab == 0 {
            OLD_PLAB_MIN
        } else {
            shard.next_old_plab
        };
        let mut plab = want.max(size);
        if plab - size == 8 {
            // The first allocation must not leave the 8-byte tail
            // `old_lab_alloc` refuses, or a fresh buffer would be handed back
            // at once.
            plab += 8;
        }
        let mut og = self.old_gen.lock();
        // gengc-round2: the BUFFER form, because this block's tail goes back
        // through `release_unused_tail`, which skips the reclaim-epoch stamp.
        // `alloc_unzeroed_buffer` is what records that intent so a debug build
        // can check the premise instead of trusting a comment; the two
        // arms below deliberately stay on plain `alloc_unzeroed`, because an
        // object on its own block never releases a tail. See
        // `OldGen::unzeroed_buffer_carves`.
        match og.alloc_unzeroed_buffer(plab, 8) {
            Some(base) => {
                drop(og);
                let base = base as usize;
                shard.next_old_plab = (want * 2).min(OLD_PLAB_MAX);
                shard.old_plab.cursor = base;
                shard.old_plab.end = base + plab;
                Self::old_lab_alloc(shard, size)
            }
            // No buffer-sized block: the object on its own, or nothing.
            None => og.alloc_unzeroed(size, 8).map(|p| p as usize),
        }
    }

    // -----------------------------------------------------------------------
    // Evacuation
    // -----------------------------------------------------------------------

    /// Forward (copy or promote) one from-space object.
    ///
    /// Returns the address every reference to `old_ptr` must now use. That is
    /// `old_ptr` itself when a guard refuses the object — identical to
    /// `forward_object_impl`'s refusal contract, and for the same reason: the
    /// refusals fire on suspected FALSE roots, where writing a forwarding
    /// pointer into the middle of a live object is the damage being avoided.
    ///
    /// # Safety
    /// `old_ptr` must be an aligned, readable address inside the young
    /// from-space span this context was built over.
    pub(crate) unsafe fn evacuate(&self, shard: &mut EvacShard, old_ptr: *mut u8) -> *mut u8 {
        let old_addr = old_ptr as usize;
        // Exact pre-GC membership: rejects an aligned INTERIOR word arriving
        // from a conservative root before anything writes through it.
        if !self.starts.contains(old_addr) {
            // Recorded for the same addresses the serial evacuator records
            // (`forward_object_impl`'s `young_from.contains` screen), so the
            // per-cycle refusal ledger does not depend on which arm copied.
            // gen r5w3/evac7: on a pinned in-place cycle a slot re-read after
            // its rewrite names a destination byte this cycle wrote; that is
            // the answer already, not a refusal (the serial arm's
            // `dest_written` test, in the same place).
            if self.in_from_extent(old_addr) && !self.in_place_dest_written(old_addr) {
                Self::note_refusal(shard, old_addr, "not-an-object-start");
            }
            return old_ptr;
        }

        // SAFETY: `old_addr` is a recorded object start inside from-space, so
        // a whole header is mapped there, and every field of `ObjectHeader`
        // (two `u32`s and an `AtomicU64`) is valid for any bit pattern.
        let hdr = unsafe { &*(old_ptr as *const ObjectHeader) };
        // ONE snapshot of the mark word drives every decision below: the
        // quartet (kind / element_type / gc_age / gc_flags) lives in it, and
        // re-reading it could pick up a racing worker's FORWARDED value
        // mid-decision.
        let observed = hdr.mark_word.load(Ordering::Acquire);
        // Validate the tags BEFORE any typed decode, from the SNAPSHOT.
        //
        // gengc-round4-move (2026-09-23): these used to be
        // `cratonvm_types::kind_tag_at` / `element_type_tag_at`, which read the
        // mark word with a plain `read_unaligned`. On this path that is a
        // non-atomic read of a location a peer worker may be CAS-ing at the
        // same moment (installing its forward) — a data race, i.e. UB under
        // the Rust memory model however benign it is on x86. The serial
        // evacuator can use the raw readers because nothing else runs. The
        // tags are part of the quartet `make_forwarded` preserves, so decoding
        // them from the atomic snapshot gives the same answer.
        let kind_tag = ObjectHeader::kind_tag(observed);
        let elem_tag = ObjectHeader::element_type_tag(observed);
        // The element-type bits are a tag only on an array (a compact instance
        // keeps identity-hash bits there).
        if object_kind_from_tag(kind_tag).is_none()
            || (kind_tag == ObjectKind::Array as u8
                && array_element_type_from_tag(elem_tag).is_none())
        {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_tag,
                elem_tag,
                "gen_evac::evacuate: invalid kind/element_type tag — false root or corrupt header",
            );
            Self::note_refusal(shard, old_addr, "invalid-kind-or-element-tag");
            return old_ptr;
        }

        if ObjectHeader::is_forwarded_mark(observed) {
            // The target is in the source's second word; a peer's claim that
            // has not published yet is waited out, UNBOUNDED here (gen
            // r5w1/young5: see `wait_for_claimed_forwarding`).
            let fwd = wait_for_claimed_forwarding(hdr);
            // gen r5w3/evac7: on a pinned in-place cycle a young destination
            // is inside from-space (a span, or the object itself for a pinned
            // or kept claim), and every such forward was installed by THIS
            // phase — a claimer or a CAS winner, which recorded its pair.
            let in_place_target = self.in_place.is_some() && self.in_from_extent(fwd);
            // Same sanity ladder as the serial path: a forwarding target must
            // be non-null, 8-aligned and inside one of this cycle's two
            // destinations. BUG-Z showed 8-aligned non-null GARBAGE reaching
            // here on a stale slot.
            if fwd == 0
                || fwd % 8 != 0
                || !((fwd >= self.to_base && fwd < self.to_end_cap)
                    || self.in_old(fwd)
                    || in_place_target)
            {
                tracing::debug!(
                    target: "cratonvm::gc::guard",
                    fwd,
                    old_ptr = ?old_ptr,
                    "gen_evac::evacuate: bad forwarding target",
                );
                // gcd d2/h: into the refusal ledger, as the serial arm now
                // records the same exit.
                Self::note_refusal(shard, old_addr, "bad-forwarding-target");
                return old_ptr;
            }
            // MUST record even though this call copied nothing — see the
            // module note (2) — UNLESS the target is in young TO-space.
            //
            // gen r4w4/young4 (2026-09-24): the serial evacuator's round-4
            // rule, applied here. To-space starts every moving cycle empty and
            // nothing but this cycle's evacuation writes a forward into it: the
            // driver seeds through `evacuate` too, so every to-space forward
            // was installed by a CAS WINNER of this phase, which pushed the
            // pair on its own shard (the `Ok` arm below), and the driver merges
            // every shard. A second copy of the pair here was one `Vec` push
            // per RE-ENCOUNTER — an edge into from-space, not an object — and
            // then one redundant hash insert and one redundant anchor probe per
            // push in the driver's serial merge loop. An OLD-gen target keeps
            // the push: it may be a selective-promotion forward from an earlier
            // non-moving cycle, for which this is the only record.
            if !(fwd >= self.to_base && fwd < self.to_end_cap) && !in_place_target {
                shard.forwards.push((old_addr, fwd));
            }
            shard.tally[4] += 1;
            return fwd as *mut u8;
        }

        // Reconstruct an owned header from the snapshot so nothing holds a
        // `&ObjectHeader` across the forwarding install.
        let Some(owned) = (unsafe { self.owned_header(old_ptr, observed) }) else {
            // A peer claimed the object between the snapshot and the shape
            // read, so the shape word may already hold its forwarding target.
            // Start over: the next pass takes the forwarded fast path.
            return unsafe { self.evacuate(shard, old_ptr) };
        };
        let is_array = owned.kind() == ObjectKind::Array;
        let kind_byte = ObjectHeader::kind_tag(observed);
        if kind_byte > 1
            || (is_array && owned.array_length() > i32::MAX as u32)
            || (!is_array && owned.num_slots() > (1 << 24))
        {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_byte,
                num_slots = owned.num_slots(),
                array_length = owned.array_length(),
                "gen_evac::evacuate: suspect header",
            );
            Self::note_refusal(shard, old_addr, "suspect-header");
            return old_ptr;
        }

        // gen r5w3/evac7 (L1): a PINNED object on a pinned in-place cycle is
        // claimed where it is (then scanned in place), before any size or
        // class screen — the serial path tests its pinned set at the same
        // point, right after the header screens. Every object of the plan is
        // a proved object start, so the screens below could only refuse it.
        if let Some(ip) = self.in_place {
            if ip.is_pinned_base(old_addr) {
                // SAFETY: `old_addr` is a proved object start whose mark word
                // was `observed`, not forwarded (the fast path above).
                return unsafe { self.claim_in_place(shard, hdr, old_ptr, observed, None) };
            }
        }

        let total_size = crate::gen_heap::gen_object_total_size(&owned);
        // A genuine object fits inside the arena it was allocated from, and
        // every footprint this collector produces is a multiple of 8. Either
        // failing means the bytes are not an object header — a false
        // conservative root — and the serial path refuses those too.
        let fits = old_addr >= self.from_base
            && old_addr
                .checked_add(total_size)
                .is_some_and(|end| end <= self.from_end_cap);
        if total_size < HEADER_SIZE || total_size % 8 != 0 || !fits {
            tracing::warn!(
                "GC: parallel evacuation skipping suspected false root at {:p} \
                 (computed size {total_size})",
                old_ptr,
            );
            // gcd d2/h: into the refusal ledger, as the serial arm now
            // records the same exit.
            Self::note_refusal(shard, old_addr, "extent-outside-from-space");
            return old_ptr;
        }
        if self.fwd_resolve_strict
            && crate::gc::resolve_class_info(owned.class_id.as_u32()).is_none()
        {
            return old_ptr;
        }

        // `crate::gen_heap::should_tenure` is the single implementation of the
        // tenuring rule, shared with the serial `forward_object_impl` and the
        // non-moving sweep's selective-promotion pass, so this evacuator and
        // that one cannot drift again. (They had: this site always saturated,
        // the other two read a bare `+ 1` on a `u8` that reaches 255.)
        let promote = crate::gen_heap::should_tenure(
            owned.gc_age(),
            self.force_promote_all,
            self.promotion_age,
        );
        // gen r5w3/evac7 (L1): a pinned in-place cycle. A PINNED object stays
        // where it is (claimed, then scanned in place). Any other survivor goes
        // where its age says — old gen, or a from-space span that was free at
        // the start of the cycle — then to the other one, and when neither can
        // take it, it STAYS (the serial `in_place_destination` order exactly).
        // (The pinned claim itself is above, where the serial path tests it.)
        let mut in_place_overflow = false;
        let mut new_addr = if let Some(ip) = self.in_place {
            let dest = if promote {
                self.promote_alloc(shard, total_size)
                    .or_else(|| Self::in_place_alloc(ip, shard, total_size))
            } else {
                match Self::in_place_alloc(ip, shard, total_size) {
                    Some(p) => Some(p),
                    None => {
                        let p = self.promote_alloc(shard, total_size);
                        in_place_overflow = p.is_some();
                        p
                    }
                }
            };
            match dest {
                Some(p) => p,
                None => {
                    // Neither can take it: keep it where it is. Sound here
                    // because from-space is not reset on this cycle; the
                    // rebuild keeps every `live` object.
                    // SAFETY: as for the pinned claim above.
                    return unsafe {
                        self.claim_in_place(shard, hdr, old_ptr, observed, Some(total_size))
                    };
                }
            }
        } else if promote {
            match self.promote_alloc(shard, total_size) {
                Some(p) => p,
                // Old gen full: fall back to to-space, exactly as the serial
                // path does. The Cheney invariant guarantees room.
                None => self.plab_alloc(shard, total_size).unwrap_or(0),
            }
        } else {
            match self.plab_alloc(shard, total_size) {
                Some(p) => p,
                // The reserved to-space region is exhausted. Reachable only
                // through forwarding-CAS losers' abandoned speculative copies,
                // which spend region bytes `plan`'s budget does not count —
                // see `PAR_EVAC_OVERFLOW_PROMOTIONS`. Tenure the object early
                // rather than abort: it is the mirror image of the promotion
                // arm's own fallback just above, and an early promotion costs
                // a little old-gen space where the alternative costs the
                // process. (gengc-round4-move, 2026-09-23.)
                None => match self.promote_alloc(shard, total_size) {
                    Some(p) => {
                        PAR_EVAC_OVERFLOW_PROMOTIONS.fetch_add(1, Ordering::Relaxed);
                        p
                    }
                    None => 0,
                },
            }
        };
        if new_addr == 0 {
            // Both destinations refused. The serial path aborts the process
            // here and explains why: a copying collector has no valid address
            // to hand back for an unrelocated object, and returning `old_ptr`
            // would dangle every live reference once from-space is reset.
            // Reaching this needs the reserved to-space region AND old gen to
            // be exhausted at once.
            eprintln!(
                "FATAL: parallel GC could not relocate a live object (tried {total_size} bytes; \
                 to-space cursor {:#x} of region ending {:#x}, and old gen could not take it \
                 either).",
                self.to_cursor.load(Ordering::Relaxed),
                self.to_region_end,
            );
            std::process::abort();
        }
        let new_ptr = new_addr as *mut u8;

        // SAFETY: source and destination are `total_size` disjoint bytes — the
        // destination came from a cursor no other worker can hand out twice.
        unsafe { std::ptr::copy_nonoverlapping(old_ptr, new_ptr, total_size) };
        // The bulk memcpy is UB for the `AtomicU64` mark word; replicate it
        // atomically from the SNAPSHOT (never a fresh load, which could pick
        // up a racing worker's FORWARDED word and stamp the destination as
        // forwarded).
        //
        // gen r5w6/pin10: with the old-gen flag or the incremented age folded
        // in first (`ObjectHeader::mark_with_gc_flags` / `mark_with_gc_age`),
        // so the destination takes ONE plain store instead of a store plus a
        // locked `fetch_or` / compare-exchange loop. No other worker can see
        // the destination before the claim below publishes it, and the pure
        // helpers compute exactly the word the RMWs left (the serial
        // `forward_object_impl` does the same since this change).
        let landed_in_old = self.in_old(new_addr);
        let dest_mark = if landed_in_old {
            ObjectHeader::mark_with_gc_flags(observed, GC_FLAG_OLD_GEN)
        } else {
            ObjectHeader::mark_with_gc_age(
                observed,
                ObjectHeader::gc_age_of(observed).saturating_add(1),
            )
        };
        // SAFETY: `new_ptr` is a freshly allocated object this worker owns,
        // fully written by the copy above.
        unsafe {
            (*(new_ptr as *mut ObjectHeader))
                .mark_word
                .store(dest_mark, Ordering::Relaxed);
        }

        // Claim the object. The copy had to happen first: writing FORWARDED
        // destroys the source's lock state, so the destination must already
        // carry the intact word.
        run_race_hook(old_ptr);
        // Claim (one CAS to FORWARDED|BUSY), then publish the target into the
        // source's second word. See `ObjectHeader::try_claim_forwarding`.
        match hdr.try_claim_forwarding(observed) {
            Ok(()) => {
                hdr.publish_claimed_forwarding(new_ptr);
                shard.forwards.push((old_addr, new_addr));
                shard.objects_copied += 1;
                let i = if landed_in_old { 0 } else { 2 };
                shard.tally[i] += total_size as u64;
                shard.tally[i + 1] += 1;
                shard.tally[5] += 1;
                shard.work.push(new_addr);
                // gen r5w3/evac7 (L1): the in-place census and the rebuild's
                // live set (a promotion needs neither: old gen is not rebuilt).
                if self.in_place.is_some() {
                    if landed_in_old {
                        if in_place_overflow {
                            shard.in_place_overflow += 1;
                        }
                    } else {
                        shard.in_place_live.push((new_addr, total_size));
                        shard.in_place_copies += 1;
                    }
                }
                new_ptr
            }
            Err(winner) if ObjectHeader::is_forwarded_mark(winner) => {
                // Lost the claim. Abandon the speculative copy — to-space
                // garbage reclaimed next cycle, or an unreferenced old-gen
                // object reclaimed by the next major — and adopt the winner's
                // address so every reference converges. The forward is
                // recorded HERE too; the equivalent G1 arm not doing so was a
                // live root-remap hole.
                EVAC_CAS_LOSSES.fetch_add(1, Ordering::Relaxed);
                // Tell the moving-young verifier that the complete object we
                // just wrote at `new_addr`'s PREDECESSOR -- our own speculative
                // copy -- is being abandoned unscanned, so it does not read the
                // pre-move addresses still in its slots as missed heap
                // rewrites. Recorded before `new_addr` is overwritten with the
                // winner's address; no-op unless the verifier is armed.
                crate::gen_heap::record_abandoned_evac_copy(new_addr);
                if landed_in_old {
                    // gengc-round1 2026-09-20. An abandoned copy in YOUNG
                    // to-space is inert: nothing points at it, and the only
                    // walk that reaches it next cycle strides it as garbage
                    // and sweeps it. An abandoned copy in OLD gen is not, and
                    // the difference is that the dirty-card scan is liveness
                    // BLIND — `walk_objects_in_card_ranges` yields every
                    // object whose header starts in a dirty card, and
                    // `scan_dirty_cards` then reads all of its reference
                    // slots as old→young roots. This object's slots still
                    // hold THIS cycle's from-space addresses. They are
                    // harmless next cycle (from-space and to-space have
                    // swapped, so nothing matches `young_from.contains`) and
                    // become live-looking again the cycle after that, when
                    // the semispaces swap back and those addresses name
                    // whatever the mutator has since allocated there. The
                    // result is arbitrary retention driven by a dead object's
                    // stale bytes — not immediate corruption, but a leak the
                    // collector cannot reason its way out of.
                    //
                    // Stamp a reference-free `int[]` of exactly the same
                    // footprint: the old-gen walk keeps striding correctly
                    // (`scan_region` derives the same `total_size` from the
                    // filler header, so `walk_objects`'s byte accounting still
                    // balances against `used_bytes` and `compact`'s
                    // GCAUD-9 check stays quiet), the slots are gone, and the
                    // block is reclaimed by the next major GC exactly as the
                    // unfilled abandonment would have been.
                    //
                    // SAFETY: `new_addr` is an 8-aligned, `total_size`-byte
                    // block this worker owns — carved from the old generation
                    // moments ago and, having lost the claim, referenced by
                    // nothing.
                    unsafe { write_filler(new_addr, total_size) };
                    PAR_EVAC_ABANDONED_OLD_BYTES.fetch_add(total_size as u64, Ordering::Relaxed);
                } else {
                    // Since the 8-byte header, a young abandoned copy is no
                    // longer guaranteed faithful either: the winner overwrites
                    // the source's second word (a long header's LENGTH) with
                    // its forwarding target once it has claimed, and this copy
                    // may have read it afterwards. A linear young walk sizes
                    // the copy from that word, so stamp the same reference-free
                    // filler here too: the footprint is right by construction.
                    //
                    // SAFETY: as above -- this worker's own `total_size`-byte
                    // block, referenced by nothing.
                    unsafe { abandon_young_copy(self.in_place.is_some(), new_addr, total_size) };
                }
                // gen r5w1/young5: the winner published (or is about to
                // publish) its target; wait it out without a bound. The
                // bounded `forwarding_address` could time out on a descheduled
                // winner and hand back NULL, which this arm used to record as
                // the forward `(old, 0)` and return as the new reference.
                new_addr = wait_for_claimed_forwarding(hdr);
                if new_addr == 0 {
                    // Unreachable: the CAS just lost to a FORWARDED word, and
                    // nothing un-forwards a source during a cycle. Refuse
                    // rather than record `(old, 0)` or hand back null.
                    Self::note_refusal(shard, old_addr, "suspect-header");
                    return old_ptr;
                }
                // Recorded even for a to-space winner, whose own shard also
                // carries the pair (`a_forwarding_cas_loser_adopts_the_winner_
                // and_records_the_forward` pins this arm's record; eliding the
                // duplicate, as the fast path does, is filed with the gen
                // r5w1/young5 review rather than changed blind).
                shard.forwards.push((old_addr, new_addr));
                shard.tally[4] += 1;
                new_addr as *mut u8
            }
            Err(_) => {
                // A non-FORWARDED loser value means an unmodelled writer.
                // Adopting its payload as an address is precisely the
                // INFLATED/FORWARDED aliasing this encoding was audited
                // against, so refuse instead.
                tracing::warn!(
                    "gen_evac::evacuate: forwarding CAS lost to a non-forwarded word at {:p}",
                    old_ptr,
                );
                // gen r5w1/young5: the speculative copy is abandoned here too,
                // so it gets what the forwarded-loser arm gives its copy: the
                // verifier is told, and a reference-free filler of the same
                // footprint replaces it. In OLD gen that is not cosmetic: the
                // dirty-card scan is liveness-blind and would read the copy's
                // stale from-space slots as old->young roots (see the arm
                // above).
                crate::gen_heap::record_abandoned_evac_copy(new_addr);
                if landed_in_old {
                    // SAFETY: `new_addr` is this worker's own 8-aligned
                    // `total_size`-byte block, carved moments ago and
                    // referenced by nothing (the claim failed).
                    unsafe { write_filler(new_addr, total_size) };
                    PAR_EVAC_ABANDONED_OLD_BYTES.fetch_add(total_size as u64, Ordering::Relaxed);
                } else {
                    // SAFETY: as above.
                    unsafe { abandon_young_copy(self.in_place.is_some(), new_addr, total_size) };
                }
                old_ptr
            }
        }
    }

    /// Rebuild an owned `ObjectHeader` from `old_ptr`'s plain fields plus a
    /// mark-word snapshot.
    ///
    /// `ptr::read`ing the whole struct would be a NON-atomic read of an atomic
    /// location, which is UB; the mark word therefore arrives as a value the
    /// caller already loaded atomically.
    ///
    /// # Safety
    /// `old_ptr` must point at a readable `ObjectHeader`, and `observed`'s
    /// kind/element-type tags must already have been validated.
    unsafe fn owned_header(&self, old_ptr: *mut u8, observed: u32) -> Option<ObjectHeader> {
        let h = old_ptr as *const ObjectHeader;
        // The quartet (kind / element_type / gc_age / gc_flags) rides in the
        // mark word, so storing the snapshot carries all four; the second
        // word (a long header's shape and hash) is copied raw.
        let owned = unsafe { ObjectHeader::snapshot(h) };
        owned.mark_word.store(observed, Ordering::Relaxed);
        // A peer claims before it overwrites the second word, so a copy taken
        // while the mark word still equals the snapshot is the object's own.
        std::sync::atomic::fence(Ordering::Acquire);
        if unsafe { (*h).mark_word.load(Ordering::Relaxed) } != observed {
            return None;
        }
        Some(owned)
    }

    // -----------------------------------------------------------------------
    // Scanning
    // -----------------------------------------------------------------------

    /// Scan one already-evacuated object (in young to-space or in old gen),
    /// forwarding every from-space reference it holds and rewriting the slot.
    ///
    /// # Safety
    /// `obj_addr` must be a live destination this cycle produced.
    unsafe fn scan_object(&self, shard: &mut EvacShard, obj_addr: usize) {
        shard.objects_scanned += 1;
        let obj_ptr = obj_addr as *mut u8;
        // SAFETY: a destination written by `evacuate`, so its header is a
        // faithful copy of a validated source header.
        let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
        let class_id = header.class_id.as_u32();
        // An old-gen holder whose referent stays young is an old→young edge
        // the next minor GC must see. The card is re-marked AFTER Phase 3's
        // `clear_all()`, so it is deferred rather than marked here.
        let holder_is_old = self.in_old(obj_addr);
        // gcd d2/h (cards3 item 3, producer (c)): once per holder, as before,
        // except a reference array on an element-precise table, which defers
        // each young-staying element's card (`HolderCardDefer`).
        let mut card_defer =
            crate::gen_heap::HolderCardDefer::new(obj_addr, header, self.element_card_base);

        // SAFETY: `obj_ptr`/`header` describe one valid copied object.
        unsafe {
            crate::gen_heap::forward_ref_slots_at(
                obj_ptr,
                header,
                |off, ref_ptr| {
                    let a = ref_ptr as usize;
                    if !self.in_from(a) {
                        // The serial scan screens with `young_from.contains`, a
                        // CAPACITY test, so a slot naming from-space past the
                        // allocation frontier reaches its forwarder and is
                        // refused and recorded there. Record it here too, so
                        // the ledger does not depend on which arm copied; the
                        // slot is left untouched, which is what the serial
                        // arm's refusal amounts to.
                        //
                        // gen r5w3/evac7: except, on a pinned in-place cycle,
                        // an address this cycle already wrote a copy to (a
                        // slot re-read after its rewrite): it is the answer,
                        // not a refusal. See `ParInPlace::dest_written`.
                        if self.in_from_extent(a) && !self.in_place_dest_written(a) {
                            Self::note_refusal(shard, a, "not-an-object-start");
                        }
                        return None;
                    }
                    // SAFETY: inside the enclosing `unsafe` block — `ref_ptr`
                    // was just screened as an address inside young from-space.
                    let new_ptr = self.evacuate(shard, ref_ptr);
                    if holder_is_old && !self.in_old(new_ptr as usize) {
                        card_defer.slot_stays_young(off, &mut shard.deferred_dirty_cards);
                    }
                    Some(new_ptr)
                },
            );
        }

        // A live object keeps its class's defining ClassLoader alive
        // (HIB-CV-24, the instance→loader edge). A young loader is evacuated
        // like any other survivor.
        //
        // gc-common w36-d: the row inside THIS from-space when VMs share the
        // class id (`common-w4b-loader-pin-collision-unroots-another-vms-statics`);
        // the site acts only on a loader in its own from-space, so that row is
        // the one it needs, and a single row is returned without the test.
        if self.loader_pin_on {
            if let Some(loader_old) = cratonvm_types::loader_pin::loader_pin_addr_where(class_id, |a| {
                self.in_from_extent(a)
            }) {
                if self.in_from_extent(loader_old) {
                    // SAFETY: `loader_old` is inside from-space, so it is a
                    // readable, aligned candidate object start.
                    let new_lp = unsafe { self.evacuate(shard, loader_old as *mut u8) };
                    if holder_is_old && !self.in_old(new_lp as usize) {
                        card_defer.whole_object();
                    }
                }
            }
        }

        card_defer.finish(&mut shard.deferred_dirty_cards);
    }

    // -----------------------------------------------------------------------
    // Transitive closure
    // -----------------------------------------------------------------------

    /// Drain every worker's seed work to a fixpoint.
    ///
    /// With `shards.len() == 1` this is a plain loop on the calling thread and
    /// spawns nothing, which is what makes the single-worker path a faithful
    /// (and testable) stand-in for the parallel one.
    ///
    /// # Safety
    /// Every address on a shard's worklist must be a destination this cycle
    /// produced.
    /// # Why a persistent pool and not `thread::scope`
    ///
    /// This spawned its helpers per pause to begin with, and the cost is not
    /// theoretical. MEASURED on a 6144-node DAG, eight workers, twelve
    /// consecutive collections: the helpers scanned **zero** destinations. Not
    /// because the sharing rule failed — because the driver drained the whole
    /// closure in about a millisecond while seven freshly-spawned OS threads
    /// were still on their way to their first lock acquisition, and by the time
    /// they arrived there was nothing left but the termination handshake.
    ///
    /// [`crate::evac_pool::EvacPool`] exists for exactly this, and its module
    /// note makes the same argument for G1: the threads are identical from one
    /// pause to the next, so creating them inside the pause buys nothing and is
    /// charged against a pause budget. Parked on a condvar they wake in
    /// microseconds instead.
    ///
    /// # Safety
    /// Every address on a shard's worklist must be a destination this cycle
    /// produced.
    pub(crate) unsafe fn drain(&self, pool: &crate::evac_pool::EvacPool, shards: &mut [EvacShard]) {
        // SAFETY: forwarded from this function's contract.
        unsafe { self.drain_seeded(pool, shards, None) }
    }

    /// [`Self::drain`], with a per-worker SEED run on every worker before it
    /// joins the closure (gen r5w3/evac7, proposal W2 of
    /// `docs/known-issues/gc/gengc-r4w4-young4-parallel-evacuator-scaling-limits-20260924.md`,
    /// `CRATONVM_GC_PAR_EVAC_CARD_SEED`).
    ///
    /// `seed(evac, shard, worker, workers)` runs on worker `worker` of
    /// `workers` (0 is the driver) with that worker's own shard, and may
    /// forward through [`Self::evacuate`] into it; whatever it queues is
    /// scanned by that worker first and shared as usual. The driver uses it to
    /// split the dirty-card roots across the workers instead of forwarding
    /// them all on one thread before the pool opens — on a workload whose
    /// survivors hang off a wide old array (the ring of
    /// `GenR4W4EvacThroughputProbe`), that serial seed was half the copying.
    ///
    /// The seed runs INSIDE [`Self::run_worker`], after its [`WorkerExit`]
    /// guard, so a panicking seed is counted dead like any other worker and
    /// cannot strand its peers. A worker still seeding is not idle, so the
    /// termination test cannot fire before every seed has run.
    ///
    /// # Safety
    /// As for [`Self::drain`]; and `seed` must only forward addresses
    /// [`Self::evacuate`]'s contract allows.
    pub(crate) unsafe fn drain_seeded(
        &self,
        pool: &crate::evac_pool::EvacPool,
        shards: &mut [EvacShard],
        seed: Option<WorkerSeed<'_, 'a>>,
    ) {
        let threads = shards.len();
        if threads <= 1 {
            if let Some(shard) = shards.first_mut() {
                if let Some(seed) = seed {
                    seed(self, &mut *shard, 0, 1);
                }
                while let Some(addr) = shard.work.pop() {
                    unsafe { self.scan_object(shard, addr) };
                }
            }
            return;
        }
        // Publish every seed the driver put on shard 0 (or anywhere) so the
        // helpers have something to take on their first acquisition.
        {
            let mut g = self.shared.lock();
            for shard in shards.iter_mut() {
                g.stack.append(&mut shard.work);
            }
        }
        let helpers = (threads - 1).min(pool.helpers());
        if helpers == 0 {
            // The pool is smaller than the worker policy asked for (or empty).
            // The termination handshake counts heads, so it has to be told the
            // truth about how many workers exist, or the drain waits forever
            // for peers that will never check in.
            // SAFETY: forwarded from this function's contract.
            unsafe { self.run_worker(&mut shards[0], 1, seed.map(|s| (s, 0))) };
            return;
        }
        let workers = helpers + 1;
        // Per-helper slots, one lock each and taken exactly once per pause. A
        // lock rather than a raw slot is what lets the dispatched body be a
        // plain `Fn`, which the pool's erased dispatch requires, with no unsafe
        // beyond the one the pool already documents.
        //
        // gen r5w1/young5: each slot starts as the CALLER's shard for that
        // helper, moved in, so whatever the driver pre-sized it with (the
        // per-worker `forwards` reservation from the previous cycle's survivor
        // count) is what the helper fills. The slots used to start from
        // `EvacShard::default()`, which threw every helper's reservation away
        // (freed inside the pause) and left only the driver's list pre-sized.
        let slots: Vec<Mutex<EvacShard>> = shards[1..=helpers]
            .iter_mut()
            .map(|s| Mutex::new(std::mem::take(s)))
            .collect();
        {
            let slots_ref = &slots;
            let body = move |i: usize| {
                let mut shard = std::mem::take(&mut *slots_ref[i].lock());
                // SAFETY: identical contract to the driver's own `run_worker`
                // call below. Helper `i` is worker `i + 1` (the driver is 0).
                unsafe { self.run_worker(&mut shard, workers, seed.map(|s| (s, i + 1))) };
                // A helper's buffer is retired by the driver after the barrier
                // along with its own; the tail is dead memory until then and
                // nothing walks to-space before it is filled.
                *slots_ref[i].lock() = shard;
            };
            pool.scope(helpers, &body, || {
                // SAFETY: as above.
                unsafe { self.run_worker(&mut shards[0], workers, seed.map(|s| (s, 0))) };
            });
        }
        for (slot, dst) in slots.into_iter().zip(shards[1..].iter_mut()) {
            *dst = slot.into_inner();
        }
    }

    /// One worker's acquire/scan/spill loop.
    ///
    /// Termination is the same protocol `young_mark::drain_parallel` uses: a
    /// worker that finds the global stack empty registers itself idle, and the
    /// worker that observes `idle == threads` declares the closure complete
    /// for everybody. A worker only ever goes idle with its own local stack
    /// empty, so "every worker idle and the global stack empty" really is the
    /// fixpoint.
    ///
    /// # Two sharing rules, and why the obvious one alone is not enough
    ///
    /// The high-water spill (`SPILL_HIGH`) is the cheap one: a worker whose
    /// local stack has run away publishes the surplus. On its own it does
    /// almost nothing, and this was MEASURED rather than reasoned about — a
    /// 6144-node DAG, 96 layers deep and 64 wide, collected on eight workers,
    /// and the driver copied **6144 objects while every helper copied 0**. A
    /// transitive closure over a graph like that keeps a frontier of about
    /// `width` addresses; it never comes within two orders of magnitude of a
    /// 2048-deep local stack, so the surplus rule never fires and the first
    /// worker to reach the seeds runs the entire closure alone. The parallel
    /// evacuator was, on that shape, an elaborate way to copy serially.
    ///
    /// So the load-bearing rule is the second one: publish HALF the local
    /// stack the moment any worker is idle. `idle_hint` is a relaxed atomic
    /// read of the same count the mutex protects — approximate on purpose,
    /// because the cost has to be a single load on the per-object path. Being
    /// wrong either way is harmless: a stale zero delays one hand-off, a stale
    /// non-zero publishes work that is then taken by whoever asks next.
    ///
    /// The acquire share is the same argument at the other end. Taking
    /// `ACQUIRE_CHUNK` unconditionally let the first waking worker swallow a
    /// seed set smaller than the chunk, so nothing was left for anybody else;
    /// taking `len / threads` leaves each of the others their share.
    ///
    /// # The seed (gen r5w3/evac7)
    ///
    /// `seed = Some((f, worker))` runs `f(self, shard, worker, threads)` first,
    /// after the exit guard (see [`Self::drain_seeded`]). Whatever it queued
    /// on the local stack is scanned before the first acquisition: the loop
    /// below scans local work FIRST and acquires second. Without a seed the
    /// local stack is empty on entry (`drain` moved every seed to the shared
    /// stack), so the first scan is a no-op and the order of operations is
    /// exactly the acquire-then-scan it always was.
    ///
    /// # Safety
    /// See [`Self::drain`].
    unsafe fn run_worker(
        &self,
        shard: &mut EvacShard,
        threads: usize,
        seed: Option<(WorkerSeed<'_, 'a>, usize)>,
    ) {
        // Declared FIRST so it drops LAST: an unwind out of this function
        // registers the death with the termination handshake. See
        // [`WorkerExit`].
        let _exit = WorkerExit {
            evac: self,
            threads,
        };
        if let Some((seed, worker)) = seed {
            seed(self, &mut *shard, worker, threads);
        }
        loop {
            while let Some(addr) = shard.work.pop() {
                unsafe { self.scan_object(shard, addr) };
                let local = shard.work.len();
                let publish = if local >= SPILL_HIGH {
                    local - SPILL_KEEP
                } else if local >= SHARE_MIN && self.idle_hint.load(Ordering::Relaxed) > 0 {
                    local / 2
                } else {
                    0
                };
                if publish > 0 {
                    let mut g = self.shared.lock();
                    g.stack.extend(shard.work.drain(..publish));
                    // Notified after the unlock (a parked waiter is requeued
                    // onto a held mutex, see `evac_pool`'s dispatch note).
                    // gce e2/o: the targeted-wake variant (W1,
                    // `CRATONVM_GC_PAR_EVAC_TARGETED_WAKE`) is removed -- no
                    // gain measured in two rounds.
                    drop(g);
                    self.cv.notify_all();
                }
            }
            // The local stack is empty: take a share of the shared stack, or
            // go idle.
            let mut g = self.shared.lock();
            loop {
                if g.done {
                    return;
                }
                let len = g.stack.len();
                if len > 0 {
                    // A fair share, not all of it: see the note above.
                    let take = len.div_ceil(threads).clamp(1, ACQUIRE_CHUNK).min(len);
                    shard.work.extend(g.stack.drain(len - take..));
                    break;
                }
                g.idle += 1;
                self.idle_hint.store(g.idle, Ordering::Relaxed);
                // `dead` is zero unless a peer unwound; counting it here is
                // what stops the survivors waiting for it forever.
                if g.idle + g.dead >= threads {
                    g.done = true;
                    self.cv.notify_all();
                    return;
                }
                self.cv.wait(&mut g);
                g.idle -= 1;
                self.idle_hint.store(g.idle, Ordering::Relaxed);
            }
        }
    }
}

/// A per-worker seed for [`ParEvac::drain_seeded`]:
/// `seed(evac, shard, worker, workers)` (gen r5w3/evac7).
pub(crate) type WorkerSeed<'s, 'a> =
    &'s (dyn Fn(&ParEvac<'a>, &mut EvacShard, usize, usize) + Sync);

// ---------------------------------------------------------------------------
// PLAB tail fillers
// ---------------------------------------------------------------------------

/// Dispose of a forwarding-CAS loser's abandoned YOUNG copy at
/// `[addr, addr + size)`.
///
/// On a to-space cycle: a reference-free filler ([`write_filler`]), so the next
/// cycle's linear walk strides it at the right size. On a parallel pinned
/// in-place cycle (`in_place`, gen r5w3/evac7): ZEROES instead. There the copy
/// sits in a from-space span that was free — hence zero — when the cycle
/// began, and the driver's rebuild frees every byte no survivor occupies
/// without wiping it (it wipes only what was allocated at the start), so a
/// filler left here would be handed to the allocator as a "zeroed" free block.
///
/// # Safety
/// The span must be 8-aligned, writable, owned by the caller and referenced by
/// nothing, as for [`write_filler`].
unsafe fn abandon_young_copy(in_place: bool, addr: usize, size: usize) {
    if in_place {
        // SAFETY: the caller's own `size`-byte block (the contract).
        unsafe { std::ptr::write_bytes(addr as *mut u8, 0, size) };
    } else {
        // SAFETY: as above.
        unsafe { write_filler(addr, size) };
    }
}

/// The forwarding target of a source a PEER has claimed, waited out without a
/// bound.
///
/// gen r5w1/young5 (2026-09-26). `ObjectHeader::forwarding_address` bounds its
/// wait on a `FORWARDED|BUSY` mark (`FORWARDING_BUSY_WAIT_LIMIT` CPU-relax
/// rounds) and then answers NULL, because header SCREENS call it on words that
/// only look forwarded. The evacuator is not such a caller: it reaches here
/// only for a proved object start (`ObjectStartBits::contains`), whose BUSY
/// mark is a genuine claim a worker of this same pause publishes two stores
/// later (`try_claim_forwarding` then `publish_claimed_forwarding`, nothing
/// between them that can fail). A winner descheduled in that window on an
/// oversubscribed host used to make the fast path return the from-space
/// address unrecorded (a dangling reference once from-space is reset) and the
/// CAS-loser arm record `(old, 0)` and hand back NULL. This keeps asking, and
/// yields between bounded waits so the winner can run.
///
/// Answers `0` only for a source that is not forwarded at all.
fn wait_for_claimed_forwarding(hdr: &ObjectHeader) -> usize {
    loop {
        let fwd = hdr.forwarding_address() as usize;
        if fwd != 0 || !hdr.is_forwarded() {
            return fwd;
        }
        std::thread::yield_now();
    }
}

/// Stamp a walkable filler over `[addr, addr + size)`.
///
/// `size` must be a non-zero multiple of 8 and the span must be dead. Uses the
/// TLAB retire path's two existing sentinels so every young walk in this crate
/// already knows how to stride the result — see the module note.
///
/// # Safety
/// The span must be 8-aligned, writable, and owned by the caller.
pub(crate) unsafe fn install_gap_filler(addr: usize, size: usize) {
    debug_assert!(
        addr % 8 == 0 && size % 8 == 0 && size > 0,
        "filler span must be an 8-aligned, non-zero multiple of 8 (addr={addr:#x} size={size})",
    );
    if size == 0 || size % 8 != 0 {
        return;
    }
    PAR_EVAC_FILLER_BYTES.fetch_add(size as u64, Ordering::Relaxed);
    // SAFETY: forwarded from this function's contract.
    unsafe { write_filler(addr, size) };
}

/// [`install_gap_filler`] without the [`PAR_EVAC_FILLER_BYTES`] tally.
///
/// gengc-round1 2026-09-20. Split out for the one other caller that needs a
/// walkable dead-object stamp but is NOT a to-space buffer tail: the
/// forwarding-CAS loser abandoning a copy it had already promoted (see
/// [`PAR_EVAC_ABANDONED_OLD_BYTES`]). Folding those bytes into `filler_bytes`
/// would make "bytes lost to PLAB tail fillers" mean two different things, and
/// the whole point of that counter is that it answers one question.
///
/// gen r4w5/pinned5 (2026-09-24): `pub(crate)` for the third such caller,
/// the pinned in-place young copy, which stamps the sub-`HEADER_SIZE` dead
/// slivers it cannot free-list (`GenerationalHeap::finish_in_place_young_cycle`)
/// and must not fold them into this module's PLAB filler census either.
///
/// # Safety
/// The span must be 8-aligned, a non-zero multiple of 8, writable, dead, and
/// owned by the caller.
pub(crate) unsafe fn write_filler(addr: usize, size: usize) {
    debug_assert!(
        addr % 8 == 0 && size % 8 == 0 && size > 0,
        "filler span must be an 8-aligned, non-zero multiple of 8 (addr={addr:#x} size={size})",
    );
    if size == 0 || size % 8 != 0 {
        return;
    }
    if size < HEADER_SIZE {
        // Too small for a header. The 8-byte sentinel: class id at +0, exact
        // gap length at +4. Zeroing instead would be byte-identical to a live
        // `new Object()` and desync every linear walk (Bug-D).
        // SAFETY: the span is at least 8 bytes and 8-aligned.
        unsafe {
            std::ptr::write(addr as *mut u32, crate::tlab::GAP_FILLER_CLASS_ID.as_u32());
            std::ptr::write((addr + 4) as *mut u32, size as u32);
        }
        return;
    }
    // A well-formed `int[]` that consumes the span exactly.
    let data_bytes = size - HEADER_SIZE;
    debug_assert_eq!(data_bytes % 8, 0, "filler payload must stay 8-aligned");
    let header = ObjectHeader::new(
        crate::tlab::TLAB_FILLER_CLASS_ID,
        ObjectKind::Array,
        ArrayElementType::Int,
        (data_bytes / 4) as u32,
        0,
    );
    // SAFETY: the span is at least `HEADER_SIZE` bytes and 8-aligned.
    unsafe {
        std::ptr::write(addr as *mut ObjectHeader, header);
        if data_bytes > 0 {
            std::ptr::write_bytes((addr + HEADER_SIZE) as *mut u8, 0, data_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gen r5w3/evac7 (L1): the span pool hands out buffers in span order,
    /// never past a span's end, retires a span that cannot serve a minimal
    /// request, and `dest_written` answers exactly "below its span's cursor".
    /// Pure arithmetic over made-up addresses: nothing is read or written.
    #[test]
    fn the_in_place_span_pool_claims_in_order_and_knows_what_it_wrote() {
        let pinned = [(0x5000usize, 64usize), (0x5100, 32)];
        let dest = [(0x1000usize, 0x1000usize, 0x1100usize), (0x2000, 0x2000, 0x3000)];
        let pool = ParInPlace::new(&pinned, &dest);
        assert!(pool.is_pinned_base(0x5000) && pool.is_pinned_base(0x5100));
        assert!(!pool.is_pinned_base(0x5008), "an interior word is not a base");
        assert!(!pool.is_pinned_base(0x4ff8) && !pool.is_pinned_base(0x5200));

        // Span 0 has 256 bytes: a 4 KiB request takes all of it.
        assert_eq!(pool.claim(64, 4096), Some((0x1000, 0x1100)));
        assert!(pool.dest_written(0x1000) && pool.dest_written(0x10f8));
        assert!(!pool.dest_written(0x1100), "one past the claim");
        assert!(!pool.dest_written(0x2000), "span 1 is untouched");
        assert!(!pool.dest_written(0x0ff8), "below every span");
        // Span 0 is empty now (and retired); span 1 serves the next claim.
        assert_eq!(pool.claim(64, 1024), Some((0x2000, 0x2400)));
        assert_eq!(pool.span_next.load(Ordering::Relaxed), 1, "span 0 retired");
        assert!(pool.dest_written(0x23f8) && !pool.dest_written(0x2400));
        // An exact claim, then one that no longer fits anywhere.
        assert_eq!(pool.claim(0xC00, 0xC00), Some((0x2400, 0x3000)));
        assert_eq!(pool.claim(8, 8), None, "every span is spent");
        assert_eq!(pool.cursors(), vec![0x1100, 0x3000]);
    }

    #[test]
    fn plan_runs_bufferless_rather_than_declining_when_slack_is_thin() {
        // Exactly enough room for the survivors and not a byte more. This is
        // very nearly the real case — a young GC triggers with from-space
        // ~99.9% full — and it must still parallelise, bufferless.
        let plan = ParEvac::plan(1024, 0x1000, 1024, 8).expect("zero slack still parallelises");
        assert_eq!(plan.workers, 8, "the worker ask is honoured");
        assert_eq!(
            plan.plab_bytes, 0,
            "no buffers: every object takes its own exact span"
        );
        assert_eq!(plan.waste_allowance, 0);

        // Slack too thin to be worth carving: still bufferless, not declined.
        let plan = ParEvac::plan(1024 + 8 * PLAB_FLOOR_BYTES, 0x1000, 1024, 8).expect("planned");
        assert_eq!(plan.plab_bytes, 0);

        // Ample slack: buffers, capped by what they are worth against the live
        // set rather than by the slack.
        let plan = ParEvac::plan(1024 + 64 * PLAB_MIN_BYTES, 0x1000, 1024, 8).expect("ample slack");
        assert!(plan.plab_bytes >= PLAB_FLOOR_BYTES);

        // The reservation, at every shape: buffers plus the abandoned-tail
        // allowance must fit inside the slack that exists. That sum IS what
        // keeps a worker from bumping past the arena.
        for (headroom, asked) in [
            (1024usize, 8usize),
            (1024 + 8 * PLAB_FLOOR_BYTES, 8),
            (1024 + 64 * PLAB_MIN_BYTES, 8),
            (1024 + 9 * PLAB_MIN_BYTES, 3),
        ] {
            let p = ParEvac::plan(headroom, 0x1000, 1024, asked).expect("planned");
            assert!(
                p.workers * p.plab_bytes + p.waste_allowance <= headroom - 1024,
                "reservation overruns the slack at plab={}",
                p.plab_bytes,
            );
        }

        // The one thing that still declines: to-space not covering from-space,
        // which the collection's own backstop should have caught first.
        assert!(ParEvac::plan(1023, 0x1000, 1024, 8).is_none());
    }

    /// The shape a real young collection actually has, as measured.
    ///
    /// `bench/BinT.java` at depth 18, `-Xmx512m`, first moving cycle:
    /// `to_headroom = 134,217,728` against `from_used = 134,096,736`. A young
    /// GC triggers with from-space **99.91% full**, so the Cheney invariant's
    /// `to_headroom >= from_used` leaves 120,992 bytes and nothing else.
    ///
    /// The first budget worst-cased abandoned tails at `from_used / 7` — 19 MB
    /// — and declined here, which means it would have declined on every cycle
    /// of every real workload. Every unit test stayed green because each sizes
    /// its to-space generously; nothing but a real run could have shown it.
    /// This test is that run, frozen.
    #[test]
    fn plan_accepts_the_measured_shape_of_a_real_young_collection() {
        const TO_HEADROOM: usize = 134_217_728;
        const FROM_USED: usize = 134_096_736;
        let plan = ParEvac::plan(TO_HEADROOM, 0x1000, FROM_USED, 8)
            .expect("a real cycle's 121 KiB of slack must be enough to parallelise");
        assert!(plan.plab_bytes >= PLAB_MIN_BYTES);
        // The reservation is the whole point: buffers plus the allowance must
        // fit in the slack, or a worker could bump past the arena.
        let slack = TO_HEADROOM - FROM_USED;
        assert!(
            8 * plan.plab_bytes + plan.waste_allowance <= slack,
            "reservation {} exceeds the {slack} bytes of slack that exist",
            8 * plan.plab_bytes + plan.waste_allowance,
        );
    }

    #[test]
    fn plan_declines_a_misaligned_to_space_cursor() {
        // Plenty of headroom, but the pad an aligned start would leave is too
        // small for any sentinel, so nothing could make it walkable.
        let head = 4096 + 4096 / 7 + 2 * PLAB_MAX_BYTES + 64;
        assert!(ParEvac::plan(head, 0x1004, 4096, 2).is_none());
        let plan = ParEvac::plan(head, 0x1008, 4096, 2).expect("an aligned cursor is accepted");
        assert_eq!(plan.region_start, 0x1008);
        // The workers' ceiling is the RESERVATION, not the whole tail: the
        // backing store maps lazily, and only `reserved` bytes get committed.
        assert_eq!(plan.region_end, 0x1008 + plan.reserved);
        assert!(
            plan.reserved <= head,
            "the reservation must fit inside the tail it was cut from",
        );
        assert!(
            plan.reserved >= 4096,
            "it must at least cover the survivors",
        );
    }

    /// The per-worker buffer must track the live set, not sit at a constant.
    ///
    /// Measured cost of the constant: eight workers on a 393 KiB live set
    /// retired 327 KiB of filler — nearly a byte of dead to-space per byte of
    /// survivor — because each held a 64 KiB buffer it could not fill. The
    /// clamp is what keeps both ends sane: a tiny cycle does not reserve
    /// megabytes, and a huge one does not degrade into a `fetch_add` per
    /// object.
    #[test]
    fn the_buffer_size_tracks_the_live_set_between_its_two_clamps() {
        let big = 1024 * 1024 * 1024;
        let plan = |from_used: usize, workers: usize| {
            ParEvac::plan(big, 0x1000, from_used, workers)
                .expect("a gigabyte of headroom covers every case here")
                .plab_bytes
        };
        // Tiny live set: the floor, not `from_used / 32`.
        assert_eq!(plan(64 * 1024, 8), PLAB_MIN_BYTES);
        // Huge live set: the ceiling, not `from_used / 32`.
        assert_eq!(plan(512 * 1024 * 1024, 8), PLAB_MAX_BYTES);
        // In between: proportional, and 8-aligned so every buffer base stays
        // on the object grid.
        let mid = plan(512 * 1024, 8);
        assert_eq!(mid, (512 * 1024 / 8 / PLAB_LIVE_DIVISOR) & !7);
        assert!(mid > PLAB_MIN_BYTES && mid < PLAB_MAX_BYTES);
        assert_eq!(mid % 8, 0);
        // More workers on the same live set means smaller buffers each, so the
        // total reserved does not grow with the worker count.
        assert!(plan(64 * 1024 * 1024, 16) <= plan(64 * 1024 * 1024, 4));
    }

    /// Everything a `ParEvac` needs, kept alive for the test's duration.
    struct Fixture {
        from: crate::arena::Arena,
        to: crate::arena::Arena,
        old: OldGen,
        obj: *mut u8,
    }

    /// One legitimate single-slot object in a fresh from-space.
    fn fixture() -> Fixture {
        let mut from = crate::arena::Arena::new(64 * 1024);
        let to = crate::arena::Arena::new(512 * 1024);
        let old = OldGen::new(64 * 1024);
        let size = HEADER_SIZE + crate::heap::SLOT_SIZE;
        let obj = from.alloc(size, 8).expect("fresh arena serves one object");
        let header = ObjectHeader::new(
            cratonvm_types::ClassId::new(5),
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            1,
        );
        // SAFETY: `obj` is a fresh `size`-byte allocation, 8-aligned.
        unsafe {
            std::ptr::write(obj as *mut ObjectHeader, header);
            std::ptr::write_bytes(obj.add(HEADER_SIZE), 0, crate::heap::SLOT_SIZE);
        }
        Fixture { from, to, old, obj }
    }

    /// Serialises the two tests that deliberately LOSE a forwarding CAS.
    ///
    /// Both read [`EVAC_CAS_LOSSES`] before and after their own call and
    /// assert an exact delta, and the counter is process-global while the
    /// harness runs tests on parallel threads — so without this lock the
    /// second test to be added silently turns the first into a flake. (The
    /// alternative, relaxing both to `>=`, would throw away the one assertion
    /// that proves the loser arm ran at all.) gengc-round1 2026-09-20.
    static CAS_LOSS_ARM: Mutex<()> = Mutex::new(());

    /// `evacuate`'s forwarding-CAS LOSER must adopt the winner's address AND
    /// record the forward.
    ///
    /// Recording is the half that is easy to leave out and impossible to
    /// notice: the loser already has the right address to hand back, so the
    /// object graph comes out correct and only the ROOT SET is wrong — a root
    /// still naming the from-space copy, unremappable because `pointer_map`
    /// has no key for it, dangling the moment from-space is reset. That is
    /// verbatim the defect G1's identical arm carried.
    #[test]
    fn a_forwarding_cas_loser_adopts_the_winner_and_records_the_forward() {
        let _serial = CAS_LOSS_ARM.lock();
        let mut fx = fixture();
        let mut starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));

        // The address the "winning" worker installs while we are mid-copy.
        // Any plausible, 8-aligned address inside a destination arena will do.
        let winner_addr = fx.to.base_ptr() as usize + 256 * 1024;
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
            .expect("512 KiB of to-space covers one object plus a buffer");
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );

        let losses_before = EVAC_CAS_LOSSES.load(Ordering::Relaxed);
        let _guard = set_race_hook(Box::new(move |src: *mut u8| {
            // SAFETY: `src` is the from-space object `evacuate` is copying.
            let h = unsafe { &*(src as *const ObjectHeader) };
            h.set_forwarding_address(winner_addr as *mut u8);
        }));

        let mut shard = EvacShard::default();
        // SAFETY: `fx.obj` is an aligned object start inside from-space.
        let out = unsafe { evac.evacuate(&mut shard, fx.obj) };

        assert_eq!(
            out as usize, winner_addr,
            "the loser must converge on the winner's address, not its own copy",
        );
        assert!(
            shard.forwards.contains(&(fx.obj as usize, winner_addr)),
            "the loser's forward went unrecorded — a root naming {:#x} could \
             never be remapped",
            fx.obj as usize,
        );
        assert_eq!(
            shard.objects_copied, 0,
            "an abandoned speculative copy must not be counted as a survivor",
        );
        assert_eq!(
            EVAC_CAS_LOSSES.load(Ordering::Relaxed),
            losses_before + 1,
            "the loser arm's own counter did not move, so this test proved nothing",
        );
    }

    /// A forwarding-CAS loser that had already PROMOTED its speculative copy
    /// must leave a reference-free filler behind, not a live-looking object.
    ///
    /// gengc-round1 2026-09-20. In young to-space an abandoned copy is inert:
    /// nothing names it and next cycle's sweep reclaims it. In OLD gen it is
    /// not, because `scan_dirty_cards` is liveness-blind — it reads the
    /// reference slots of every object whose header starts in a dirty card. An
    /// abandoned promotion's slots still hold THIS cycle's from-space
    /// addresses, which become live-looking again two cycles later when the
    /// semispaces swap back. The filler removes the slots while keeping the
    /// block exactly as walkable (and exactly as reclaimable) as before.
    #[test]
    fn a_cas_loser_that_promoted_stamps_a_filler_over_the_abandoned_block() {
        let _serial = CAS_LOSS_ARM.lock();
        let mut fx = fixture();
        // `ObjectStartBits::insert` takes `&self`, so no `mut` is needed here.
        let starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));
        let winner_addr = fx.to.base_ptr() as usize + 256 * 1024;
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
        let promoted_size = HEADER_SIZE + crate::heap::SLOT_SIZE;
        let abandoned_before = PAR_EVAC_ABANDONED_OLD_BYTES.load(Ordering::Relaxed);
        {
            let evac = ParEvac::new(
                fx.from.base_ptr() as usize,
                fx.from.used(),
                fx.from.capacity(),
                &starts,
                fx.to.base_ptr() as usize,
                fx.to.capacity(),
                &plan,
                &mut fx.old,
                // force_promote_all: send the speculative copy to OLD gen,
                // which is the arm this test exists for.
                true,
                3,
                false,
                false,
            );
            let guard = set_race_hook(Box::new(move |src: *mut u8| {
                // SAFETY: `src` is the from-space object `evacuate` is copying.
                let h = unsafe { &*(src as *const ObjectHeader) };
                h.set_forwarding_address(winner_addr as *mut u8);
            }));
            let mut shard = EvacShard::default();
            // SAFETY: `fx.obj` is an aligned object start inside from-space.
            let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
            assert_eq!(
                out as usize, winner_addr,
                "the loser must still converge on the winner's address",
            );
            assert_eq!(
                shard.tally[1], 0,
                "an abandoned promotion is not a survivor"
            );
            drop(guard);
            // Hand the promotion buffer's never-written tail back so the walk
            // below sees the abandoned block and nothing else.
            evac.retire_plab(&mut shard);
        }
        assert_eq!(
            PAR_EVAC_ABANDONED_OLD_BYTES.load(Ordering::Relaxed),
            abandoned_before + promoted_size as u64,
            "the abandoned-promotion arm's own counter did not move, so this \
             test proved nothing",
        );

        let objects = fx.old.walk_objects();
        assert_eq!(
            objects.len(),
            1,
            "the old-gen walk must still stride the abandoned block as one object",
        );
        let (ptr, size) = objects[0];
        assert_eq!(
            size, promoted_size,
            "the filler must consume the abandoned block exactly, or `compact`'s \
             walked-bytes-vs-used-bytes check abandons every future compaction",
        );
        // SAFETY: `walk_objects` yielded this as a valid object start.
        let header = unsafe { &*(ptr as *const ObjectHeader) };
        assert_eq!(header.class_id, crate::tlab::TLAB_FILLER_CLASS_ID);
        assert_eq!(header.kind(), ObjectKind::Array);
        assert_eq!(
            header.element_type(),
            ArrayElementType::Int,
            "an int[] has no reference slots for the dirty-card scan to read",
        );
    }

    /// Rotating a worker's YOUNG to-space buffer must not cost it its OLD-gen
    /// promotion buffer.
    ///
    /// gengc-round1 2026-09-20. `plab_alloc`'s refill path called
    /// `retire_plab`, which also returns the promotion buffer's tail to the
    /// old generation — under the old-gen mutex, on the young allocation path.
    /// Every to-space buffer rotation therefore took the collector's one
    /// shared lock and forced the next promotion to take it again to carve a
    /// replacement buffer, which is precisely the per-object mutex traffic the
    /// promotion buffer was introduced to remove.
    #[test]
    fn rotating_the_young_buffer_keeps_the_promotion_buffer() {
        let mut fx = fixture();
        let starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
            .expect("512 KiB of to-space buys buffers");
        assert!(
            plan.plab_bytes > 0,
            "this test needs a BUFFERED cycle; a bufferless one never rotates",
        );
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );
        let mut shard = EvacShard::default();

        // Carve a promotion buffer and remember exactly where it stands.
        let promoted = evac
            .promote_alloc(&mut shard, 32)
            .expect("a fresh old gen serves a promotion buffer");
        assert!(evac.in_old(promoted));
        let (old_cursor, old_end) = (shard.old_plab.cursor, shard.old_plab.end);
        assert!(old_end > old_cursor, "the promotion buffer is live");

        // Carve the young buffer, then fill it until it has to rotate.
        evac.plab_alloc(&mut shard, 48)
            .expect("the first object carves the to-space buffer");
        let first_plab_end = shard.plab.end;
        assert!(first_plab_end > 0);
        let mut rotated = false;
        for _ in 0..1024 {
            evac.plab_alloc(&mut shard, 48)
                .expect("the reservation covers these objects");
            if shard.plab.end != first_plab_end {
                rotated = true;
                break;
            }
        }
        assert!(rotated, "the buffer never rotated, so nothing was proved");
        assert!(
            !shard.plab_gaps.is_empty(),
            "the abandoned tail must be published for filling, or the arena \
             stops being walkable as next cycle's from-space",
        );
        assert_eq!(
            (shard.old_plab.cursor, shard.old_plab.end),
            (old_cursor, old_end),
            "the promotion buffer must survive a young-buffer rotation untouched",
        );
    }

    /// The already-forwarded FAST path must record too, for the same reason —
    /// when the target is in OLD gen, where it may be a selective-promotion
    /// forward from an earlier non-moving cycle that nothing else records.
    ///
    /// gen r4w4/young4 (2026-09-24): a YOUNG to-space target is not
    /// re-recorded. Every to-space forward is installed by a CAS winner of this
    /// very phase, whose own push is merged; the fast path's copy was a
    /// duplicate pushed once per re-encountered edge. Both halves are pinned
    /// here.
    #[test]
    fn an_already_forwarded_object_still_records_its_forward() {
        // -> the shard's forwards after one fast-path hit on `target(fx)`.
        fn fast_path_forwards(
            target_of: fn(&Fixture) -> usize,
        ) -> (usize, usize, Vec<(usize, usize)>) {
            let mut fx = fixture();
            let mut starts = crate::young_mark::ObjectStartBits::new(
                fx.from.base_ptr() as usize,
                fx.from.used(),
            );
            assert!(starts.insert(fx.obj as usize));
            let target = target_of(&fx);
            // SAFETY: `fx.obj` carries a valid header written by `fixture`.
            unsafe {
                (*(fx.obj as *const ObjectHeader)).set_forwarding_address(target as *mut u8);
            }

            let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
            let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
            let evac = ParEvac::new(
                fx.from.base_ptr() as usize,
                fx.from.used(),
                fx.from.capacity(),
                &starts,
                fx.to.base_ptr() as usize,
                fx.to.capacity(),
                &plan,
                &mut fx.old,
                false,
                3,
                false,
                false,
            );
            let mut shard = EvacShard::default();
            // SAFETY: as above.
            let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
            assert_eq!(out as usize, target, "the fast path hands back the forward");
            assert_eq!(shard.objects_copied, 0, "nothing was copied");
            assert_eq!(shard.tally[4], 1, "counted as a re-encounter either way");
            (fx.obj as usize, target, shard.forwards)
        }

        // An OLD-gen target: recorded.
        let (obj, target, forwards) = fast_path_forwards(|fx| fx.old.extent().0 + 64);
        assert_eq!(forwards, vec![(obj, target)]);

        // A young TO-space target: its winner recorded it; no duplicate.
        let (_, _, forwards) = fast_path_forwards(|fx| fx.to.base_ptr() as usize + 128 * 1024);
        assert!(
            forwards.is_empty(),
            "a to-space forward is this phase's own and already recorded by its winner",
        );
    }

    /// A forwarding word left over from an earlier cycle can hold 8-aligned
    /// non-null GARBAGE (BUG-Z). Following it would send the root remap into
    /// freed memory, so it has to be refused — the object is left unmoved,
    /// exactly as the serial evacuator's identical guard does.
    #[test]
    fn a_forwarding_target_outside_both_destinations_is_refused() {
        let mut fx = fixture();
        let mut starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));
        // SAFETY: `fx.obj` carries a valid header written by `fixture`.
        unsafe {
            (*(fx.obj as *const ObjectHeader)).set_forwarding_address(0x1110 as *mut u8);
        }

        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );
        let mut shard = EvacShard::default();
        // SAFETY: as above.
        let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
        assert_eq!(out, fx.obj, "a bogus forward must leave the object unmoved");
        assert!(shard.forwards.is_empty(), "and must not be recorded");
    }

    /// An address that is inside from-space and 8-aligned but is NOT an object
    /// start — an interior word arriving from a conservative root — must be
    /// handed straight back. Forwarding through it would install a pointer in
    /// the middle of a live object.
    #[test]
    fn an_interior_address_is_refused_without_writing_anything() {
        let mut fx = fixture();
        let mut starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));
        // SAFETY: `fx.obj + 8` is inside the object's own allocation.
        let interior = unsafe { fx.obj.add(8) };

        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2).expect("plan");
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );
        // SAFETY: `interior` is a readable, aligned address inside from-space.
        let mut shard = EvacShard::default();
        let out = unsafe { evac.evacuate(&mut shard, interior) };
        assert_eq!(out, interior);
        assert!(shard.forwards.is_empty());
        // gengc-round4-move: and the refusal is RECORDED, in the serial
        // evacuator's vocabulary, for the driver to put in the cycle's ledger.
        assert_eq!(
            shard.refusals,
            vec![(interior as usize, "not-an-object-start")],
            "a parallel refusal must reach the same ledger a serial one does",
        );
        // SAFETY: the object header is intact if nothing was written through it.
        let h = unsafe { &*(fx.obj as *const ObjectHeader) };
        assert!(!h.is_forwarded(), "the real object must be untouched");
    }

    /// gengc-round4-move: the bufferless lever's plan keeps the worker count,
    /// drops every buffer, and reserves exactly the survivors — the
    /// reservation a thin-slack cycle already runs on.
    #[test]
    fn a_bufferless_plan_keeps_the_workers_and_reserves_exactly_the_survivors() {
        const FROM_USED: usize = 1024 * 1024;
        let plan = ParEvac::plan(64 * 1024 * 1024, 0x1000, FROM_USED, 8).expect("ample slack");
        assert!(
            plan.plab_bytes > 0,
            "precondition: a BUFFERED plan to strip"
        );
        let workers = plan.workers;
        let b = plan.into_bufferless();
        assert_eq!(b.workers, workers, "the lever must not change the width");
        assert_eq!(b.plab_bytes, 0);
        assert_eq!(b.waste_allowance, 0);
        assert_eq!(b.reserved, FROM_USED, "exactly the survivors");
        assert_eq!(b.region_start, 0x1000);
        assert_eq!(b.region_end, 0x1000 + FROM_USED);
    }

    /// gengc-round4-move: the shared to-space cursor never moves past the
    /// region, so a refusal is exact and cannot be manufactured by a peer's
    /// failing request (the `fetch_add`/`fetch_sub` undo it replaced left the
    /// cursor inflated for the duration of every failed bump).
    #[test]
    fn the_shared_cursor_never_leaves_the_region() {
        let mut fx = fixture();
        let starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
            .expect("plan")
            .into_bufferless();
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            false,
            3,
            false,
            false,
        );
        let first = evac.bump_shared(8).expect("the region holds the survivors");
        assert_eq!(first, plan.region_start);
        // More than what is left: refused, and the cursor did not move.
        assert!(evac.bump_shared(plan.reserved).is_none());
        assert_eq!(evac.to_cursor_end(), plan.region_start + 8);
        // Exactly what is left: served, and the cursor lands on the end.
        let rest = plan.reserved - 8;
        assert!(evac.bump_shared(rest).is_some());
        assert_eq!(evac.to_cursor_end(), plan.region_end);
        // Nothing left at all.
        assert!(evac.bump_shared(8).is_none());
        assert_eq!(
            evac.to_cursor_end(),
            plan.region_end,
            "a refusal must not move the cursor"
        );
    }

    /// gengc-round4-move: a young survivor that finds the reserved to-space
    /// region exhausted is TENURED, not a `process::abort()`.
    ///
    /// The region can run out on a near-total-survival cycle because a
    /// forwarding-CAS loser's abandoned copy spends bytes `plan` never
    /// budgeted. This drives the exhausted state directly.
    #[test]
    fn an_exhausted_to_space_region_tenures_instead_of_aborting() {
        let mut fx = fixture();
        let starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));
        let (to_cursor, to_headroom) = fx.to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, fx.from.used(), 2)
            .expect("plan")
            .into_bufferless();
        let evac = ParEvac::new(
            fx.from.base_ptr() as usize,
            fx.from.used(),
            fx.from.capacity(),
            &starts,
            fx.to.base_ptr() as usize,
            fx.to.capacity(),
            &plan,
            &mut fx.old,
            // Not force-promoted, and the fixture object is age 0 against a
            // promotion age of 3: it WANTS young to-space.
            false,
            3,
            false,
            false,
        );
        // Spend the whole region, as abandoned CAS-loser copies would.
        evac.bump_shared(plan.reserved)
            .expect("the region is exactly this big");
        let before = PAR_EVAC_OVERFLOW_PROMOTIONS.load(Ordering::Relaxed);
        let mut shard = EvacShard::default();
        // SAFETY: `fx.obj` is an aligned object start inside from-space.
        let out = unsafe { evac.evacuate(&mut shard, fx.obj) };
        assert_ne!(out, fx.obj, "the object must still be relocated");
        assert!(
            evac.in_old(out as usize),
            "with to-space spent, the survivor must land in old gen",
        );
        // SAFETY: `out` is the object `evacuate` just wrote.
        let h = unsafe { &*(out as *const ObjectHeader) };
        assert!(
            h.is_old_gen(),
            "an early-tenured copy must carry the old-gen flag"
        );
        assert_eq!(shard.tally[1], 1, "and be tallied as a promotion");
        assert!(
            PAR_EVAC_OVERFLOW_PROMOTIONS.load(Ordering::Relaxed) > before,
            "the overflow arm's own counter did not move",
        );
        evac.retire_plab(&mut shard);
    }

    /// gengc-round4-move: a worker that UNWINDS out of the drain must not
    /// strand its peers on the termination handshake.
    ///
    /// Termination waits for every worker to register idle; a panicking worker
    /// never does, so before `WorkerExit` the survivors — the driver included
    /// — waited on `cv` forever, inside a stop-the-world pause, with the panic
    /// that caused it never re-raised. Here worker 1 of 2 panics (via the
    /// race-hook seam, mid-`evacuate`) and worker 2 must still come home.
    #[test]
    fn a_worker_that_unwinds_does_not_strand_its_peers() {
        use cratonvm_types::{ObjectRef, Value};
        let mut from = crate::arena::Arena::new(64 * 1024);
        let to = crate::arena::Arena::new(512 * 1024);
        let mut old = OldGen::new(64 * 1024);
        let size = HEADER_SIZE + crate::heap::SLOT_SIZE;
        let parent = from.alloc(size, 8).expect("parent");
        let child = from.alloc(size, 8).expect("child");
        for (p, cid) in [(parent, 5u32), (child, 6u32)] {
            let header = ObjectHeader::new(
                cratonvm_types::ClassId::new(cid),
                ObjectKind::Object,
                ArrayElementType::Reference,
                0,
                1,
            );
            // SAFETY: `p` is a fresh `size`-byte allocation, 8-aligned.
            unsafe {
                std::ptr::write(p as *mut ObjectHeader, header);
                std::ptr::write_bytes(p.add(HEADER_SIZE), 0, crate::heap::SLOT_SIZE);
            }
        }
        // parent.field0 = child (a legacy 16-byte `Value` cell).
        // SAFETY: the cell is inside `parent`'s body; `child` is a live start.
        unsafe {
            std::ptr::write(
                parent.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(child))),
            );
        }
        let starts = crate::young_mark::ObjectStartBits::new(from.base_ptr() as usize, from.used());
        assert!(starts.insert(parent as usize));
        assert!(starts.insert(child as usize));
        let (to_cursor, to_headroom) = to.parallel_evacuation_region();
        let plan = ParEvac::plan(to_headroom, to_cursor, from.used(), 2).expect("plan");
        let evac = ParEvac::new(
            from.base_ptr() as usize,
            from.used(),
            from.capacity(),
            &starts,
            to.base_ptr() as usize,
            to.capacity(),
            &plan,
            &mut old,
            false,
            3,
            false,
            false,
        );

        // Copy the parent (no hook yet), then publish its copy as the only
        // work: whoever scans it evacuates the child and hits the hook.
        let mut s0 = EvacShard::default();
        // SAFETY: `parent` is an aligned object start inside from-space.
        unsafe { evac.evacuate(&mut s0, parent) };
        assert_eq!(s0.work.len(), 1, "the parent's copy is queued for scanning");
        evac.shared.lock().stack.append(&mut s0.work);

        let hook = set_race_hook(Box::new(|_| panic!("injected evacuation fault")));
        let died = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // SAFETY: the only queued address is the parent's fresh copy.
            unsafe { evac.run_worker(&mut s0, 2, None) }
        }));
        drop(hook);
        assert!(died.is_err(), "precondition: worker 1 must have unwound");
        assert_eq!(evac.shared.lock().dead, 1, "the unwind was not registered");

        let (tx, rx) = std::sync::mpsc::channel();
        let evac_ref = &evac;
        std::thread::scope(|sc| {
            sc.spawn(move || {
                let mut s1 = EvacShard::default();
                // SAFETY: the shared stack is empty; nothing is scanned.
                unsafe { evac_ref.run_worker(&mut s1, 2, None) };
                let _ = tx.send(());
            });
            let came_home = rx.recv_timeout(std::time::Duration::from_secs(10)).is_ok();
            if !came_home {
                // Release the stranded worker so the scope can join, then fail.
                evac_ref.shared.lock().done = true;
                evac_ref.cv.notify_all();
            }
            assert!(
                came_home,
                "the surviving worker waited forever for a peer that had unwound"
            );
        });
    }

    #[test]
    fn a_big_filler_is_a_walkable_int_array_and_a_small_one_is_the_sentinel() {
        let mut buf = vec![0xAAu8; 256];
        let base = (buf.as_mut_ptr() as usize + 7) & !7;
        // SAFETY: `base..base+64` is inside `buf` and 8-aligned.
        unsafe { install_gap_filler(base, 64) };
        // SAFETY: a filler header was just written here.
        let h = unsafe { &*(base as *const ObjectHeader) };
        assert_eq!(h.class_id, crate::tlab::TLAB_FILLER_CLASS_ID);
        assert_eq!(h.kind(), ObjectKind::Array);
        assert_eq!(
            crate::gen_heap::gen_object_total_size(h),
            64,
            "the filler must consume its span exactly or the next walk desyncs"
        );

        // SAFETY: `base+64 .. base+72` is inside `buf` and 8-aligned.
        unsafe { install_gap_filler(base + 64, 8) };
        // SAFETY: the sentinel's two u32s were just written here.
        let cid = unsafe { std::ptr::read((base + 64) as *const u32) };
        let gap = unsafe { std::ptr::read((base + 68) as *const u32) };
        assert_eq!(cid, crate::tlab::GAP_FILLER_CLASS_ID.as_u32());
        assert_eq!(gap, 8);
    }
}
