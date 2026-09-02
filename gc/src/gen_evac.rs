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
//!    from-space is reset.
//! 3. **Destinations never overlap.** To-space is carved into per-worker
//!    buffers by one `fetch_add` on a shared cursor; old-gen promotions go
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
//! length at +4) for the 8/16/24/32-byte cases below it. Using the existing
//! sentinels rather than inventing a third is the point: a new filler shape
//! would need every one of the ~14 walks that special-case these to learn
//! about it, and the one that did not would desync.
//!
//! A cycle that runs BUFFERLESS (see [`ParEvac::plan`] — the common case, since
//! a young collection triggers with from-space nearly full) leaves no tails at
//! all: every object is an exact span off the shared cursor, so to-space comes
//! out as densely packed as the serial collector leaves it.

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
const PLAB_LIVE_DIVISOR: usize = 4;

/// Fraction of a buffer above which an object bypasses it and takes its own
/// exact span from the shared cursor: `plab_bytes / PLAB_DIRECT_SHIFT_DIV`.
///
/// Keeps one large object from displacing a buffer's worth of small ones.
/// It is NOT what bounds the wasted tail — [`ParEvac::waste_allowance`] is.
const PLAB_DIRECT_SHIFT_DIV: usize = 8;

/// Share of the to-space slack spent on in-flight buffers; the rest becomes
/// the retirement allowance. See [`ParEvac::plan`].
const SLACK_TO_BUFFERS_DIV: usize = 2;

/// A worker's first old-gen promotion buffer (gen-gc-five item 4).
///
/// Promotion used to be one `OldGen::alloc` per object under the old-gen
/// mutex: a walk up the size buckets, a best-fit scan inside one, the split
/// remainder re-pushed, the sorted free-list cache invalidated, and a
/// `memset` of the block that the copy then overwrote byte for byte. A
/// worker now carves a buffer with `OldGen::alloc_unzeroed` — the copy is the
/// write — and bumps promoted objects out of it; the mutex is taken once per
/// buffer instead of once per object.
const OLD_PLAB_MIN: usize = 16 * 1024;
/// The promotion buffer's ceiling; each refill doubles up to this.
const OLD_PLAB_MAX: usize = 256 * 1024;
/// A promoted object at least this large bypasses the buffer and takes its
/// own block, so one large array cannot strand most of a buffer.
const OLD_PLAB_DIRECT_MIN: usize = 32 * 1024;

/// Batch size a worker takes from the shared worklist per acquisition.
const ACQUIRE_CHUNK: usize = 256;
/// Local worklist depth at which a worker publishes its surplus unprompted.
const SPILL_HIGH: usize = 2048;
/// Depth a worker keeps for itself when it spills.
const SPILL_KEEP: usize = 512;
/// Smallest local stack a worker will split in half for an IDLE peer.
///
/// Below this the hand-off costs more than the work it moves — a lock, a
/// notify, and a cold cache line at the far end for one or two objects.
const SHARE_MIN: usize = 8;

/// Times a worker LOST the forwarding CAS and adopted the winner's target.
///
/// A parallel evacuator whose CAS never loses is one whose object graph never
/// shared a child between two workers — which is a statement about the
/// workload, not about the code. Published so "the loser arm is exercised" is
/// a number rather than an assumption; the equivalent G1 counter exists
/// because the loser arm carried an unrecorded forward for months.
pub static EVAC_CAS_LOSSES: AtomicU64 = AtomicU64::new(0);

/// Young collections whose copy phase actually ran in parallel.
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
    /// reference they hold was forwarded and stayed in young to-space.
    pub(crate) deferred_dirty_cards: Vec<usize>,
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
}

/// The plan a driver commits to before opening the parallel phase.
pub(crate) struct EvacPlan {
    /// Absolute address of the first byte workers may allocate.
    pub(crate) region_start: usize,
    /// One past the last byte workers may allocate.
    ///
    /// Also exactly what the driver must COMMIT before opening the phase —
    /// see [`crate::arena::Arena::commit_evacuation_region`]. It is a bound on
    /// what the phase can consume, not the whole to-space tail, so committing
    /// it does not charge the reservation for a semi-space the cycle will not
    /// touch.
    pub(crate) region_end: usize,
    /// Bytes each worker claims per to-space buffer, sized against what this
    /// cycle has to copy. See [`PLAB_LIVE_DIVISOR`].
    pub(crate) plab_bytes: usize,
    /// Bytes of abandoned buffer tail this cycle may spend. See
    /// [`ParEvac::plan`].
    pub(crate) waste_allowance: usize,
    /// Workers this cycle can actually afford buffers for, which may be FEWER
    /// than the policy asked for. See [`ParEvac::plan`].
    pub(crate) workers: usize,
}

struct DrainState {
    stack: Vec<usize>,
    idle: usize,
    done: bool,
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
        // `fetch_add` per object, measured against a `memcpy` per object, and
        // it keeps every worker copying. A buffer is what removes that atomic
        // when there is room to pay for it, nothing more.
        let desired =
            (from_used / workers / PLAB_LIVE_DIVISOR).clamp(PLAB_MIN_BYTES, PLAB_MAX_BYTES);
        let plab_bytes = match (slack / SLACK_TO_BUFFERS_DIV / workers).min(desired) & !7 {
            n if n < PLAB_FLOOR_BYTES => 0,
            n => n,
        };
        let buffers = plab_bytes * workers;
        // The abandoned-tail allowance is BOUNDED rather than "whatever the
        // buffers did not take", because `region_end` is now a promise the
        // driver has to make good in committed memory (see below): an
        // allowance of the whole slack would commit the entire to-space on
        // every cycle and undo the reservation's lazy commit.
        let waste_allowance = (slack - buffers).min(buffers.max(PLAB_MIN_BYTES));
        // EVERY byte this phase can consume, and therefore every byte the
        // driver must commit before a worker writes: one exact span per
        // survivor (at most `from_used`), one in-flight buffer tail per worker
        // (`buffers`), and the abandoned tails the allowance permits. Nothing
        // else takes region. The `min` is a belt — `waste_allowance` is capped
        // at `slack - buffers`, so the sum cannot exceed `to_headroom`.
        let region_bytes = (from_used + buffers + waste_allowance).min(to_headroom);
        Some(EvacPlan {
            region_start: to_cursor_addr,
            region_end: to_cursor_addr + region_bytes,
            plab_bytes,
            waste_allowance,
            workers,
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
                done: false,
            }),
            cv: Condvar::new(),
            idle_hint: AtomicUsize::new(0),
        }
    }

    /// Is `addr` inside young from-space's live extent?
    #[inline]
    pub(crate) fn in_from(&self, addr: usize) -> bool {
        addr >= self.from_base && addr < self.from_base + self.from_used
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
    /// 3. by an exact span off the shared cursor, one `fetch_add`.
    ///
    /// (3) is also the direct path for an object large enough to distort a
    /// buffer. It is the fallback rather than the failure case precisely
    /// because the allowance can run out: a young collection triggers with
    /// from-space ~99.9% full, so on a real cycle there is very little slack to
    /// spend and most of it goes to the buffers themselves. Degrading to an
    /// atomic add per object is a throughput cost paid against a memcpy; a
    /// fourth option that abandoned tails anyway would be a correctness cost.
    ///
    /// Returns `None` only when the reserved region is exhausted, which
    /// [`Self::plan`]'s budget makes unreachable; the caller treats it as the
    /// same "cannot relocate" invariant violation the serial path does.
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
        self.retire_plab(shard);
        let base = self.bump_shared(self.plab_bytes)?;
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
    fn bump_shared(&self, size: usize) -> Option<usize> {
        let addr = self.to_cursor.fetch_add(size, Ordering::Relaxed);
        if addr + size > self.to_region_end {
            // Undo so the cursor the driver publishes stays inside the arena.
            self.to_cursor.fetch_sub(size, Ordering::Relaxed);
            return None;
        }
        Some(addr)
    }

    /// Hand this worker's unused PLAB tail to the driver's filler list, and
    /// its unused promotion-buffer tail back to the old generation.
    pub(crate) fn retire_plab(&self, shard: &mut EvacShard) {
        let (cur, end) = (shard.plab.cursor, shard.plab.end);
        shard.plab.cursor = 0;
        shard.plab.end = 0;
        if end > cur && cur != 0 {
            shard.plab_gaps.push((cur, end - cur));
        }
        self.retire_old_plab(shard);
    }

    /// Return the never-written tail of the worker's promotion buffer to the
    /// old generation's free list. `OldGen::release_unused_tail` does not
    /// stamp the reclaim epoch: the tail never held an object, so no
    /// concurrent-mark remark snapshot can name an address in it, and a young
    /// cycle retiring its buffers does not invalidate an in-flight old-gen
    /// sweep. The tail is 0 or at least `HEADER_SIZE` by [`Self::old_lab_alloc`]'s
    /// rule, so it is always a legal free block.
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
            unsafe { self.old_gen.lock().release_unused_tail(cur as *mut u8, tail) };
        }
    }

    /// Bump `size` bytes out of the worker's promotion buffer, refusing an
    /// allocation that would leave exactly 8 bytes: an 8-byte remainder can
    /// neither go back to the old-gen free list (its minimum block is
    /// `HEADER_SIZE`) nor carry an `int[]` filler, so the buffer is retired
    /// with a tail of at least 24 bytes instead.
    fn old_lab_alloc(shard: &mut EvacShard, size: usize) -> Option<usize> {
        let after = shard.old_plab.cursor.checked_add(size)?;
        if after > shard.old_plab.end || shard.old_plab.cursor == 0 {
            return None;
        }
        if shard.old_plab.end - after == 8 {
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
            return self.old_gen.lock().alloc_unzeroed(size, 8).map(|p| p as usize);
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
        match og.alloc_unzeroed(plab, 8) {
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
            return old_ptr;
        }
        // Validate the raw tag bytes BEFORE any typed enum read. Loading an
        // out-of-range `#[repr(u8)]` discriminant is immediate UB, so the
        // check cannot come after (this is the SIGILL that
        // `forward_object_impl` documents at length).
        let kind_tag = unsafe { cratonvm_types::kind_tag_at(old_ptr) };
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(old_ptr) };
        if object_kind_from_tag(kind_tag).is_none()
            || array_element_type_from_tag(elem_tag).is_none()
        {
            tracing::debug!(
                target: "cratonvm::gc::guard",
                old_ptr = ?old_ptr,
                kind_tag,
                elem_tag,
                "gen_evac::evacuate: invalid kind/element_type tag — false root or corrupt header",
            );
            return old_ptr;
        }

        let hdr = unsafe { &*(old_ptr as *const ObjectHeader) };
        // ONE snapshot of the mark word drives every decision below: the
        // quartet (kind / element_type / gc_age / gc_flags) lives in it, and
        // re-reading it could pick up a racing worker's FORWARDED value
        // mid-decision.
        let observed = hdr.mark_word.load(Ordering::Acquire);

        if ObjectHeader::is_forwarded_mark(observed) {
            let fwd = ObjectHeader::forwarding_target(observed) as usize;
            // Same sanity ladder as the serial path: a forwarding target must
            // be non-null, 8-aligned and inside one of this cycle's two
            // destinations. BUG-Z showed 8-aligned non-null GARBAGE reaching
            // here on a stale slot.
            if fwd == 0
                || fwd % 8 != 0
                || !((fwd >= self.to_base && fwd < self.to_end_cap) || self.in_old(fwd))
            {
                tracing::debug!(
                    target: "cratonvm::gc::guard",
                    fwd,
                    old_ptr = ?old_ptr,
                    "gen_evac::evacuate: bad forwarding target",
                );
                return old_ptr;
            }
            // MUST record even though this call copied nothing — see the
            // module note (2).
            shard.forwards.push((old_addr, fwd));
            shard.tally[4] += 1;
            return fwd as *mut u8;
        }

        // Reconstruct an owned header from the snapshot so nothing holds a
        // `&ObjectHeader` across the forwarding install.
        let owned = unsafe { self.owned_header(old_ptr, observed) };
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
            return old_ptr;
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
            return old_ptr;
        }
        if self.fwd_resolve_strict
            && crate::gc::resolve_class_info(owned.class_id.as_u32()).is_none()
        {
            return old_ptr;
        }

        // `saturating_add`: `gc_age` is a `u8` written with `saturating_add`
        // too, so 255 is reachable and `age + 1` would be an overflow panic in
        // a debug build — inside a GC worker, where the panic surfaces as a
        // poisoned pause rather than anything legible.
        let promote =
            self.force_promote_all || owned.gc_age().saturating_add(1) >= self.promotion_age;
        let mut new_addr = if promote {
            match self.promote_alloc(shard, total_size) {
                Some(p) => p,
                // Old gen full: fall back to to-space, exactly as the serial
                // path does. The Cheney invariant guarantees room.
                None => self.plab_alloc(shard, total_size).unwrap_or(0),
            }
        } else {
            self.plab_alloc(shard, total_size).unwrap_or(0)
        };
        if new_addr == 0 {
            // Both destinations refused. The serial path aborts the process
            // here and explains why: a copying collector has no valid address
            // to hand back for an unrelocated object, and returning `old_ptr`
            // would dangle every live reference once from-space is reset.
            // `plan`'s budget is what makes this unreachable.
            eprintln!(
                "FATAL: parallel GC could not relocate a live object (tried {total_size} bytes; \
                 to-space cursor {:#x} of region ending {:#x}). The reservation in \
                 ParEvac::plan should have made this unreachable.",
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
        unsafe {
            (*(new_ptr as *mut ObjectHeader))
                .mark_word
                .store(observed, Ordering::Relaxed);
        }
        let landed_in_old = self.in_old(new_addr);
        {
            // SAFETY: `new_ptr` is a freshly allocated, fully written object.
            // (`set_gc_age` / `add_gc_flags` take `&self` — the quartet lives
            // in the atomic mark word — so a shared borrow is enough.)
            let new_header = unsafe { &*(new_ptr as *const ObjectHeader) };
            if landed_in_old {
                new_header.add_gc_flags(GC_FLAG_OLD_GEN);
            } else {
                new_header.set_gc_age(new_header.gc_age().saturating_add(1));
            }
        }

        // Claim the object. The copy had to happen first: writing FORWARDED
        // destroys the source's lock state, so the destination must already
        // carry the intact word.
        run_race_hook(old_ptr);
        match hdr.mark_word.compare_exchange(
            observed,
            ObjectHeader::make_forwarded(observed, new_addr),
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                shard.forwards.push((old_addr, new_addr));
                shard.objects_copied += 1;
                let i = if landed_in_old { 0 } else { 2 };
                shard.tally[i] += total_size as u64;
                shard.tally[i + 1] += 1;
                shard.tally[5] += 1;
                shard.work.push(new_addr);
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
                new_addr = ObjectHeader::forwarding_target(winner) as usize;
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
    unsafe fn owned_header(&self, old_ptr: *mut u8, observed: u64) -> ObjectHeader {
        let h = old_ptr as *const ObjectHeader;
        let mut owned = ObjectHeader::new(
            unsafe { std::ptr::addr_of!((*h).class_id).read() },
            ObjectKind::Object,
            ArrayElementType::Reference,
            0,
            0,
        );
        owned.shape = unsafe { std::ptr::addr_of!((*h).shape).read() };
        // The quartet (kind / element_type / gc_age / gc_flags) rides in bits
        // 48..63 of the mark word, so this single store carries all four.
        owned.mark_word.store(observed, Ordering::Relaxed);
        owned
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
        let mut stayed_young = false;

        // SAFETY: `obj_ptr`/`header` describe one valid copied object.
        unsafe {
            crate::gen_heap::forward_ref_slots(obj_ptr, header, |ref_ptr| {
                if !self.in_from(ref_ptr as usize) {
                    return None;
                }
                // SAFETY: inside the enclosing `unsafe` block — `ref_ptr` was
                // just screened as an address inside young from-space.
                let new_ptr = self.evacuate(shard, ref_ptr);
                if holder_is_old && !self.in_old(new_ptr as usize) {
                    stayed_young = true;
                }
                Some(new_ptr)
            });
        }

        // A live object keeps its class's defining ClassLoader alive
        // (HIB-CV-24, the instance→loader edge). A young loader is evacuated
        // like any other survivor.
        if self.loader_pin_on {
            if let Some(loader_old) = cratonvm_types::loader_pin::loader_pin_addr(class_id) {
                if self.in_from(loader_old) {
                    // SAFETY: `loader_old` is inside from-space, so it is a
                    // readable, aligned candidate object start.
                    let new_lp = unsafe { self.evacuate(shard, loader_old as *mut u8) };
                    if holder_is_old && !self.in_old(new_lp as usize) {
                        stayed_young = true;
                    }
                }
            }
        }

        if stayed_young {
            shard.deferred_dirty_cards.push(obj_addr);
        }
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
        let threads = shards.len();
        if threads <= 1 {
            if let Some(shard) = shards.first_mut() {
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
            unsafe { self.run_worker(&mut shards[0], 1) };
            return;
        }
        let workers = helpers + 1;
        // Per-helper slots, one lock each and taken exactly once per pause. A
        // lock rather than a raw slot is what lets the dispatched body be a
        // plain `Fn`, which the pool's erased dispatch requires, with no unsafe
        // beyond the one the pool already documents.
        let slots: Vec<Mutex<EvacShard>> = (0..helpers)
            .map(|_| Mutex::new(EvacShard::default()))
            .collect();
        {
            let slots_ref = &slots;
            let body = move |i: usize| {
                let mut shard = EvacShard::default();
                // SAFETY: identical contract to the driver's own `run_worker`
                // call below.
                unsafe { self.run_worker(&mut shard, workers) };
                // A helper's buffer is retired by the driver after the barrier
                // along with its own; the tail is dead memory until then and
                // nothing walks to-space before it is filled.
                *slots_ref[i].lock() = shard;
            };
            pool.scope(helpers, &body, || {
                // SAFETY: as above.
                unsafe { self.run_worker(&mut shards[0], workers) };
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
    /// # Safety
    /// See [`Self::drain`].
    unsafe fn run_worker(&self, shard: &mut EvacShard, threads: usize) {
        loop {
            {
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
                    if g.idle == threads {
                        g.done = true;
                        self.cv.notify_all();
                        return;
                    }
                    self.cv.wait(&mut g);
                    g.idle -= 1;
                    self.idle_hint.store(g.idle, Ordering::Relaxed);
                }
            }
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
                    drop(g);
                    self.cv.notify_all();
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PLAB tail fillers
// ---------------------------------------------------------------------------

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
        // The region is the phase's CONSUMPTION BOUND, not the whole tail:
        // since 2026-09-02 the driver must commit it before a worker writes
        // (`Arena::commit_evacuation_region`), and committing the tail would
        // charge the reservation for a semi-space this cycle never touches.
        assert_eq!(
            plan.region_end - plan.region_start,
            4096 + 2 * plan.plab_bytes + plan.waste_allowance,
        );
        assert!(plan.region_end <= 0x1008 + head, "and never past the tail");
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
            let prev = h.mark_word.load(Ordering::Relaxed);
            h.mark_word.store(
                ObjectHeader::make_forwarded(prev, winner_addr),
                Ordering::Relaxed,
            );
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

    /// The already-forwarded FAST path must record too, for the same reason.
    #[test]
    fn an_already_forwarded_object_still_records_its_forward() {
        let mut fx = fixture();
        let mut starts =
            crate::young_mark::ObjectStartBits::new(fx.from.base_ptr() as usize, fx.from.used());
        assert!(starts.insert(fx.obj as usize));
        let target = fx.to.base_ptr() as usize + 128 * 1024;
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
        assert_eq!(out as usize, target);
        assert_eq!(shard.forwards, vec![(fx.obj as usize, target)]);
        assert_eq!(shard.objects_copied, 0, "nothing was copied");
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
        // SAFETY: the object header is intact if nothing was written through it.
        let h = unsafe { &*(fx.obj as *const ObjectHeader) };
        assert!(!h.is_forwarded(), "the real object must be untouched");
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
