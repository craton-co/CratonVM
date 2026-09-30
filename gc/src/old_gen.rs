// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Old generation free-list allocator with mark-compact collection.
//!
//! The old generation uses a free-list allocator for allocation and a sliding
//! mark-compact collector for major GC. Objects are allocated from a
//! size-segregated free list. During major GC, live objects are compacted
//! toward the start of the heap, eliminating fragmentation entirely.
//!
//! **Which of those actually runs (gen r4/oldgen, 2026-09-23).** The sliding
//! compactor ([`OldGen::compact`]) is **off by default** and has been since
//! 2026-08-03: `gen_heap::major_gc` asks `oldgen_compact_enabled()`
//! (`CRATONVM_OLDGEN_COMPACT` / `CRATONVM_GC=oldgen-compact`, opt-in), and
//! without it every old-generation collection — on the moving young path as
//! well as the non-moving one — is the IN-PLACE mark-sweep
//! (`old_gen_gc(compact = false)`): dead blocks go back on the free list where
//! they lie and [`OldGen::coalesce_free_blocks`] merges adjacent ones at the
//! end of each sweep. So on a default run nothing ever defragments the
//! generation: coalescing merges touching free spans, it cannot move a live
//! object out of the way. Several documents (and `major_gc`'s own name) still
//! call the default old-gen collection a "mark-compact"; it is not one.
//!
//! **Backing store (gen r4w2/oldgen2, 2026-09-23).** The generation sits on
//! `reservation::HeapStore` like every other arena in the crate: its capacity
//! is reserved address space, pages are committed as the allocator first hands
//! them out (a refusal is an allocation failure), and a compaction hands the
//! whole granules of the range it empties back to the OS. [`OldGen::contains`]
//! answers for the committed ("readable") part only. `CRATONVM_GC_RESERVE=0`
//! restores a wholly committed block.
//!
//! **Sizing (gen r4w4/oldgen4, 2026-09-24).** The generation's COMMITTED end
//! grows on demand within its reservation (the allocator commits what it hands
//! out) and, since this wave, SHRINKS after a stop-the-world old-gen
//! collection: whole granules of the trailing free block above
//! `max(-Xms prefix, used / 0.3)` are decommitted
//! ([`OldGen::resize_after_collection`], HotSpot's `MaxHeapFreeRatio = 70`),
//! so `Runtime.totalMemory()` follows the live set down as it follows it up.
//! The RESERVATION — the generation's share of `-Xmx` — does not move: the
//! young/old split is the budget's, not this file's. A walk that desyncs no
//! longer stops reclamation ([`OldGen::walk_objects_with_gaps`]), an allocation
//! refused for fragmentation rather than fullness arms a compaction request
//! ([`OldGen::arm_fragmentation_compaction`]), and the best-fit scan runs over
//! four size classes per power of two with a bounded non-fit probe.
//!
//! **Compaction around pins (gen r4w5/oldcompact5, 2026-09-24).** The sliding
//! compactor needs every root rewritable, so the non-moving path — which a
//! JIT-warm process takes on nearly every cycle, and which the
//! allocation-failure ladder's explicit major request forces — could never
//! answer a fragmentation request. [`OldGen::compact_around_pins`] is the same
//! slide with PINNED objects as fixed islands: an object a conservative word
//! names keeps its address, everything else slides down into the holes
//! between islands. Opt-in (`CRATONVM_GC_OLD_PINNED_COMPACT`); see
//! `GenerationalHeap::sweep_old_gen_non_moving` for where the pins come from.
//!
//! ## Allocation strategy (round-5 #14 fix)
//!
//! The previous implementation kept a single `Vec<FreeBlock>` sorted by
//! offset and scanned the whole list on every allocation, paying O(N) per
//! `alloc` call. After a long-running mutator built up tens of thousands
//! of free fragments this became the dominant CPU cost of old-gen
//! allocation.
//!
//! The current implementation segregates free blocks into power-of-2 size
//! buckets (8, 16, 32, …, 512 MB). Allocation picks the smallest bucket
//! whose nominal size satisfies the request, then pops a block from the
//! front — amortised O(1). When all blocks in a bucket are exhausted the
//! allocator escalates to the next bucket up. Best-fit semantics are
//! preserved within a bucket by tracking the smallest-suitable block in a
//! single pass.
//!
//! The sorted-by-offset view needed for coalescing and compaction is
//! rebuilt on demand from the per-bucket vectors.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use crate::gc_flags;
use crate::heap::{
    array_data_size, array_element_type_from_tag, object_kind_from_tag, ArrayElementType,
    ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, GC_FLAG_MARKED, HEADER_SIZE, REF_ELEMENT_SIZE,
    SLOT_SIZE,
};
use crate::reservation::{HeapStore, GRANULE};

// gen r4w3/cards3 (2026-09-23): the block-offset table and the dirty-card
// object walk built on it, in a child module so it can reach this file's
// private walker helpers without adding hunks here beyond the three hook
// points (`alloc_from_buckets_scan`, `free`, `compact_walked`) and the field.
mod bot;
pub use bot::{BlockOffsetStats, CardRangeHit, ObjectAt};
// gen r5w5/old9 (2026-09-27): the O(live) in-place sweep over a marked set
// the block-offset oracle admitted (`CRATONVM_GC_OLD_LIVE_SWEEP`, opt-in).
mod live_sweep;
pub use live_sweep::{LiveClosure, LiveSweepOutcome, LiveSweepRefusal};
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size};
use cratonvm_types::{ObjectRef, Value};

/// Cached `CRATONVM_DBG_SEEDHUNT` gate (bc math-ec `0x4`). When on,
/// `update_refs_in_object` logs any referent whose `forwarding_ptr` is
/// non-null but `< 0x1000` — the §6.1 suspect that would write
/// `Object(Some(0x4))` into a live referrer's field during major-GC
/// compaction. See bc-math-ec-gc-0x4-handoff.md §6.1.
#[inline]
fn seedhunt_enabled() -> bool {
    gc_flags().dbg_seedhunt
}

/// A contiguous free block in the old generation.
#[derive(Debug, Clone, Copy)]
struct FreeBlock {
    /// Byte offset from the start of the data buffer.
    offset: usize,
    /// Size in bytes.
    size: usize,
}

/// One size class of the segregated free list: the blocks, plus an UPPER
/// BOUND on their sizes.
///
/// gen r4w3/oldgen3 (2026-09-23),
/// `gengc-r4w2-oldgen2-best-fit-scan-is-unbounded-over-non-fitting-blocks`.
/// The best-fit scan in [`OldGen::alloc_from_buckets_scan`] caps how many
/// FITTING blocks it compares, but walks every too-small block in the
/// request's start bucket — so after an in-place sweep leaves tens of
/// thousands of same-size holes in one class, every request a little larger
/// than those holes paid a scan of all of them, under the old-gen lock, and
/// then escalated anyway. `size_bound` lets the scan prove "nothing here
/// fits" in O(1): if the bound is below the request, no block can fit, and
/// the scan would have walked the whole bucket to reach exactly that
/// conclusion. Skipping it therefore changes no placement — the next bucket
/// is tried exactly as before — only the cost of getting there.
///
/// The invariant is one-sided: `size_bound >= every block's size`. [`push`]
/// raises it, [`clear`] resets it, [`swap_remove`] leaves it (still an upper
/// bound), and the scan TIGHTENS it to the exact maximum whenever it has just
/// walked the whole bucket without finding a fit. A stale-high bound costs a
/// scan; a low one would skip a fitting block and could turn into a spurious
/// OOM — which is why every mutation goes through these three methods, and
/// why the blocks are not reachable mutably any other way (no `DerefMut`).
///
/// The read side derefs to `[FreeBlock]`, so every existing reader
/// (`len`, indexing, `iter`, `iter().flatten()` on the bucket list) is
/// unchanged.
///
/// [`push`]: Bucket::push
/// [`clear`]: Bucket::clear
/// [`swap_remove`]: Bucket::swap_remove
#[derive(Debug, Default)]
struct Bucket {
    blocks: Vec<FreeBlock>,
    /// `>=` the size of every block in `blocks`; `0` when it is empty after
    /// a [`Bucket::clear`].
    size_bound: usize,
}

impl Bucket {
    #[inline]
    fn push(&mut self, block: FreeBlock) {
        self.size_bound = self.size_bound.max(block.size);
        self.blocks.push(block);
    }

    #[inline]
    fn clear(&mut self) {
        self.blocks.clear();
        self.size_bound = 0;
    }

    /// Removing a block cannot make the bound too LOW, so it stays.
    #[inline]
    fn swap_remove(&mut self, i: usize) -> FreeBlock {
        self.blocks.swap_remove(i)
    }

    /// Set the bound to `exact_max`, which the caller has just computed as
    /// the maximum over EVERY block currently in the bucket.
    #[inline]
    fn tighten(&mut self, exact_max: usize) {
        debug_assert!(self.blocks.iter().all(|b| b.size <= exact_max));
        self.size_bound = exact_max;
    }
}

impl std::ops::Deref for Bucket {
    type Target = [FreeBlock];
    #[inline]
    fn deref(&self) -> &[FreeBlock] {
        &self.blocks
    }
}

impl<'a> IntoIterator for &'a Bucket {
    type Item = &'a FreeBlock;
    type IntoIter = std::slice::Iter<'a, FreeBlock>;
    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.blocks.iter()
    }
}

/// gen r4w4/oldgen4 (2026-09-24): size classes per power of two, as a bit
/// count — TLSF's second-level index
/// (`gengc-r4w2-oldgen2-best-fit-scan-is-unbounded-over-non-fitting-blocks`,
/// the page's first preference). Four classes split `[2^n, 2^(n+1))` into
/// quarters, so a class's blocks differ by less than 25 % in size instead of
/// 2x. Below 64 bytes a class holds exactly one 8-aligned size, so a class
/// full of holes one step too small for a request cannot exist there at all;
/// above it the too-small residue is a quarter of what it was, and
/// [`NONFIT_PROBE_LIMIT`] bounds what is left.
const SUB_CLASS_BITS: u32 = 2;

/// Size classes per power of two (see [`SUB_CLASS_BITS`]).
const SUB_CLASSES: usize = 1 << SUB_CLASS_BITS;

/// Number of size buckets in the segregated free list.
///
/// Power-of-two range `k` (blocks in `[1 << (MIN_BUCKET_SHIFT + k),
/// 1 << (MIN_BUCKET_SHIFT + k + 1))`) is split into [`SUB_CLASSES`]
/// equal-width classes, with the top class also catching everything above.
/// 28 ranges starting at 8 bytes cover `[8 B .. 2 GiB)`, which is more than
/// enough for any realistic heap. (Before gen r4w4/oldgen4 there was one
/// bucket per power of two.)
const NUM_BUCKETS: usize = 28 * SUB_CLASSES;

/// Smallest tracked block size is `1 << MIN_BUCKET_SHIFT` = 8 bytes.
const MIN_BUCKET_SHIFT: u32 = 3;

/// gen r4w4/oldgen4 (2026-09-24): how many TOO-SMALL blocks the best-fit scan
/// walks in one class before it DEFERS the rest of that class and tries the
/// larger classes first.
///
/// [`BEST_FIT_PROBE_LIMIT`] caps the FITTING candidates; nothing capped the
/// non-fitting ones, so a class holding thousands of holes slightly smaller
/// than a request, plus one fit behind them, cost a walk of all of them per
/// request. The deferral keeps the "cannot spuriously OOM" property exactly:
/// the rest of the class is scanned after all when no larger class yields a
/// fit, so a request fails only if the exhaustive scan would have failed too.
/// What it gives up is the tight hole deep in the class when a larger block
/// exists — a placement change confined to a class that already holds more
/// than this many too-small blocks in front of every fit. **Not tuned.**
const NONFIT_PROBE_LIMIT: usize = 64;

/// How many FITTING blocks [`OldGen::alloc_from_buckets`] will look at before
/// settling for the best one it has seen.
///
/// gengc-round1 2026-09-20. Not tuned — no measurement was possible in the
/// session that added it, and none is claimed. It is a runaway guard, sized so
/// that the ordinary case (a bucket holding a handful of blocks) is unaffected
/// and the pathological one (one bucket holding every block freed by a
/// non-compacting sweep) is bounded. See the comment at the use site for why
/// capping only the FITTING candidates is what keeps the allocator from
/// spuriously reporting OOM.
const BEST_FIT_PROBE_LIMIT: usize = 64;

/// `K` in [`OldGen::fragmentation_warrants_repair`]: how many times over the
/// free list must hold the bytes for the largest request recently served
/// before "there are bytes but no room" is the right diagnosis.
///
/// gengc-round2 2026-09-20. **Not tuned** — no measurement was possible in the
/// session that added it. `4` is the value the gap page proposed as a starting
/// point; the shape of the predicate matters more than the constant, because
/// the constant only decides how early a repair the collector was always
/// willing to do gets scheduled.
pub const FRAGMENTATION_FIT_FACTOR: usize = 4;

/// gen r4w3/oldgen3 (2026-09-23): the smallest free block, in WHOLE granules
/// of its interior, that [`OldGen::after_in_place_sweep`] counts as a
/// give-back candidate — and gives back when `CRATONVM_GC_OLD_GIVE_BACK` is
/// on. Four 2 MiB granules is the value the gap page proposed; **not tuned**.
/// Small holes are left alone on purpose: the steady-state reuse of a hole a
/// few granules wide is exactly the page-fault storm a give-back would cause.
pub const GIVE_BACK_MIN_GRANULES: usize = 4;

/// gen r4w3/oldgen3: an in-place give-back runs at most once per this many
/// old-gen collections (the pass's hysteresis). **Not tuned.**
pub const GIVE_BACK_EVERY: u64 = 4;

/// Pick the bucket index that fits `size`. Result is in `[0, NUM_BUCKETS)`.
///
/// gen r4w4/oldgen4: the class is `(floor(log2 n) - MIN_BUCKET_SHIFT) *
/// SUB_CLASSES + q`, where `q` is the quarter of `[2^lg, 2^(lg+1))` that `n`
/// falls in (the two bits below the leading one). Class lower bounds are
/// strictly increasing in the index, which is the one property the search
/// relies on: every block in a class above `bucket_for(n)` is larger than `n`.
#[inline]
fn bucket_for(size: usize) -> usize {
    if size <= (1usize << MIN_BUCKET_SHIFT) {
        return 0;
    }
    // floor(log2(n)); `size > 8`, so `lg >= 3 >= SUB_CLASS_BITS`.
    let lg = (usize::BITS - 1 - size.leading_zeros()) as usize;
    let sub = (size >> (lg - SUB_CLASS_BITS as usize)) & (SUB_CLASSES - 1);
    let idx = lg
        .saturating_sub(MIN_BUCKET_SHIFT as usize)
        .saturating_mul(SUB_CLASSES)
        .saturating_add(sub);
    idx.min(NUM_BUCKETS - 1)
}

/// Pick the smallest bucket index that *might* contain a block large enough
/// to satisfy `size`. Allocators start their search here and escalate upward.
///
/// CRIT (round-5 GC #2, spurious OOM): the previous implementation rounded
/// `size` *up* to the next power of two and used that bucket as the search
/// start — but bucket `k` holds blocks in `[2^(s+k), 2^(s+k+1))`, indexed
/// by `floor(log2(block_size))`. For a request of 100 bytes, the old code
/// jumped to bucket 4 (lower bound 128) and skipped bucket 3, which holds
/// blocks in `[64, 128)`. A 100-byte free block lives in bucket 3; the
/// allocator would miss it entirely and report OOM despite having a
/// fitting block on hand.
///
/// The correct starting bucket is `floor(log2(size))`: that bucket may
/// hold blocks ranging from `size` up to `2*size - 1`, which are valid
/// fits. The per-bucket best-fit scan filters out blocks too small to
/// satisfy the request (`total_needed <= block.size`), so starting one
/// bucket lower than the old code costs only an extra cheap scan in the
/// rare worst case and *cannot* spuriously OOM.
///
/// gen r4w4/oldgen4: with sub-classes the same argument holds class by class —
/// start at the class CONTAINING `size` (it may hold blocks in
/// `[class_lo, size)`, which the scan filters), every higher class holds only
/// blocks `>= its lower bound > size`.
#[inline]
fn min_satisfying_bucket(size: usize) -> usize {
    // The same formula as `bucket_for`, by construction.
    bucket_for(size)
}

/// How many times [`OldGen::coalesce_free_blocks`] ran, and how many free
/// blocks it eliminated. Reported by `VmHeap::print_gc_summary` so "is the
/// old-gen coalescer doing anything in this workload?" is answerable from a
/// log instead of a debugger — the question that decides whether a
/// fragmentation fix is load-bearing or an inert lever.
pub static COALESCE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static BLOCKS_MERGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times [`OldGen::scan_region`]/[`OldGen::scan_region_filtered`]
/// found an invalid `ObjectKind`/`ArrayElementType` tag byte at what should
/// have been an object boundary and stopped that scan stripe instead of
/// trusting it.
///
/// A non-zero value means the walk desynced from real object headers and
/// landed on payload bytes (a stale free-list sliver, a mis-sized prior
/// object, an unparsed TLAB tail, ...). Before this guard existed, that same
/// desync read the payload byte as a typed `#[repr(u8)]` enum — instant UB
/// for a discriminant outside the declared set, which optimized code can
/// lower to a hardware trap (`HIB-DCAST-LATEPHASE.1`,
/// `source-debug-jit-conservative-root-invalid-header-tag-sigill.md`
/// fixed the same class of bug for the conservative-root validators but
/// never reached this walk). This counter turns that silent-until-it-traps
/// failure mode into something a regression test or a log can see.
pub static WALK_DESYNC_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GCAUD-2: how many old-gen compactions were ABANDONED because Phase 0's
/// live-set closure escaped the object walk (see
/// [`OldGen::close_live_set_over_old_gen`]). A non-zero value means the
/// generation was retained wholesale for that cycle — a reclamation the
/// collector declined to make rather than manufacture a dangling slot — and
/// that some earlier phase left a live reference to an unwalked address.
/// Silent before this counter existed; a compaction that reclaims nothing and
/// a compaction that had nothing to reclaim look identical from outside.
pub static COMPACT_ESCAPE_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// GCAUD-9 (2026-08-03): how many old-gen compactions were ABANDONED because
/// `walk_objects` covered fewer bytes than `used_bytes` says are allocated —
/// i.e. some region's `scan_region` broke early on an implausible header and
/// left real (possibly live, possibly overlay-only-referenced) memory outside
/// the walked set. See the check at the top of [`OldGen::compact`]. Distinct
/// from `COMPACT_ESCAPE_HITS`: that counter fires when a walked, MARKED
/// object's ordinary field points outside the walk; this one fires when the
/// walk itself is incomplete, which `COMPACT_ESCAPE_HITS`'s ordinary-field
/// closure cannot detect for an object reachable only through a Rust-side
/// overlay side table.
pub static COMPACT_WALK_GAP_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// GCAUD-9 follow-up (2026-08-03): how many times `scan_region` broke a
/// region's walk early on an implausible header. Distinct from
/// `COMPACT_WALK_GAP_HITS` (which counts abandoned *compactions*, one per
/// GC cycle): this counts every individual break, including ones a later
/// `compact()` call re-discovers at the exact same offset because nothing
/// upstream has fixed the underlying header. Gates the raw-byte dump in
/// [`scan_region`]'s break arm to the first few hits so a persistent,
/// unmoving break point doesn't spam every subsequent GC cycle.
pub static SCAN_REGION_BREAK_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// gen r4/oldgen (2026-09-23): how many times [`OldGen::coalesce_free_blocks`]
/// returned at once because nothing had been added to the free list since the
/// previous full merge — see [`OldGen::free_list_maximal`]. Those calls used to
/// re-collect and re-sort the whole free list to discover the same "nothing is
/// adjacent" answer, and the call that does it most is the allocation-failure
/// retry in [`OldGen::alloc_impl`]: every promotion attempt into a full old
/// generation (once per surviving object in a serial Cheney cycle) paid an
/// O(n log n) sort of the free list under the old-gen lock.
pub static COALESCE_SKIPPED_MAXIMAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// gen r4/oldgen (2026-09-23): the live-set closure's instruments
/// (`docs/feature-designs/gc-round-20260920-generational.md` item 5, whose own
/// first step is "add a counter for the observed pass count, so the change has
/// a before-number").
///
/// The closure ([`OldGen::close_live_set_over_old_gen`]) used to be a
/// re-scan-everything fixpoint; it is now one linear pass plus a worklist
/// drain. The pre-change algorithm's pass count is recoverable exactly from
/// these, which is the point:
///
/// * `CALLS` — closures run (in-place sweeps + compactions that got that far).
/// * `RESCUED` — objects promoted to live in total (0 on a healthy mark).
/// * `MULTI_PASS_CALLS` — calls that rescued anything. The old fixpoint ran
///   exactly ONE pass on every other call and AT LEAST TWO full passes over
///   the live set on each of these.
/// * `DEEP_CALLS` — calls whose worklist drain itself rescued something.
///   Exactly the calls on which the old fixpoint's SECOND pass would have
///   promoted something (the drain's first scans are the objects that pass
///   would have been the first to scan), so it ran at least THREE full passes
///   on each of these, and one more per further backward level of the chain.
/// * `WORKLIST_SCANS` — objects scanned by the drain. This is all the work the
///   new form does beyond one linear pass; the old form paid a whole extra
///   linear pass for the first of them.
pub static CLOSE_LIVE_SET_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// See [`CLOSE_LIVE_SET_CALLS`].
pub static CLOSE_LIVE_SET_RESCUED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// See [`CLOSE_LIVE_SET_CALLS`].
pub static CLOSE_LIVE_SET_MULTI_PASS_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// See [`CLOSE_LIVE_SET_CALLS`].
pub static CLOSE_LIVE_SET_DEEP_CALLS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
/// See [`CLOSE_LIVE_SET_CALLS`].
pub static CLOSE_LIVE_SET_WORKLIST_SCANS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// gen r4/oldgen (2026-09-23): how many times
/// [`OldGen::compact_with_walked_objects`] was handed an object grid that was
/// derived before the free list last changed and re-walked instead of trusting
/// it. Expected to stay 0: the one production caller (`gen_heap::old_gen_gc`)
/// walks, marks and compacts under one old-gen lock with no allocation or free
/// in between. A non-zero value means that stopped being true.
pub static COMPACT_STALE_GRID_REWALKS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// gen r4w2/oldgen2 (2026-09-23): why [`OldGen::major_trigger`] said what it
/// said about running an old-generation collection this young cycle.
///
/// `gengc-r4-oldgen-major-trigger-has-no-hysteresis-FIXED-20260924.md`: the trigger
/// used to be a bare occupancy test, so once the old LIVE set sat above 75 %
/// of capacity every young pause also ran a full old-gen collection that could
/// reclaim almost nothing. The variants separate the reasons so the policy can
/// be A/B'd from one binary, and so the "would the hysteresis have skipped
/// this?" question has an answer on a run where the hysteresis is OFF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MajorTrigger {
    /// Below the 75 % occupancy floor and nothing asked: do not collect.
    NotDue,
    /// An explicit request (`System.gc()`). Always collects.
    Requested,
    /// At or above the floor, and enough has been promoted since the last
    /// old-gen collection (or there has been none) that another one can be
    /// expected to reclaim something. Collects.
    Occupancy,
    /// At or above the floor and NOT grown enough, but an old-gen allocation
    /// (a promotion, or a direct old-gen allocation) has FAILED since the last
    /// old-gen collection. Collects: a failure is the one signal the
    /// hysteresis must never sit on, because it is how a genuinely full
    /// generation reaches the `OutOfMemoryError` ladder.
    AllocationFailure,
    /// At or above the floor, not grown enough, no failure — and the
    /// hysteresis is ON. Does NOT collect.
    Suppressed,
    /// The same state as [`Self::Suppressed`] with the hysteresis OFF (the
    /// default). DOES collect, exactly as the bare occupancy test always did;
    /// counted separately so a default run reports how many collections the
    /// policy would have skipped.
    WouldSuppress,
}

impl MajorTrigger {
    /// Does this verdict run an old-gen collection?
    #[inline]
    pub fn runs(self) -> bool {
        !matches!(self, MajorTrigger::NotDue | MajorTrigger::Suppressed)
    }
}

/// gen r4w2/oldgen2 (2026-09-23): per-generation counters behind the
/// old-gen trigger — the instrument the no-hysteresis gap page asked for
/// before any policy change. Per-instance (a field of [`OldGen`]), not a
/// process global, so a process with two heaps reports per heap.
///
/// Read with [`OldGen::trigger_stats`] (or
/// `GenerationalHeap::old_gen_trigger_stats`). The line that answers the gap
/// page's question is `low_yield` against `collections`: if the second tracks
/// the first on a workload, most of its old-gen collections are pure cost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OldGenTriggerStats {
    /// Old-gen collections that finished (moving-path `major_gc` and the
    /// non-moving path's in-place sweep).
    pub collections: u64,
    /// ...of which reclaimed less than `capacity / 16` bytes.
    pub low_yield: u64,
    /// Trigger verdicts by reason — see [`MajorTrigger`].
    pub requested: u64,
    pub occupancy: u64,
    pub allocation_failure: u64,
    pub suppressed: u64,
    pub would_suppress: u64,
    /// Allocations this generation refused (not counting optional promotion
    /// BUFFER carves, whose caller falls back to an object-sized block).
    pub alloc_failures: u64,
    /// Allocations refused because the OS would not COMMIT the pages — as
    /// distinct from a free list with no fitting block. Both surface as the
    /// same `None`, and so as the same `OutOfMemoryError`; this says which.
    pub commit_refusals: u64,
    /// Allocations the free list served, and the blocks the best-fit scan
    /// walked past because they were too SMALL. The ratio is the per-
    /// allocation cost `gengc-r4w2-oldgen2-best-fit-scan-is-unbounded-over-non-fitting-blocks-FIXED-20260924.md`
    /// is about: the fit cap bounds fitting candidates only.
    pub allocs: u64,
    pub nonfit_probes: u64,
    // --- gen r4w3/oldgen3 (2026-09-23) ---------------------------------
    /// Start buckets the best-fit scan SKIPPED in O(1) because their
    /// [`Bucket`] size bound proved no block in them could fit — each one a
    /// whole walk of too-small blocks that `nonfit_probes` would otherwise
    /// have counted.
    pub bucket_skips: u64,
    /// In-place old-gen sweeps that freed NOTHING because their object walk
    /// did not cover [`OldGen::used`] (a `scan_region` break left allocated
    /// bytes unwalked). See `gengc-r4-mark-walk-desync-tail-is-untraced`.
    pub sweep_walk_gap_skips: u64,
    /// Old-gen collections noted by the CONCURRENT sweep (also counted in
    /// `collections`).
    pub concurrent_collections: u64,
    /// Bytes old-gen collections reclaimed, summed.
    pub freed_bytes: u64,
    /// Collections the trigger ran as [`MajorTrigger::WouldSuppress`] — the
    /// ones `CRATONVM_GC_OLD_TRIGGER_HYSTERESIS` would have skipped — and
    /// what they reclaimed: the bytes the policy would have left in the
    /// generation (until the next collection it allows), and how many of them
    /// were low-yield anyway. The OFF arm of the A/B reads the policy's COST
    /// from these, not just its frequency.
    pub would_suppress_freed_bytes: u64,
    pub would_suppress_low_yield: u64,
    /// Sampled at every collection's end: the free list's block count and
    /// largest block (the fragmentation page's "missing instrument"), and how
    /// many collections ended with [`OldGen::fragmentation_repair_due`] true.
    pub free_blocks_after_last: u64,
    pub largest_free_after_last: u64,
    pub fragmentation_repair_due: u64,
    /// Bytes of WHOLE granules inside free blocks spanning at least
    /// [`GIVE_BACK_MIN_GRANULES`] granules, below the high-water mark —
    /// sampled after each in-place sweep (`give_back_candidate_bytes`) and its
    /// run maximum. The number that says whether an in-place give-back
    /// (`CRATONVM_GC_OLD_GIVE_BACK`) could return anything on a workload.
    pub give_back_candidate_bytes: u64,
    pub give_back_candidate_bytes_max: u64,
    /// In-place give-back passes that ran (flag on), and what they returned.
    pub in_place_give_backs: u64,
    pub in_place_given_back_bytes: u64,
}

/// Non-moving free-list allocator for the old generation.
///
/// Objects are allocated from a size-segregated free list (round-5 #14
/// fix). Freed blocks are coalesced with adjacent free blocks during
/// compaction; intermediate freeing skips the coalesce scan (which was
/// the other O(N) cost in the original implementation) and lets the next
/// major GC absorb the fragmentation in one sweep.
pub struct OldGen {
    /// Backing storage. `len() == capacity` so pointers can be computed
    /// into it, but the bytes are *not* eagerly zeroed (round-11 perf) —
    /// `alloc` zeroes every region before handing it out.
    ///
    /// gen r4w2/oldgen2 (2026-09-23): a [`HeapStore`], not a `Vec<u8>`
    /// (`gengc-r4-oldgen-backing-store-is-committed-up-front-and-never-returned`).
    /// On the default reserving store the capacity is RESERVED address space
    /// and pages are committed in [`GRANULE`] units at the one site that hands
    /// storage to a writer ([`Self::alloc_from_buckets`]); before this, the
    /// whole of the old generation's share of `-Xmx` was committed at startup
    /// on Windows whatever `-Xms` said. `CRATONVM_GC_RESERVE=0`, a capacity
    /// below one granule, and a refused reservation all give the wholly
    /// committed [`HeapStore::Owned`] block, where every commit is a no-op.
    ///
    /// Never accessed through a slice: on the reserving arm a `&[u8]` over the
    /// whole capacity would claim dereferenceability for pages that are not
    /// mapped. Readers take `(base, end)` integers ([`Self::extent`]).
    data: HeapStore,
    /// Size-segregated free list: `buckets[k]` holds blocks whose size
    /// falls in bucket `k`. Each bucket is treated as a LIFO stack —
    /// `push`/`pop` are both amortised O(1).
    ///
    /// gen r4w3/oldgen3: each is a [`Bucket`], which carries an upper bound
    /// on its block sizes so the best-fit scan can skip a class that cannot
    /// fit the request without walking it.
    buckets: Vec<Bucket>,
    /// Total bytes currently allocated (excluding free space).
    used_bytes: usize,
    /// PERF (gc-oldgen-perf): cache of the offset-sorted free-block view.
    ///
    /// `walk_objects` / `walk_objects_in_card_ranges` / `compact` all need
    /// the free blocks in ascending-offset order to locate object boundaries
    /// (the gaps between free blocks are the allocated regions). The previous
    /// code re-`collect()`ed every block out of all 28 buckets and ran a fresh
    /// `sort_by_key` on *every* call. Several GC phases call `walk_objects`
    /// repeatedly with no intervening `alloc`/`free` (e.g. the
    /// `concurrent_mark` rescan loop and the multiple sequential sweep/verify
    /// passes in `gen_heap`), so that view is recomputed identically many
    /// times per cycle.
    ///
    /// We cache the sorted view and rebuild it lazily only when the buckets
    /// have actually changed. `dirty` is set by every bucket mutation
    /// (`alloc`, `free`, and `compact`'s free-list rebuild); while it stays
    /// clear the cached `Vec` is byte-for-byte the same sort that would have
    /// been recomputed, so behaviour is identical — only redundant work is
    /// removed. `RefCell`/`Cell` give interior mutability so the `&self`
    /// walkers can refresh the cache; `OldGen` lives inside a `Mutex`, which
    /// only requires `Send` (satisfied), not `Sync`.
    sorted_free_cache: RefCell<Vec<FreeBlock>>,
    /// `true` when `buckets` has been mutated since `sorted_free_cache` was
    /// last rebuilt, so the cache must be regenerated before use.
    sorted_free_dirty: Cell<bool>,
    /// GCAUD-4 — monotone stamp of "old-gen storage has been RECLAIMED or
    /// RELOCATED since you last looked".
    ///
    /// Every side table an out-of-STW consumer keys on an old-gen ADDRESS
    /// (`ConcurrentMarker`'s mark bitmap and its `sweep_eligible` snapshot are
    /// the live examples) is only valid while no other collector has handed a
    /// block back to the free list or slid an object. Both events make an
    /// address ambiguous: after `compact` the address names a different
    /// object, and after `free` + a later `alloc` it names a brand-new one —
    /// and in both cases the stale table says "existed at remark, not marked",
    /// which is the sweep's licence to FREE it.
    ///
    /// This is the generational-heap analogue of G1's
    /// `recycled_in_generation` stamp (defect G1-8): a bare address is not an
    /// identity, an address plus an epoch is. Bumped by [`Self::free`] and by
    /// [`Self::compact`]; read via [`Self::reclaim_epoch`].
    reclaim_epoch: u64,
    /// One past the highest offset any allocation has ever covered.
    ///
    /// PERF (gengc-round1, 2026-09-20). [`Self::compact`]'s Phase 4 used to
    /// `write_bytes(base + compacted_end, 0, capacity - compacted_end)` —
    /// every byte of the trailing free block, on every major GC, inside the
    /// pause. On a 512 MiB old generation holding a 20 MiB live set that is
    /// ~490 MiB of `memset` per compaction, and it also *undoes* the reason
    /// [`Self::new`] skips the eager zero pass: the pages above the high-water
    /// mark have never been touched, so zeroing them is what commits them.
    ///
    /// Bytes at or above this mark have not been written since construction:
    /// `alloc_impl` is the only site that hands storage out, and `compact`'s
    /// slide only ever moves objects DOWN. So zeroing `[compacted_end,
    /// high_water)` is observationally identical to zeroing the whole tail,
    /// and it is also exactly the region the zero exists for — the H2-CID0
    /// diagnostic wants a stale reference to a compacted-away object to read
    /// an all-zero header, and such an object necessarily lived below this
    /// mark.
    high_water: usize,
    /// Largest single request served since the last
    /// [`Self::repair_fragmentation`], maintained at the one site that hands
    /// storage out.
    ///
    /// gengc-round2. `free_bytes_and_largest` has always exposed the pair that
    /// distinguishes "full" from "fragmented", and nothing consumed it as a
    /// policy input because the pair alone cannot: "the largest hole is 900
    /// bytes" is good news or bad news depending on what is about to be asked
    /// for. The allocator is the only component that sees every size, so it is
    /// the only place the third number can come from. See
    /// [`Self::fragmentation_warrants_repair`].
    recent_max_request: usize,
    /// How many times [`Self::repair_fragmentation`] merged something.
    /// Per-instance rather than a process-global counter, so a process with
    /// more than one heap reports per heap.
    fragmentation_repairs: u64,
    /// GCAUD-4 follow-up (gengc-round2, 2026-09-20) — debug-only registry of
    /// the unzeroed BUFFER carves that [`Self::release_unused_tail`] is
    /// allowed to hand a tail back from.
    ///
    /// `release_unused_tail` skips the `reclaim_epoch` bump, and the argument
    /// for that is "the span never held an object". That argument is a claim
    /// about this file's whole call graph and nothing checked it — the only
    /// things verified were size, alignment and range. Its blast radius is the
    /// worst one here: if it is ever false, an in-flight concurrent old-gen
    /// sweep frees a live block and the symptom surfaces later as a stale
    /// reference reading an all-zero `ClassId(0)` header (the H2-CID0 shape).
    ///
    /// The registry makes the claim checkable by recording INTENT. Only
    /// [`Self::alloc_unzeroed_buffer`] registers, and it is the one call the
    /// parallel evacuator uses for a promotion buffer; [`Self::alloc_unzeroed`]
    /// — the call `promote_alloc` uses when an object gets a block of its own —
    /// registers nothing. So the two failure modes the gap page names both
    /// become detectable in any debug build:
    ///
    /// * a future caller that uses `alloc_unzeroed` for something else and
    ///   releases a tail that WAS written has no registry entry, and
    /// * routing part of `promote_alloc`'s "object on its own block" fallback
    ///   through `release_unused_tail` likewise has none.
    ///
    /// An entry is retired when its tail is released. A buffer consumed to the
    /// byte never releases a tail, so its entry leaks; the list is capped and
    /// evicted FIFO, which makes this a TRIPWIRE rather than a proof — an
    /// evicted entry can only produce a false negative, never a false alarm.
    ///
    /// `#[cfg(debug_assertions)]`, so release builds pay nothing and the
    /// struct does not carry the field at all.
    #[cfg(debug_assertions)]
    unzeroed_buffer_carves: Vec<(usize, usize)>,
    /// gen r4/oldgen (2026-09-23) — `true` when a full
    /// [`Self::coalesce_free_blocks`] merge has run and nothing has been ADDED
    /// to the free list since, so running it again is guaranteed to merge
    /// nothing.
    ///
    /// Only [`Self::free`] and [`Self::release_unused_tail`] add a block, and
    /// both clear this. [`Self::alloc_from_buckets`] removes a block and may
    /// push back its padding and its tail, but it cannot create adjacency on a
    /// non-overlapping list: the padding starts where the removed block
    /// started and the tail ends where it ended, and the removed block was
    /// adjacent to nothing (that is what the last merge established); the
    /// other edge of each piece touches the bytes just handed out. `compact`
    /// leaves at most one block, which is trivially maximal.
    ///
    /// On an OVERLAPPING list (a double free, see `OLD_FREE_LIST_OVERLAPS`) a
    /// split can place a piece flush against a block that overlaps live
    /// memory; skipping that merge is the conservative direction, since the
    /// merge would widen a block that already covers a live object.
    free_list_maximal: bool,
    /// gen r4/oldgen (2026-09-23) — monotone count of free-list mutations,
    /// bumped by [`Self::invalidate_sorted_free`], which every bucket mutation
    /// already calls. An object grid from [`Self::walk_objects`] is exact for
    /// as long as this is unchanged (the walk derives allocated extents from
    /// the free list and object sizes from headers the collector does not
    /// rewrite); [`Self::compact_with_walked_objects`] uses it to refuse a
    /// stale grid. `Cell` because `invalidate_sorted_free` takes `&self`.
    free_list_seq: Cell<u64>,
    /// gen r4w2/oldgen2 (2026-09-23) — every offset below this is backed by
    /// COMMITTED memory (unless [`Self::readable_holes`]), so a read there
    /// cannot fault. [`Self::contains`] answers against this rather than the
    /// capacity.
    ///
    /// The invariant that makes a single number enough: on the reserving
    /// store, [`Self::alloc_from_buckets`] commits `[block.offset,
    /// alloc_end)` before it hands anything out, and every free block starts
    /// at or below `high_water` (the initial block starts at 0; a split's tail
    /// starts at the end of what was just handed out; `free`,
    /// `release_unused_tail` and `coalesce_free_blocks` only ever produce
    /// blocks inside or merging already-allocated space; `compact` leaves one
    /// block at the new high-water). So each hand-out commits from a point at
    /// or below the old high-water mark up to the new one, and the committed
    /// set is always a superset of `[0, max high_water ever)` — and, by
    /// [`READ_SLACK`], of a header's width past it. Nothing is ever
    /// DEcommitted below it except through [`HeapStore::reset_range`]'s
    /// commit-limit arm and (gen r5w3/oldgen7, opt-in) the interior decommit,
    /// both of which set `readable_holes`.
    ///
    /// gen r4w4/oldgen4 (2026-09-24): a shrink
    /// ([`Self::resize_after_collection`]) decommits the trailing free run and
    /// LOWERS this to the new committed end, which can then sit below
    /// `high_water`. The invariant this number carries is therefore stated
    /// without `high_water`: `[0, readable_end)` is committed, and every
    /// ALLOCATED byte lies below it (a shrink keeps everything below the
    /// trailing free run; a hand-out commits from `min(block.offset,
    /// readable_end)` to its end before moving it).
    ///
    /// Equal to the capacity on the wholly-committed [`HeapStore::Owned`] arm,
    /// so `contains` is exactly the old range test there.
    ///
    /// gen r5w3/oldgen7: the opt-in top placement of humongous arrays
    /// ([`Self::alloc_from_top`]) commits only under the array, so it can raise
    /// this above an uncommitted gap; it re-derives `readable_holes` when it
    /// does, which is what keeps "committed below `readable_end` unless
    /// `readable_holes`" true.
    readable_end: usize,
    /// gen r4w2/oldgen2 — a give-back left an uncommitted run below
    /// `readable_end` (the Windows arm of `reset_range` released pages and the
    /// OS then refused to commit them again). From then on `contains`
    /// consults the per-granule commit bitmap instead of the one number.
    /// Expected never to be set outside a machine at its commit limit.
    ///
    /// gen r5w3/oldgen7: also set by the opt-in interior decommit
    /// ([`Self::decommit_interior_free_runs`]), which makes it routine; and no
    /// longer a one-way latch — the allocator re-derives it whenever it
    /// commits below the committed end while it is set
    /// ([`Self::refresh_readable_holes`]), so it means exactly "some granule
    /// below `readable_end` is uncommitted".
    readable_holes: bool,
    /// gen r4w2/oldgen2 — the trigger instrument; see [`OldGenTriggerStats`].
    trigger_stats: OldGenTriggerStats,
    /// gen r4w3/oldgen3 — the verdict of the last counted trigger decision
    /// ([`Self::major_trigger`]) not yet consumed by a collection's end, so
    /// [`Self::note_collection_end`] can attribute what a collection reclaimed
    /// to the reason it ran (`would_suppress_freed_bytes`).
    pending_trigger_verdict: Option<MajorTrigger>,
    /// gen r4w3/oldgen3 — `trigger_stats.collections` when the last in-place
    /// give-back pass ran; the pass's hysteresis. See
    /// [`Self::after_in_place_sweep`].
    last_give_back_at: Option<u64>,
    /// gen r4w2/oldgen2 — `used()` when the last old-gen collection finished,
    /// `None` before the first. The hysteresis term of [`Self::major_trigger`]
    /// is "how much has been promoted SINCE then".
    used_after_last_collection: Option<usize>,
    /// gen r4w2/oldgen2 — `trigger_stats.alloc_failures` when the last old-gen
    /// collection finished. A larger current count means an allocation has
    /// failed since, which bypasses the hysteresis.
    alloc_failures_at_last_collection: u64,
    /// gen r4w2/oldgen2 — see [`Self::bytes_given_back`].
    bytes_given_back: u64,
    /// gen r4w3/cards3 — the block-offset table behind
    /// [`Self::walk_card_ranges`] (`old_gen/bot.rs`). Maintained at three
    /// hook points only: `alloc_from_buckets_scan` (anchor the new block's
    /// cards), `free` (lower the trusted prefix) and `compact_walked`
    /// (discard it). `RefCell` because the young pause reaches the walk
    /// through `&OldGen` and extends the trusted prefix lazily, like
    /// `sorted_free_cache`.
    bot: RefCell<bot::BlockOffsetTable>,
    // --- gen r4w4/oldgen4 (2026-09-24) ------------------------------------
    /// The shrink floor: bytes [`Self::commit_initial_prefix`] committed for
    /// `-Xms`. [`Self::resize_after_collection`] never decommits below it.
    commit_floor: usize,
    /// `readable_end` when the last stop-the-world old-gen collection ended
    /// (`None` before the first): the shrink's damping. A committed end that
    /// has advanced since means the generation needed MORE memory during the
    /// interval, and shrinking now would only be re-committed by the next one.
    readable_end_at_last_resize: Option<usize>,
    /// gen r4w6/oldpin6 — young pauses left in which to resize after a
    /// concurrent cycle's sweep ended (0: none due). Set by
    /// [`Self::note_concurrent_collection_end`], cleared by any
    /// [`Self::resize_after_collection`]. See
    /// [`Self::resize_after_concurrent_sweep_if_due`].
    concurrent_resize_attempts: u8,
    /// gen r5w6/old10 — the attempts the LAST post-concurrent resize left in
    /// its episode (whether or not it kept one), so the interior half can keep
    /// an attempt when IT was damped
    /// ([`Self::keep_concurrent_resize_attempt`]). Zeroed by every
    /// [`Self::resize_after_collection`] that is not a post-concurrent one.
    concurrent_resize_left: u8,
    /// Largest allocation refused for FRAGMENTATION (see
    /// [`Self::note_refused_request`]) not yet handed to the compaction
    /// trigger; `0` when none is pending.
    frag_pending_request: usize,
    /// The trigger asked the next old-gen collection to COMPACT; consumed by
    /// [`Self::take_fragmentation_compaction_request`].
    frag_compaction_requested: bool,
    /// gen r5w1/oldgen5 — the pending compaction request came (at least in
    /// part) from a refused HUMONGOUS allocation
    /// ([`Self::note_humongous_refusal`]). Cleared with the request.
    humongous_refusal_pending: bool,
    /// gen r5w2/alloc6 — the largest refused humongous request (its reserved
    /// extent) behind [`Self::humongous_refusal_pending`], so the collection
    /// that answers it can say whether it made room
    /// ([`Self::humongous_refused_request`]). Cleared with the request.
    humongous_refused_bytes: usize,
    /// This wave's counters — see [`OldGenSizingStats`].
    sizing: OldGenSizingStats,
    /// How many objects the previous full walk yielded, to size the next
    /// walk's `Vec` in one allocation instead of ~log2(n) regrowth copies of a
    /// buffer that can reach the generation's object count.
    last_walk_len: Cell<usize>,
    // --- gen r5w3/oldgen7 (2026-09-26) ----------------------------------------
    /// [`Self::committed_bytes`] when the last interior-decommit pass
    /// ([`Self::decommit_interior_free_runs`]) ended, `None` before the first:
    /// that pass's damping, the interior twin of `readable_end_at_last_resize`.
    /// A committed size that has GROWN since means the generation needed the
    /// memory during the interval (a hole it re-committed counts: holes lie
    /// below `readable_end`, so the tail damping alone cannot see them).
    interior_committed_at_last: Option<usize>,
    /// The largest capacity [`Self::grow_after_refusal`] may grow the
    /// generation to — the RESERVATION's usable extent when the generation was
    /// built with headroom ([`Self::with_growth_headroom`],
    /// `CRATONVM_GC_OLD_BORROW_YOUNG`), else the capacity itself (no growth).
    growth_max: usize,
    /// The largest request (rounded as `alloc` rounds it) this generation
    /// REFUSED since the last [`Self::grow_after_refusal`]; `0` when none.
    /// Optional promotion-buffer carves do not count
    /// ([`Self::alloc_unzeroed_buffer`] restores it, as it restores the other
    /// refusal instruments).
    refused_since_growth_check: usize,
}

/// How many outstanding unzeroed-buffer carves [`OldGen`] remembers in a debug
/// build. One live buffer per GC worker is the real number; the slack absorbs
/// entries left behind by a buffer that was consumed exactly to its end and so
/// never released a tail.
#[cfg(debug_assertions)]
const UNZEROED_CARVE_REGISTRY_CAP: usize = 1024;

/// gen r4w2/oldgen2 (2026-09-23) — how far past [`OldGen::readable_end`] the
/// reserving store keeps memory committed.
///
/// [`OldGen::contains`] admits `p` when `p < readable_end`, and many of its
/// callers go on to read a whole HEADER at `p` (the mark's plausibility
/// screens, fed by conservative guesses). Without slack, a guess in the last
/// `HEADER_SIZE - 1` bytes below a granule-aligned `readable_end` would pass
/// `contains` and then read into the next, uncommitted granule. Committing
/// `HEADER_SIZE` bytes further makes every header read at a contained address
/// safe; it costs at most one extra granule, only when an allocation ends
/// within 16 bytes of a granule boundary.
const READ_SLACK: usize = HEADER_SIZE;

/// gen r4w4/oldgen4 (2026-09-24) — HotSpot's `MaxHeapFreeRatio` default: after
/// a stop-the-world old-gen collection the COMMITTED generation is shrunk
/// toward `used * 100 / (100 - OLD_MAX_FREE_PERCENT)` (so at most 70 % of it
/// is free), never below the `-Xms` prefix and never below the last allocated
/// byte. See [`OldGen::resize_after_collection`]. **HotSpot's value, not
/// tuned here.**
pub const OLD_MAX_FREE_PERCENT: usize = 70;

/// gen r4w4/oldgen4 — a shrink is only worth its syscalls (and the re-fault
/// cost if the generation grows back) when it releases at least this many
/// whole granules. The same threshold as the in-place give-back, for the same
/// reason ([`GIVE_BACK_MIN_GRANULES`]).
pub const SHRINK_MIN_GRANULES: usize = GIVE_BACK_MIN_GRANULES;

/// gen r4w4/oldgen4 (2026-09-24) — per-generation counters for the sizing,
/// fragmentation-trigger, walk-gap and allocator-bound changes of this wave.
///
/// Kept apart from [`OldGenTriggerStats`] on purpose: that struct is the
/// old-gen TRIGGER's instrument (and another lane's this wave); these describe
/// how the generation's committed size, free list and object walk behaved.
/// Per-instance, not a process global. Read with [`OldGen::sizing_stats`] (or
/// `GenerationalHeap::old_gen_sizing_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OldGenSizingStats {
    /// The reservation: the generation's maximum size (its share of `-Xmx`).
    pub reserved_bytes: u64,
    /// Bytes the OS currently backs ([`OldGen::committed_bytes`]) — HotSpot's
    /// "committed" for the tenured pool — and its run maximum.
    pub committed_bytes: u64,
    pub committed_peak: u64,
    /// The shrink floor: the prefix `-Xms` committed at startup, which a
    /// shrink never goes below (HotSpot never shrinks a heap below `-Xms`).
    pub commit_floor: u64,
    /// Shrinks that ran, and the bytes they decommitted.
    pub shrinks: u64,
    pub shrunk_bytes: u64,
    /// Collections at which a shrink was due but DAMPED because the committed
    /// end had advanced since the previous collection (steady promotion would
    /// only re-commit what a shrink released).
    pub shrinks_damped: u64,
    /// Allocations refused while the free list held the request in total —
    /// "there are bytes but no room" — and the largest such request still
    /// waiting for a trigger.
    pub fragmentation_refusals: u64,
    pub fragmentation_pending_request: u64,
    /// Fragmentation compactions the trigger asked for, and compactions that
    /// completed (any cause; an abandoned or downgraded one does not count).
    pub fragmentation_compactions_requested: u64,
    pub compactions: u64,
    /// ...of the requests, those the allocation-failure ladder made on its way
    /// to `OutOfMemoryError` ([`OldGen::request_fragmentation_compaction`]).
    pub oom_compaction_requests: u64,
    /// In-place sweeps that RECOVERED from an incomplete object walk by
    /// scanning the unwalked gaps conservatively (instead of freeing nothing),
    /// and the unwalked bytes the last one covered.
    pub walk_gap_recoveries: u64,
    pub walk_gap_bytes_last: u64,
    /// Compacting collections taken to the in-place arm because the walk had
    /// a recoverable gap (the compactor cannot slide over unwalked bytes).
    pub walk_gap_compact_downgrades: u64,
    /// Best-fit scans that deferred a class after [`NONFIT_PROBE_LIMIT`]
    /// too-small blocks, and deferred classes that had to be finished anyway
    /// because no larger class fit.
    pub nonfit_deferrals: u64,
    pub nonfit_deferral_resumes: u64,
    // --- gen r4w5/oldcompact5 (2026-09-24) -----------------------------------
    /// Compactions around pinned objects ([`OldGen::compact_around_pins`], the
    /// non-moving path's answer to a fragmentation request under
    /// `CRATONVM_GC_OLD_PINNED_COMPACT`) that completed. Also counted in
    /// [`Self::compactions`].
    pub pinned_compactions: u64,
    /// Requests the pinned compaction declined (gate, walk gap, escape); the
    /// collection then reclaimed in place, as before this wave.
    pub pinned_compaction_refusals: u64,
    /// The last completed pinned compaction's pinned objects, and the bytes
    /// every pinned compaction has moved.
    pub pinned_compaction_pins_last: u64,
    pub pinned_compaction_moved_bytes: u64,
    // --- gen r4w6/oldpin6 (2026-09-24) ----------------------------------------
    /// Resizes run at a YOUNG pause because a concurrent cycle's sweep had
    /// ended since the last resize ([`OldGen::resize_after_concurrent_sweep_if_due`]);
    /// their shrinks are also counted in [`Self::shrinks`] / [`Self::shrunk_bytes`].
    pub concurrent_sweep_resizes: u64,
    /// Of those, the ones that shrank.
    pub concurrent_sweep_shrinks: u64,
    // --- gen r5w1/oldgen5 (2026-09-26) ----------------------------------------
    /// Compaction requests armed by a refused HUMONGOUS allocation
    /// ([`OldGen::note_humongous_refusal`]); also counted in
    /// [`Self::fragmentation_compactions_requested`].
    pub humongous_compaction_requests: u64,
    /// Of the non-moving path's answers to such a request, the compaction
    /// PLANS built by DEFAULT (no `CRATONVM_GC_OLD_PINNED_COMPACT`) because
    /// the pause had no conservative root to pin. Each then either completed
    /// ([`Self::pinned_compactions`]) or was refused
    /// ([`Self::pinned_compaction_refusals`]).
    pub humongous_default_compactions: u64,
    // --- gen r5w3/oldgen7 (2026-09-26) ----------------------------------------
    /// Interior-decommit passes that released something
    /// ([`OldGen::decommit_interior_free_runs`], `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`),
    /// and the bytes they released in total. The bytes are also added to
    /// `OldGen::bytes_given_back`; they are NOT counted in [`Self::shrinks`] /
    /// [`Self::shrunk_bytes`], which are the tail shrink's.
    pub interior_decommits: u64,
    pub interior_decommitted_bytes: u64,
    /// Interior-decommit passes skipped because the committed size had grown
    /// since the previous pass (the damping).
    pub interior_decommits_damped: u64,
    /// Allocations that, while holes existed below the committed end (an
    /// interior decommit, or the Windows commit-limit arm), committed at least
    /// one granule by a commit starting below that end — i.e. (at most) the
    /// re-commits of released interior granules. The cost side of the
    /// interior decommit's A/B.
    pub hole_recommits: u64,
    /// Capacity growths into the reservation's headroom after a refused
    /// allocation ([`OldGen::grow_after_refusal`], `CRATONVM_GC_OLD_BORROW_YOUNG`),
    /// and the bytes the generation grew by in total.
    pub borrow_growths: u64,
    pub borrowed_bytes: u64,
    /// The largest capacity the generation may grow to (equal to
    /// [`Self::reserved_bytes`] when it was built without headroom).
    pub growth_max_bytes: u64,
    /// Humongous arrays placed from the TOP of the generation
    /// ([`OldGen::alloc_from_top`], `CRATONVM_GC_OLD_HUMONGOUS_TOP`), and the
    /// requests that fell back to the ordinary best fit (no block could hold
    /// them carved from its end, or the store cannot leave holes).
    pub humongous_top_allocs: u64,
    pub humongous_top_fallbacks: u64,
    // --- gen r5w5/old9 (2026-09-27) -------------------------------------------
    /// Stop-the-world old-gen collections answered by the O(live) sweep
    /// ([`OldGen::sweep_dead_runs_around`], `CRATONVM_GC_OLD_LIVE_SWEEP`),
    /// and attempts it abandoned to the walked path (an unanswerable oracle
    /// query, an inconsistent live grid, a verify-mode disagreement).
    pub live_sweeps: u64,
    pub live_sweep_fallbacks: u64,
    /// Dead runs the live sweeps freed, and their bytes, in total.
    pub live_sweep_dead_runs: u64,
    pub live_sweep_freed_bytes: u64,
    /// Objects the last live sweep kept.
    pub live_sweep_kept_last: u64,
    /// Walk breaks planted by `CRATONVM_DBG_OLD_PLANT_WALK_BREAK`
    /// ([`OldGen::plant_walk_break`], debug; at most one per generation).
    pub walk_breaks_planted: u64,
    // --- gcd d4/n (2026-09-28) -------------------------------------------------
    /// Compactions a MOVING young cycle's major would have run (a pending
    /// request, or `CRATONVM_OLDGEN_COMPACT`) that the pause VETOED because
    /// compiled frames were live (`CRATONVM_GC_MOVING_MAJOR_JIT_GUARD`) or the
    /// young pin ledger licensed the cycle
    /// ([`OldGen::note_moving_major_compaction_veto`]); the major reclaimed in
    /// place instead.
    pub moving_major_compaction_vetoes: u64,
    /// Requested majors whose old mark seeded young from the TRUE roots
    /// (`gen_heap::TrueRootYoung`, `CRATONVM_GC_FULL_GC_TRUE_ROOTS`), the ones
    /// that kept the legacy seed (a promotion this cycle, an old walk gap, a
    /// planned pinned compaction; gcd d5/r: each fallback says which on a
    /// rate-limited `info` line, target `cratonvm::gc`), and the young
    /// survivors the true-root seed left out, summed (the objects the young
    /// phase kept only for dead old data).
    pub true_root_majors: u64,
    pub true_root_fallbacks: u64,
    pub true_root_young_excluded: u64,
    // --- gcd d9/a (2026-09-28) -------------------------------------------------
    /// `true_root_fallbacks` split by WHY, indexed by
    /// [`TrueRootFallback::index`] (`oldsz_true_root_fb_<label>`). A fallback
    /// counted through the reason-less [`OldGen::note_true_root_major`]`(None)`
    /// is in the total only.
    pub true_root_fallback_reasons: [u64; TrueRootFallback::COUNT],
    /// Promotions of this pause's young phase that a true-root major resolved
    /// (`source -> destination`) instead of seeding every destination as a
    /// root, summed; and those whose destination the old mark did not reach,
    /// which the collection freed and dropped from the pointer map it returns.
    pub true_root_promotions: u64,
    pub true_root_promotions_dead: u64,
    /// Young bytes a true-root major reclaimed in its own pause. Always 0
    /// since gce e2/c removed step 1 (`CRATONVM_GC_FULL_GC_TRUE_ROOTS_FREE_YOUNG`);
    /// kept so the `[GC] oldgen_sizing:` line (`oldsz_true_root_young_freed_bytes`)
    /// keeps its format until its owner drops the key.
    pub true_root_young_freed_bytes: u64,
}

impl OldGenSizingStats {
    /// gcd d9/a — [`Self::true_root_fallback_reasons`] for one reason.
    pub fn true_root_fallbacks_for(&self, reason: TrueRootFallback) -> u64 {
        self.true_root_fallback_reasons[reason.index()]
    }
}

/// gcd d9/a — why a REQUESTED Generational major kept the legacy young seed
/// (every young survivor, and every promotion destination of the pause, a
/// root of the old mark) instead of the true-root seed
/// (`gen_heap::TrueRootYoung`, `CRATONVM_GC_FULL_GC_TRUE_ROOTS`). Counted per
/// reason in [`OldGenSizingStats::true_root_fallback_reasons`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrueRootFallback {
    /// SAFETY fallback: the old-gen object walk has gaps. The true-root trace
    /// needs every old object on the grid (an unwalked old holder's young
    /// targets could not be found by the fixed point).
    WalkGap,
    /// SAFETY fallback: a promotion destination of this pause is not an object
    /// base of the old-gen walk, so its source extent cannot be resolved.
    PromotionOffGrid,
    /// The young phase promoted objects and
    /// `CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0` keeps the gcd d4/n..d8 rule
    /// (a promoting cycle forfeits the true-root seed).
    Promoted,
    /// A pinned compaction is planned and `..._WIDE=0` keeps the old rule.
    PinnedPlan,
    /// The requested major ran as a MOVING young cycle's Phase 5 major
    /// (`CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG`): that path has no true-root seed.
    MovingCycle,
}

impl TrueRootFallback {
    /// Number of reasons (the length of the counter array).
    pub const COUNT: usize = 5;
    /// Every reason, in counter order.
    pub const ALL: [Self; Self::COUNT] = [
        Self::WalkGap,
        Self::PromotionOffGrid,
        Self::Promoted,
        Self::PinnedPlan,
        Self::MovingCycle,
    ];

    /// The counter slot.
    pub fn index(self) -> usize {
        match self {
            Self::WalkGap => 0,
            Self::PromotionOffGrid => 1,
            Self::Promoted => 2,
            Self::PinnedPlan => 3,
            Self::MovingCycle => 4,
        }
    }

    /// The `oldsz_true_root_fb_<label>` suffix.
    pub fn label(self) -> &'static str {
        match self {
            Self::WalkGap => "walk_gap",
            Self::PromotionOffGrid => "promotion_off_grid",
            Self::Promoted => "promoted",
            Self::PinnedPlan => "pinned_plan",
            Self::MovingCycle => "moving_cycle",
        }
    }

    /// Why, for the rate-limited `info` line.
    pub fn why(self) -> &'static str {
        match self {
            Self::WalkGap => "the old-gen walk has gaps",
            Self::PromotionOffGrid => {
                "a promotion destination is not an object base of the old-gen walk"
            }
            Self::Promoted => {
                "the young phase of this pause promoted objects \
                 (CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0: their destinations are roots)"
            }
            Self::PinnedPlan => {
                "a pinned old-gen compaction is planned (CRATONVM_GC_FULL_GC_TRUE_ROOTS_WIDE=0)"
            }
            Self::MovingCycle => "the requested major ran on the moving young cycle (Phase 5)",
        }
    }
}

/// gen r4w6/oldpin6 — how many consecutive young pauses a concurrent sweep's
/// end keeps retrying a DAMPED resize (see
/// [`OldGen::resize_after_concurrent_sweep_if_due`]). The damping skips a
/// shrink when the committed end advanced since the previous resize; after a
/// concurrent cycle there may be no further old-gen collection for a long
/// time, so a single damped attempt would leave the peak committed for good.
/// Bounded, so a generation that keeps growing does not pay a free-list sort
/// on every young pause.
pub const CONCURRENT_RESIZE_ATTEMPTS: u8 = 4;

/// gen r4w4/oldgen4 — what [`OldGen::resize_after_collection`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OldGenResize {
    /// Bytes decommitted by a shrink (0 when none ran).
    pub released: usize,
    /// A shrink was due but damped (see [`OldGenSizingStats::shrinks_damped`]).
    pub damped: bool,
}

/// gen r4w4/oldgen4 — how one allocated region's object walk ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegionScan {
    /// Every object of the region was visited.
    Done,
    /// The budget ran out; the offset is the first object not visited.
    Budget(usize),
    /// The walk stopped at an implausible header at this offset; the bytes
    /// from there to the region's end were not walked.
    Break(usize),
}

/// gen r4w4/oldgen4 — what one bucket's best-fit scan found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BucketScan {
    /// The index of the best fitting block.
    Fit(usize),
    /// No block fits; `largest` is the exact maximum block size in the bucket.
    NoFit { largest: usize },
    /// [`NONFIT_PROBE_LIMIT`] too-small blocks and no fit yet: resume at
    /// `resume` if no larger class fits; `largest` covers `[0, resume)`.
    Deferred { resume: usize, largest: usize },
}

impl OldGen {
    /// Create a new old generation with the given capacity.
    pub fn new(capacity: usize) -> Self {
        // Round-11 perf: skip the eager `vec![0u8; capacity]` zero pass.
        // A 128 MiB old gen previously touched every page on construction
        // (zero-fill + page commit) before a single object was allocated.
        //
        // `alloc` is the *sole* point that hands a byte to a caller, and
        // it always `write_bytes(ptr, 0, size)` before returning — so no
        // legitimate caller can observe an uninitialised byte as data.
        // `walk_objects`/`scan_region`/`compact` only ever read headers
        // inside *allocated* regions (the gaps between free blocks); a
        // freshly-constructed `OldGen` has the whole capacity as a single
        // free block, so the walk scans nothing. `compact` zeroes any
        // freed tail it creates.
        //
        // We still need `data.len() == capacity` because pointers are
        // computed as `data.as_mut_ptr().add(offset)` and `capacity()`
        // relies on `data.len()`.
        //
        // gen r4w2/oldgen2 (2026-09-23): the store RESERVES the capacity and
        // commits nothing yet (see the `data` field doc). This replaces a
        // `Vec::with_capacity` + `set_len`, which skipped the zero pass but
        // still took the whole commit charge up front on Windows — the one
        // arena in the generational heap that `-Xms` could not lower. The
        // fallback arm (`HeapStore::Owned`) is a zeroed block, so it is at
        // least as initialised as the `set_len` buffer it replaces.
        Self::new_on(HeapStore::new(capacity, "old-gen"), capacity)
    }

    /// The body of [`Self::new`] over an already-built store (gen r5w3/oldgen7:
    /// split out so [`Self::with_growth_headroom`] can hand it a store whose
    /// reservation extends past its usable length). The capacity is
    /// `data.len()`; `bot_capacity` (at least that) sizes the block-offset
    /// table.
    fn new_on(data: HeapStore, bot_capacity: usize) -> Self {
        let capacity = data.len();
        let bot_capacity = bot_capacity.max(capacity);
        let readable_end = match &data {
            // Wholly committed: every offset is readable, and `contains` keeps
            // its historical meaning of a bare range test.
            HeapStore::Owned(_) => data.len(),
            // Nothing committed yet.
            HeapStore::Reserved(_) => 0,
        };
        // gen r4/oldgen (2026-09-23): a `Vec<u8>` carries no alignment
        // guarantee, and two paths here disagree about what they align.
        // `alloc_from_buckets` aligns the ABSOLUTE address (it pads the block);
        // `compact`'s Phase 1 aligns the OFFSET (`(write_cursor + 7) & !7`)
        // and places survivors at `base + offset`. The two agree only when
        // `base` itself is 8-aligned — which every mainstream allocator
        // delivers for a block this size (malloc and HeapAlloc both give 16),
        // and which nothing checked. A misaligned base would make compaction
        // slide every survivor onto a misaligned address. Say so loudly in the
        // build that can.
        debug_assert!(
            capacity == 0 || (data.as_ptr() as usize) % 8 == 0,
            "OldGen backing store is not 8-aligned; compact() aligns offsets, not addresses",
        );
        let mut buckets: Vec<Bucket> = (0..NUM_BUCKETS).map(|_| Bucket::default()).collect();
        // Seed the initial block in the bucket that fits the full capacity.
        let initial = FreeBlock {
            offset: 0,
            size: capacity,
        };
        buckets[bucket_for(capacity)].push(initial);
        Self {
            data,
            buckets,
            used_bytes: 0,
            // Start dirty: the cache is empty and the first walk rebuilds it.
            sorted_free_cache: RefCell::new(Vec::new()),
            sorted_free_dirty: Cell::new(true),
            reclaim_epoch: 0,
            // Nothing has been handed out, so nothing above offset 0 has been
            // written (`new` deliberately does not zero — see above).
            high_water: 0,
            recent_max_request: 0,
            fragmentation_repairs: 0,
            #[cfg(debug_assertions)]
            unzeroed_buffer_carves: Vec::new(),
            // One block (or none, for a zero capacity): nothing to merge.
            free_list_maximal: true,
            free_list_seq: Cell::new(0),
            readable_end,
            readable_holes: false,
            trigger_stats: OldGenTriggerStats::default(),
            pending_trigger_verdict: None,
            last_give_back_at: None,
            used_after_last_collection: None,
            alloc_failures_at_last_collection: 0,
            bytes_given_back: 0,
            // gen r4w3/cards3: `CRATONVM_GC_OLD_BOT` (default on), read once.
            bot: RefCell::new(bot::BlockOffsetTable::new(bot_capacity, gc_flags().gc_old_bot)),
            // gen r4w4/oldgen4: no `-Xms` prefix yet; nothing to damp against.
            commit_floor: 0,
            readable_end_at_last_resize: None,
            concurrent_resize_attempts: 0,
            concurrent_resize_left: 0,
            frag_pending_request: 0,
            frag_compaction_requested: false,
            humongous_refusal_pending: false,
            humongous_refused_bytes: 0,
            sizing: OldGenSizingStats::default(),
            last_walk_len: Cell::new(0),
            // gen r5w3/oldgen7: no interior pass yet; no headroom (see
            // `with_growth_headroom`); nothing refused.
            interior_committed_at_last: None,
            growth_max: capacity,
            refused_since_growth_check: 0,
        }
    }

    /// gen r5w3/oldgen7 (2026-09-26) — [`Self::new`] with its reservation
    /// extended to `max_capacity` bytes, of which only `capacity` are usable
    /// until [`Self::grow_after_refusal`] grows it
    /// (`CRATONVM_GC_OLD_BORROW_YOUNG`,
    /// `gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget`).
    ///
    /// The growth is IN PLACE: the reserving store raises its usable length
    /// inside the reservation it already holds (`HeapStore::grow_to`), so the
    /// base — every old-gen address — never moves. That is only possible on
    /// the reserving store; on the wholly committed fallback (a capacity
    /// below one granule, `CRATONVM_GC_RESERVE=0`, a refused reservation)
    /// growing would reallocate, so this is exactly [`Self::new`] there and
    /// the generation never grows.
    ///
    /// The block-offset table is sized for `max_capacity` from the start (its
    /// invariant constrains only allocated cards, and grown space is free), so
    /// a growth needs nothing from it. The CARD TABLE must be sized for
    /// `max_capacity` by the caller
    /// (`GenerationalHeap::with_sizes_max_young_and_old_max` sizes it by
    /// [`Self::growth_max`]): the barrier indexes it by address.
    pub fn with_growth_headroom(capacity: usize, max_capacity: usize) -> Self {
        if max_capacity <= capacity {
            return Self::new(capacity);
        }
        let data = HeapStore::with_headroom(capacity, max_capacity, "old-gen");
        if !matches!(data, HeapStore::Reserved(_)) {
            drop(data);
            return Self::new(capacity);
        }
        let mut og = Self::new_on(data, max_capacity);
        og.growth_max = max_capacity;
        og
    }

    /// gen r4w2/oldgen2 — the backing store's per-granule commit bitmap, for
    /// the generational heap's lock-free conservative-root screen
    /// (`GenerationalHeap::commit_bits`, slot 2). `None` on the wholly
    /// committed arm, which needs no screen.
    pub fn commit_bits(&self) -> Option<std::sync::Arc<[std::sync::atomic::AtomicU64]>> {
        self.data.commit_bits()
    }

    /// gen r4w2/oldgen2 — bytes of this generation the OS has actually been
    /// asked to back. Equal to [`Self::capacity`] on the wholly committed arm;
    /// on the reserving store it grows in [`GRANULE`] steps as allocation
    /// reaches new pages. `GenerationalHeap::os_committed_bytes` reports it.
    pub fn committed_bytes(&self) -> usize {
        self.data.committed_bytes()
    }

    /// gen r4w2/oldgen2 — `-Xms`: commit the first `bytes` of the generation
    /// now (clamped to the capacity), and report how many bytes of it are
    /// committed afterwards. A refusal commits nothing further and is not an
    /// error: `-Xms` is a startup hint, and the allocation path commits on
    /// demand anyway (and turns a refusal THERE into an allocation failure).
    ///
    /// Raises [`Self::readable_end`]: committed bytes are readable whether or
    /// not an object has been placed in them yet (they read as zero).
    pub fn commit_initial_prefix(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.data.len());
        if n == 0 {
            return 0;
        }
        let commit_end = n.saturating_add(READ_SLACK).min(self.data.len());
        if self.data.commit_range(0, commit_end) {
            self.readable_end = self.readable_end.max(n);
            // gen r4w4/oldgen4: `-Xms` is the floor a shrink never goes below
            // (HotSpot never shrinks a heap below its initial size).
            self.commit_floor = self.commit_floor.max(n);
            n
        } else {
            0
        }
    }

    /// gen r4w2/oldgen2 — the trigger instrument (see
    /// [`OldGenTriggerStats`]).
    pub fn trigger_stats(&self) -> OldGenTriggerStats {
        self.trigger_stats
    }

    /// gen r4w2/oldgen2 — record that an old-gen collection has just finished.
    /// `used_before` is [`Self::used`] as it was when the collection started.
    ///
    /// Called by `GenerationalHeap::major_gc` and
    /// `GenerationalHeap::sweep_old_gen_non_moving`, the two callers of the
    /// shared old-gen collector, after it returns. Feeds both the instrument
    /// (`collections`, `low_yield`) and the hysteresis term of
    /// [`Self::major_trigger`].
    pub fn note_collection_end(&mut self, used_before: usize) {
        let used = self.used_bytes;
        let freed = used_before.saturating_sub(used);
        let low_yield = freed < self.capacity() / 16;
        self.trigger_stats.collections = self.trigger_stats.collections.wrapping_add(1);
        if low_yield {
            self.trigger_stats.low_yield = self.trigger_stats.low_yield.wrapping_add(1);
        }
        self.used_after_last_collection = Some(used);
        self.alloc_failures_at_last_collection = self.trigger_stats.alloc_failures;

        // gen r4w3/oldgen3 — instruments only; nothing below feeds a decision.
        let s = &mut self.trigger_stats;
        s.freed_bytes = s.freed_bytes.wrapping_add(freed as u64);
        // Attribute this collection to the verdict that ran it: a
        // `WouldSuppress` collection is one the hysteresis would have skipped,
        // so what it reclaimed is what the policy would have cost.
        if self.pending_trigger_verdict.take() == Some(MajorTrigger::WouldSuppress) {
            s.would_suppress_freed_bytes = s.would_suppress_freed_bytes.wrapping_add(freed as u64);
            if low_yield {
                s.would_suppress_low_yield = s.would_suppress_low_yield.wrapping_add(1);
            }
        }
        // The fragmentation page's per-cycle sample. O(free blocks), once per
        // old-gen collection — the collection has just coalesced them.
        let (free_bytes, largest) = self.free_bytes_and_largest();
        let blocks = self.free_block_count();
        let due = Self::fragmentation_warrants_repair(
            self.capacity(),
            free_bytes,
            largest,
            self.recent_max_request,
        );
        let s = &mut self.trigger_stats;
        s.free_blocks_after_last = blocks as u64;
        s.largest_free_after_last = largest as u64;
        if due {
            s.fragmentation_repair_due = s.fragmentation_repair_due.wrapping_add(1);
        }
    }

    /// gen r4w3/oldgen3 — count an in-place sweep that freed nothing because
    /// its object walk did not cover [`Self::used`]; returns the count BEFORE
    /// this one, so the caller can rate-limit its log line per generation.
    pub fn note_sweep_walk_gap(&mut self) -> u64 {
        let n = self.trigger_stats.sweep_walk_gap_skips;
        self.trigger_stats.sweep_walk_gap_skips = n.wrapping_add(1);
        n
    }

    /// gen r4w3/oldgen3 — [`Self::note_collection_end`] for the CONCURRENT
    /// sweep (`ConcurrentMarker::concurrent_sweep*`), which knows what it
    /// freed rather than what `used()` was when the cycle began: it runs in
    /// slices with promotions landing between them, so `used()` at its start
    /// is not a baseline. `used() + freed` is exactly the `used_before` a
    /// single-hold collection would have passed.
    ///
    /// Before this, a concurrent cycle's reclamation was invisible to the
    /// trigger: the hysteresis kept measuring growth from the last STW
    /// collection, and `collections` / `low_yield` never counted the cycle
    /// (wave-2 `oldgen2` cross-lane request).
    ///
    /// Any pending STW trigger verdict belongs to a young cycle, not to this
    /// collection, so it is dropped rather than attributed.
    pub fn note_concurrent_collection_end(&mut self, freed_bytes: usize) {
        self.pending_trigger_verdict = None;
        self.trigger_stats.concurrent_collections =
            self.trigger_stats.concurrent_collections.wrapping_add(1);
        let used_before = self.used_bytes.saturating_add(freed_bytes);
        self.note_collection_end(used_before);
        // gen r4w6/oldpin6: the committed size is resized at the next young
        // pause, not here — this runs inside a sweep slice, beside mutators.
        // See `resize_after_concurrent_sweep_if_due`.
        self.concurrent_resize_attempts = CONCURRENT_RESIZE_ATTEMPTS;
    }

    /// gen r4w2/oldgen2 — should THIS young cycle also collect the old
    /// generation? A pure function of the numbers, so the policy is testable
    /// without a heap; [`Self::major_trigger`] evaluates it against the live
    /// ones.
    ///
    /// With `hysteresis == false` this runs a collection in exactly the cases
    /// the pre-2026-09-23 expression did —
    /// `(capacity > 0 && used >= capacity * 75 / 100) || requested` — and only
    /// the REASON it reports differs ([`MajorTrigger::WouldSuppress`] instead
    /// of [`MajorTrigger::Occupancy`] where the hysteresis would have said no).
    /// That equivalence is what lets the default stay byte-for-byte unchanged;
    /// `tests::the_trigger_with_hysteresis_off_is_the_bare_occupancy_test`
    /// pins it exhaustively over a grid.
    ///
    /// With `hysteresis == true` (`CRATONVM_GC_OLD_TRIGGER_HYSTERESIS`,
    /// opt-in), the 75 % floor stays and a second condition is added, from the
    /// gap page's proposal: collect again only once
    /// `max(capacity / 16, (capacity - used_after_last) / 2)` bytes have been
    /// added since the last old-gen collection — half of what that collection
    /// left free, but never more often than every sixteenth of the generation.
    /// An allocation FAILURE since the last collection bypasses it.
    pub fn major_trigger_verdict(
        capacity: usize,
        used: usize,
        used_after_last: Option<usize>,
        failures_since_last: u64,
        requested: bool,
        hysteresis: bool,
    ) -> MajorTrigger {
        if requested {
            return MajorTrigger::Requested;
        }
        // The same expression, in the same integer arithmetic, as the two
        // trigger sites used — so the floor cannot move by a rounding step.
        if capacity == 0 || used < capacity * 75 / 100 {
            return MajorTrigger::NotDue;
        }
        let grown_enough = match used_after_last {
            None => true,
            Some(after) => {
                let grown = used.saturating_sub(after);
                let headroom = capacity.saturating_sub(after);
                grown >= (capacity / 16).max(headroom / 2)
            }
        };
        if grown_enough {
            return MajorTrigger::Occupancy;
        }
        if failures_since_last > 0 {
            return MajorTrigger::AllocationFailure;
        }
        if hysteresis {
            MajorTrigger::Suppressed
        } else {
            MajorTrigger::WouldSuppress
        }
    }

    /// [`Self::major_trigger_verdict`] against this generation's live numbers,
    /// counted into [`Self::trigger_stats`]. Call it once per trigger
    /// decision — it is the decision's record, not a query.
    pub fn major_trigger(&mut self, requested: bool, hysteresis: bool) -> MajorTrigger {
        let v = self.major_trigger_peek(requested, hysteresis);
        // gen r4w3/oldgen3: remembered for the collection this verdict runs,
        // so its end can say what a `WouldSuppress` collection reclaimed.
        self.pending_trigger_verdict = v.runs().then_some(v);
        let s = &mut self.trigger_stats;
        match v {
            MajorTrigger::NotDue => {}
            MajorTrigger::Requested => s.requested = s.requested.wrapping_add(1),
            MajorTrigger::Occupancy => s.occupancy = s.occupancy.wrapping_add(1),
            MajorTrigger::AllocationFailure => {
                s.allocation_failure = s.allocation_failure.wrapping_add(1)
            }
            MajorTrigger::Suppressed => s.suppressed = s.suppressed.wrapping_add(1),
            MajorTrigger::WouldSuppress => s.would_suppress = s.would_suppress.wrapping_add(1),
        }
        v
    }

    /// gen r4w3/oldgen3 — [`Self::major_trigger_verdict`] against the live
    /// numbers WITHOUT recording a decision: for the callers that only ask
    /// "is the old generation due?" (`GenerationalHeap::old_gen_needs_gc` —
    /// the concurrent cycle's trigger and two allocation-path GC gates), which
    /// run far more often than a collection is decided and must not inflate
    /// the per-reason counts [`Self::major_trigger`] keeps.
    pub fn major_trigger_peek(&self, requested: bool, hysteresis: bool) -> MajorTrigger {
        let failures_since = self
            .trigger_stats
            .alloc_failures
            .wrapping_sub(self.alloc_failures_at_last_collection);
        Self::major_trigger_verdict(
            self.capacity(),
            self.used_bytes,
            self.used_after_last_collection,
            failures_since,
            requested,
            hysteresis,
        )
    }

    /// gen r4w4/concmark4 — [`Self::used`] when the last old-gen collection
    /// (STW or concurrent) finished, `None` before the first: the hysteresis
    /// state, read by the concurrent-start policy
    /// (`concurrent_mark::ConcurrentStartPolicy::start_due`) so a cycle does
    /// not reopen over a live set no collection can bring below its threshold.
    #[inline]
    pub fn used_after_last_collection(&self) -> Option<usize> {
        self.used_after_last_collection
    }

    /// gen r4w4/concmark4 — old-gen allocations refused since the last
    /// old-gen collection finished (the hysteresis bypass's input, as
    /// [`Self::major_trigger_peek`] computes it). The concurrent-first policy
    /// never lets the STW collection defer to an open cycle while this is
    /// non-zero: a failure is how a full generation reaches the OOM ladder.
    #[inline]
    pub fn alloc_failures_since_last_collection(&self) -> u64 {
        self.trigger_stats
            .alloc_failures
            .wrapping_sub(self.alloc_failures_at_last_collection)
    }

    /// gen r4/oldgen — the free-list mutation count (see the field doc). Read
    /// it right after [`Self::walk_objects`] and hand both to
    /// [`Self::compact_with_walked_objects`].
    #[inline]
    pub fn free_list_seq(&self) -> u64 {
        self.free_list_seq.get()
    }

    /// GCAUD-4 — the current reclamation epoch (see the field doc).
    ///
    /// A consumer that caches anything keyed on an old-gen address across a
    /// window in which it does NOT hold the old-gen lock must record this
    /// value and re-check it before acting on the cache. An unequal value
    /// means at least one block was freed or relocated in between, so every
    /// cached address may now name a different object; the only safe response
    /// is to discard the cache.
    #[inline]
    pub fn reclaim_epoch(&self) -> u64 {
        self.reclaim_epoch
    }

    /// Mark the cached offset-sorted free-block view stale.
    ///
    /// PERF (gc-oldgen-perf): called from every site that mutates `buckets`
    /// (`alloc`, `free`, and `compact`'s free-list rebuild). Cheap — just
    /// flips a `Cell<bool>`; the actual recompute is deferred to the next
    /// walker that needs the sorted view.
    #[inline]
    fn invalidate_sorted_free(&self) {
        self.sorted_free_dirty.set(true);
        // gen r4/oldgen: every bucket mutation comes through here, so this is
        // also the one place the grid-staleness stamp can move.
        self.free_list_seq.set(self.free_list_seq.get().wrapping_add(1));
    }

    /// Run `f` with the offset-sorted free-block view, rebuilding the cached
    /// view first only if the buckets changed since it was last built.
    ///
    /// PERF (gc-oldgen-perf): replaces the per-call `buckets.iter().flatten()
    /// .collect()` + `sort_by_key` that `walk_objects` /
    /// `walk_objects_in_card_ranges` / `compact` each used to do unconditionally.
    /// The produced slice is the *exact same* ascending-offset ordering as
    /// before (same elements, same comparator), so every walk is unchanged;
    /// when nothing mutated the buckets between two walks the second one reuses
    /// the cache and skips the collect+sort entirely.
    ///
    /// The closure receives a borrowed `&[FreeBlock]`; the `RefCell` borrow is
    /// held only for the duration of `f`. Callers must not mutate the buckets
    /// (which would require `&mut self`) from inside `f`, and none do.
    fn with_sorted_free_blocks<R>(&self, f: impl FnOnce(&[FreeBlock]) -> R) -> R {
        if self.sorted_free_dirty.get() {
            let mut cache = self.sorted_free_cache.borrow_mut();
            cache.clear();
            cache.extend(self.buckets.iter().flatten().copied());
            cache.sort_by_key(|b| b.offset);
            self.sorted_free_dirty.set(false);
        }
        let cache = self.sorted_free_cache.borrow();
        f(&cache)
    }

    /// Allocate `size` bytes with the given alignment from the segregated
    /// free list.
    ///
    /// Round-5 #14: search starts at the smallest bucket guaranteed to
    /// satisfy `size` and escalates upward. Within a bucket the scan is
    /// best-fit, but bucket sizes mean the worst-case scan touches only
    /// blocks roughly the right size — not the entire free list.
    /// Amortised O(1).
    ///
    /// Round-9 gc CRIT-3 fix: a request of `size == 0` previously
    /// returned `self.data.as_mut_ptr()` (i.e. the start of the backing
    /// buffer) without reserving any bytes. The caller would treat that
    /// pointer as a fresh, non-aliasing allocation while it actually
    /// aliased whatever was already living at offset 0 — typically the
    /// header of the first old-gen object. A subsequent write through
    /// the bogus pointer corrupted live state. Round up zero-sized
    /// requests to the smallest plausible object footprint
    /// (`HEADER_SIZE`-or-larger, 8-byte aligned) so every call returns
    /// a fresh, non-aliasing block.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_impl(size, align, true)
    }

    /// [`Self::alloc`] without the zeroing pass.
    ///
    /// For a caller that overwrites every byte it is handed before anything
    /// can read the block: the parallel evacuator's promotion buffers
    /// (`gen_evac`), where the copy is the write. Zeroing there was one
    /// `memset` of every promoted byte followed by one `memcpy` over it.
    ///
    /// The caller inherits the obligation `alloc`'s zeroing discharged: no
    /// walker may reach the block before it holds objects end to end, and an
    /// unused tail must go back through [`Self::release_unused_tail`] before
    /// the next `walk_objects`.
    pub fn alloc_unzeroed(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        self.alloc_impl(size, align, false)
    }

    /// [`Self::alloc_unzeroed`] for a block the caller intends to carve
    /// several objects out of and then return the unused tail of.
    ///
    /// Behaviourally identical to `alloc_unzeroed` — same free-list search,
    /// same absent zero pass, same `# Safety` obligation. What it adds is a
    /// declaration of INTENT: this and only this call makes the resulting
    /// block's tail eligible for [`Self::release_unused_tail`], which skips
    /// the `reclaim_epoch` stamp on the argument that a buffer tail never held
    /// an object. See [`Self::unzeroed_buffer_carves`] for why that argument
    /// needed a checkable form; in a release build this is exactly
    /// `alloc_unzeroed`.
    ///
    /// # Caller obligation
    /// As [`Self::alloc_unzeroed`], plus: hand the unused remainder back
    /// through [`Self::release_unused_tail`], and do not write into it first.
    pub fn alloc_unzeroed_buffer(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        // gen r4/oldgen (2026-09-23): a promotion BUFFER is not a request the
        // fragmentation predicate should be scaled by. `recent_max_request` is
        // "the largest thing that will next ask for room", and a buffer is
        // optional — `gen_evac::promote_alloc` falls back to an object-sized
        // block when no buffer-sized one exists. Counting it pinned the scale
        // at the buffer size (up to `OLD_PLAB_MAX`, 256 KiB) on every run that
        // promotes in parallel, so `fragmentation_warrants_repair` would call
        // a generation whose largest hole is merely smaller than a buffer
        // "fragmented" — the false positive its third clause exists to
        // suppress. The objects carved out of the buffer are all smaller than
        // `OLD_PLAB_DIRECT_MIN`; the ones that are not get their own
        // `alloc_unzeroed` call, which still counts.
        let recent_max_before = self.recent_max_request;
        // gen r4w2/oldgen2: likewise a failed BUFFER carve is not an
        // allocation failure — the caller retries with an object-sized block,
        // and only that retry failing is the signal the old-gen trigger's
        // hysteresis must not sit on (see `MajorTrigger::AllocationFailure`).
        let failures_before = self.trigger_stats.alloc_failures;
        // gen r4w4/oldgen4: and for the same reason a refused BUFFER is not a
        // fragmentation signal — the object-sized retry is the real request.
        let frag_before = (self.frag_pending_request, self.sizing.fragmentation_refusals);
        // gen r5w3/oldgen7: nor is it a reason to GROW the generation
        // (`grow_after_refusal`) — only the object-sized retry is.
        let refused_before = self.refused_since_growth_check;
        let p = self.alloc_impl(size, align, false);
        self.recent_max_request = recent_max_before;
        self.trigger_stats.alloc_failures = failures_before;
        (self.frag_pending_request, self.sizing.fragmentation_refusals) = frag_before;
        self.refused_since_growth_check = refused_before;
        let p = p?;
        #[cfg(debug_assertions)]
        {
            let offset = p as usize - self.data.as_ptr() as usize;
            // `alloc_from_buckets` rounds the request the same way `free`
            // does; record the ROUNDED extent, which is what it reserved and
            // what `high_water` moved to.
            let reserved = (size.max(HEADER_SIZE.max(8)) + align - 1) & !(align - 1);
            if self.unzeroed_buffer_carves.len() >= UNZEROED_CARVE_REGISTRY_CAP {
                self.unzeroed_buffer_carves.remove(0);
            }
            self.unzeroed_buffer_carves.push((offset, reserved));
        }
        Some(p)
    }

    fn alloc_impl(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
        // gen r4w4/oldgen4: a commit refusal on either attempt below is the
        // OS's limit, not fragmentation; see `note_refused_request`.
        let commit_refusals_before = self.trigger_stats.commit_refusals;
        if let Some(p) = self.alloc_from_buckets(size, align, zero) {
            return Some(p);
        }
        // FRAGMENTATION FIX (xt-helper-window OOM, 2026-07-31): the free list
        // is size-segregated and `free` never looks at a block's neighbours,
        // because coalescing was deferred to `compact`. When compaction cannot
        // run — the old generation reclaimed IN PLACE by
        // `old_gen_gc(compact = false)` under conservative JIT roots — that
        // deferral never comes due, and the free list degenerates into one
        // isolated block per dead object: the SUM of free bytes stays large
        // while the LARGEST block shrinks toward a single object, so a modest
        // array request fails on a generation that is mostly free. Before
        // reporting failure, merge adjacent blocks once and retry. Costs
        // nothing on the success path, and turns a spurious `OutOfMemoryError`
        // into an allocation whenever the bytes are physically there.
        if self.coalesce_free_blocks() > 0 {
            let retry = self.alloc_from_buckets(size, align, zero);
            if retry.is_some() {
                return retry;
            }
        }
        // gen r4w2/oldgen2: the one place every refused old-gen allocation
        // passes through — promotion or direct — so the old-gen trigger can
        // tell "full" from "not grown enough to be worth collecting".
        self.trigger_stats.alloc_failures = self.trigger_stats.alloc_failures.wrapping_add(1);
        // gen r4w4/oldgen4: "full" or "fragmented"? The fragmentation trigger
        // (`arm_fragmentation_compaction`) needs to know which.
        let commit_refused = self.trigger_stats.commit_refusals != commit_refusals_before;
        self.note_refused_request(size, commit_refused);
        // gen r5w3/oldgen7: the input of `grow_after_refusal` (a no-op unless
        // the generation was built with headroom). Not a commit refusal: that
        // is the OS's limit, and a larger capacity would only be refused too.
        if !commit_refused {
            let request = (size.max(HEADER_SIZE.max(8)) + 7) & !7;
            self.refused_since_growth_check = self.refused_since_growth_check.max(request);
        }
        None
    }

    /// Merge adjacent (and, defensively, overlapping) free blocks into maximal
    /// runs and rebuild the size buckets. Returns how many blocks the merge
    /// eliminated — `0` means the list was already maximally coalesced and a
    /// retry cannot help.
    ///
    /// `compact` rebuilds the free list as a single trailing block and so has
    /// never needed this; it exists for the paths that reclaim old-gen storage
    /// WITHOUT compacting (`old_gen_gc(compact = false)`, reached whenever a
    /// live JIT frame's roots are conservative and therefore un-rewritable).
    ///
    /// O(n log n) in the number of free blocks. Called once per in-place
    /// old-gen sweep and once as a last-ditch step before `alloc` gives up.
    ///
    /// gen r4/oldgen (2026-09-23): the second caller made the old "never on a
    /// hot path" claim false. When old gen is full, EVERY promotion attempt
    /// fails — once per surviving object in a serial Cheney cycle, once per
    /// promotion-buffer refill (twice: buffer, then object) in a parallel one,
    /// once per candidate in the non-moving selective promotion — and each
    /// failure ran this sort under the old-gen lock to rediscover that nothing
    /// is adjacent. It now returns at once when nothing has been freed since
    /// the last full merge (`free_list_maximal`, counted in
    /// [`COALESCE_SKIPPED_MAXIMAL`]).
    pub fn coalesce_free_blocks(&mut self) -> usize {
        // gen r4/oldgen (2026-09-23): nothing was added since the last full
        // merge, so this one would re-collect and re-sort the whole free list
        // to find nothing adjacent. See `free_list_maximal`. Checked before
        // `COALESCE_CALLS` so that counter still means "merges attempted".
        if self.free_list_maximal {
            COALESCE_SKIPPED_MAXIMAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return 0;
        }
        COALESCE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if crate::gc_flags().no_oldgen_coalesce {
            return 0;
        }
        let before: usize = self.buckets.iter().map(|b| b.len()).sum();
        if before < 2 {
            self.free_list_maximal = true;
            return 0;
        }
        let mut blocks: Vec<FreeBlock> = self.buckets.iter().flatten().copied().collect();
        blocks.sort_unstable_by_key(|b| b.offset);

        // H2-CID0 (2026-08-02): the list is sorted here anyway, so one
        // comparison per block answers "did something free the same span
        // twice?". The young sweep has had `DOUBLE_FREE_SPANS` since
        // 2026-08-01; old gen had no equivalent, and a double free there
        // produces the same all-zero-header face (the allocator serves the
        // bytes twice and `alloc` zeroes them under the first owner).
        {
            let mut overlaps = 0usize;
            let mut first: Option<(usize, usize, usize)> = None;
            for w in blocks.windows(2) {
                if w[0].offset + w[0].size > w[1].offset {
                    overlaps += 1;
                    if first.is_none() {
                        first = Some((w[0].offset, w[0].size, w[1].offset));
                    }
                }
            }
            if overlaps > 0 {
                crate::gen_heap::OLD_FREE_LIST_OVERLAPS
                    .fetch_add(overlaps as u64, std::sync::atomic::Ordering::Relaxed);
                static REPORTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                if REPORTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 8 {
                    let (o, s, n) = first.unwrap_or((0, 0, 0));
                    tracing::error!(
                        target: "cratonvm::gc::guard",
                        pairs = overlaps,
                        first = format!("+{o:#x}+{s:#x} overlaps +{n:#x}"),
                        "old-gen free list contains OVERLAPPING blocks — a span was freed \
                         twice. The allocator can serve the same bytes to two objects and \
                         zero them under the first owner.",
                    );
                }
            }
        }

        let mut merged: Vec<FreeBlock> = Vec::with_capacity(blocks.len());
        for b in blocks {
            match merged.last_mut() {
                // STRICT adjacency only. The union of two touching free spans
                // is provably free, so merging them can never hand out live
                // memory. An OVERLAPPING pair is deliberately left alone: it
                // means something already double-freed or over-freed a span,
                // and silently collapsing it would both mask that bug and risk
                // widening a block over a live object. `walk_objects` derives
                // "allocated" as the GAPS between free blocks, so this
                // conservative choice also leaves the object walk unchanged.
                Some(last) if last.offset + last.size == b.offset => {
                    last.size += b.size;
                }
                _ => merged.push(b),
            }
        }

        // Either way the list is now maximal: every strictly adjacent pair has
        // been merged (or there was none).
        self.free_list_maximal = true;
        if merged.len() == before {
            // Nothing was adjacent — leave the buckets untouched so the caller
            // knows a retry is pointless.
            return 0;
        }
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        for b in &merged {
            self.buckets[bucket_for(b.size)].push(*b);
        }
        self.invalidate_sorted_free();
        let eliminated = before - merged.len();
        BLOCKS_MERGED.fetch_add(eliminated as u64, std::sync::atomic::Ordering::Relaxed);
        eliminated
    }

    /// The size-segregated bucket walk. Split out of [`Self::alloc`] so the
    /// coalesce-and-retry step can run it twice.
    fn alloc_from_buckets(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
        // Round-9 gc CRIT-3: reserve at least HEADER_SIZE bytes (and
        // never less than 8 for alignment headroom) so an `alloc(0)`
        // can't alias an existing allocation.
        let size = size.max(HEADER_SIZE.max(8));
        // Keep the reserved extent consistent with the young arena: compact
        // bodies may be non-aligned, but the next object must not start after
        // an unowned padding gap.
        let size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

        let base = self.data.as_ptr() as usize;
        let start_bucket = min_satisfying_bucket(size + align - 1);
        // gen r4w2/oldgen2: blocks walked past as too small, across the whole
        // search; folded into the stats once, on either exit.
        let mut nonfit: u64 = 0;
        let p = self.alloc_from_buckets_scan(base, start_bucket, size, align, zero, &mut nonfit);
        self.trigger_stats.nonfit_probes = self.trigger_stats.nonfit_probes.wrapping_add(nonfit);
        if p.is_some() {
            self.trigger_stats.allocs = self.trigger_stats.allocs.wrapping_add(1);
        }
        p
    }

    /// The body of [`Self::alloc_from_buckets`]: the bucket walk itself.
    /// Split out (gen r4w2/oldgen2) only so the probe count is recorded on
    /// every exit without threading it through each `return`.
    fn alloc_from_buckets_scan(
        &mut self,
        base: usize,
        start_bucket: usize,
        size: usize,
        align: usize,
        zero: bool,
        nonfit: &mut u64,
    ) -> Option<*mut u8> {
        // Walk buckets from the smallest guaranteed-fit upward.
        //
        // gen r4w4/oldgen4: then, if one class's scan was DEFERRED after
        // `NONFIT_PROBE_LIMIT` too-small blocks and no larger class fit,
        // finish that class — so the search is exhaustive exactly when it has
        // to be (a request that would otherwise fail), and bounded otherwise.
        // At most one class is deferred per request.
        let mut next_bucket = start_bucket;
        let mut deferred: Option<(usize, usize, usize)> = None;
        let mut deferral_used = false;
        loop {
            let (bucket_idx, from, largest_before) = if next_bucket < NUM_BUCKETS {
                next_bucket += 1;
                (next_bucket - 1, 0, 0)
            } else if let Some(d) = deferred.take() {
                self.sizing.nonfit_deferral_resumes =
                    self.sizing.nonfit_deferral_resumes.wrapping_add(1);
                d
            } else {
                break;
            };
            // gen r4w3/oldgen3: every block here is at most `size_bound`
            // bytes, and a fit needs `padding + size <= block.size` with
            // `padding >= 0` — so a bound below `size` proves the scan below
            // would walk the whole bucket and find nothing. Skip it and try the
            // next class, which is exactly where that scan would have ended up:
            // same placement, O(1) instead of O(bucket). See `Bucket`.
            if from == 0 && self.buckets[bucket_idx].size_bound < size {
                if !self.buckets[bucket_idx].is_empty() {
                    self.trigger_stats.bucket_skips =
                        self.trigger_stats.bucket_skips.wrapping_add(1);
                }
                continue;
            }
            // Best-fit scan within this bucket — see `best_fit_in_bucket`.
            let may_defer = !deferral_used && from == 0;
            let i = match self.best_fit_in_bucket(
                base,
                bucket_idx,
                from,
                largest_before,
                size,
                align,
                nonfit,
                may_defer,
            ) {
                BucketScan::Fit(i) => i,
                BucketScan::NoFit { largest } => {
                    // gen r4w3/oldgen3: no fit, so the scan walked every block
                    // (it only stops early on a fit, or on a deferral, whose
                    // resume carries the prefix's maximum here) and `largest`
                    // is the bucket's exact maximum. The next request of this
                    // size or larger skips the bucket in O(1) until a push
                    // raises it.
                    self.buckets[bucket_idx].tighten(largest);
                    continue;
                }
                BucketScan::Deferred { resume, largest } => {
                    deferral_used = true;
                    self.sizing.nonfit_deferrals = self.sizing.nonfit_deferrals.wrapping_add(1);
                    deferred = Some((bucket_idx, resume, largest));
                    continue;
                }
            };
            // gen r4w2/oldgen2 (2026-09-23): COMMIT BEFORE HANDING OUT. On the
            // reserving store (`HeapStore::Reserved`) the pages of a block the
            // allocator has never reached are address space only, and writing
            // into them is a fault, not a zero. Commit from the block's START
            // (not the aligned address) so the padding piece pushed back below
            // is backed too — that is what keeps `readable_end` a single
            // number (see its doc). Done before the bucket is touched, so a
            // refusal leaves the free list exactly as it was.
            //
            // Cheap when already committed (a bitmap test per granule, no
            // syscall) and a no-op on the wholly committed arm. A refusal is
            // the OS's commit limit, and it becomes the same `None` a full
            // free list gives — i.e. the same promotion failure / the same
            // `OutOfMemoryError` — counted in `commit_refusals` so a log can
            // tell the two apart.
            {
                let block = self.buckets[bucket_idx][i];
                let block_addr = base + block.offset;
                let aligned_addr = (block_addr + align - 1) & !(align - 1);
                let alloc_end = block.offset + (aligned_addr - block_addr) + size;
                if alloc_end > self.readable_end || self.readable_holes {
                    // `READ_SLACK` past the end: see its doc.
                    let commit_end = alloc_end.saturating_add(READ_SLACK).min(self.data.len());
                    // gen r4w4/oldgen4: from `readable_end` when the block starts
                    // ABOVE it. Before shrinking existed every free block started
                    // at or below the committed end, so `block.offset` sufficed;
                    // a shrink decommits the tail and lowers `readable_end`, and
                    // an un-coalesced piece of that tail may then start above
                    // it. Committing only `[block.offset, ..)` would leave
                    // `[readable_end, block.offset)` unbacked under the single
                    // number `contains` trusts. On the bitmap arm
                    // (`readable_holes`) the number is not trusted, so the
                    // block's own range is enough.
                    let commit_from = if self.readable_holes {
                        block.offset
                    } else {
                        block.offset.min(self.readable_end)
                    };
                    // gen r5w3/oldgen7: what the hole bookkeeping below needs.
                    let holes_before = self.readable_holes;
                    let readable_end_before = self.readable_end;
                    let committed_before = self.data.committed_bytes();
                    if !self
                        .data
                        .commit_range(commit_from, commit_end - commit_from)
                    {
                        self.note_commit_refusal(commit_from, commit_end - commit_from);
                        return None;
                    }
                    self.readable_end = self.readable_end.max(alloc_end);
                    // gen r4w4/oldgen4: the committed end only grows here.
                    let committed = self.data.committed_bytes() as u64;
                    if committed > self.sizing.committed_peak {
                        self.sizing.committed_peak = committed;
                    }
                    // gen r5w3/oldgen7: with holes below the committed end
                    // (an interior decommit, or the Windows commit-limit arm),
                    // a commit that started below it may have refilled one.
                    // Count it (the interior decommit's cost side) and recount
                    // the holes, so `contains` drops back to the one-number
                    // test once every hole is refilled instead of consulting
                    // the bitmap for the rest of the run. Only when something
                    // was committed, so the steady state pays one comparison.
                    if holes_before
                        && commit_from < readable_end_before
                        && committed as usize > committed_before
                    {
                        self.sizing.hole_recommits = self.sizing.hole_recommits.wrapping_add(1);
                        self.refresh_readable_holes();
                    }
                }
            }
            // swap_remove keeps the bucket O(1).
            let block = self.buckets[bucket_idx].swap_remove(i);
            // PERF (gc-oldgen-perf): buckets change here (and via the pushes
            // below) — invalidate the cached sorted free-block view once.
            self.invalidate_sorted_free();
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            let alloc_offset = block.offset + padding;

            // Re-insert any leftover padding into its correct bucket.
            if padding > 0 {
                let pad_block = FreeBlock {
                    offset: block.offset,
                    size: padding,
                };
                self.buckets[bucket_for(padding)].push(pad_block);
            }
            // Re-insert the tail leftover (after `padding + size`) into
            // its correct bucket.
            let remaining = block.size - padding - size;
            if remaining > 0 {
                let tail_block = FreeBlock {
                    offset: alloc_offset + size,
                    size: remaining,
                };
                self.buckets[bucket_for(remaining)].push(tail_block);
            }

            self.used_bytes += size;
            // gengc-round1: this is the ONLY site that hands old-gen storage
            // to a writer, so it is the only place the high-water mark can
            // move. See the field's doc for what `compact` does with it.
            self.high_water = self.high_water.max(alloc_offset + size);
            // gen r4w3/cards3: the one hand-out site, so the one place the
            // block-offset table learns a new object start (see `bot.rs`).
            self.bot.get_mut().note_alloc(alloc_offset, size);
            // gengc-round2: and it is likewise the only place that sees every
            // request size, which is the third number the fragmentation
            // predicate needs. One `max` on an already-hot local.
            self.recent_max_request = self.recent_max_request.max(size);
            // SAFETY: alloc_offset is within [0, self.data.len()) and size bytes fit
            // within the selected free block bounds.
            let ptr = unsafe { self.data.as_mut_ptr().add(alloc_offset) };
            // Round-5 #14 — sole zeroing point. The previous code zeroed
            // both here AND in `free`; the free-side zero was redundant
            // because every byte handed back to a caller flows through
            // this path.
            // SAFETY: ptr points to alloc_offset within the data buffer with at
            // least size bytes available. Zero-initializing the allocated region.
            if zero {
                unsafe {
                    std::ptr::write_bytes(ptr, 0, size);
                }
            }
            return Some(ptr);
        }

        None
    }

    /// Best-fit scan of one bucket, from block index `from` (0, or a deferred
    /// scan's resume point). `largest_before` is the maximum block size in
    /// `[0, from)`, so a [`BucketScan::NoFit`] always reports the bucket's
    /// exact maximum.
    ///
    /// gen r4w4/oldgen4: split out of [`Self::alloc_from_buckets_scan`]
    /// unchanged apart from the deferral, which only a first scan
    /// (`may_defer`) can take: after [`NONFIT_PROBE_LIMIT`] too-small blocks
    /// with no fit seen, it stops and lets the caller try the larger classes
    /// first (see the constant).
    #[allow(clippy::too_many_arguments)]
    fn best_fit_in_bucket(
        &self,
        base: usize,
        bucket_idx: usize,
        from: usize,
        largest_before: usize,
        size: usize,
        align: usize,
        nonfit: &mut u64,
        may_defer: bool,
    ) -> BucketScan {
        let bucket = &self.buckets[bucket_idx];
        let len = bucket.len();
        let mut best: Option<usize> = None;
        let mut best_waste: usize = usize::MAX;
        let mut fits_seen: usize = 0;
        let mut nonfit_here: usize = 0;
        // gen r4w3/oldgen3: the largest block walked, which is the exact
        // bucket maximum whenever the scan finds no fit (it then never stops
        // early, so it has seen every block).
        let mut largest_seen: usize = largest_before;
        for i in from..len {
            let block = bucket[i];
            largest_seen = largest_seen.max(block.size);
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            let total_needed = padding + size;
            if total_needed <= block.size {
                let waste = block.size - total_needed;
                if waste < best_waste {
                    best_waste = waste;
                    best = Some(i);
                    if waste == 0 {
                        break;
                    }
                }
                // PERF (gengc-round1, 2026-09-20): stop REFINING the best fit
                // after a bounded number of candidates. The module header
                // claims this scan is "amortised O(1)" because "bucket sizes
                // are bounded within a factor of 2" — but that bounds each
                // block's SIZE, not the bucket's LENGTH. A workload that frees a
                // hundred thousand same-class objects without compacting (which
                // is exactly what `old_gen_gc(compact = false)` leaves behind
                // under conservative JIT roots) puts all of them in one bucket,
                // and every subsequent allocation in that class walks all of
                // them unless it hits an exact fit — reinstating the O(N) cost
                // the segregated free list was built to remove.
                //
                // The cap is on FITTING candidates only, so it can never
                // change whether a request succeeds: if the bucket holds no fit
                // at all the scan is still exhaustive and the search still
                // escalates, which is what keeps the round-5 "cannot spuriously
                // OOM" property intact. All it gives up is a marginally tighter
                // fit among blocks whose sizes already differ by less than 2x
                // (by less than 25 % since gen r4w4/oldgen4's sub-classes).
                fits_seen += 1;
                if fits_seen >= BEST_FIT_PROBE_LIMIT {
                    break;
                }
            } else {
                // gen r4w2/oldgen2: the half of this scan the fit cap does not
                // bound — see `OldGenTriggerStats::nonfit_probes`.
                *nonfit += 1;
                nonfit_here += 1;
                // gen r4w4/oldgen4: ...and this bounds it, without giving up
                // the fit behind the run (see `NONFIT_PROBE_LIMIT`). Only when
                // there is something left to defer.
                if may_defer && best.is_none() && nonfit_here >= NONFIT_PROBE_LIMIT && i + 1 < len
                {
                    return BucketScan::Deferred {
                        resume: i + 1,
                        largest: largest_seen,
                    };
                }
            }
        }
        match best {
            Some(i) => BucketScan::Fit(i),
            None => BucketScan::NoFit {
                largest: largest_seen,
            },
        }
    }

    /// gen r4w2/oldgen2 — the OS refused to commit pages an allocation needed.
    /// Cold: reached only at the machine's commit limit.
    #[cold]
    #[inline(never)]
    fn note_commit_refusal(&mut self, offset: usize, len: usize) {
        let n = self.trigger_stats.commit_refusals;
        self.trigger_stats.commit_refusals = n.wrapping_add(1);
        if n < 8 {
            tracing::warn!(
                target: "cratonvm::gc",
                offset,
                len,
                committed = self.data.committed_bytes(),
                capacity = self.data.len(),
                "old-gen allocation refused: the OS would not commit the pages it needs. \
                 This is the system commit limit, not a full free list; it surfaces as the \
                 same allocation failure (and, if nothing frees memory, the same \
                 OutOfMemoryError)",
            );
        }
    }

    /// Free a previously allocated block, returning it to the free list.
    ///
    /// Round-5 #14: the redundant per-free zero pass has been dropped —
    /// `alloc` zeros every byte before handing it out, so freed bytes are
    /// never observable as data by a future caller. Coalescing with
    /// neighboring free blocks is deferred to the next mark-compact pass
    /// to keep this path O(1).
    ///
    /// # Safety
    /// `ptr` must point to a block previously allocated from this OldGen,
    /// and `size` must be the exact size of that allocation.
    pub unsafe fn free(&mut self, ptr: *mut u8, size: usize) {
        // Mirror the rounding `alloc` applied so accounting stays
        // symmetric: `alloc` reserved `size.max(HEADER_SIZE.max(8))`
        // bytes (and added that rounded amount to `used_bytes`), so a
        // caller that frees with the originally-requested sub-HEADER_SIZE
        // size must decrement — and return to the free list — the same
        // rounded amount. This expression MUST match `alloc`'s rounding.
        //
        // GCAUD-1 (2026-08-01): the clause above was only HALF of `alloc`'s
        // rounding. `alloc_from_buckets` follows the `max` with
        // `size.checked_add(align - 1).map(|v| v & !(align - 1))`, i.e. it
        // rounds the reservation UP to a multiple of `align`, charges
        // `used_bytes` that rounded amount, and starts the next object there.
        // `free` did not round, so an unrounded `free(ptr, 44)` after
        // `alloc(44, 8)` returned a 44-byte block covering a 48-byte
        // reservation and left a 4-byte sliver OUTSIDE the free list. That
        // sliver reads as ALLOCATED to `walk_objects` (which derives allocated
        // extents as the gaps between free blocks), so the walk resumes at a
        // non-object-start, mis-parses it as a header, and `break`s — dropping
        // every object after it from the walk. `compact` then treats those
        // objects as absent and Phase 4 rebuilds the free list over them.
        //
        // Every production caller passes an already-8-aligned `total_size`
        // from `walk_objects`, and every production `alloc` uses `align = 8`,
        // so the asymmetry is latent rather than live — the same shape as the
        // TLAB audit's T-2. Rounding UP to 8 here is exactly `alloc`'s
        // arithmetic for `align <= 8` and is strictly conservative for a
        // larger alignment (it returns no more than was reserved, so it can
        // never hand a live neighbour's bytes to the free list).
        let size = (size.max(HEADER_SIZE.max(8)) + 7) & !7;

        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        // gengc-round1 2026-09-20: the assertion above is the only thing that
        // stood between an out-of-range `ptr` and a catastrophic free list. In
        // release, `addr - base` wraps to a near-`usize::MAX` offset, the block
        // is pushed into a bucket, and the very next `walk_objects` treats the
        // gap below it as an allocated region and `scan_region`s hundreds of
        // terabytes of unmapped address space. Refusing the free leaks one
        // block; accepting it corrupts every walk this generation ever does.
        if addr < base
            || addr
                .checked_add(size)
                .is_none_or(|e| e > base + self.data.len())
        {
            tracing::error!(
                target: "cratonvm::gc::guard",
                ptr = format!("{addr:#x}"),
                size,
                base = format!("{base:#x}"),
                capacity = self.data.len(),
                "OldGen::free called with a block outside this generation's storage — \
                 refusing the free rather than pushing a wild offset onto the free list",
            );
            return;
        }
        let offset = addr - base;
        // gen r4w3/cards3: a freed object may have been some card's anchor;
        // every card above it is re-derived before it is trusted again.
        self.bot.get_mut().note_free(offset);

        self.used_bytes = self.used_bytes.saturating_sub(size);
        // GCAUD-4: this address may be handed to a different object by the
        // next `alloc`, so every address-keyed cache taken before now is
        // ambiguous from here on.
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);

        // Push into the appropriate size bucket — O(1).
        // Coalescing happens during the next `compact()` call.
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        // gen r4/oldgen: a new block may touch an existing one.
        self.free_list_maximal = false;
        // PERF (gc-oldgen-perf): a new free block changes the sorted view.
        self.invalidate_sorted_free();
    }

    /// Hand back the NEVER-WRITTEN tail of a block [`Self::alloc_unzeroed`]
    /// carved, without stamping [`Self::reclaim_epoch`].
    ///
    /// [`Self::free`] bumps the epoch because the bytes it releases held an
    /// object whose address a concurrent marker's remark snapshot may still
    /// name; hand the address to a new object and "existed at remark, not
    /// marked" becomes a licence to free something live. A promotion buffer's
    /// tail never held an object: it was free space at every remark that
    /// preceded this pause (a block that was live at the remark reaches the
    /// buckets only through `free`, which bumped the epoch already), so no
    /// snapshot can name any address in it, and returning it changes nothing
    /// any address-keyed cache believes. That is why a young collection can
    /// retire its buffers every cycle without invalidating an in-flight
    /// concurrent old-gen sweep.
    ///
    /// # Safety
    /// `[ptr, ptr + size)` must be the unused tail of a block obtained from
    /// this old gen, at least `HEADER_SIZE` bytes, 8-aligned, and never
    /// written since it was carved.
    pub unsafe fn release_unused_tail(&mut self, ptr: *mut u8, size: usize) {
        debug_assert!(size >= HEADER_SIZE && size % 8 == 0 && (ptr as usize) % 8 == 0);
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        debug_assert!(addr >= base && addr + size <= base + self.data.len());
        // gengc-round1 2026-09-20: same release-mode guard as `free`. A
        // promotion buffer's tail arrives from a GC worker's arithmetic, and a
        // wild offset pushed onto the free list turns every later
        // `walk_objects` into a scan of unmapped memory.
        if size == 0
            || addr < base
            || addr
                .checked_add(size)
                .is_none_or(|e| e > base + self.data.len())
        {
            tracing::error!(
                target: "cratonvm::gc::guard",
                ptr = format!("{addr:#x}"),
                size,
                base = format!("{base:#x}"),
                capacity = self.data.len(),
                "OldGen::release_unused_tail called with a span outside this \
                 generation's storage — refusing it rather than pushing a wild \
                 offset onto the free list",
            );
            return;
        }
        let offset = addr - base;
        #[cfg(debug_assertions)]
        self.check_release_is_a_registered_buffer_tail(offset, size);
        self.used_bytes = self.used_bytes.saturating_sub(size);
        self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        // gen r4/oldgen: a released tail is flush against whatever remained
        // of the block its buffer was carved from — usually free.
        self.free_list_maximal = false;
        self.invalidate_sorted_free();
    }

    /// gengc-round2 — the checked form of `release_unused_tail`'s
    /// "this span never held an object" premise. See
    /// [`Self::unzeroed_buffer_carves`].
    ///
    /// `[offset, offset + size)` must be a SUFFIX of an outstanding carve made
    /// by [`Self::alloc_unzeroed_buffer`]. A suffix, not merely a subset:
    /// `retire_old_plab` only ever releases `[cursor, end)`, and a middle
    /// chunk would mean something below it is being kept while something above
    /// it is handed back, which no promotion-buffer shape produces.
    ///
    /// The matched entry is retired: a buffer's life ends when its tail goes
    /// back. Firing here means the epoch-skip premise is broken, so it is a
    /// `debug_assert!` — an unnoticed violation is a concurrent sweep freeing a
    /// live block, which is not a thing to log and continue past in the build
    /// that is looking for it.
    #[cfg(debug_assertions)]
    fn check_release_is_a_registered_buffer_tail(&mut self, offset: usize, size: usize) {
        let end = offset + size;
        let found = self
            .unzeroed_buffer_carves
            .iter()
            .position(|&(c_off, c_size)| offset >= c_off && end == c_off + c_size);
        match found {
            Some(i) => {
                self.unzeroed_buffer_carves.swap_remove(i);
            }
            None => {
                tracing::error!(
                    target: "cratonvm::gc::guard",
                    offset,
                    size,
                    outstanding = self.unzeroed_buffer_carves.len(),
                    "OldGen::release_unused_tail: the released span is not the tail of \
                     any outstanding alloc_unzeroed_buffer carve. This path skips the \
                     reclaim-epoch stamp on the premise that the span never held an \
                     object; if that premise is false, an in-flight concurrent old-gen \
                     sweep can free a live block (the H2-CID0 shape).",
                );
                debug_assert!(
                    false,
                    "release_unused_tail(+{offset:#x}, {size}) is not the tail of an \
                     outstanding alloc_unzeroed_buffer carve — the reclaim-epoch skip's \
                     premise does not hold for it",
                );
            }
        }
    }

    /// Returns true if the given pointer falls within this old generation's storage.
    ///
    /// gen r4w2/oldgen2 (2026-09-23): within its READABLE storage. On the
    /// wholly committed store that is the whole capacity, exactly as before.
    /// On the reserving store it is `[base, base + readable_end)` — every
    /// object this generation has ever held lies below that mark, so the
    /// answer for a real object is unchanged; what changes is that an address
    /// in reserved-but-never-committed space is no longer "in old gen".
    ///
    /// That is deliberate and it is the safe direction. Some thirty call sites
    /// treat `contains(p)` as licence to read a header at `p` — the mark's
    /// plausibility screens (`old_gen_mark_candidate_plausible`,
    /// `victim8_neighbor_explains_zero_prefix`) among them, fed by conservative
    /// GUESSES — and before the store reserved, such a read of never-allocated
    /// space returned zeros. On an uncommitted page it would fault. Nothing
    /// can live there, so declining it loses no object.
    ///
    /// [`Self::extent`] still reports the full reserved range, for callers
    /// that only classify.
    ///
    /// gen r5w3/oldgen7 — HOLES. Below `readable_end` a granule can be
    /// uncommitted: the Windows commit-limit arm of a give-back, and the
    /// opt-in interior decommit ([`Self::decommit_interior_free_runs`]).
    /// Then (`readable_holes`) the answer comes from the commit bitmap, for
    /// the byte at `ptr` AND the last byte of a header read there
    /// ([`READ_SLACK`] past it, clamped to the reservation) — the hole-edge
    /// twin of the `READ_SLACK` the committed END keeps, so a caller that
    /// reads a header at a contained address cannot straddle into a released
    /// granule. No real object byte is ever refused by the second probe: the
    /// allocator commits `READ_SLACK` past every object it hands out, and the
    /// interior decommit starts its holes `READ_SLACK` past a free block's
    /// start. Readers that go on to read a claimed EXTENT ask
    /// [`Self::contains_range`]. Every other old-gen reader reads only
    /// allocated regions (the walks, the card scan, the block-offset table,
    /// the compactors after they re-commit), or a live object's own fields.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr() as usize;
        let p = ptr as usize;
        if p < base {
            return false;
        }
        let off = p - base;
        if off >= self.readable_end {
            return false;
        }
        if !self.readable_holes {
            return true;
        }
        // A hole below the mark: ask the bitmap, over the header's width.
        let header_end = off
            .saturating_add(READ_SLACK - 1)
            .min(self.data.len().saturating_sub(1));
        self.data.is_committed_at(off) && self.data.is_committed_at(header_end)
    }

    /// True when `ptr` lies inside an **allocated** span of old gen.
    ///
    /// [`Self::contains`] is a bare range check over the whole backing store,
    /// so it answers `true` for memory that has already been returned to the
    /// free list — the non-moving old-gen sweep (`old_gen_gc(compact = false)`)
    /// reclaims dead blocks IN PLACE and does not zero them, so the dead
    /// object's bytes stay put and the address keeps passing `contains`. This
    /// is the discriminator a liveness query needs; see
    /// `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
    ///
    /// O(log n) in the free-block count, over the same offset-sorted view
    /// `walk_objects` already caches.
    pub fn is_allocated_addr(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr() as usize;
        let addr = ptr as usize;
        if addr < base || addr >= base + self.data.len() {
            return false;
        }
        let off = addr - base;
        self.with_sorted_free_blocks(|sorted| {
            // Free blocks are offset-sorted and, on a healthy list, disjoint,
            // so the last block starting at or before `off` is the only one
            // that can cover it. gen r4/oldgen: they are NOT guaranteed
            // disjoint (a double free leaves an overlapping or nested pair —
            // `OLD_FREE_LIST_OVERLAPS`, and the `max` in `walk_objects`). On
            // such a list an address inside an outer block but past a nested
            // one reads as ALLOCATED here. That error only ever goes the
            // over-retaining way (a caller keeps state for a dead address):
            // whenever this answers FREE, the block it found really is on the
            // free list and really covers `off`.
            let i = sorted.partition_point(|b| b.offset <= off);
            i == 0 || {
                let b = &sorted[i - 1];
                off >= b.offset + b.size
            }
        })
    }

    /// Backing-storage extent as plain integers: `[lo, hi)`.
    ///
    /// `OldGen` is not `Sync` (it owns the storage), so a parallel young-sweep
    /// worker cannot hold a `&OldGen` just to run `contains`. This exposes the
    /// same range test as two `usize`s the workers can copy.
    pub fn extent(&self) -> (usize, usize) {
        let base = self.data.as_ptr() as usize;
        (base, base + self.data.len())
    }

    /// Get the base pointer of the backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Total bytes currently allocated.
    pub fn used(&self) -> usize {
        self.used_bytes
    }

    /// Total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// One past the highest offset any allocation has ever covered.
    ///
    /// Two consumers, and the second is the interesting one:
    ///
    /// * [`Self::compact`]'s Phase 4, which bounds its zero pass by it;
    /// * **a card-table scan bound.** Every dirty card index is derived from
    ///   the address of an OBJECT — both the Rust barrier and the JIT's inline
    ///   one compute `(src_addr - old_base) / CARD_SIZE` from the holder's
    ///   header — and every old-gen object lies below this mark by
    ///   construction, since [`Self::alloc_from_buckets`] is the only site
    ///   that hands storage out and is the only site that moves the mark. So
    ///   `high_water` is an upper bound on "which card could possibly be
    ///   dirty" that survives the JIT's raw byte store, which is what makes it
    ///   different from a summary map generated code would have to maintain.
    ///   See [`crate::card_table::CardTable::set_scan_bound_bytes`] for the
    ///   rest of that contract — in particular that a publisher must refresh
    ///   the bound after any old-gen allocation and before mutators resume,
    ///   because promotion moves this mark.
    ///
    /// Note that [`Self::compact`] LOWERS this to `compacted_end`. A card-table
    /// bound must therefore never be lowered to track it: cards above
    /// `compacted_end` can still be dirty from before the compaction, which is
    /// why `set_scan_bound_bytes` is monotone upward.
    ///
    /// gen r5w3/oldgen7: [`Self::alloc_from_top`] (opt-in,
    /// `CRATONVM_GC_OLD_HUMONGOUS_TOP`) is a second hand-out site and moves the
    /// mark too — to the TOP of the generation as soon as one humongous array
    /// is placed there, so the card scan bound covers the whole generation
    /// from then on (the proposal's accepted cost; the summary map keeps the
    /// scan at `bound / 64` bytes plus the dirty groups).
    pub fn high_water(&self) -> usize {
        self.high_water
    }

    /// Walk all allocated objects in the old generation.
    ///
    /// This iterates through allocated regions (gaps between free blocks)
    /// and yields each object's pointer and header. Used by the mark-sweep
    /// collector to iterate old-gen objects.
    ///
    /// Returns a Vec of (object pointer, total object size) pairs.
    pub fn walk_objects(&self) -> Vec<(*mut u8, usize)> {
        self.walk_objects_with_gaps().0
    }

    /// [`Self::walk_objects`], plus the byte ranges the walk could NOT cover:
    /// one `[break_offset, region_end)` per allocated region whose scan
    /// stopped at an implausible header (`SCAN_REGION_BREAK_HITS`,
    /// `WALK_DESYNC_HITS`). Offsets, ascending, disjoint.
    ///
    /// gen r4w4/oldgen4 (2026-09-24),
    /// `gengc-r4w3-oldgen3-a-walk-desync-stops-all-old-gen-reclamation`: the
    /// in-place sweep used to learn only THAT its walk was short (walked bytes
    /// `!= used`) and then freed nothing, forever, because the bad header
    /// never goes away. Knowing WHERE the unwalked bytes are lets it treat
    /// them as a conservative root range and keep reclaiming everything else
    /// (`GenerationalHeap::old_gen_gc`). When the free list is healthy the
    /// walked bytes plus the gap bytes equal [`Self::used`] exactly; a caller
    /// that finds otherwise must keep failing closed.
    ///
    /// Also sizes the result `Vec` from the previous walk (item 1 of
    /// `gengc-oldgen-compact-is-serial-and-o-heap`: one allocation instead of
    /// ~log2(n) regrowth copies of a buffer the size of the object count).
    pub fn walk_objects_with_gaps(&self) -> (Vec<(*mut u8, usize)>, Vec<(usize, usize)>) {
        let mut objects = Vec::with_capacity(self.last_walk_len.get());
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        let base = self.data.as_ptr() as usize;

        // Round-5 #14: rebuild the offset-sorted view from segregated
        // buckets on demand. Free is now O(1) and walk-objects pays the
        // sort cost up front; the trade is favourable because alloc/free
        // run on the hot path and walk_objects only at GC time.
        //
        // PERF (gc-oldgen-perf): the collect+sort is now memoised via
        // `with_sorted_free_blocks` — when the buckets are unchanged since the
        // previous walk (common across GC phases that call `walk_objects`
        // repeatedly without allocating) the cached ordering is reused instead
        // of being rebuilt from scratch. The slice contents are identical to
        // the old inline `collect()`+`sort_by_key`, so the walk is unchanged.
        self.with_sorted_free_blocks(|sorted_free| {
            let mut cursor: usize = 0;
            for block in sorted_free {
                // Allocated region from cursor to block.offset
                if block.offset > cursor {
                    if let RegionScan::Break(at) =
                        self.scan_region(base, cursor, block.offset, &mut objects)
                    {
                        gaps.push((at, block.offset));
                    }
                }
                // `max` because the free list is only guaranteed sorted by
                // OFFSET, not disjoint. A block nested inside its predecessor
                // (`[100,300)` then `[150,200)`) would otherwise pull the cursor
                // back to 200 and the next gap would be walked from inside the
                // first block — parsing free bytes as object headers, which is
                // how a phantom header comes to subsume live objects. Nested
                // blocks mean a double free (see `OLD_FREE_LIST_OVERLAPS`); this
                // makes the walk safe while that is being diagnosed rather than
                // silently mis-parsing.
                cursor = cursor.max(block.offset + block.size);
            }
            // Region after last free block
            if cursor < self.data.len() {
                let end = self.data.len();
                if let RegionScan::Break(at) = self.scan_region(base, cursor, end, &mut objects) {
                    gaps.push((at, end));
                }
            }
        });

        self.last_walk_len.set(objects.len());
        (objects, gaps)
    }

    /// Walk old-gen objects, retaining only those whose start offset falls
    /// inside one of the supplied dirty-card offset ranges.
    ///
    /// Round-11 perf: the minor-GC dirty-card scan previously called
    /// [`Self::walk_objects`], which allocates a `Vec` holding *every*
    /// live old-gen object and sorts the entire free list, even when only
    /// a handful of cards are dirty. This variant bounds two of those
    /// costs to the dirty set:
    ///
    /// * the result `Vec` only ever holds objects in dirty cards, so its
    ///   size is O(dirty objects) rather than O(all old-gen objects);
    /// * an allocated region that overlaps no dirty card is walked for
    ///   *cursor advancement only* — no `(ptr, size)` pairs are pushed.
    ///
    /// `dirty_ranges` must be a list of `[start, end)` byte offsets
    /// (relative to the data buffer) sorted ascending by `start` and
    /// non-overlapping; the card table produces exactly that. Object
    /// boundaries are still derived header-by-header from the sorted free
    /// list — that derivation is what makes the scan *correct*, so it is
    /// preserved unchanged; only the work *per object* is bounded.
    pub fn walk_objects_in_card_ranges(
        &self,
        dirty_ranges: &[(usize, usize)],
    ) -> Vec<(*mut u8, usize)> {
        let mut gaps = Vec::new();
        self.walk_objects_in_card_ranges_with_gaps(dirty_ranges, &mut gaps)
    }

    /// [`Self::walk_objects_in_card_ranges`], plus the gap windows of
    /// [`Self::walk_card_ranges_with_gaps`] (gen r5w1/oldgen5): for each
    /// region whose walk broke at a header it cannot size, the part of
    /// `[break, region_end)` a dirty range reaches, appended to `gaps`.
    pub(crate) fn walk_objects_in_card_ranges_with_gaps(
        &self,
        dirty_ranges: &[(usize, usize)],
        gaps: &mut Vec<(usize, usize)>,
    ) -> Vec<(*mut u8, usize)> {
        let mut objects = Vec::new();
        if dirty_ranges.is_empty() {
            return objects;
        }
        let base = self.data.as_ptr() as usize;

        // Same offset-sorted free-list view as `walk_objects`; needed to
        // locate true object boundaries (old-gen layout is header-following
        // with no external object index).
        //
        // PERF (gc-oldgen-perf): shares the memoised sorted view with
        // `walk_objects` via `with_sorted_free_blocks` rather than re-collecting
        // and re-sorting the buckets here. Same ordering, same early-exit once
        // the cursor passes the last dirty card.
        let last_dirty_end = dirty_ranges[dirty_ranges.len() - 1].1;
        self.with_sorted_free_blocks(|sorted_free| {
            let mut cursor: usize = 0;
            for block in sorted_free {
                if block.offset > cursor {
                    if let Some(at) = self.scan_region_filtered(
                        base,
                        cursor,
                        block.offset,
                        dirty_ranges,
                        &mut objects,
                    ) {
                        if let Some(w) = bot::gap_window(dirty_ranges, at, block.offset) {
                            bot::push_gap(gaps, w);
                        }
                    }
                }
                // `max`, for exactly the reason `walk_objects` gives: the free
                // list is guaranteed sorted by OFFSET but NOT disjoint, and a
                // block nested inside its predecessor (`[100,300)` then
                // `[150,200)`) drags the cursor BACKWARDS to 200, after which
                // the next gap is walked from inside the first block and free
                // bytes are parsed as object headers.
                //
                // gengc-round1 2026-09-20: `walk_objects` has carried this
                // guard since the `OLD_FREE_LIST_OVERLAPS` double-free
                // diagnostic landed; this walk — which is the one the minor
                // GC's old→young card scan actually uses — never got it. The
                // asymmetry matters more here than there, because a phantom
                // header on this path does not merely mis-report an object: it
                // truncates the dirty-card root set (`scan_region_filtered`
                // `break`s on the implausible size it derives), and a lost
                // old→young root is a live young object freed.
                cursor = cursor.max(block.offset + block.size);
                // Allocated regions are address-ordered; once the cursor passes
                // the last dirty card there is nothing left to collect.
                if cursor >= last_dirty_end {
                    return;
                }
            }
            if cursor < self.data.len() {
                let end = self.data.len();
                if let Some(at) =
                    self.scan_region_filtered(base, cursor, end, dirty_ranges, &mut objects)
                {
                    if let Some(w) = bot::gap_window(dirty_ranges, at, end) {
                        bot::push_gap(gaps, w);
                    }
                }
            }
        });

        objects
    }

    /// Validate the `ObjectKind`/`ArrayElementType` tag bytes at `ptr` before
    /// any walker constructs a typed `&ObjectHeader` there.
    ///
    /// `scan_region`/`scan_region_filtered` derive object boundaries purely
    /// from arithmetic (`offset += total_size`), trusting that the cursor
    /// always lands on a real header. When that trust is violated — by any
    /// of the several walk-desync causes this file documents elsewhere, or
    /// one not yet found — the cursor lands on arbitrary payload bytes
    /// (a Java `int`/`long`/pointer field, leftover free-list bytes, ...).
    /// `ObjectHeader::kind`/`element_type` are `#[repr(u8)]` enums with only
    /// a handful of valid discriminants; loading an out-of-range byte into
    /// either as a *typed* enum is immediate undefined behaviour, and
    /// optimized code is free to lower that UB into a hardware trap rather
    /// than doing anything resembling "the wrong thing but not crashing" —
    /// which is exactly the `SIGILL` in `OldGen::compact` this fixes
    /// (`HIB-DCAST-LATEPHASE.1`; faulting RVA was identical across repeated
    /// crashes, i.e. a deterministic trap site, not stack-smash noise).
    ///
    /// Mirrors the fix already applied to the conservative-root validators
    /// in `gen_heap.rs`/`g1.rs` (see
    /// `source-debug-jit-conservative-root-invalid-header-tag-sigill.md`):
    /// read the raw tag bytes and validate them through
    /// `object_kind_from_tag`/`array_element_type_from_tag` *before* ever
    /// forming a `&ObjectHeader` reference and touching the typed field.
    ///
    /// Returns the validated `ObjectKind` on success. Returns `None` — and
    /// bumps [`WALK_DESYNC_HITS`] — when either tag byte is not a valid
    /// discriminant; the caller must treat this exactly like the existing
    /// `HumongousFiller`/implausible-size guards and stop scanning the
    /// current stripe rather than trust the cursor further.
    fn validate_header_tags_or_desync(ptr: *mut u8, offset: usize) -> Option<ObjectKind> {
        // SAFETY: `ptr` is `base + offset` where `offset` falls inside the
        // `[start_offset, end_offset)` sub-range of an allocated region within
        // `self.data` that the caller is scanning, so the two single-byte tag
        // reads at the fixed header offsets are in-bounds. Reading a `u8`
        // through a raw pointer has no validity requirement beyond
        // in-bounds-and-readable, so this cannot itself be the UB the rest of
        // this function exists to avoid.
        let kind_tag = unsafe { cratonvm_types::kind_tag_at(ptr) };
        let elem_tag = unsafe { cratonvm_types::element_type_tag_at(ptr) };
        let kind = object_kind_from_tag(kind_tag);
        let element_type = cratonvm_types::element_tag_ok(kind_tag, elem_tag).then_some(());
        match (kind, element_type) {
            (Some(kind), Some(())) => Some(kind),
            _ => {
                WALK_DESYNC_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tracing::warn!(
                    offset,
                    kind_tag,
                    elem_tag,
                    "old-gen walk: invalid ObjectKind/ArrayElementType tag at a walk \
                     boundary — the walk has desynced from real object headers; \
                     stopping this scan stripe instead of reading a corrupt header as \
                     a typed enum. See \
                     source-debug-jit-conservative-root-invalid-header-tag-sigill.md."
                );
                None
            }
        }
    }

    /// Like [`Self::scan_region`], but only pushes objects whose start
    /// offset lies within one of `dirty_ranges`. Every object boundary is
    /// still visited so the cursor advances correctly; the filter only
    /// gates whether the `(ptr, size)` pair is collected.
    ///
    /// gen r5w1/oldgen5: returns the offset of the header the walk broke at
    /// (the bytes from there to `end_offset` were not walked), or `None` when
    /// the region was walked to its end or past the last dirty range.
    fn scan_region_filtered(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        dirty_ranges: &[(usize, usize)],
        objects: &mut Vec<(*mut u8, usize)>,
    ) -> Option<usize> {
        let mut offset = start_offset;
        // PERF 2026-08-02: `dirty_ranges` is sorted ascending, non-overlapping
        // and coalesced (see `gen_heap::scan_dirty_cards`, which sorts the card
        // indices and merges adjacent cards before calling here), and `offset`
        // only ever increases across this loop. So the range that could contain
        // `offset` can be tracked with a monotone cursor. This used to be a
        // `dirty_ranges.iter().any(..)` per object, making the dirty-card scan
        // O(old-gen objects x dirty ranges) — quadratic in the size of the old
        // generation once a workload promotes a lot and dirties a lot.
        //
        // Measured 2026-08-02, `perf record -F 199` against
        // `BeanRegistrationsAotContributionTests
        // #applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles` (10001
        // generated bean definitions, so a large old gen and a large dirty set):
        // `scan_region_filtered` was **73.7% of ALL CPU samples** before this
        // change and does not appear in the profile at all after it.
        //
        // Scope the claim honestly: removing that 73.7% did NOT make that test
        // pass — at the time it still exhausted the heap in javac, it just got
        // there sooner. (The test passes as of 2026-08-06, for an unrelated
        // reason: see
        // `beanregistrations-verylarge-heap-footprint-FIXED-20260806.md`.)
        // And on the 1001-definition
        // sibling, whose old gen is small enough that the quadratic never bites,
        // an alternating 2-binary A/B is within noise (221 s vs 229 s). The
        // justification for this change is the algorithm and the profile, not a
        // wall-clock win on any particular test.
        let mut range_idx = dirty_ranges.partition_point(|&(_, end)| end <= offset);
        while offset < end_offset {
            let ptr = (base + offset) as *mut u8;
            let Some(kind) = Self::validate_header_tags_or_desync(ptr, offset) else {
                return Some(offset);
            };
            // SAFETY: `offset` is a valid object boundary within an
            // allocated region of the data buffer (see `scan_region`), and
            // the kind/element_type tag bytes were just validated above.
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            if kind == ObjectKind::HumongousFiller {
                return Some(offset);
            }
            let raw_size = if kind == ObjectKind::Array {
                ARRAY_DATA_OFFSET
                    + array_data_size(header.array_length() as usize, header.element_type())
                        .expect("array_data_size overflow in old_gen scan")
            } else {
                // Compact reference-field layout: a promoted compact object's body
                // size is in the header's `array_length`, not `num_slots*SLOT_SIZE`.
                // `object_body_size` honours the per-object `GC_FLAG_COMPACT` bit
                // (legacy objects still size as `num_slots*SLOT_SIZE`). Without this
                // the walker strides `num_slots*16` over a compact object, desyncing
                // the whole old-gen walk and tripping the size-consistency skip in
                // `scan_dirty_cards` (→ missed old→young roots → corruption).
                cratonvm_types::object_instance_size(header)
            };
            let total_size = raw_size.checked_add(7).map(|size| size & !7).unwrap_or(0);
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                return Some(offset);
            }
            // Collect only if the object's start lands in a dirty card.
            // Advance the cursor past every range that ends at or before this
            // object; what remains is the only range that can contain it.
            while range_idx < dirty_ranges.len() && dirty_ranges[range_idx].1 <= offset {
                range_idx += 1;
            }
            let Some(&(range_start, _)) = dirty_ranges.get(range_idx) else {
                // Past the last dirty range: no later object in this region can
                // qualify, and the caller derives its own cursor from the free
                // list rather than from how far this walk got.
                return None;
            };
            if offset >= range_start {
                objects.push((ptr, total_size));
            }
            offset += total_size;
        }
        None
    }

    /// Scan a contiguous allocated region for objects.
    ///
    /// gen r4w4/oldgen4: returns how the region ended, so
    /// [`Self::walk_objects_with_gaps`] can record where a break left bytes
    /// unwalked (never [`RegionScan::Budget`]: the budget is unlimited).
    fn scan_region(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        objects: &mut Vec<(*mut u8, usize)>,
    ) -> RegionScan {
        // gen r4w3/oldgen3: the one body, shared with the budgeted walk
        // (`walk_objects_from`). An unlimited budget never reaches zero, so
        // this is the same loop it always was.
        let mut unlimited = usize::MAX;
        self.scan_region_budget(
            base,
            start_offset,
            end_offset,
            &mut unlimited,
            &mut |ptr: *mut u8, size: usize| objects.push((ptr, size)),
        )
    }

    /// Visit the objects of a contiguous allocated region, from `start_offset`
    /// (which must be an object start), at most `*budget` of them.
    ///
    /// Returns [`RegionScan::Budget`] with the start of the first object NOT
    /// visited when the budget ran out before the region did,
    /// [`RegionScan::Done`] when the region was finished, and
    /// [`RegionScan::Break`] when the walk broke on an implausible header —
    /// which ends the region exactly as it always has (see the counters
    /// below); gen r4w4/oldgen4 only made the break offset visible.
    ///
    /// gen r4w3/oldgen3 (2026-09-23): split out of `scan_region` so a caller
    /// can walk the generation in bounded slices (the concurrent sweep) or
    /// without materialising a `Vec` of every object (`for_each_object`).
    fn scan_region_budget<F: FnMut(*mut u8, usize)>(
        &self,
        base: usize,
        start_offset: usize,
        end_offset: usize,
        budget: &mut usize,
        f: &mut F,
    ) -> RegionScan {
        let mut offset = start_offset;
        while offset < end_offset {
            if *budget == 0 {
                return RegionScan::Budget(offset);
            }
            let ptr = (base + offset) as *mut u8;
            let Some(kind) = Self::validate_header_tags_or_desync(ptr, offset) else {
                return RegionScan::Break(offset);
            };
            // SAFETY: the kind/element_type tag bytes were just validated above.
            let header = unsafe { &*(ptr as *const ObjectHeader) };
            // Round-9 gc CRIT-1: HumongousFiller is a synthetic walker
            // sentinel installed by the regional GC (see `g1.rs` and
            // `region.rs`). It should never appear in the non-regional
            // old-gen layout, but defensively skip the rest of the
            // current scan stripe instead of mis-parsing it as a real
            // object (which would corrupt the offset cursor).
            if kind == ObjectKind::HumongousFiller {
                return RegionScan::Break(offset);
            }
            let raw_size = if kind == ObjectKind::Array {
                ARRAY_DATA_OFFSET
                    + array_data_size(header.array_length() as usize, header.element_type())
                        .expect("array_data_size overflow in old_gen scan")
            } else {
                // Compact reference-field layout: honour the per-object
                // `GC_FLAG_COMPACT` bit (body size in `array_length`); legacy
                // objects still size as `num_slots*SLOT_SIZE`. See the matching
                // note in `scan_region_filtered`.
                cratonvm_types::object_instance_size(header)
            };
            let total_size = raw_size.checked_add(7).map(|size| size & !7).unwrap_or(0);
            // Sanity check: if total_size is 0 or too large, stop scanning
            if total_size < HEADER_SIZE || offset + total_size > end_offset {
                let n = SCAN_REGION_BREAK_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if n < 8 {
                    // Raw header bytes, not the typed struct: the whole point
                    // is that this header may not be trustworthy to decode as
                    // one, and a `Debug` format on an out-of-range `kind`
                    // walked exactly this wild-pointer bug once already (see
                    // gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md).
                    // SAFETY: `ptr` is inside the allocated region between
                    // `start_offset` and `end_offset`, both within `self.data`;
                    // HEADER_SIZE bytes at `ptr` are therefore in-bounds.
                    let raw_bytes: [u8; HEADER_SIZE] =
                        unsafe { std::ptr::read(ptr as *const [u8; HEADER_SIZE]) };
                    tracing::warn!(
                        offset,
                        end_offset,
                        total_size,
                        raw_size,
                        kind = ObjectHeader::kind_tag(header.mark_word.load(std::sync::atomic::Ordering::Relaxed)),
                        element_type = header.element_type() as u8,
                        array_length = header.array_length(),
                        num_slots = header.num_slots(),
                        bytes = ?raw_bytes,
                        "old-gen scan_region: BREAK on implausible header — dumping raw bytes \
                         so the corruption can finally be seen instead of inferred",
                    );
                }
                return RegionScan::Break(offset);
            }
            f(ptr, total_size);
            *budget -= 1;
            offset += total_size;
        }
        RegionScan::Done
    }

    /// Visit every allocated object, in ascending address order, without
    /// materialising the `Vec` [`Self::walk_objects`] returns.
    ///
    /// gen r4w3/oldgen3 (2026-09-23), step 1 of
    /// `gengc-oldgen-compact-is-serial-and-o-heap`: the same walk (the same
    /// sorted free-list view, the same `scan_region` body, the same desync
    /// breaks), for the consumers that only ever loop over it once. `f` runs
    /// while the free-list view is borrowed, so it must not mutate the
    /// generation — which it cannot, holding only `&self`.
    pub fn for_each_object<F: FnMut(*mut u8, usize)>(&self, f: F) {
        let _ = self.walk_objects_from(0, usize::MAX, f);
    }

    /// Walk at most `max_objects` allocated objects, in ascending address
    /// order, starting at offset `from`; return where to resume, or `None`
    /// when the walk reached the end of the generation.
    ///
    /// gen r4w3/oldgen3 (2026-09-23), for the sliced concurrent sweep.
    ///
    /// **`from` must be `0`, or an offset this function returned** — and the
    /// object that starts there must not have been freed or moved since. A
    /// returned offset is always the start of an object that was allocated
    /// when it was returned: either the next object of the region the budget
    /// ran out in, or the first object of the next allocated region (the walk
    /// computes that before returning rather than returning a free-block
    /// start, which a later allocation could straddle). Anything that frees or
    /// slides old-gen storage bumps [`Self::reclaim_epoch`], so a caller that
    /// holds the epoch unchanged between two calls — apart from its own frees
    /// BELOW the returned offset — keeps the precondition. An allocation in
    /// between can only fill free space, which never contains the returned
    /// offset, and objects it places above the offset are walked like any
    /// other.
    ///
    /// Each region is re-derived from the CURRENT sorted free list on every
    /// call (O(free blocks)), so a region that grew or shrank at its edges
    /// since the previous slice is walked as it is now.
    pub fn walk_objects_from<F: FnMut(*mut u8, usize)>(
        &self,
        from: usize,
        max_objects: usize,
        mut f: F,
    ) -> Option<usize> {
        let base = self.data.as_ptr() as usize;
        let len = self.data.len();
        let mut budget = max_objects;
        // A zero-sized sentinel block at the end turns "the region after the
        // last free block" into one more loop iteration, exactly as
        // `walk_objects`' trailing `scan_region` call does.
        let sentinel = [FreeBlock {
            offset: len,
            size: 0,
        }];
        self.with_sorted_free_blocks(|sorted_free| {
            let mut cursor: usize = 0;
            for block in sorted_free.iter().chain(sentinel.iter()) {
                if block.offset > cursor {
                    // Allocated region `[cursor, block.offset)`.
                    let region_end = block.offset.min(len);
                    if region_end > from {
                        let start = cursor.max(from);
                        if budget == 0 {
                            // The first unvisited object: this region's start,
                            // or `from` itself when it lies inside it.
                            return Some(start);
                        }
                        if let RegionScan::Budget(next) =
                            self.scan_region_budget(base, start, region_end, &mut budget, &mut f)
                        {
                            return Some(next);
                        }
                    }
                }
                // `max`, for the reason `walk_objects` gives: sorted by offset
                // is not disjoint, and a nested block must not rewind.
                cursor = cursor.max(block.offset + block.size);
            }
            None
        })
    }

    // -----------------------------------------------------------------------
    // Mark-Compact Collection (Session 26)
    // -----------------------------------------------------------------------

    /// Compact all marked (live) objects toward the start of the heap using
    /// sliding compaction. This eliminates all fragmentation — after compaction,
    /// the free list is a single contiguous block at the end.
    ///
    /// **Algorithm (4 phases):**
    /// 1. **Compute forwarding addresses** — walk objects in address order,
    ///    assign each live object a destination at the next free position.
    /// 2. **Update internal references** — for each live object, rewrite
    ///    reference slots to use forwarding addresses.
    /// 3. **Slide objects** — copy each object to its forwarding address
    ///    (low-to-high order guarantees no data corruption since dest ≤ src).
    /// 4. **Rebuild free list** — single free block from compacted end to
    ///    capacity.
    ///
    /// **Precondition:** live objects have `GC_FLAG_MARKED` set in their headers.
    /// **Postcondition:** `GC_FLAG_MARKED` and `forwarding_ptr` are cleared on
    /// all surviving objects. Dead objects are reclaimed.
    ///
    /// Returns a pointer map (old_addr → new_addr) for objects that moved.
    /// Objects that stay in place are NOT included in the map.
    pub fn compact(&mut self) -> cratonvm_types::PointerMap {
        self.compact_with_drop_flags(&HashMap::new())
    }

    /// [`Self::compact`], plus the caller's per-block explanation of why the
    /// mark could have missed each block this compaction is about to drop.
    ///
    /// H2-CID0: the flags are threaded through to the old-gen reclamation ring
    /// so a `checkcast` failing on a stale reference minutes later says WHY the
    /// block was unmarked, not just that it was. See
    /// `gen_heap::OLD_FREED_FLAG_WATCHED`.
    pub fn compact_with_drop_flags(
        &mut self,
        drop_flags: &HashMap<usize, u8>,
    ) -> cratonvm_types::PointerMap {
        let objects = self.walk_objects();
        self.compact_walked(objects, drop_flags)
    }

    /// [`Self::compact_with_drop_flags`] over an object grid the caller has
    /// ALREADY derived with [`Self::walk_objects`], instead of walking the
    /// generation again.
    ///
    /// gen r4/oldgen (2026-09-23), item 3 of
    /// `gengc-core-old-gen-pause-path-redundant-walks`: `gen_heap::old_gen_gc`
    /// walks old gen once up front as its mark oracle, and the compacting arm
    /// then walked it a second time here — a full header-by-header pass and a
    /// `Vec` the size of the generation, inside the pause, to rebuild a slice
    /// it already held.
    ///
    /// `walked_at_seq` must be [`Self::free_list_seq`] read when `objects` was
    /// walked. If the free list has changed since, the grid may name freed or
    /// miss newly allocated blocks, so it is DISCARDED and the generation is
    /// re-walked (counted in [`COMPACT_STALE_GRID_REWALKS`]) — the same result
    /// the old unconditional walk gave, never a compaction over a stale grid.
    /// Mark bits are not part of the stamp and need not be: the walk does not
    /// read them, and setting them is the whole point of what happens between
    /// the walk and this call.
    pub fn compact_with_walked_objects(
        &mut self,
        objects: Vec<(*mut u8, usize)>,
        walked_at_seq: u64,
        drop_flags: &HashMap<usize, u8>,
    ) -> cratonvm_types::PointerMap {
        let objects = if walked_at_seq == self.free_list_seq.get() {
            objects
        } else {
            let n = COMPACT_STALE_GRID_REWALKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    target: "cratonvm::gc::guard",
                    walked_at_seq,
                    now = self.free_list_seq.get(),
                    "OldGen::compact_with_walked_objects: the supplied object grid predates a \
                     free-list change; re-walking rather than compacting over it",
                );
            }
            drop(objects);
            self.walk_objects()
        };
        self.compact_walked(objects, drop_flags)
    }

    /// The body of [`Self::compact_with_drop_flags`], over a grid that is
    /// exact for the current free list.
    fn compact_walked(
        &mut self,
        objects: Vec<(*mut u8, usize)>,
        drop_flags: &HashMap<usize, u8>,
    ) -> cratonvm_types::PointerMap {
        let base = self.data.as_mut_ptr();
        // GCAUD-4: bumped up front, so it covers both abandoned paths below
        // (this one and Phase 0's) — each clears mark bits, which is itself a
        // change no address-keyed snapshot taken earlier may assume away.
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);
        // gen r4w3/cards3: compaction relocates every survivor, so no
        // block-offset anchor survives it. Up front, for the same reason as
        // the epoch: the abandoned paths below have to be covered too.
        self.bot.get_mut().note_relayout();

        // GCAUD-9 (2026-08-03): `walk_objects`/`scan_region` deliberately
        // `break`s a region's scan early on an implausible header ("Sanity
        // check: if total_size is 0 or too large, stop scanning") rather than
        // re-syncing — a safe recovery for a diagnostic READ, but `compact`
        // does not just read `objects`: Phase 3 below PHYSICALLY OVERWRITES
        // memory outside it by sliding survivors into the freed space. Any
        // object `scan_region` silently dropped from a region it broke out of
        // early — including a correctly-marked, live one, if the ONLY thing
        // still pointing at it is a Rust-side overlay side table
        // (`lhm_overlay`/`ll_overlay`/etc., which Phase 0's escape check below
        // cannot see — it only walks ordinary header ref-slots) — never
        // enters `live_objects`, gets no `pointer_map` entry, and has its
        // memory handed to whatever survivor Phase 3 slides on top of it.
        // Every live reference to it (an overlay entry, in particular) then
        // points at that survivor's data instead — exactly the "unrelated
        // object's bytes read back through a stale-but-not-obviously-wrong
        // pointer" shape the residual corruption in
        // map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md
        // keeps presenting as (Follow-up 4).
        //
        // `used_bytes` is independently maintained by `alloc`/`free` — it is
        // the ground truth for "how many bytes are currently allocated
        // (live or dead-but-unfreed)" and does not depend on re-parsing
        // headers. A complete, un-broken walk must account for exactly that
        // many bytes (every allocated byte belongs to exactly one walked
        // object; free bytes are excluded by construction — `scan_region` is
        // only ever called on the gaps BETWEEN free blocks). If it does not,
        // some region's scan broke early and there is live-or-dead-but-real
        // allocated memory this compaction cannot see — abandon it and
        // over-retain for one more cycle, the same fail-safe direction Phase
        // 0's escape check already takes below, rather than risk physically
        // overwriting memory whose occupant is unknown.
        let walked_bytes: usize = objects.iter().map(|&(_, sz)| sz).sum();
        if walked_bytes != self.used_bytes {
            let n = COMPACT_WALK_GAP_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    walked_bytes,
                    used_bytes = self.used_bytes,
                    walked_objects = objects.len(),
                    "old-gen compaction ABANDONED: walk_objects covered fewer bytes than are \
                     allocated -- a region's scan broke early on an implausible header and left \
                     live-or-dead memory outside the walked set. Sliding survivors into it would \
                     overwrite an object nothing in this walk can prove is dead. Retaining the \
                     whole generation for this cycle instead.",
                );
            }
            for &(obj_ptr, _size) in &objects {
                // SAFETY: `walk_objects` yielded this as a valid object start.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            return cratonvm_types::PointerMap::default();
        }

        // gen r5w1/oldgen5: a give-back left an UNCOMMITTED run below
        // `readable_end` (`readable_holes`, the Windows commit-limit arm of
        // `reset_range` in `zero_or_give_back` / `after_in_place_sweep`). Such
        // a run is always inside a free block, which is exactly where Phase 3
        // slides survivors, so a destination there faulted inside the
        // `memmove`. This compactor re-commits the holes first — every
        // destination lies below `readable_end` — and abandons, like the
        // walk-gap arm above, when the OS still refuses (since gen r5w3/oldgen7
        // `compact_around_pins` does the same; it used to refuse on any hole,
        // `PinnedCompactRefusal::ReadableHoles`). The interior decommit
        // (`decommit_interior_free_runs`) makes such holes routine.
        // Since gen r5w1 a refused humongous allocation arms this compaction
        // by default, so the arm is no longer confined to an opt-in run.
        if self.readable_holes {
            let end = self.readable_end.saturating_add(READ_SLACK).min(self.data.len());
            if self.data.commit_range(0, end) {
                self.readable_holes = false;
            } else {
                tracing::warn!(
                    readable_end = self.readable_end,
                    "old-gen compaction ABANDONED: an uncommitted give-back hole below the \
                     committed end could not be re-committed, and a survivor could slide into \
                     it. Reclaiming nothing this cycle.",
                );
                for &(obj_ptr, _size) in &objects {
                    // SAFETY: `walk_objects` yielded this as a valid object start.
                    let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                    header.clear_gc_flags(GC_FLAG_MARKED);
                }
                return cratonvm_types::PointerMap::default();
            }
        }

        // Phase 0 (dangling-ref guard): close the live set under "referenced
        // by a live old-gen object".
        //
        // Phase 1 assigns a `forwarding_ptr` only to MARKED objects, and Phase
        // 2 rewrites a referrer's slot only when the target's `forwarding_ptr`
        // is non-null. For an in-old-gen target, a null `forwarding_ptr` after
        // Phase 1 means exactly one thing: the target was UNMARKED (floating
        // garbage). Previously Phase 2 silently skipped such slots, leaving the
        // live referrer pointing at the target's *old* location — which Phase 3
        // then slides another object onto (or Phase 4 zeroes), turning the slot
        // into a dangling pointer that the mutator later dereferences.
        //
        // That situation should not arise if the marker is transitively
        // correct (a live referrer's targets are themselves live). But the
        // compactor must not *corrupt the heap* when the marker under-marks:
        // GC correctness is fail-safe here. So we conservatively promote any
        // unmarked old-gen object reachable from a marked one to live (a small
        // mark-closure fixpoint restricted to old-gen), guaranteeing every slot
        // a live object can hold gets a forwarding address in Phase 1. This
        // retains a little floating garbage until the next cycle (cheap, safe)
        // rather than producing a dangling pointer (fatal). It is task option
        // (a) — "treat any object reachable from a live referrer as live" —
        // applied locally and bounded to the objects we are about to walk.
        // GCAUD-2 (2026-08-01): Phase 0 also answers "can Phase 1 give every
        // reference a live object holds a forwarding address?". When it
        // reports an ESCAPE — a live referrer pointing at an in-old-gen
        // address that `walk_objects` did not yield — the answer is no, and
        // sliding anything is guaranteed to leave that slot dangling. Abandon
        // the compaction: clear the marks this cycle set (so the postcondition
        // "no survivor leaves with GC_FLAG_MARKED" still holds and the next
        // cycle starts clean), touch neither the free list nor `used_bytes`,
        // and return an empty map so the caller runs no fixups. Nothing is
        // reclaimed this cycle; over-retention is the only safe response.
        let data_span = self.extent();
        if Self::close_live_set_over_old_gen(&objects, data_span, &[]).1 {
            let n = COMPACT_ESCAPE_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(
                    walked_objects = objects.len(),
                    "old-gen compaction ABANDONED: a live object references an in-old-gen \
                     address the object walk did not yield (freed block or walk desync). \
                     Relocating would leave that slot dangling; retaining the whole \
                     generation for this cycle instead.",
                );
            }
            for &(obj_ptr, _size) in &objects {
                // SAFETY: `walk_objects` yielded this as a valid object start.
                let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            return cratonvm_types::PointerMap::default();
        }

        // Phase 1: Compute forwarding addresses for live objects.
        // `write_cursor` tracks the next available byte offset (8-byte aligned).
        let mut write_cursor: usize = 0;
        let mut pointer_map: cratonvm_types::PointerMap = cratonvm_types::PointerMap::default();
        // (src_ptr, total_size, dest_ptr, saved_mark) for each live object.
        //
        // `saved_mark` exists because of the 32 -> 24 header shrink. Forwarding
        // now lives in the mark word, and this is a SLIDING compactor: it
        // installs the forward in Phase 1 and copies in Phase 3, so the write
        // that says "forwarded" destroys the object's lock state BEFORE the copy
        // that would have carried it to the destination. A copying collector
        // does not have this problem (it copies first, then clobbers the
        // source); this one does, and the failure would be silent — every thin
        // lock in the old generation reset, and every INFLATED word's single
        // strong `Arc<Monitor>` reference dropped on the floor, leaving the slid
        // survivor with no monitor. Snapshot before clobbering, restore after
        // the copy. See `header-shrink.md` §4.3.
        let mut live_objects: Vec<(*mut u8, usize, *mut u8, u32)> = Vec::new();
        // Every live object's destination, moved or not. Phase 2 resolves a
        // referent through this map rather than through a forwarding state in
        // its header: since the 8-byte header a header forward parks the
        // target in the object's SECOND WORD -- a compact instance's first
        // field, a long header's shape -- and Phase 2 reads exactly those
        // words of every live holder while the forwards would be installed.
        let mut destinations: rustc_hash::FxHashMap<usize, usize> =
            rustc_hash::FxHashMap::default();

        for &(obj_ptr, total_size) in &objects {
            let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
            if header.gc_flags() & GC_FLAG_MARKED == 0 {
                // H2-CID0 (2026-08-01): remember what this address held before
                // compaction drops it. The storage either ends up under a slid
                // survivor or inside the zeroed tail Phase 4 writes, and in the
                // latter case a still-live reference to it reads an all-zero
                // header — `ClassId(0)`, i.e. `java.lang.Object`. Recording the
                // class here is what lets the `checkcast` reporter say WHICH
                // object was reclaimed instead of only WHERE.
                crate::gen_heap::record_old_freed(
                    obj_ptr as usize,
                    total_size,
                    header.class_id.as_u32(),
                    ObjectHeader::kind_tag(
                        header.mark_word.load(std::sync::atomic::Ordering::Relaxed),
                    ),
                    crate::gen_heap::OLD_FREED_SITE_COMPACT,
                    drop_flags.get(&(obj_ptr as usize)).copied().unwrap_or(0),
                );
                continue; // Dead object — skip
            }
            let aligned = (write_cursor + 7) & !7;
            let dest = unsafe { base.add(aligned) };

            // Record the destination for Phase 2 (see `destinations`). The
            // header is left untouched; the mark snapshot is kept so Phase 3
            // can restore the word exactly after the slide.
            let saved_mark = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            destinations.insert(obj_ptr as usize, dest as usize);

            if obj_ptr != dest {
                pointer_map.insert(obj_ptr as usize, dest as usize);
            } else if crate::gc_quiescence::is_watched_referent(obj_ptr as usize) {
                // RandomizedContext WeakHashMap<Thread,...> fix (same class of
                // bug as gen_heap.rs's non-moving young sweep — see
                // gc_quiescence::is_watched_referent doc comment): this live
                // object happened not to move during sliding compaction, so
                // it gets no `pointer_map` entry ("objects that stay in place
                // are NOT included in the map", by design, above). Post-GC
                // reference processing's `is_marked` check treats an address
                // absent from `pointer_map` as "did not survive" — correct
                // for a moved object, wrong for one that simply stayed put.
                // A long-lived object (e.g. a test suite's own `Thread`
                // mirror, promoted to old gen after surviving many young
                // GCs) is exactly the kind of object likely to stay at a
                // stable position across compactions. Record an identity
                // entry so a live Weak/Soft/Phantom reference watching this
                // address is not wrongly cleared.
                pointer_map.insert(obj_ptr as usize, dest as usize);
            }
            live_objects.push((obj_ptr, total_size, dest, saved_mark));
            write_cursor = aligned + total_size;
        }

        // Phase 2: Update references within live old-gen objects.
        // Each reference slot that points to a moved old-gen object is rewritten
        // to the object's forwarding address.
        for &(obj_ptr, _, _, _) in &live_objects {
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            Self::update_refs_in_object(obj_ptr, header, data_span, &destinations);
        }

        // Phase 3: Slide objects to their forwarding addresses.
        // Processing order is low-to-high by source address, and dest ≤ src
        // for sliding compaction, so `std::ptr::copy` (memmove) is safe even
        // for overlapping regions.
        for &(src, size, dest, saved_mark) in &live_objects {
            if src != dest {
                unsafe { std::ptr::copy(src, dest, size) };
            }
            // Clear GC metadata on the (possibly moved) object. Restoring the
            // snapshot both retires the forward (the mark word IS the forwarding
            // slot now) and gives the survivor back the lock state Phase 1
            // overwrote — NEUTRAL, a thin lock's owner/recursion, or an INFLATED
            // word and the one strong `Arc<Monitor>` reference it owns.
            let final_header = unsafe { &mut *(dest as *mut ObjectHeader) };
            final_header
                .mark_word
                .store(saved_mark, std::sync::atomic::Ordering::Relaxed);
            final_header.clear_gc_flags(GC_FLAG_MARKED);
        }

        // Phase 4: Rebuild free list — one contiguous block at the end.
        // Round-5 #14: clears every bucket so deferred free()s coalesce
        // into the single trailing block formed by compaction.
        // PERF (gc-oldgen-perf): the buckets are about to be fully rewritten,
        // so the cached sorted free-block view is stale — invalidate it once.
        self.invalidate_sorted_free();
        // gengc-round2: every outstanding unzeroed-buffer carve is gone —
        // compaction rebuilds the free list from scratch and slides objects,
        // so no recorded `(offset, size)` names what it used to. Keeping stale
        // entries could only produce a FALSE ACCEPT in the tail-release check.
        #[cfg(debug_assertions)]
        self.unzeroed_buffer_carves.clear();
        let compacted_end = (write_cursor + 7) & !7;
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        if compacted_end < self.data.len() {
            // Zero the freed region for safety — but only up to the high-water
            // mark (gengc-round1, 2026-09-20). Bytes at or above `high_water`
            // have never been handed to a writer, so they are already in
            // whatever state `OldGen::new` left them in and no stale reference
            // can name an object there. Zeroing to `data.len()` instead cost
            // one `memset` of the ENTIRE trailing free block per major GC —
            // inside the pause, growing with heap CAPACITY rather than with
            // the garbage reclaimed — and committed every page of a lazily
            // mapped store that `new` goes out of its way not to touch.
            //
            // The diagnostic the zero exists for is unaffected: H2-CID0 wants
            // a still-live reference to a compacted-away object to read an
            // all-zero `ClassId(0)` header rather than plausible garbage, and
            // every object this compaction dropped lived below `high_water`.
            //
            // gen r4w2/oldgen2 (2026-09-23): and where the store can, the whole
            // granules of that range go back to the OS instead of being
            // `memset` — see `zero_or_give_back`. `[compacted_end, zero_end)`
            // holds no live object after Phase 3's slide, which is the
            // obligation `reset_range` puts on its caller.
            let zero_end = self.high_water.min(self.data.len());
            self.zero_or_give_back(compacted_end, zero_end);
            let free_size = self.data.len() - compacted_end;
            self.buckets[bucket_for(free_size)].push(FreeBlock {
                offset: compacted_end,
                size: free_size,
            });
        }
        self.used_bytes = compacted_end;
        // gen r4/oldgen: at most one block — nothing left to merge.
        self.free_list_maximal = true;
        // Everything above the compacted end is free and now provably zero
        // (either just zeroed, or never written), so the mark returns with it.
        self.high_water = compacted_end;
        // gen r4w4/oldgen4: a completed compaction leaves ONE free block, so a
        // fragmentation refusal recorded before it is answered.
        self.frag_pending_request = 0;
        self.frag_compaction_requested = false;
        self.humongous_refusal_pending = false;
        self.humongous_refused_bytes = 0;
        self.sizing.compactions = self.sizing.compactions.wrapping_add(1);

        pointer_map
    }

    /// Zero `[start, end)` — a range that provably holds no live object — and,
    /// where the backing store can do it without making the range fault,
    /// hand the physical pages of its whole granules back to the OS instead.
    ///
    /// gen r4w2/oldgen2 (2026-09-23), item 3 of
    /// `gengc-r4-oldgen-backing-store-is-committed-up-front-and-never-returned`.
    /// `compact`'s Phase 4 used to `memset` this range, which kept every page
    /// of a generation's peak resident for the life of the process *and*
    /// dirtied it. [`HeapStore::reset_range`] releases the pages but leaves
    /// them mapped and readable, reading back as zero — the H2-CID0 contract
    /// the `memset` existed for (a stale reference to a compacted-away object
    /// reads an all-zero header) and the one [`Self::alloc`] relies on for
    /// nothing, since it zeroes what it hands out anyway. `reset_range`, not
    /// `decommit_range`: a decommitted granule FAULTS on touch, and the JIT's
    /// published read bounds and [`Self::contains`] both assume this range
    /// stays readable.
    ///
    /// Only on Linux and Windows, where a reset granule is documented to read
    /// back as zero (`reservation.rs`); elsewhere (macOS's `MADV_FREE` may keep
    /// the old bytes) this is the `memset` it always was. Partial granules at
    /// either edge are zeroed, and so is the whole middle if the OS refused
    /// any part of the reset, so the range is zero on every path.
    fn zero_or_give_back(&mut self, start: usize, end: usize) {
        let end = end.min(self.data.len());
        if end <= start {
            return;
        }
        let reset_reads_zero = cfg!(any(target_os = "linux", target_os = "windows"));
        let lo = start.div_ceil(GRANULE) * GRANULE;
        let hi = (end / GRANULE) * GRANULE;
        if !(reset_reads_zero && hi > lo && matches!(self.data, HeapStore::Reserved(_))) {
            // The pre-2026-09-23 `memset`. On the reserving store `fill_zero`
            // skips an uncommitted granule, which already reads as zero.
            self.data.fill_zero(start, end);
            return;
        }
        self.data.fill_zero(start, lo);
        self.data.fill_zero(hi, end);
        let (reset, unmapped) = self.data.reset_range(lo, hi - lo, "old-gen compact tail");
        if unmapped > 0 {
            // The Windows arm released pages and the OS refused to commit them
            // again: a real hole below `readable_end`. `contains` and the
            // allocator's commit step consult the bitmap from now on.
            self.readable_holes = true;
        }
        if reset + unmapped < hi - lo {
            // Some run was refused before anything changed, so it still holds
            // its old bytes. Zero the middle the old way (`fill_zero` skips
            // whatever did go uncommitted).
            self.data.fill_zero(lo, hi);
        }
        self.bytes_given_back = self
            .bytes_given_back
            .wrapping_add((reset + unmapped) as u64);
    }

    /// gen r4w2/oldgen2 — bytes of physical memory [`Self::zero_or_give_back`]
    /// (and, since gen r4w3/oldgen3, [`Self::after_in_place_sweep`]) has
    /// handed back to the OS over this generation's life.
    pub fn bytes_given_back(&self) -> u64 {
        self.bytes_given_back
    }

    /// The whole-granule interiors `[lo, hi)` of free blocks that span at
    /// least [`GIVE_BACK_MIN_GRANULES`] granules, clipped to the high-water
    /// mark (a granule wholly above it was never written, so there is nothing
    /// physical to give back and resetting it would only cost a syscall).
    fn give_back_candidates(&self) -> Vec<(usize, usize)> {
        // gen r4w4/oldgen4: and to the committed end, which a shrink
        // (`resize_after_collection`) can lower below the high-water mark —
        // a decommitted granule has nothing physical to give back either.
        let top = self
            .high_water
            .min(self.readable_end)
            .div_ceil(GRANULE)
            .saturating_mul(GRANULE);
        let mut out = Vec::new();
        for b in self.buckets.iter().flatten() {
            let lo = b.offset.div_ceil(GRANULE).saturating_mul(GRANULE);
            let hi = ((b.offset + b.size) / GRANULE * GRANULE).min(top);
            if hi > lo && (hi - lo) / GRANULE >= GIVE_BACK_MIN_GRANULES {
                out.push((lo, hi));
            }
        }
        out
    }

    /// gen r4w3/oldgen3 (2026-09-23) — the end of an IN-PLACE old-gen sweep:
    /// sample how much could be handed back to the OS and, when `give_back`
    /// (`CRATONVM_GC_OLD_GIVE_BACK`, opt-in) allows it, hand it back.
    /// Returns the bytes given back.
    ///
    /// `gengc-r4w2-oldgen2-in-place-sweep-never-returns-pages-and-old-gen-cannot-grow`
    /// item 1. Compaction has returned the whole granules it empties since
    /// wave 2 (`zero_or_give_back`), but compaction is opt-in, so on a default
    /// run a generation that peaks once keeps its peak RSS for life. This
    /// returns the whole-granule interiors of LARGE free blocks after an
    /// in-place sweep, under three conditions:
    ///
    /// * occupancy after the sweep is BELOW the 75 % trigger floor — a
    ///   generation above it is about to refill those pages;
    /// * the block spans at least [`GIVE_BACK_MIN_GRANULES`] whole granules —
    ///   small holes are the steady-state reuse this must not turn into a
    ///   page-fault storm;
    /// * at most once per [`GIVE_BACK_EVERY`] collections.
    ///
    /// The sample (`OldGenTriggerStats::give_back_candidate_bytes`) is taken
    /// on EVERY call, flag or not: it is the number that decides whether the
    /// flag is worth an A/B on a workload.
    ///
    /// # Why `reset_range` is safe here
    ///
    /// [`HeapStore::reset_range`] must run while nothing can touch the range
    /// (its Windows arm is `MEM_DECOMMIT` + `MEM_COMMIT`, and a load between
    /// the two faults). The only caller is `GenerationalHeap::old_gen_gc`'s
    /// in-place arm, which runs inside a young stop-the-world pause with the
    /// old-gen lock held, and every byte reset here is inside a FREE block:
    ///
    /// * mutators are parked, and a mutator never reads a free block anyway
    ///   (a live reference cannot name one — that is what the sweep proved);
    /// * the concurrent marker reads old-gen memory only inside a slice that
    ///   holds the old-gen lock, and only live objects on its queue — and its
    ///   thread is parked between slices at this pause;
    /// * the concurrent SWEEP never gets here (it does not call this), and its
    ///   slices likewise hold the lock;
    /// * walks (`walk_objects`, `walk_objects_from`) read only the gaps
    ///   BETWEEN free blocks, and `is_allocated_addr` reads the free list, not
    ///   the bytes.
    ///
    /// A reset granule stays committed and readable and reads back as zero,
    /// so [`Self::contains`], `readable_end` and the JIT read bounds are
    /// unaffected; the Windows commit-limit arm (`unmapped > 0`) is handled
    /// exactly as [`Self::zero_or_give_back`] handles it. Nothing relies on a
    /// free block's stale bytes: [`Self::alloc`] zeroes what it hands out and
    /// [`Self::alloc_unzeroed`]'s caller overwrites it — the one observable
    /// change is diagnostic (a dangling read of a freed block inside a reset
    /// granule sees zeros sooner than it would after reuse).
    pub fn after_in_place_sweep(&mut self, give_back: bool) -> usize {
        let candidates = self.give_back_candidates();
        let candidate_bytes: usize = candidates.iter().map(|&(lo, hi)| hi - lo).sum();
        let s = &mut self.trigger_stats;
        s.give_back_candidate_bytes = candidate_bytes as u64;
        s.give_back_candidate_bytes_max =
            s.give_back_candidate_bytes_max.max(candidate_bytes as u64);
        if !give_back || candidates.is_empty() {
            return 0;
        }
        if !matches!(self.data, HeapStore::Reserved(_)) {
            return 0;
        }
        let cap = self.capacity();
        if cap == 0 || self.used_bytes >= cap * 75 / 100 {
            return 0;
        }
        let now = self.trigger_stats.collections;
        if self
            .last_give_back_at
            .is_some_and(|at| now.wrapping_sub(at) < GIVE_BACK_EVERY)
        {
            return 0;
        }
        self.last_give_back_at = Some(now);
        let mut total = 0usize;
        for (lo, hi) in candidates {
            let (reset, unmapped) =
                self.data
                    .reset_range(lo, hi - lo, "old-gen in-place sweep give-back");
            if unmapped > 0 {
                // A real hole below `readable_end`, as in `zero_or_give_back`:
                // `contains` and the allocator's commit step consult the
                // bitmap from now on.
                self.readable_holes = true;
            }
            total += reset + unmapped;
        }
        self.bytes_given_back = self.bytes_given_back.wrapping_add(total as u64);
        let s = &mut self.trigger_stats;
        s.in_place_give_backs = s.in_place_give_backs.wrapping_add(1);
        s.in_place_given_back_bytes = s.in_place_given_back_bytes.wrapping_add(total as u64);
        total
    }

    /// Invoke `f` once for each in-old-gen reference target of `obj_ptr`.
    ///
    /// Mirrors the slot-walking logic in [`Self::update_refs_in_object`]
    /// (object fields are 16-byte `Value` slots; reference arrays are 8-byte
    /// compact pointers) but only *reads* targets — it never writes the slot.
    /// Targets outside `[data_start, data_end)` (young gen, metaspace, native)
    /// are filtered out, matching the bounds gate Phase 2 uses, so a caller
    /// only ever sees old-gen referents.
    ///
    /// `total_size` MUST be the size `walk_objects` computed for this exact
    /// object (`HEADER_SIZE + object_body_size(..)` at walk time) and is used
    /// to cap every arm's iteration count — the same pattern
    /// `mark_young_to_old_refs` already uses (`max_slots = body_bytes /
    /// SLOT_SIZE`). `HIB-DCAST-LATEPHASE.1`: this function used to re-read
    /// `header.array_length()`/`header.num_slots()` fresh at call time and
    /// trust them completely, with no cap at all. A caller may run this on an
    /// object well after `walk_objects` validated it — `close_live_set_over_old_gen`'s
    /// Phase 0 fixpoint, for one, calls this from a `for &(obj_ptr, _size) in
    /// objects` loop with `_size` unused — and a header field that reads
    /// differently on that later pass than it did during the walk (this file
    /// and its siblings document several distinct causes of exactly that) hits
    /// an UNBOUNDED `for slot_idx in 0..num_slots` stride into unmapped
    /// memory: the deterministic `SIGSEGV` inside this function's inlined
    /// legacy-object arm, reached via `close_live_set_over_old_gen`, against
    /// the real `DefaultCatalogAndSchemaTest` workload. Capping by the
    /// WALKED size — ground truth this function does not need to re-derive
    /// or guess at — closes that regardless of why the live re-read
    /// disagrees.
    ///
    /// The header is *not* borrowed across the `f` callback: the four scalar
    /// fields needed to drive the walk are copied out up front via
    /// `read_unaligned` on raw field pointers. This matters because a caller
    /// (e.g. the Phase 0 closure) may write to the *referent's* header from
    /// inside `f`, and for a self-referential object the referent is this very
    /// header — holding a live `&ObjectHeader` across that write would alias a
    /// `&mut` to the same bytes. Reading scalars up front keeps the borrow
    /// short and the walk sound under a self-loop.
    fn for_each_old_gen_ref(
        obj_ptr: *mut u8,
        total_size: usize,
        data: (usize, usize),
        mut f: impl FnMut(usize),
    ) {
        // gen r4w2/oldgen2: `[start, end)` of the backing store as integers
        // (`OldGen::extent`) — it used to be `&self.data` as a slice, which on
        // a reserving store would claim every reserved page is readable.
        let (data_start, data_end) = data;
        let body_bytes = total_size.saturating_sub(HEADER_SIZE);

        // Snapshot the layout-driving fields, then drop the reference before
        // any callback runs (see the aliasing note above — `f` may mutate the
        // object's header/fields).
        //
        // gc-common w3-e (2026-09-23): the snapshot used to include the
        // compact oop map as an owned `Arc` (`compact_oop_scan`), i.e. an
        // atomic clone/drop pair on the class layout's refcount per walked
        // object. It now keeps the two scalars the lookup is keyed on and
        // resolves the layout BY VALUE below, through
        // `with_compact_oop_scan_ids`, which never holds a `&ObjectHeader`
        // while `f` runs.
        let (kind, element_type, array_length, num_slots, is_compact, class_id) = unsafe {
            let h = &*(obj_ptr as *const ObjectHeader);
            (
                h.kind(),
                h.element_type(),
                h.array_length(),
                h.num_slots(),
                crate::is_compact_object(h),
                h.class_id.as_u32(),
            )
        };

        if kind == ObjectKind::Array {
            if element_type == ArrayElementType::Reference {
                // Cap by the WALKED size, not the freshly-read `array_length`
                // — see this function's doc comment.
                let max_elems = body_bytes / ref_element_size();
                let elems = (array_length as usize).min(max_elems);
                for i in 0..elems {
                    let slot = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET + i * ref_element_size()) };
                    let raw: u64 = unsafe { read_ref_slot(slot) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            f(ref_ptr);
                        }
                    }
                }
            }
        } else if is_compact {
            // HIB-DCAST-LATEPHASE.1: `compact_oop_scan` returns `None` both
            // for "this is a legacy object" (its documented contract) and,
            // via its internal `class_layout_for_fields(..)?`, for "this IS
            // a compact object (`GC_FLAG_COMPACT` set) but its class's
            // layout is not registered right now". Gating on `is_compact`
            // (the header bit, independent of the registry) rather than
            // `compact.is_some()` keeps the second case from falling into
            // the legacy arm below, which would misread this object's
            // packed compact body under the legacy `num_slots * SLOT_SIZE`
            // formula and stride past its real extent — a `SIGSEGV` reached
            // this way against the real `DefaultCatalogAndSchemaTest`
            // workload. A compact object whose layout cannot be resolved has
            // no provably-safe reference slots to visit; skip it.
            let _ = crate::heap::with_compact_oop_scan_ids(class_id, num_slots, |layout, body| {
                // Compact object: 8-byte reference slots at the oop-map offsets.
                for &off in &layout.ref_disps {
                    let off = off as usize;
                    if off + ref_field_size() > body {
                        break;
                    }
                    let slot = unsafe { obj_ptr.add(off) };
                    let raw: u64 = unsafe { read_ref_slot(slot) };
                    if raw != 0 {
                        let ref_ptr = raw as usize;
                        if ref_ptr >= data_start && ref_ptr < data_end {
                            f(ref_ptr);
                        }
                    }
                }
            });
        } else {
            // Cap by the WALKED size, not the freshly-read `num_slots` — see
            // this function's doc comment.
            let max_slots = body_bytes / SLOT_SIZE;
            let slots = (num_slots as usize).min(max_slots);
            for slot_idx in 0..slots {
                let slot = unsafe { obj_ptr.add(HEADER_SIZE + slot_idx * SLOT_SIZE) };
                let value = unsafe { std::ptr::read(slot as *const Value) };
                if let Value::Object(Some(ref_obj)) = value {
                    let ref_ptr = ref_obj.as_ptr() as usize;
                    if ref_ptr >= data_start && ref_ptr < data_end {
                        f(ref_ptr);
                    }
                }
            }
        }
    }

    /// Promote any UNMARKED old-gen object reachable from a MARKED old-gen
    /// object to live (set `GC_FLAG_MARKED`), iterating to a fixpoint.
    ///
    /// This is the dangling-reference guard described in [`Self::compact`]
    /// Phase 0. After this returns, the marked set is closed under the
    /// old-gen reference relation, so every reference a live object holds to
    /// an in-old-gen target will see a non-null `forwarding_ptr` in Phase 2 —
    /// no live referrer can be left pointing at an un-relocated (and about to
    /// be overwritten/zeroed) target.
    ///
    /// `objects` is the address-ordered `(ptr, size)` list from
    /// [`Self::walk_objects`]; it is treated as the complete set of old-gen
    /// objects. The closure is conservative (it only ever *adds* survivors)
    /// so it can never drop a live object; the ordinary worst case is
    /// retaining a little floating garbage for one extra cycle.
    ///
    /// Cost: one linear pass over `objects` that scans every MARKED object's
    /// reference slots, plus a worklist drain that scans each object the pass
    /// promoted BEHIND its cursor exactly once. O(objects + live refs) in
    /// every case.
    ///
    /// gen r4/oldgen (2026-09-23): this used to be a re-scan-everything
    /// fixpoint — repeat the linear pass until one promotes nothing. The
    /// healthy case was the same single pass, but every further level of an
    /// under-marked chain that points BACKWARDS in address order cost another
    /// full pass over the whole live set, i.e. O(passes × live refs), and the
    /// unhealthy case is exactly when the collector is already in trouble.
    /// The result is identical, not approximately so: the final marked set is
    /// the least set containing the initial marks and closed under walked-base
    /// edges out of marked objects, every object in it is scanned exactly once
    /// (a referent promoted AHEAD of the cursor is scanned when the cursor
    /// reaches it, since it is marked by then; one promoted at or behind it is
    /// queued), so `escaped` sees the same edges and `objects_promoted` counts
    /// the same 0→1 flips. `tests::the_worklist_closure_matches_the_old_fixpoint_exactly`
    /// holds the old algorithm as its oracle. Instruments:
    /// [`CLOSE_LIVE_SET_CALLS`] and its siblings, which also give the old
    /// algorithm's pass count for free.
    ///
    /// # GCAUD-2 (2026-08-01) — the escape case, and why it must fail closed
    ///
    /// "`objects` is the complete set of old-gen objects" is an assumption,
    /// not a check, and the whole Phase 0 → Phase 1 → Phase 2 chain rests on
    /// it. A live object can hold an in-old-gen reference to an address that
    /// is **not** in `objects` for two reachable reasons:
    ///
    /// * the target's block is on the FREE list — `walk_objects` derives
    ///   allocated extents as the gaps between free blocks, so a freed block
    ///   is invisible to it. That is exactly the state the in-place sweep
    ///   leaves behind when the mark under-marks (defect 4 in
    ///   `map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md`);
    /// * `scan_region` hit a header anomaly and `break`ed out of an allocated
    ///   region, dropping every object after it in that region.
    ///
    /// The pre-fix code responded to both by blindly `|=`-ing `GC_FLAG_MARKED`
    /// into `ref_ptr + cratonvm_types::GC_FLAGS_BYTE_OFFSET` — an unvalidated byte inside a free
    /// block or another object's payload — and then Phase 1, which iterates
    /// only `objects`, gave that address no forwarding pointer. Phase 2 leaves
    /// the referrer's slot on the pre-compaction address, and Phase 3 slides a
    /// different object onto it: a dangling pointer manufactured by the
    /// compactor itself, i.e. precisely the failure the whole of Phase 0
    /// exists to prevent.
    ///
    /// So an escaped referent is now (a) never written through, and (b)
    /// reported to the caller, which abandons the compaction. Over-retaining
    /// the entire old generation for one cycle is the fail-safe answer to "I
    /// cannot relocate this heap without leaving a dangling slot".
    ///
    /// Returns `(objects_promoted, escaped)`. `escaped` is `true` when the
    /// closure ran off `objects`; `objects_promoted` counts the under-marked
    /// objects this pass rescued and is the signal that the marker missed a
    /// push site (it is `0` on every healthy cycle).
    ///
    /// gen r4w4/oldgen4: `gaps` are the ABSOLUTE `[start, end)` ranges the
    /// walk could not cover (ascending; empty on a complete walk). A reference
    /// into one is not an escape: the gap's bytes stay allocated (the sweep
    /// never frees what it did not walk) and its outgoing references were
    /// already marked conservatively by the caller. The compactor always
    /// passes `&[]` — it never runs over a gap.
    fn close_live_set_over_old_gen(
        objects: &[(*mut u8, usize)],
        data: (usize, usize),
        gaps: &[(usize, usize)],
    ) -> (usize, bool) {
        Self::close_live_set_over_old_gen_collecting(objects, data, gaps, None)
    }

    /// [`Self::close_live_set_over_old_gen`], also appending the BASE of
    /// every object it rescued to `rescued_out` (in rescue order) when given.
    ///
    /// gce e1/c: the true-root major needs the rescued objects themselves,
    /// not just their count, to trace their young targets
    /// (`gcd-d9a-true-root-seed-skips-young-targets-of-close-live-set-rescues-20260928`).
    fn close_live_set_over_old_gen_collecting(
        objects: &[(*mut u8, usize)],
        data: (usize, usize),
        gaps: &[(usize, usize)],
        mut rescued_out: Option<&mut Vec<usize>>,
    ) -> (usize, bool) {
        let in_gap = |addr: usize| {
            let i = gaps.partition_point(|&(s, _)| s <= addr);
            i > 0 && addr < gaps[i - 1].1
        };
        // `walk_objects` yields ascending object starts, so a binary search
        // over the same slice is an exact membership test with no extra
        // allocation. Assert the ordering rather than assume it: this is the
        // predicate that decides whether a mark-bit write is legal.
        debug_assert!(
            objects.windows(2).all(|w| w[0].0 < w[1].0),
            "close_live_set_over_old_gen requires ascending object starts",
        );
        // Index of `addr` in `objects` iff it is a walked object BASE. The
        // index (not just membership) is what lets the linear pass tell a
        // referent it has already passed from one it has yet to reach.
        let walked_index = |addr: usize| {
            objects
                .binary_search_by_key(&addr, |&(p, _)| p as usize)
                .ok()
        };

        let mut escaped = false;
        let mut promoted_total = 0usize;
        let mut drain_promoted = 0usize;
        let mut worklist_scans = 0usize;
        // Indices into `objects` of objects promoted after the linear cursor
        // had already passed them, so nothing else will ever scan them.
        let mut worklist: Vec<usize> = Vec::new();

        // Phase A — one linear pass, in address order, over every object that
        // is marked when the cursor reaches it (which includes anything an
        // earlier object in this pass promoted ahead of the cursor).
        for (i, &(obj_ptr, size)) in objects.iter().enumerate() {
            // Snapshot the marked bit; don't hold a header borrow while the
            // closure below may write the same header (self-loop case).
            // SAFETY: `obj_ptr` is an object base `walk_objects` yielded, and
            // nothing has relocated it.
            let is_marked =
                unsafe { (*(obj_ptr as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 };
            if !is_marked {
                continue; // only trace *live* referrers
            }
            Self::for_each_old_gen_ref(obj_ptr, size, data, |ref_ptr| {
                // GCAUD-2: `for_each_old_gen_ref` filters only on the backing
                // store's [start, end) range — no alignment, no object-start
                // validation. Writing a mark bit through an address the
                // compactor cannot also FORWARD is what turns an under-marking
                // bug into heap corruption, so refuse the write and record the
                // escape instead.
                let Some(j) = walked_index(ref_ptr) else {
                    escaped |= !in_gap(ref_ptr);
                    return;
                };
                // SAFETY: `ref_ptr` was just proven to be one of the object
                // bases `walk_objects` yielded, so it is a valid old-gen object
                // header. The pre-pass runs before any relocation, so the
                // referent is still at its original address. No outstanding
                // borrow of this header is live here (fields were copied out
                // before the walk), so the `&mut` does not alias.
                let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                    ref_header.add_gc_flags(GC_FLAG_MARKED);
                    promoted_total += 1;
                    if let Some(out) = rescued_out.as_deref_mut() {
                        out.push(ref_ptr);
                    }
                    // Ahead of the cursor: this pass scans it when it gets
                    // there. Behind it (`j == i` is impossible — `i` is
                    // marked): nothing will, so queue it.
                    if j < i {
                        worklist.push(j);
                    }
                }
            });
        }

        // Phase B — drain. Every entry was promoted exactly once (the 0→1 flip
        // is what queues it), so every object is scanned at most once here and
        // never also by Phase A.
        while let Some(j) = worklist.pop() {
            let (obj_ptr, size) = objects[j];
            worklist_scans += 1;
            Self::for_each_old_gen_ref(obj_ptr, size, data, |ref_ptr| {
                // GCAUD-2, as above.
                let Some(k) = walked_index(ref_ptr) else {
                    escaped |= !in_gap(ref_ptr);
                    return;
                };
                // SAFETY: as in Phase A — `ref_ptr` is a walked object base,
                // not yet relocated, and no borrow of its header is live.
                let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                    ref_header.add_gc_flags(GC_FLAG_MARKED);
                    promoted_total += 1;
                    drain_promoted += 1;
                    if let Some(out) = rescued_out.as_deref_mut() {
                        out.push(ref_ptr);
                    }
                    worklist.push(k);
                }
            });
        }

        use std::sync::atomic::Ordering::Relaxed;
        CLOSE_LIVE_SET_CALLS.fetch_add(1, Relaxed);
        if promoted_total > 0 {
            CLOSE_LIVE_SET_RESCUED.fetch_add(promoted_total as u64, Relaxed);
            CLOSE_LIVE_SET_MULTI_PASS_CALLS.fetch_add(1, Relaxed);
        }
        if drain_promoted > 0 {
            CLOSE_LIVE_SET_DEEP_CALLS.fetch_add(1, Relaxed);
        }
        if worklist_scans > 0 {
            CLOSE_LIVE_SET_WORKLIST_SCANS.fetch_add(worklist_scans as u64, Relaxed);
        }
        (promoted_total, escaped)
    }

    /// Close the marked set over "referenced by a live old-gen object" **in
    /// place**, without relocating anything.
    ///
    /// This is the same fixpoint [`Self::compact`]'s Phase 0 runs
    /// ([`Self::close_live_set_over_old_gen`]), exposed for the *other* old-gen
    /// reclaimer: the in-place sweep in
    /// `GenerationalHeap::old_gen_gc(compact = false)`, which is the collector
    /// that runs whenever a live JIT frame makes the root set conservative.
    ///
    /// # Why the in-place sweep needs it (GCAUD-8)
    ///
    /// That sweep decides "free this block" from `GC_FLAG_MARKED == 0` and
    /// nothing else, and its mark has eight push sites, each of which can fail
    /// *open* — dropping a genuine reference instead of over-retaining. When
    /// one does, the sweep hands a still-referenced block back to the free
    /// list. The compactor has been closed against exactly that under-marking
    /// since Phase 0 was written ("the compactor must not corrupt the heap when
    /// the marker under-marks"); the sweep was not, even though it is the arm
    /// that *creates* the freed-but-referenced block the compactor then refuses
    /// to relocate (`COMPACT_ESCAPE_HITS`).
    ///
    /// Running the closure before the free loop makes the two arms agree: an
    /// unmarked object that a marked object still points at is promoted to
    /// live and retained for one more cycle, transitively. Genuine garbage —
    /// unmarked and unreferenced — is untouched by the closure and is still
    /// freed, which is what the positive controls pin.
    ///
    /// `objects` must be the `walk_objects()` output for THIS old gen, taken
    /// under the same lock and with nothing allocated or freed since: the
    /// closure's admission proof is membership in that slice, so a stale slice
    /// would authorise a mark-bit write into a recycled address.
    ///
    /// Returns `(objects_promoted, escaped)`. `escaped` has the same meaning as
    /// in Phase 0 — a live referrer named an in-old-gen address the walk did
    /// not yield, i.e. a block that is *already* on the free list (or behind a
    /// `scan_region` anomaly). The sweep cannot un-free such a block, so it
    /// reports rather than aborts; see the call site.
    pub fn close_live_set(&self, objects: &[(*mut u8, usize)]) -> (usize, bool) {
        Self::close_live_set_over_old_gen(objects, self.extent(), &[])
    }

    /// [`Self::close_live_set`] over a walk that left `gaps` (OFFSETS, as
    /// [`Self::walk_objects_with_gaps`] returns them) uncovered: a reference
    /// into a gap is not reported as an escape, because the gap is retained
    /// whole and its own references are the caller's conservative roots.
    ///
    /// gen r4w4/oldgen4 — the in-place sweep's walk-gap recovery; without this
    /// every recovered cycle would also trip `OLD_SWEEP_ESCAPE_HITS`, the
    /// tripwire for a REAL escape (a block already freed under a live
    /// pointer), and the two would be indistinguishable in a log.
    pub fn close_live_set_with_gaps(
        &self,
        objects: &[(*mut u8, usize)],
        gaps: &[(usize, usize)],
    ) -> (usize, bool) {
        let base = self.data.as_ptr() as usize;
        let abs: Vec<(usize, usize)> = gaps.iter().map(|&(s, e)| (base + s, base + e)).collect();
        Self::close_live_set_over_old_gen(objects, self.extent(), &abs)
    }

    /// [`Self::close_live_set`] (or, with non-empty `gaps` OFFSETS,
    /// [`Self::close_live_set_with_gaps`]), appending the base of every object
    /// it rescued to `rescued_out`. Same counters, same marks.
    ///
    /// gce e1/c: the true-root major runs this inside its mark, before the
    /// finalizer round, and sends what it rescues back through the mark body,
    /// so the rescued objects' young targets reach the fixed point.
    pub fn close_live_set_collecting(
        &self,
        objects: &[(*mut u8, usize)],
        gaps: &[(usize, usize)],
        rescued_out: &mut Vec<usize>,
    ) -> (usize, bool) {
        let base = self.data.as_ptr() as usize;
        let abs: Vec<(usize, usize)> = gaps.iter().map(|&(s, e)| (base + s, base + e)).collect();
        Self::close_live_set_over_old_gen_collecting(
            objects,
            self.extent(),
            &abs,
            Some(rescued_out),
        )
    }

    /// Update reference slots within a single live old-gen object so they point
    /// to forwarding addresses of moved objects.
    ///
    /// Handles both regular object fields (16-byte `Value` slots) and reference
    /// arrays (8-byte compact pointers).
    ///
    /// Dangling-ref invariant: by the time this runs, Phase 0
    /// ([`Self::close_live_set_over_old_gen`]) has promoted every in-old-gen
    /// target of a live object to live, and Phase 1 has stamped a forwarding
    /// address on every live object. Therefore any in-bounds target read here
    /// MUST have a non-null `forwarding_ptr`; a null one would mean a live
    /// referrer points at floating garbage and the rewrite below would be
    /// skipped, leaving a dangling pointer. We `debug_assert!` against that to
    /// catch a regression in the closure, and in release simply leave the slot
    /// untouched (the closure makes this case unreachable in practice).
    fn update_refs_in_object(
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        data: (usize, usize),
        destinations: &rustc_hash::FxHashMap<usize, usize>,
    ) {
        let (data_start, data_end) = data;

        // Remap every reference into a relocated (forwarded) in-old-gen object.
        // Arrays + compact objects use 8-byte pointer slots; legacy objects use
        // 16-byte Value cells. `forward_ref_slots` rewrites a slot only when the
        // closure returns Some(new_addr).
        // SAFETY: `obj_ptr`/`header` are a valid live old-gen object under STW.
        unsafe {
            crate::gen_heap::forward_ref_slots(obj_ptr, header, |ref_ptr| {
                let r = ref_ptr as usize;
                if r < data_start || r >= data_end {
                    return None; // reference outside the compacted old-gen region
                }
                if let Some(&dest) = destinations.get(&r) {
                    if seedhunt_enabled() && dest < 0x1000 {
                        let ref_header = &*(ref_ptr as *const ObjectHeader);
                        eprintln!(
                            "[gcfwd] write small fwd: holder@0x{:x} cid={} \
                             referent@0x{:x} cid={} marked={} fwd=0x{:x}",
                            obj_ptr as usize,
                            header.class_id.as_u32(),
                            r,
                            ref_header.class_id.as_u32(),
                            ref_header.gc_flags() & GC_FLAG_MARKED != 0,
                            dest,
                        );
                    }
                    Some(dest as *mut u8)
                } else {
                    // Dangling-ref guard: an in-old-gen target with a null
                    // forwarding pointer is an UNMARKED object that Phase 0
                    // should have promoted. If this fires, the live closure
                    // missed an edge and leaving the slot as-is would dangle.
                    debug_assert!(
                        false,
                        "old_gen.compact: live holder@0x{:x} -> unmarked old-gen \
                         referent@0x{:x} (no forwarding addr); \
                         close_live_set_over_old_gen missed an edge",
                        obj_ptr as usize, r,
                    );
                    None
                }
            });
        }
    }

    /// Returns the number of contiguous free blocks (fragmentation metric).
    /// Total bytes on the free list, and the size of the LARGEST single
    /// free block. The pair is what distinguishes "the generation is full"
    /// from "the generation is mostly free but too fragmented to serve this
    /// request" — the failure mode that produced a spurious
    /// `OutOfMemoryError` before the free list learned to coalesce without
    /// compaction. Walks every bucket, so it is a diagnostic-path helper, not
    /// an allocator fast path.
    pub fn free_bytes_and_largest(&self) -> (usize, usize) {
        let mut total = 0usize;
        let mut largest = 0usize;
        for b in self.buckets.iter().flatten() {
            total += b.size;
            largest = largest.max(b.size);
        }
        (total, largest)
    }

    pub fn free_block_count(&self) -> usize {
        // Round-5 #14: count across all size buckets.
        self.buckets.iter().map(|b| b.len()).sum()
    }

    /// Returns the size of the largest contiguous free block.
    pub fn largest_free_block(&self) -> usize {
        // Round-5 #14: the largest block lives in the highest non-empty bucket;
        // scan that bucket for the actual maximum.
        for bucket in self.buckets.iter().rev() {
            if let Some(&max) = bucket.iter().map(|b| &b.size).max() {
                return max;
            }
        }
        0
    }

    // -----------------------------------------------------------------
    // Fragmentation policy (gengc-round2)
    // -----------------------------------------------------------------

    /// The largest single request this generation has served since the last
    /// [`Self::repair_fragmentation`].
    ///
    /// This is the scale the fragmentation predicate compares the largest free
    /// block against: "there are bytes but no room" is only meaningful
    /// relative to the size of the thing that will next ask for room, and the
    /// allocator is the only component that sees every size.
    pub fn recent_max_request(&self) -> usize {
        self.recent_max_request
    }

    /// How many times [`Self::repair_fragmentation`] has actually merged
    /// something on this generation. Per-instance, not a process global, so a
    /// multi-heap process reports per heap.
    pub fn fragmentation_repairs(&self) -> u64 {
        self.fragmentation_repairs
    }

    /// Does the free list look FRAGMENTED rather than FULL?
    ///
    /// Pure function of the four numbers, so a policy can be tested without a
    /// heap — which is the point: this predicate is the thing
    /// `docs/internal/gc/gengc-oldgen-fragmentation-has-no-trigger-FIXED-20260924.md`
    /// says is missing, and a predicate nobody can test in isolation is how it
    /// stays missing.
    ///
    /// It is the literal reading of the sentence `free_bytes_and_largest`'s
    /// own doc uses — "the generation is mostly free but too fragmented to
    /// serve this request" — with `recent_max_request` standing in for "this
    /// request". Three clauses, each suppressing a specific false positive:
    ///
    /// * `recent_max_request == 0` — nothing has been allocated since the last
    ///   repair, so there is no scale to judge "too small" against, and a
    ///   generation that is merely idle must not provoke a pause. This is also
    ///   the HYSTERESIS: [`Self::repair_fragmentation`] resets the running
    ///   max, so the predicate cannot fire again until real allocation
    ///   resumes.
    /// * `free_bytes < K * recent_max_request` — the generation is genuinely
    ///   near-full for the sizes it is being asked for. Coalescing cannot
    ///   conjure bytes that are not there, and the OOM escalation ladder is
    ///   the right response, not a scheduled repair.
    /// * `largest_free >= recent_max_request` — some single hole still serves
    ///   the biggest thing recently asked for, so the free list is doing its
    ///   job however many pieces it is in. Fragmentation that costs nothing is
    ///   not worth a pause.
    ///
    /// Note that `capacity` is deliberately UNUSED in the decision: a
    /// capacity-relative floor answers "is the heap full", which is a
    /// different question and one the OOM ladder already asks. It is kept in
    /// the signature because a policy that grows a hysteresis or a
    /// "don't bother below X" term will want it, and because a caller reading
    /// the three live numbers has it to hand anyway.
    ///
    /// `K` = [`FRAGMENTATION_FIT_FACTOR`] is **not tuned**; no measurement was
    /// possible in the session that added it and none is claimed.
    pub fn fragmentation_warrants_repair(
        capacity: usize,
        free_bytes: usize,
        largest_free: usize,
        recent_max_request: usize,
    ) -> bool {
        let _ = capacity;
        if recent_max_request == 0 {
            return false;
        }
        if free_bytes < FRAGMENTATION_FIT_FACTOR.saturating_mul(recent_max_request) {
            return false;
        }
        largest_free < recent_max_request
    }

    /// [`Self::fragmentation_warrants_repair`] evaluated against this
    /// generation's live numbers.
    ///
    /// Walks every bucket (via [`Self::free_bytes_and_largest`]), so this is a
    /// once-per-reclamation policy question, not something for an allocation
    /// path.
    ///
    /// **Nothing in the collector calls this yet.** The collection decision
    /// lives in `gen_heap.rs`; see the gap page for the shape of the policy
    /// that should consume it, and in particular for why the
    /// conservative-root path — which cannot compact at all — is the one that
    /// needs [`Self::repair_fragmentation`] rather than a compaction.
    ///
    /// gen r4/oldgen (2026-09-23) re-check: the in-place old-gen sweep — which
    /// is every old-gen collection on a default run, compacting or not, see the
    /// module header — already ends with an unconditional
    /// `coalesce_free_blocks`, and has since 2026-07-31. So a repair scheduled
    /// by this predicate only adds value BETWEEN old-gen collections — in
    /// practice, the tails promotion buffers release each young cycle, since
    /// the concurrent sweep (`concurrent_mark.rs`) coalesces after freeing
    /// too — and a
    /// fragmentation-triggered COMPACTION cannot run at all until
    /// `CRATONVM_OLDGEN_COMPACT` stops being opt-in.
    pub fn fragmentation_repair_due(&self) -> bool {
        let (free_bytes, largest) = self.free_bytes_and_largest();
        Self::fragmentation_warrants_repair(
            self.capacity(),
            free_bytes,
            largest,
            self.recent_max_request,
        )
    }

    /// The scheduled, *state*-triggered counterpart to the coalesce-on-failure
    /// step inside [`Self::alloc_impl`]: merge adjacent free blocks now,
    /// rather than when an allocation has already failed.
    ///
    /// Returns how many blocks the merge eliminated. Resets
    /// [`Self::recent_max_request`], which is what keeps
    /// [`Self::fragmentation_repair_due`] from firing every cycle on a
    /// generation that is legitimately tight — see that predicate's first
    /// clause.
    ///
    /// Safe to call at any safepoint: it only ever does work the collector is
    /// already willing to do on the allocation-failure path, and it never
    /// moves an object, so the conservative-root path can run it.
    pub fn repair_fragmentation(&mut self) -> usize {
        let merged = self.coalesce_free_blocks();
        if merged > 0 {
            self.fragmentation_repairs = self.fragmentation_repairs.wrapping_add(1);
        }
        self.recent_max_request = 0;
        merged
    }
}

// ---------------------------------------------------------------------------
// gen r4w4/oldgen4 (2026-09-24): sizing, fragmentation trigger, walk-gap
// recovery. A separate `impl` block so the hunks the other wave-4 lanes touch
// in the main one (trigger, BOT hooks) stay textually apart from these.
// ---------------------------------------------------------------------------
impl OldGen {
    /// This wave's counters, with the live sizing numbers filled in.
    pub fn sizing_stats(&self) -> OldGenSizingStats {
        OldGenSizingStats {
            reserved_bytes: self.data.len() as u64,
            committed_bytes: self.data.committed_bytes() as u64,
            committed_peak: self
                .sizing
                .committed_peak
                .max(self.data.committed_bytes() as u64),
            commit_floor: self.commit_floor as u64,
            fragmentation_pending_request: self.frag_pending_request as u64,
            growth_max_bytes: self.growth_max.max(self.data.len()) as u64,
            ..self.sizing
        }
    }

    /// The generation's COMMITTED size — HotSpot's "committed" for the tenured
    /// pool (`MemoryUsage.getCommitted()` of `Tenured Gen`). Between
    /// [`Self::committed_bytes`]'s granules and the reservation
    /// ([`Self::capacity`], the pool's `max`): it grows as allocation commits
    /// pages and falls when [`Self::resize_after_collection`] decommits the
    /// free tail. Never below the used bytes it holds.
    pub fn committed_capacity(&self) -> usize {
        self.data.committed_bytes().min(self.data.len()).max(self.used_bytes)
    }

    /// Where the generation's trailing free run starts: the smallest `t` such
    /// that `[t, capacity)` is covered by free blocks (so no allocated byte
    /// lies at or above it). `capacity` when the last byte is allocated.
    ///
    /// Tolerates an un-coalesced tail (`[a, b)`, `[b, cap)` chains) and an
    /// overlapping list (a double free: `cursor`-style `max`), in both cases
    /// only ever answering HIGHER than the truth — the safe direction for a
    /// shrink bound.
    fn tail_free_start(&self) -> usize {
        let len = self.data.len();
        self.with_sorted_free_blocks(|sorted| {
            let mut t = len;
            for b in sorted.iter().rev() {
                let end = b.offset.saturating_add(b.size);
                if end >= t && b.offset < t {
                    t = b.offset;
                } else if end < t {
                    break;
                }
            }
            t
        })
    }

    /// gen r4w4/oldgen4 — resize the COMMITTED generation at the end of a
    /// stop-the-world old-gen collection (`gengc-r4w2-oldgen2-in-place-sweep-never-returns-pages-and-old-gen-cannot-grow`).
    ///
    /// **Growth** needs no call: the allocator commits what it hands out, so
    /// the committed end follows the live set up to the reservation. This is
    /// the other direction. When `allow_shrink`:
    ///
    /// * **target** = `max(commit_floor, tail_free_start, used * 100 / (100 -
    ///   OLD_MAX_FREE_PERCENT))`, granule-rounded up (with [`READ_SLACK`]) —
    ///   HotSpot's `MaxHeapFreeRatio` rule, never below `-Xms`, never below
    ///   the last allocated byte;
    /// * **damped**: not when the committed end ADVANCED since the previous
    ///   collection's resize — the generation needed more memory during that
    ///   interval, and would re-commit what a shrink released (a steady
    ///   promoter would otherwise decommit and re-fault the same granules
    ///   every cycle). HotSpot damps with `_shrink_factor` (0, 10, 40, 100 %
    ///   over consecutive shrinking GCs) for the same reason;
    /// * **worth it**: at least [`SHRINK_MIN_GRANULES`] whole granules.
    ///
    /// The shrink is `HeapStore::decommit_range` of `[keep_end, capacity)` and
    /// `readable_end` lowered to `keep_end`, so `committed_bytes` — and with it
    /// `Runtime.totalMemory()` and the pool's committed — falls.
    ///
    /// # Why this is safe (the invariants)
    ///
    /// * Every byte decommitted is inside the trailing FREE run: `keep_end >=
    ///   tail_free_start`, so no object, live or dead-but-unfreed, is there.
    /// * `contains` answers from `readable_end`, which is lowered in the same
    ///   step, and the lock-free conservative screen reads the store's live
    ///   commit bitmap, whose bits `decommit_range` clears BEFORE the syscall.
    /// * The allocator re-commits on hand-out from `min(block.offset,
    ///   readable_end)` (see `alloc_from_buckets_scan`), so a block that now
    ///   starts above the committed end is backed from the committed end up —
    ///   `readable_end` stays a single trustworthy number.
    /// * Walks read only allocated regions; the BOT consults only cards whose
    ///   first byte is allocated; the card table and the concurrent marker's
    ///   bitmap cover the RESERVATION and are not resized; the card scan
    ///   bound is monotone and may stay above the committed end (a clean
    ///   card there is never read as an object).
    /// * `high_water` is NOT lowered: bytes at or above it were never written
    ///   (and a decommitted granule reads as zero when re-committed), so its
    ///   "above here is zero" contract still holds.
    /// * The caller holds the old-gen lock inside a stop-the-world pause: the
    ///   concurrent marker and sweep read old-gen memory only inside slices
    ///   that hold the same lock, and only allocated memory.
    ///
    /// The one behaviour change is for a DANGLING reference into the released
    /// tail (a use-after-free the collector already committed): a compiled
    /// load through it faults instead of reading stale bytes, exactly as for
    /// the young generation's `CRATONVM_GEN_UNCOMMIT`. `CRATONVM_GC_OLD_SHRINK=0`
    /// is the kill switch (the caller passes `allow_shrink = false`).
    pub fn resize_after_collection(&mut self, allow_shrink: bool) -> OldGenResize {
        let mut out = OldGenResize::default();
        // No previous resize counts as "advanced": the generation has only
        // ever grown so far, and the first shrinking collection shrinks
        // nothing — HotSpot's `_shrink_factor` starts at 0 % for the same
        // reason.
        let advanced = self
            .readable_end_at_last_resize
            .is_none_or(|prev| self.readable_end > prev);
        if allow_shrink && matches!(self.data, HeapStore::Reserved(_)) {
            let len = self.data.len();
            let by_ratio = self
                .used_bytes
                .saturating_mul(100)
                / (100 - OLD_MAX_FREE_PERCENT);
            let target = self
                .commit_floor
                .max(self.tail_free_start())
                .max(by_ratio)
                .min(len);
            let keep_end = target
                .saturating_add(READ_SLACK)
                .div_ceil(GRANULE)
                .saturating_mul(GRANULE)
                .min(len);
            let committed_above = self.data.committed_spans(keep_end, len);
            let committed_above: usize = committed_above.iter().map(|&(_, l)| l).sum();
            if committed_above >= SHRINK_MIN_GRANULES * GRANULE {
                if advanced {
                    out.damped = true;
                    self.sizing.shrinks_damped = self.sizing.shrinks_damped.wrapping_add(1);
                } else {
                    // `usize::MAX - keep_end` reaches the END OF THE
                    // RESERVATION (rounded up to a granule), so a capacity
                    // that is not a granule multiple still has its last
                    // partial granule released.
                    let released = self.data.decommit_range(
                        keep_end,
                        usize::MAX - keep_end,
                        "old-gen soft-capacity shrink",
                    );
                    if released > 0 {
                        // gen r4w4 (orchestrator, 2026-09-24): keep `READ_SLACK`
                        // committed ABOVE `readable_end`, as every other writer
                        // of it does (`alloc`, `commit_initial_prefix`). With
                        // `readable_end = keep_end` a conservative guess in the
                        // last `HEADER_SIZE - 1` bytes passed `contains` and the
                        // mark screen's header read faulted on the first
                        // decommitted page (`old_gen_mark_candidate_plausible`,
                        // SIGSEGV at a granule boundary in
                        // `GenR4W4HeapFullThrashProbe` under gc-stress).
                        // `keep_end >= target + READ_SLACK`, so this stays at or
                        // above the last allocated byte.
                        self.readable_end =
                            self.readable_end.min(keep_end.saturating_sub(READ_SLACK));
                        // gen r5w3/oldgen7: holes that were below the old end
                        // may now all lie above the new one.
                        if self.readable_holes {
                            self.refresh_readable_holes();
                        }
                        self.sizing.shrinks = self.sizing.shrinks.wrapping_add(1);
                        self.sizing.shrunk_bytes =
                            self.sizing.shrunk_bytes.wrapping_add(released as u64);
                        self.bytes_given_back = self.bytes_given_back.wrapping_add(released as u64);
                        out.released = released;
                    }
                }
            }
        }
        self.readable_end_at_last_resize = Some(self.readable_end);
        // gen r4w6/oldpin6: any resize answers a pending post-concurrent one
        // (a stop-the-world collection in the same pause must not be followed
        // by a second, undamped attempt).
        self.concurrent_resize_attempts = 0;
        // gen r5w6/old10: nor re-armed by the interior half's keep (the
        // post-concurrent caller sets the budget again after this returns).
        self.concurrent_resize_left = 0;
        out
    }

    /// gen r4w6/oldpin6 (2026-09-24) — the committed-size resize for a
    /// CONCURRENT old-gen cycle, run at a young pause after its sweep ended
    /// (`docs/internal/gc/gengc-r4w5-oldcompact5-old-gen-shrink-waits-for-a-stop-the-world-collection-SUPERSEDED-20260928.md`).
    ///
    /// `None` when no concurrent sweep has ended since the last resize.
    /// Otherwise one [`Self::resize_after_collection`] — the same target, the
    /// same damping, the same `SHRINK_MIN_GRANULES` threshold — and, when it
    /// was DAMPED, the attempt is kept for the next young pause, at most
    /// [`CONCURRENT_RESIZE_ATTEMPTS`] times in all. The damping asks "did the
    /// committed end advance since the previous resize?"; after a concurrent
    /// cycle the next old-gen collection may be far away, so without the retry
    /// the first (damped) attempt would be the last.
    ///
    /// # Why a young pause may decommit (the safety argument)
    ///
    /// Exactly [`Self::resize_after_collection`]'s: the caller is inside a
    /// stop-the-world young pause and holds the old-gen lock, which is all
    /// that argument uses. Only the trailing FREE run is decommitted
    /// (`keep_end >= tail_free_start`, with [`READ_SLACK`] kept committed above
    /// the lowered `readable_end`). A concurrent cycle that is open across
    /// this pause reads old-gen memory only in slices under the same lock and
    /// only inside allocated regions (its eligibility snapshot, mark stack and
    /// sweep resume offset all name objects, none of which lies in the free
    /// tail), and its bitmap covers the reservation, which does not move. The
    /// lock-free conservative screen reads the commit bitmap, whose bits
    /// `decommit_range` clears before the syscall.
    ///
    /// NOT called from the concurrent sweep itself
    /// ([`Self::note_concurrent_collection_end`] only arms it): that slice
    /// runs beside mutators, and nothing argues a decommit there.
    pub fn resize_after_concurrent_sweep_if_due(
        &mut self,
        allow_shrink: bool,
    ) -> Option<OldGenResize> {
        if self.concurrent_resize_attempts == 0 {
            return None;
        }
        let left = self.concurrent_resize_attempts - 1;
        let out = self.resize_after_collection(allow_shrink);
        self.sizing.concurrent_sweep_resizes = self.sizing.concurrent_sweep_resizes.wrapping_add(1);
        if out.released > 0 {
            self.sizing.concurrent_sweep_shrinks =
                self.sizing.concurrent_sweep_shrinks.wrapping_add(1);
        }
        if out.damped {
            self.concurrent_resize_attempts = left;
        }
        self.concurrent_resize_left = left;
        Some(out)
    }

    /// gen r5w6/old10 — keep one more post-concurrent resize attempt for the
    /// next young pause, within the episode's budget
    /// ([`CONCURRENT_RESIZE_ATTEMPTS`] in all): the attempts the last
    /// [`Self::resize_after_concurrent_sweep_if_due`] left. Returns whether an
    /// attempt is now pending.
    ///
    /// The tail half keeps its attempt when IT was damped; the interior half
    /// ([`Self::decommit_interior_after_concurrent_resize`]) has its own
    /// damping (committed size grew since its previous pass — which, after a
    /// growth phase, is ALWAYS true on the first pass after the drop), and an
    /// undamped tail resize used to end the episode on that same pause. The
    /// interior half then waited for the NEXT concurrent cycle's end — on a
    /// program that peaks and settles, the periodic cycle 16 young pauses
    /// later, or never (`GenR4W6OldShrinkProbe`). The caller also keeps an
    /// attempt when the interior half could not run at all because a
    /// concurrent cycle is open.
    pub fn keep_concurrent_resize_attempt(&mut self) -> bool {
        self.concurrent_resize_attempts =
            self.concurrent_resize_attempts.max(self.concurrent_resize_left);
        self.concurrent_resize_attempts > 0
    }

    /// gen r5w6/old10 — [`Self::decommit_interior_free_runs`] as the interior
    /// half of a POST-CONCURRENT resize: when the pass is damped, the episode
    /// keeps an attempt ([`Self::keep_concurrent_resize_attempt`]), so the next
    /// young pause runs it again instead of the next concurrent cycle's end.
    /// Bounded by the same budget as the tail half's retries. Returns the bytes
    /// released.
    pub fn decommit_interior_after_concurrent_resize(&mut self) -> usize {
        let damped_before = self.sizing.interior_decommits_damped;
        let released = self.decommit_interior_free_runs();
        if self.sizing.interior_decommits_damped != damped_before {
            let _ = self.keep_concurrent_resize_attempt();
        }
        released
    }

    /// gen r4w6/oldpin6 — young pauses left to resize after a concurrent
    /// sweep (0: none due). Diagnostics and tests.
    pub fn concurrent_resize_attempts(&self) -> u8 {
        self.concurrent_resize_attempts
    }

    /// gen r4w6/oldpin6 — the COMMITTED parts of `[0, end)` (clamped to the
    /// reservation), as maximal `(absolute address, length)` runs, ascending.
    ///
    /// For raw diagnostic scans of the generation
    /// (`gengc-r4w5-review5-old-gen-diagnostics-scan-to-high-water-past-a-shrink`):
    /// [`Self::high_water`] is NOT lowered by a shrink
    /// ([`Self::resize_after_collection`] decommits `[keep_end, capacity)` and
    /// keeps the mark), so "below the high-water mark" no longer implies
    /// "mapped". A scan that reads only inside these runs cannot touch a
    /// decommitted granule — including the holes below `readable_end` that
    /// the Windows commit-limit arm can leave (`readable_holes`), which a
    /// bare `min(high_water, readable_end)` bound would not exclude. On the
    /// wholly committed store it is the single run `[base, base + end)`.
    pub fn committed_spans_below(&self, end: usize) -> Vec<(usize, usize)> {
        self.data.committed_spans(0, end.min(self.data.len()))
    }

    // -----------------------------------------------------------------
    // gen r5w3/oldgen7 (2026-09-26): interior decommit
    // -----------------------------------------------------------------

    /// Recompute [`Self::readable_holes`] from the commit bitmap: is any
    /// granule below `readable_end` uncommitted? Stops at the first hole.
    ///
    /// `readable_holes` used to be a one-way latch (set by the Windows
    /// commit-limit arm, cleared only by a compaction's re-commit), so after
    /// the first hole every [`Self::contains`] consulted the bitmap for the
    /// rest of the run. The interior decommit makes holes routine, so the
    /// allocator re-derives the flag whenever it commits below the committed
    /// end while holes exist (`alloc_from_buckets_scan`): the flag is then
    /// exactly "a hole exists", and `contains` drops back to the one-number
    /// test once the holes are refilled. O(granules below `readable_end`)
    /// bitmap loads, paid only on such a commit.
    fn refresh_readable_holes(&mut self) {
        if !matches!(self.data, HeapStore::Reserved(_)) {
            self.readable_holes = false;
            return;
        }
        let end = self.readable_end.min(self.data.len());
        self.readable_holes =
            end > 0 && (0..=(end - 1) / GRANULE).any(|g| !self.data.is_committed_at(g * GRANULE));
    }

    /// [`Self::contains`] for a whole RANGE: is every byte of
    /// `[ptr, ptr + len)` readable storage of this generation?
    ///
    /// Without holes this is `contains(ptr) && contains(ptr + len - 1)`, since
    /// `[0, readable_end)` is committed end to end. With holes (an interior
    /// decommit, [`Self::decommit_interior_free_runs`]) both ends can be
    /// committed while a granule between them is not, so every granule the
    /// range spans is tested. The screens that go on to read an object a
    /// CONSERVATIVE word names, up to the end its header claims
    /// (`gen_heap::old_gen_mark_candidate_plausible`,
    /// `victim8_neighbor_explains_zero_prefix`), ask this: a stale header in a
    /// free block can claim an extent across a released granule, and scanning
    /// it would fault. O(1) without holes; O(granules spanned) with them.
    pub fn contains_range(&self, ptr: *const u8, len: usize) -> bool {
        if len == 0 {
            return self.contains(ptr);
        }
        let base = self.data.as_ptr() as usize;
        let p = ptr as usize;
        if p < base {
            return false;
        }
        let off = p - base;
        let Some(last) = off.checked_add(len - 1) else {
            return false;
        };
        if last >= self.readable_end {
            return false;
        }
        if !self.readable_holes {
            return true;
        }
        (off / GRANULE..=last / GRANULE).all(|g| self.data.is_committed_at(g * GRANULE))
    }

    /// gen r5w3/oldgen7 — decommit the whole granules of large free blocks
    /// BELOW the trailing free run
    /// (`gengc-r5w1-oldgen5-proposal-old-gen-interior-decommit`,
    /// `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`, opt-in). Returns the bytes
    /// released.
    ///
    /// [`Self::resize_after_collection`] returns only `[keep_end, capacity)`,
    /// so one survivor near the top pins the whole committed extent
    /// (`GenR4W6OldShrinkProbe`: 243 MB committed for 5 MB used). This is its
    /// interior half, run by the caller right after it
    /// (`GenerationalHeap::decommit_old_interior_if_enabled`), under the same
    /// rules:
    ///
    /// * **target** — the committed size may fall to
    ///   `max(commit_floor, used * 100 / (100 - OLD_MAX_FREE_PERCENT))`, the
    ///   tail shrink's `MaxHeapFreeRatio` target; nothing is released unless the committed
    ///   size is at least [`SHRINK_MIN_GRANULES`] granules above it, and never
    ///   more than the excess. Largest blocks first.
    /// * **damped** — not when the committed size GREW since the previous
    ///   pass (the generation needed the memory in between; a re-committed
    ///   hole counts), and not on the first pass.
    /// * **worth it** — a block qualifies only when its whole-granule
    ///   interior spans at least [`SHRINK_MIN_GRANULES`] granules.
    ///
    /// # What makes a hole safe to leave behind
    ///
    /// * Every released byte is inside a FREE block below `tail_free_start`:
    ///   no object, live or dead-but-unfreed, is there. The hole starts
    ///   [`READ_SLACK`] past the block's start (rounded up to a granule), so the
    ///   header-width read slack past the allocated object below it stays
    ///   committed, exactly as `resize_after_collection` keeps it above the
    ///   lowered `readable_end`.
    /// * `readable_holes` is set, so [`Self::contains`] consults the commit
    ///   bitmap — for the header's width, not only its first byte — and
    ///   [`Self::contains_range`] for whole claimed extents; the lock-free
    ///   conservative screen (`GenerationalHeap::is_object_address`) reads
    ///   the store's live bitmap over the header and then the whole claimed
    ///   extent, and `decommit_range` clears the bits BEFORE the syscall.
    /// * The allocator re-commits from the block's own start when holes exist
    ///   (`alloc_from_buckets_scan`); walks read only allocated regions;
    ///   `fill_zero`, `reset_range` and `committed_spans` skip uncommitted
    ///   granules; `compact_walked` and [`Self::compact_around_pins`] re-commit
    ///   `[0, readable_end)` before they slide into free space.
    /// * The caller runs this inside a stop-the-world pause holding the
    ///   old-gen lock, and never while a CONCURRENT cycle is open: that
    ///   cycle's mark stack may still name an object a stop-the-world
    ///   collection has since freed, and its scan checks only the object's
    ///   ends. Mutators hold references to allocated objects only; a DANGLING
    ///   reference into a released granule faults instead of reading stale
    ///   bytes — the trade the tail shrink and `CRATONVM_GEN_UNCOMMIT` already
    ///   make, and the crash handler names the site
    ///   (`reservation::recent_decommit_covering`, `"old-gen interior decommit"`).
    ///
    /// Nothing about the free list, the object grid or `reclaim_epoch`
    /// changes: decommitting free memory changes no object's identity.
    pub fn decommit_interior_free_runs(&mut self) -> usize {
        if !matches!(self.data, HeapStore::Reserved(_)) {
            return 0;
        }
        let committed = self.data.committed_bytes();
        let prev = self.interior_committed_at_last.replace(committed);
        let by_ratio = self
            .used_bytes
            .saturating_mul(100)
            / (100 - OLD_MAX_FREE_PERCENT);
        let target = self.commit_floor.max(by_ratio);
        let excess = committed.saturating_sub(target);
        if excess < SHRINK_MIN_GRANULES * GRANULE {
            return 0;
        }
        // No previous pass counts as "grew", like the tail shrink's damping:
        // a generation that has only ever grown releases nothing on its first
        // collection.
        if prev.is_none_or(|p| committed > p) {
            self.sizing.interior_decommits_damped =
                self.sizing.interior_decommits_damped.wrapping_add(1);
            return 0;
        }
        let tail = self.tail_free_start();
        // Over the offset-sorted view, and only when the free list is
        // DISJOINT: a double free leaves a block overlapping (or nesting in)
        // another (`OLD_FREE_LIST_OVERLAPS`), and such a block can cover a
        // live object — reading it is survivable, releasing it is not. The
        // walks tolerate that list; this pass declines it outright.
        let runs: Option<Vec<(usize, usize)>> = self.with_sorted_free_blocks(|sorted| {
            if !sorted
                .windows(2)
                .all(|w| w[0].offset.saturating_add(w[0].size) <= w[1].offset)
            {
                return None;
            }
            Some(
                sorted
                    .iter()
                    .filter_map(|b| {
                        let end = b.offset.saturating_add(b.size).min(tail);
                        let lo = b.offset.saturating_add(READ_SLACK).div_ceil(GRANULE) * GRANULE;
                        let hi = end / GRANULE * GRANULE;
                        (hi > lo && (hi - lo) / GRANULE >= SHRINK_MIN_GRANULES).then_some((lo, hi))
                    })
                    .collect(),
            )
        });
        let Some(mut runs) = runs else {
            return 0;
        };
        if runs.is_empty() {
            return 0;
        }
        // Largest first; ties by address so the order is deterministic.
        runs.sort_unstable_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)).then(a.0.cmp(&b.0)));
        let mut left = excess / GRANULE * GRANULE;
        let mut released = 0usize;
        for (lo, hi) in runs {
            if left < GRANULE {
                break;
            }
            let len = (hi - lo).min(left);
            let r = self
                .data
                .decommit_range(lo, len, "old-gen interior decommit");
            released += r;
            left = left.saturating_sub(r);
        }
        if released > 0 {
            // Holes below `readable_end` now exist: `contains` and the
            // allocator's commit step consult the bitmap from here on (until
            // `refresh_readable_holes` finds them refilled).
            self.readable_holes = true;
            self.bytes_given_back = self.bytes_given_back.wrapping_add(released as u64);
            self.sizing.interior_decommits = self.sizing.interior_decommits.wrapping_add(1);
            self.sizing.interior_decommitted_bytes = self
                .sizing
                .interior_decommitted_bytes
                .wrapping_add(released as u64);
            self.interior_committed_at_last = Some(self.data.committed_bytes());
        }
        released
    }

    // -----------------------------------------------------------------
    // gen r5w3/oldgen7 (2026-09-26): growing into the young budget
    // -----------------------------------------------------------------

    /// gen r5w3/oldgen7 — at the end of a stop-the-world old-gen collection,
    /// grow the generation's CAPACITY in place when an allocation was refused
    /// since the previous call and the collection did not make room for it
    /// (`gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget`,
    /// `CRATONVM_GC_OLD_BORROW_YOUNG`). Returns the bytes the capacity grew
    /// by (`0` without headroom, which is every generation built by
    /// [`Self::new`]).
    ///
    /// HotSpot Serial's old generation may grow to `MaxHeapSize - MaxNewSize`
    /// (2/3 of `-Xmx` at `NewRatio = 2`); this collector's default split
    /// fixes it at 1/2, so a tenured set between the two throws
    /// `OutOfMemoryError` here and not there. With headroom
    /// ([`Self::with_growth_headroom`]) the refusal is answered the way
    /// HotSpot's `TenuredGeneration` answers one after a full collection:
    ///
    /// * **when** — a request `r` was refused since the last call (not a
    ///   promotion-buffer carve, not a commit refusal) and, after this
    ///   collection, the generation still has less than `used + r` bytes of
    ///   capacity or less than `MinHeapFreeRatio = 40` % free;
    /// * **to** — `max(used + r, used * 100 / 60)`, rounded up to a granule,
    ///   capped at the headroom's end ([`Self::growth_max`]); nothing when the
    ///   cap cannot hold `used + r` (the refusal stands, as it would on
    ///   HotSpot past its own maximum).
    ///
    /// The growth is `HeapStore::grow_to` inside the reservation (the base does
    /// not move), the new span `[old capacity, new capacity)` pushed as a free
    /// block (the next `coalesce_free_blocks` merges it with a free tail), and
    /// `reclaim_epoch` bumped: a concurrent cycle's bitmap was sized for the
    /// old capacity, and an open cycle must be abandoned rather than meet an
    /// object outside it. Nothing is committed here; the allocator commits
    /// what it hands out, as always.
    ///
    /// The CALLER owns the rest of the budget: it runs this only inside a
    /// stop-the-world pause holding the old-gen lock, republishes the region
    /// bounds before mutators resume (the lock-free screens read them), and
    /// caps the young trigger so the young generation cedes what the old one
    /// took (`GenerationalHeap::cede_young_budget_to_old`).
    pub fn grow_after_refusal(&mut self) -> usize {
        let refused = std::mem::take(&mut self.refused_since_growth_check);
        let cap = self.data.len();
        if refused == 0 || self.growth_max <= cap || !matches!(self.data, HeapStore::Reserved(_)) {
            return 0;
        }
        let used = self.used_bytes;
        // gen r5w6/old10: the refused request needs ONE block. When the
        // collection left no free block that large — a fragmentation refusal
        // the compaction could not answer (the non-moving path's pinned
        // compaction around conservative words, a declined plan, a walk gap)
        // — `used + refused` bytes of capacity do not make room: the new span
        // is pushed at the old end and merges only with the trailing free
        // run. Grow so that run holds the request: `tail_free_start + refused`.
        // (Before, such a refusal either grew nothing — `used + refused` fit
        // the capacity in total — or grew to a capacity whose tail still could
        // not hold it.) With a block that large already free, as before.
        let need = if self.largest_free_block() >= refused {
            used.saturating_add(refused)
        } else {
            self.tail_free_start().max(used).saturating_add(refused)
        };
        let min_free_ratio = used.saturating_mul(100) / 60;
        let want = need.max(min_free_ratio);
        if want <= cap {
            // The collection made room.
            return 0;
        }
        if need > self.growth_max {
            return 0;
        }
        let new_cap = want
            .div_ceil(GRANULE)
            .saturating_mul(GRANULE)
            .min(self.growth_max);
        // With compressed oops the heap's regions must stay inside the
        // narrow-oop window fixed at VM start: the republish after this growth
        // (`store_region_bounds_locked`) PANICS on a region that left it
        // (`compressed_oops::assert_region_encodable`). A growth that would
        // leave it is not taken; the refusal stands.
        let new_end = (self.data.as_ptr() as usize).saturating_add(new_cap) as u64;
        if cratonvm_types::narrow_oop::narrow_oops_enabled()
            && new_end > cratonvm_types::narrow_oop::narrow_limit()
        {
            return 0;
        }
        let base_before = self.data.as_ptr();
        let base_after = self.data.grow_to(new_cap);
        if base_after != base_before || self.data.len() != new_cap {
            // Cannot happen on the reserving store within its reservation;
            // if it ever did, every old-gen address would be wrong. Say so
            // rather than carry on.
            tracing::error!(
                target: "cratonvm::gc::guard",
                cap,
                new_cap,
                len = self.data.len(),
                "OldGen::grow_after_refusal: the store did not grow in place",
            );
            debug_assert!(false, "OldGen::grow_after_refusal: the store did not grow in place");
            return 0;
        }
        let grown = new_cap - cap;
        self.buckets[bucket_for(grown)].push(FreeBlock {
            offset: cap,
            size: grown,
        });
        self.free_list_maximal = false;
        self.invalidate_sorted_free();
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);
        let _ = self.coalesce_free_blocks();
        self.sizing.borrow_growths = self.sizing.borrow_growths.wrapping_add(1);
        self.sizing.borrowed_bytes = self.sizing.borrowed_bytes.wrapping_add(grown as u64);
        grown
    }

    /// gen r5w3/oldgen7 — the largest capacity [`Self::grow_after_refusal`]
    /// may grow this generation to: its reservation's usable extent when it
    /// was built with headroom, else its capacity.
    pub fn growth_max(&self) -> usize {
        self.growth_max.max(self.data.len())
    }

    // -----------------------------------------------------------------
    // gen r5w3/oldgen7 (2026-09-26): humongous arrays from the top
    // -----------------------------------------------------------------

    /// gen r5w3/oldgen7 — allocate a HUMONGOUS request from the top of the
    /// generation (`gengc-r5w1-oldgen5-proposal-humongous-arrays-from-the-top-of-old-gen`,
    /// `CRATONVM_GC_OLD_HUMONGOUS_TOP`, opt-in; the one caller is
    /// `GenerationalHeap::try_alloc_array_humongous`, the only site that knows
    /// an object is humongous). `zero` as [`Self::alloc`] / [`Self::alloc_unzeroed`].
    ///
    /// Takes the free block with the HIGHEST offset that can hold the request
    /// carved from its END, and carves it there. Ordinary requests keep the
    /// best fit, which fills from low addresses, so humongous arrays cluster
    /// at the top and small survivors at the bottom: a dead humongous array's
    /// span merges with its dead neighbours and with the free tail (which the
    /// tail shrink can return), instead of leaving a hole between long-lived
    /// small objects that the next promotion buffer splits for good.
    ///
    /// # The commit, and why it needs holes
    ///
    /// The block is committed ONLY under the array (plus [`READ_SLACK`]), not
    /// from `readable_end` up: committing the gap would commit memory nobody
    /// uses, which is the opposite of the point. So `[readable_end, carve)`
    /// can stay uncommitted below the new `readable_end` — a hole, handled by
    /// the machinery the interior decommit uses (`readable_holes`: `contains`
    /// and the allocator consult the commit bitmap; the compactors re-commit
    /// before they slide). The header-width invariant `contains` relies on
    /// holds: the gap starts at the old `readable_end`, above which
    /// `READ_SLACK` was already committed, and the array's own end gets
    /// `READ_SLACK` here.
    ///
    /// Falls back to the ordinary allocation (and its refusal bookkeeping)
    /// when no block fits carved from its end, and on the wholly committed
    /// store (where "top" buys nothing a hole could give back).
    pub fn alloc_from_top(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
        if matches!(self.data, HeapStore::Reserved(_)) {
            if let Some(p) = self.alloc_from_top_scan(size, align, zero) {
                self.sizing.humongous_top_allocs = self.sizing.humongous_top_allocs.wrapping_add(1);
                return Some(p);
            }
        }
        self.sizing.humongous_top_fallbacks = self.sizing.humongous_top_fallbacks.wrapping_add(1);
        self.alloc_impl(size, align, zero)
    }

    /// The body of [`Self::alloc_from_top`]: find the highest carve, commit
    /// under it, split the block. `None` leaves the free list exactly as it
    /// was (no fitting block, or the OS refused the commit — which the
    /// ordinary path the caller falls back to will then meet and count).
    fn alloc_from_top_scan(&mut self, size: usize, align: usize, zero: bool) -> Option<*mut u8> {
        // The same rounding as `alloc_from_buckets`, so `free` of the
        // returned extent matches.
        let size = size.max(HEADER_SIZE.max(8));
        let size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;
        let base = self.data.as_ptr() as usize;
        // (bucket, index in it, carve offset) of the highest carve seen.
        let mut best: Option<(usize, usize, usize)> = None;
        for bucket_idx in min_satisfying_bucket(size)..NUM_BUCKETS {
            let bucket = &self.buckets[bucket_idx];
            if bucket.size_bound < size {
                continue;
            }
            for (i, b) in bucket.iter().enumerate() {
                if b.size < size {
                    continue;
                }
                let block_addr = base + b.offset;
                let carve_addr = (block_addr + b.size - size) & !(align - 1);
                if carve_addr < block_addr {
                    continue;
                }
                let off = carve_addr - base;
                if best.is_none_or(|(_, _, o)| off > o) {
                    best = Some((bucket_idx, i, off));
                }
            }
        }
        let (bucket_idx, i, off) = best?;
        let alloc_end = off + size;
        // Commit under the array only; see the method doc.
        let commit_end = alloc_end.saturating_add(READ_SLACK).min(self.data.len());
        if !self.data.commit_range(off, commit_end - off) {
            return None;
        }
        let readable_end_before = self.readable_end;
        self.readable_end = self.readable_end.max(alloc_end);
        if off > readable_end_before || self.readable_holes {
            // The gap below the carve may be uncommitted: re-derive the flag
            // (exact, and false again when the gap happens to be committed).
            self.refresh_readable_holes();
        }
        let committed = self.data.committed_bytes() as u64;
        if committed > self.sizing.committed_peak {
            self.sizing.committed_peak = committed;
        }
        let block = self.buckets[bucket_idx].swap_remove(i);
        self.invalidate_sorted_free();
        if off > block.offset {
            self.buckets[bucket_for(off - block.offset)].push(FreeBlock {
                offset: block.offset,
                size: off - block.offset,
            });
        }
        let block_end = block.offset + block.size;
        if block_end > alloc_end {
            self.buckets[bucket_for(block_end - alloc_end)].push(FreeBlock {
                offset: alloc_end,
                size: block_end - alloc_end,
            });
        }
        // The same bookkeeping as the one other hand-out site
        // (`alloc_from_buckets_scan`): used bytes, the high-water mark, the
        // block-offset table, the fragmentation scale.
        self.used_bytes += size;
        self.high_water = self.high_water.max(alloc_end);
        self.bot.get_mut().note_alloc(off, size);
        self.recent_max_request = self.recent_max_request.max(size);
        self.trigger_stats.allocs = self.trigger_stats.allocs.wrapping_add(1);
        // SAFETY: `[off, alloc_end)` lies inside the removed free block, which
        // is inside the store, and was just committed.
        let ptr = unsafe { self.data.as_mut_ptr().add(off) };
        if zero {
            // SAFETY: as above; `size` bytes at `ptr` are committed and ours.
            unsafe { std::ptr::write_bytes(ptr, 0, size) };
        }
        Some(ptr)
    }

    // -----------------------------------------------------------------
    // Fragmentation trigger
    // -----------------------------------------------------------------

    /// An allocation of `size` bytes was refused. Record it as a
    /// FRAGMENTATION refusal when the free list holds the request in TOTAL —
    /// "there are bytes but no room", the state `free_bytes_and_largest`
    /// exists to name, and exactly the state a compaction turns into a fit —
    /// and not when the OS refused a commit (`commit_refused`), which no
    /// compaction can help. (No `FRAGMENTATION_FIT_FACTOR` margin: that factor
    /// scales the PROACTIVE predicate, which judges a request nobody has made
    /// yet; a refusal is a concrete request that failed.)
    ///
    /// `capacity - used` IS the free-list total whenever the list is exact
    /// (every mutation moves `used_bytes` by what it moves onto or off the
    /// list), so this is O(1) — it runs on the refusal path, which a
    /// promotion-failure storm hits once per surviving object.
    fn note_refused_request(&mut self, size: usize, commit_refused: bool) {
        if commit_refused {
            return;
        }
        let request = (size.max(HEADER_SIZE.max(8)) + 7) & !7;
        let free = self.data.len().saturating_sub(self.used_bytes);
        if free >= request {
            self.frag_pending_request = self.frag_pending_request.max(request);
            self.sizing.fragmentation_refusals = self.sizing.fragmentation_refusals.wrapping_add(1);
        }
    }

    /// The largest fragmentation-refused request not yet handed to the
    /// trigger (0: none).
    pub fn fragmentation_pending_request(&self) -> usize {
        self.frag_pending_request
    }

    /// gen r4w4/oldgen4 — the post-collection half of the fragmentation
    /// trigger: after an old-gen collection has coalesced its free list, if
    /// [`Self::fragmentation_repair_due`] still holds — the largest free block
    /// cannot serve the largest request served since the last repair, though
    /// the free bytes could `FRAGMENTATION_FIT_FACTOR` times over — record
    /// that request as pending, exactly as a refusal would have. Consumes the
    /// scale (`recent_max_request = 0`), which is the predicate's own
    /// hysteresis: it cannot fire again until real allocation resumes.
    ///
    /// Returns whether it armed anything. Called only when the fragmentation
    /// trigger is enabled, so the default run's `fragmentation_repair_due`
    /// instrument is untouched.
    pub fn note_fragmentation_after_collection(&mut self) -> bool {
        if !self.fragmentation_repair_due() {
            return false;
        }
        self.frag_pending_request = self.frag_pending_request.max(self.recent_max_request);
        self.recent_max_request = 0;
        true
    }

    /// gen r4w4/oldgen4 — the trigger's question, asked once per old-gen
    /// trigger decision: is a fragmentation compaction pending? If so, turn
    /// the pending request into a COMPACTION REQUEST for the next old-gen
    /// collection (consumed by [`Self::take_fragmentation_compaction_request`])
    /// and return `true`, so the caller runs a collection even below the
    /// occupancy floor.
    ///
    /// One request per refusal episode: the pending request is cleared here,
    /// so a collection that cannot compact (the non-moving path, an interior
    /// root's downgrade) does not re-trigger on every young cycle — only a NEW
    /// refusal re-arms it.
    pub fn arm_fragmentation_compaction(&mut self) -> bool {
        if self.frag_pending_request == 0 {
            return false;
        }
        self.frag_pending_request = 0;
        self.frag_compaction_requested = true;
        self.sizing.fragmentation_compactions_requested = self
            .sizing
            .fragmentation_compactions_requested
            .wrapping_add(1);
        true
    }

    /// gen r4w4/oldgen4 — the allocation-failure ladder's form of the
    /// fragmentation question, for ONE request: the generation's free bytes
    /// could hold `request` (rounded as `alloc` rounds it) but no single free
    /// block can. No `FRAGMENTATION_FIT_FACTOR` margin: on the path to
    /// `OutOfMemoryError` "it would fit after a compaction" is the whole test.
    /// O(free blocks in the top non-empty class), for a path that is about to
    /// throw anyway.
    pub fn fragmented_for(&self, request: usize) -> bool {
        let need = (request.max(HEADER_SIZE.max(8)) + 7) & !7;
        let free = self.data.len().saturating_sub(self.used_bytes);
        free >= need && self.largest_free_block() < need
    }

    /// gen r4w4/oldgen4 — ask the next old-gen collection to COMPACT, now
    /// (the allocation-failure ladder's request; see
    /// `GenerationalHeap::request_old_gen_compaction_if_fragmented_for`).
    /// Consumed by [`Self::take_fragmentation_compaction_request`].
    pub fn request_fragmentation_compaction(&mut self) {
        self.frag_compaction_requested = true;
        self.sizing.fragmentation_compactions_requested = self
            .sizing
            .fragmentation_compactions_requested
            .wrapping_add(1);
        self.sizing.oom_compaction_requests = self.sizing.oom_compaction_requests.wrapping_add(1);
    }

    /// Consume the compaction request [`Self::arm_fragmentation_compaction`]
    /// (or [`Self::request_fragmentation_compaction`]) made. The collector that can compact (`GenerationalHeap::major_gc`)
    /// asks this to decide; the one that cannot (the non-moving sweep) calls
    /// it to discard the request, so it cannot linger into a much later cycle.
    ///
    /// gen r5w1/oldgen5: also clears [`Self::humongous_refusal_pending`] —
    /// the two are one request; read that one first when it matters.
    pub fn take_fragmentation_compaction_request(&mut self) -> bool {
        self.humongous_refusal_pending = false;
        self.humongous_refused_bytes = 0;
        std::mem::take(&mut self.frag_compaction_requested)
    }

    /// gen r5w1/oldgen5 (2026-09-26) — a HUMONGOUS allocation (a large array
    /// that goes straight to this generation) of `reserved` bytes was refused.
    /// Ask the next old-gen collection to COMPACT, as the fragmentation
    /// trigger does, whether or not the free list holds the request in total
    /// NOW: the refusal is typically seen before that collection has freed the
    /// dead objects between the survivors, so "free bytes >= request" (the
    /// test [`Self::note_refused_request`] applies) is false exactly when a
    /// compaction is what HotSpot Serial's full collection would do next.
    ///
    /// `GenR4W4HumongousFragProbe`: 17 MiB arrays A, B, C, A and C dropped, a
    /// 33 MiB request. The first refusal sees 13 MiB free; the collection it
    /// provokes frees A and C IN PLACE, leaving 17 + 30 MiB in two holes, and
    /// the request is refused again until `OutOfMemoryError`. Compacting in
    /// that collection slides B down and leaves one 47 MiB block.
    ///
    /// A request larger than the generation cannot be helped and arms
    /// nothing. Returns whether it armed. The caller gates it on
    /// `CRATONVM_GC_OLD_OOM_COMPACT` (default on), the allocation-failure
    /// ladder's compaction switch: a humongous refusal IS that ladder's first
    /// step.
    pub fn note_humongous_refusal(&mut self, reserved: usize) -> bool {
        if reserved > self.data.len() {
            return false;
        }
        self.frag_compaction_requested = true;
        self.humongous_refusal_pending = true;
        self.humongous_refused_bytes = self.humongous_refused_bytes.max(reserved);
        self.sizing.fragmentation_compactions_requested = self
            .sizing
            .fragmentation_compactions_requested
            .wrapping_add(1);
        self.sizing.humongous_compaction_requests =
            self.sizing.humongous_compaction_requests.wrapping_add(1);
        true
    }

    /// gcd d4/n (2026-09-28,
    /// `gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words`) —
    /// count a compaction the moving path's major VETOED (its request was
    /// consumed as usual and the collection ran in place): a compiled loop's
    /// derived cursor into an old array, or a parked peer's native-band or
    /// register word, is in no root set and no oop map, so a slide would leave
    /// it dangling. Returns the count before this one (for rate-limited
    /// logging); [`OldGenSizingStats::moving_major_compaction_vetoes`].
    pub fn note_moving_major_compaction_veto(&mut self) -> u64 {
        let n = self.sizing.moving_major_compaction_vetoes;
        self.sizing.moving_major_compaction_vetoes = n.wrapping_add(1);
        n
    }

    /// gcd d4/n — count a requested major's young seed: `Some(excluded)` for
    /// a true-root seed that left `excluded` young survivors out, `None` for
    /// one that kept the legacy seed. See
    /// [`OldGenSizingStats::true_root_majors`].
    ///
    /// gcd d5/r: returns the bumped counter's new value (`true_root_majors`
    /// or `true_root_fallbacks`), for the caller's rate-limited log line.
    pub fn note_true_root_major(&mut self, outcome: Option<usize>) -> u64 {
        match outcome {
            Some(excluded) => {
                self.sizing.true_root_majors = self.sizing.true_root_majors.wrapping_add(1);
                self.sizing.true_root_young_excluded = self
                    .sizing
                    .true_root_young_excluded
                    .wrapping_add(excluded as u64);
                self.sizing.true_root_majors
            }
            None => {
                self.sizing.true_root_fallbacks = self.sizing.true_root_fallbacks.wrapping_add(1);
                self.sizing.true_root_fallbacks
            }
        }
    }

    /// gcd d9/a — count a requested major that kept the legacy young seed,
    /// by reason (the total `true_root_fallbacks` and the reason's slot).
    /// Returns the new total, for the caller's rate-limited log line.
    pub fn note_true_root_fallback(&mut self, reason: TrueRootFallback) -> u64 {
        let slot = &mut self.sizing.true_root_fallback_reasons[reason.index()];
        *slot = slot.wrapping_add(1);
        self.note_true_root_major(None)
    }

    /// gcd d9/a — a true-root major resolved `total` promotions of its pause
    /// and freed `dead` of them (see
    /// [`OldGenSizingStats::true_root_promotions`]).
    pub fn note_true_root_promotions(&mut self, total: usize, dead: usize) {
        self.sizing.true_root_promotions =
            self.sizing.true_root_promotions.wrapping_add(total as u64);
        self.sizing.true_root_promotions_dead =
            self.sizing.true_root_promotions_dead.wrapping_add(dead as u64);
    }


    /// gen r5w1/oldgen5 — is the pending compaction request (at least in part)
    /// a refused humongous allocation's ([`Self::note_humongous_refusal`])?
    pub fn humongous_refusal_pending(&self) -> bool {
        self.humongous_refusal_pending && self.frag_compaction_requested
    }

    /// gen r5w2/alloc6 — the reserved extent of the largest refused humongous
    /// request behind [`Self::humongous_refusal_pending`], or 0 when none is
    /// pending. Read by the collection that answers the request BEFORE it
    /// consumes it, so its log line can say whether the compaction made room
    /// (`largest_free_after >= request`) — the one fact that separates "the
    /// compaction ran and was enough" from "it ran and pins were in the way"
    /// on `GenR4W5OldPinnedCompactProbe`.
    pub fn humongous_refused_request(&self) -> usize {
        if self.humongous_refusal_pending() {
            self.humongous_refused_bytes
        } else {
            0
        }
    }

    /// gen r5w1/oldgen5 — count a non-moving collection that answered a
    /// humongous refusal with a compaction by default.
    pub fn note_humongous_default_compaction(&mut self) {
        self.sizing.humongous_default_compactions =
            self.sizing.humongous_default_compactions.wrapping_add(1);
    }

    // -----------------------------------------------------------------
    // Walk-gap recovery
    // -----------------------------------------------------------------

    /// gen r4w4/oldgen4 — record an in-place sweep that RECOVERED from an
    /// incomplete walk (`gap_bytes` unwalked, scanned conservatively) instead
    /// of freeing nothing; returns the count BEFORE this one so the caller can
    /// rate-limit its log line per generation.
    pub fn note_walk_gap_recovery(&mut self, gap_bytes: usize) -> u64 {
        let n = self.sizing.walk_gap_recoveries;
        self.sizing.walk_gap_recoveries = n.wrapping_add(1);
        self.sizing.walk_gap_bytes_last = gap_bytes as u64;
        n
    }

    /// gen r4w4/oldgen4 — count a compacting collection sent to the in-place
    /// arm because its walk had a recoverable gap.
    pub fn note_walk_gap_compact_downgrade(&mut self) {
        self.sizing.walk_gap_compact_downgrades =
            self.sizing.walk_gap_compact_downgrades.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// gen r4w5/oldcompact5 (2026-09-24): sliding compaction around pinned objects.
// A separate `impl` block, like the wave-4 one above, so the hunks stay apart.
// ---------------------------------------------------------------------------

/// Why [`OldGen::compact_around_pins`] declined to compact.
///
/// On every refusal NOTHING was moved: the free list, `used_bytes`,
/// `high_water`, the block-offset table and `reclaim_epoch` are untouched, and
/// mark bits have only ever been ADDED (the pins, and what Phase 0's closure
/// promoted). So the caller can run the in-place sweep over the same grid, and
/// that is what `GenerationalHeap::old_gen_gc` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinnedCompactRefusal {
    /// The grid predates a free-list change (`walked_at_seq` is stale).
    StaleGrid,
    /// The walk did not account for every allocated byte (a walk gap, or an
    /// unhealthy free list): sliding would overwrite bytes nobody walked.
    WalkGap,
    /// A live object references an in-old-gen address the walk did not yield,
    /// so that slot could not be forwarded (the compactor's GCAUD-2 rule).
    Escape,
    /// A give-back or an interior decommit left an uncommitted run below
    /// `readable_end` (`readable_holes`) and the OS refused to commit it
    /// again. It lies inside a free block, which is exactly where a slide
    /// writes, so a destination there would fault. (gen r5w3/oldgen7: the
    /// compaction re-commits the holes first; before that it refused on any
    /// hole.)
    ReadableHoles,
}

impl PinnedCompactRefusal {
    /// A short name for a log line.
    pub fn label(self) -> &'static str {
        match self {
            Self::StaleGrid => "stale object grid",
            Self::WalkGap => "incomplete object walk",
            Self::Escape => "a live reference escapes the walk",
            Self::ReadableHoles => {
                "an uncommitted hole below the committed end the OS would not re-commit"
            }
        }
    }
}

/// What a completed [`OldGen::compact_around_pins`] did.
#[derive(Debug, Default)]
pub struct PinnedCompaction {
    /// `old address -> new address` for every object that moved, plus an
    /// identity entry for every watched referent that did not (the same
    /// contract as [`OldGen::compact`]'s map).
    pub pointer_map: cratonvm_types::PointerMap,
    /// FINAL addresses of the survivors that hold a reference into the
    /// `young` range the caller passed: the cards the caller must dirty so the
    /// next young collection still finds those old→young edges.
    pub young_ref_holders: Vec<usize>,
    /// Pinned objects (walked bases), objects moved, and bytes moved.
    pub pinned: usize,
    pub moved: usize,
    pub moved_bytes: usize,
    /// `used()` before and after (the dead bytes this compaction reclaimed).
    pub used_before: usize,
    pub used_after: usize,
    /// The largest free block before and after.
    pub largest_free_before: usize,
    pub largest_free_after: usize,
    /// gcd d9/a: the entries of the `watch` list
    /// ([`OldGen::compact_around_pins_watching`]) that were walked objects
    /// this compaction DROPPED (dead), ascending. Empty without a list.
    pub watch_dropped: Vec<usize>,
}

impl OldGen {
    /// Sliding mark-compact with PINNED objects: every object whose base is in
    /// `pins` keeps its address; every other marked object slides down to the
    /// lowest address above the previous survivor. Dead objects are dropped.
    ///
    /// gen r4w5/oldcompact5,
    /// `docs/internal/gc/gengc-r4w4-oom-humongous-on-fragmented-old-gen-FIXED-20260924.md`.
    /// [`Self::compact`] needs a root set it can rewrite completely; the
    /// non-moving collection runs exactly when it cannot (conservative words
    /// name old-gen objects and cannot be rewritten), so a fragmented old
    /// generation there had no repair and a humongous request threw a false
    /// `OutOfMemoryError`. The caller pins what a conservative word names and
    /// this moves everything else — G1's pinned regions, at object grain, or
    /// the "dense prefix" idea with any number of fixed islands.
    ///
    /// # Algorithm (one pass per phase, all O(objects + reference slots))
    ///
    /// * **Phase 0**: set `GC_FLAG_MARKED` on every pin (a pin is retained, so
    ///   what it references must be too), then close the marked set
    ///   ([`Self::close_live_set_over_old_gen`]). An escape refuses.
    /// * **Phase 1**: forwarding. A cursor starts at offset 0. A pinned object
    ///   gets `dest = src`, the bytes `[cursor, src)` become a free GAP, and the
    ///   cursor moves to its end. A movable object gets `dest = cursor`. Every
    ///   live object's mark word gets its forward (the pinned ones to
    ///   themselves), after snapshotting the lock state, exactly as in
    ///   [`Self::compact`].
    /// * **Phase 2**: every live object's reference slots are forwarded, and
    ///   an object with a reference into `young` is recorded (at its DEST).
    /// * **Phase 3**: slide, low to high (`memmove`), restoring each mark word.
    /// * **Phase 4**: the free list is the gaps plus the tail; the tail is
    ///   zeroed or given back as [`Self::compact`] does it.
    ///
    /// # Why the slide cannot overwrite anything (the invariant)
    ///
    /// Walked objects are ascending and disjoint, and every size and offset is
    /// a multiple of 8. By induction, `dest <= src` for every live object: the
    /// cursor after object `k` is `dest_k + size_k <= src_k + size_k <=
    /// src_{k+1}`. A pinned object `P` is processed before every object above
    /// it, so the cursor is at or above `P`'s end for all of them — no
    /// movable object lands on a pin. A movable object's destination ends at
    /// or below its own source's end, and every object below it has already
    /// been copied, so `memmove` in address order never overwrites a source
    /// that is still to be read.
    ///
    /// # What stays true afterwards
    ///
    /// * **`readable_end`** does not move, and every live byte is still below
    ///   it (nothing moves up), so `[0, readable_end)` stays committed and
    ///   [`READ_SLACK`] above it is untouched.
    /// * **`high_water`** becomes the end of the last survivor; the tail above
    ///   it is zeroed or reset to zero ([`Self::zero_or_give_back`]), so "above
    ///   the mark reads zero" still holds. The gaps in front of pins are
    ///   zeroed too (gen r4w6, review6 finding 4): unlike a block the in-place
    ///   sweep frees, which holds dead objects, a gap can hold a MOVED live
    ///   object's source image, forwarded header included, and H2-CID0 wants
    ///   a stale reference into compacted-away memory to read a zero header.
    /// * **`used_bytes`** is the sum of the survivors, and the free list (gaps
    ///   plus tail) is the rest of the capacity, so `capacity - used` is still
    ///   the free-list total. No two free blocks touch (a pinned survivor
    ///   separates every gap from the next piece), so the list is maximal.
    /// * **The block-offset table** is discarded (`note_relayout`) and
    ///   re-derived lazily, as after [`Self::compact`].
    /// * **`reclaim_epoch`** moves, so an open concurrent cycle's address-keyed
    ///   tables are abandoned.
    ///
    /// On `Err` nothing moved: see [`PinnedCompactRefusal`].
    ///
    /// `objects` must be [`Self::walk_objects`]' output taken at
    /// `walked_at_seq`, with the caller's marks set. `pins` are absolute
    /// object bases; entries that are not walked bases are ignored (a pin
    /// must name an object). `young` is `[lo, hi)` of the young generation.
    pub fn compact_around_pins(
        &mut self,
        objects: &[(*mut u8, usize)],
        walked_at_seq: u64,
        pins: Vec<usize>,
        young: (usize, usize),
        drop_flags: &HashMap<usize, u8>,
    ) -> Result<PinnedCompaction, PinnedCompactRefusal> {
        self.compact_around_pins_with_element_cards(
            objects,
            walked_at_seq,
            pins,
            young,
            drop_flags,
            None,
        )
    }

    /// [`Self::compact_around_pins`], recording young references at ELEMENT
    /// grain for reference arrays when `element_card_base` is `Some(card
    /// table base)` (an element-precise card table).
    ///
    /// gcd d3/n (2026-09-27), `gengc-r4w3-cards3-precise-array-cards-residuals`
    /// item 3, producer (d): [`PinnedCompaction::young_ref_holders`] held one
    /// FINAL base per survivor that references young, and the caller dirties
    /// those addresses' cards -- the HEADER card, "scan the whole object". A
    /// surviving wide reference array with one young element was then read
    /// whole at the next young pause, and on every pause after it until the
    /// element was promoted. With `element_card_base` it records
    /// `dest + ARRAY_DATA_OFFSET + i * ref_element_size()` for each young
    /// element `i`, one per card (`gen_heap::HolderCardDefer`, the rule the
    /// moving path's promoted scans use), which the consumer
    /// (`scan_dirty_cards_inner`) already reads element-precisely when the
    /// header card is clean. Every other survivor, and every survivor when it
    /// is `None`, records its final base exactly as before.
    pub fn compact_around_pins_with_element_cards(
        &mut self,
        objects: &[(*mut u8, usize)],
        walked_at_seq: u64,
        pins: Vec<usize>,
        young: (usize, usize),
        drop_flags: &HashMap<usize, u8>,
        element_card_base: Option<usize>,
    ) -> Result<PinnedCompaction, PinnedCompactRefusal> {
        self.compact_around_pins_watching(
            objects,
            walked_at_seq,
            pins,
            young,
            drop_flags,
            element_card_base,
            &[],
        )
    }

    /// [`Self::compact_around_pins_with_element_cards`], also reporting which
    /// entries of `watch` (ascending object bases) it DROPPED as dead
    /// ([`PinnedCompaction::watch_dropped`]).
    ///
    /// gcd d9/a: the true-root major (`gen_heap::TrueRootYoung`) no longer
    /// seeds this pause's promotion destinations as roots, so a destination
    /// the mark did not reach is dead, and the caller must stop reporting its
    /// young source as a survivor. After the slide the address of a dropped
    /// object may hold a moved one, so the answer is recorded here, at the
    /// only point that knows it: Phase 1's dead arm, after Phase 0's closure.
    #[allow(clippy::too_many_arguments)]
    pub fn compact_around_pins_watching(
        &mut self,
        objects: &[(*mut u8, usize)],
        walked_at_seq: u64,
        mut pins: Vec<usize>,
        young: (usize, usize),
        drop_flags: &HashMap<usize, u8>,
        element_card_base: Option<usize>,
        watch: &[usize],
    ) -> Result<PinnedCompaction, PinnedCompactRefusal> {
        debug_assert!(
            watch.windows(2).all(|w| w[0] < w[1]),
            "compact_around_pins_watching: `watch` must be ascending",
        );
        if walked_at_seq != self.free_list_seq.get() {
            return Err(PinnedCompactRefusal::StaleGrid);
        }
        // Every destination below must be committed memory. `[0, readable_end)`
        // is, and every destination lies below its object's source, which is
        // allocated and so below `readable_end` — unless a give-back or an
        // interior decommit left an uncommitted run inside a free block
        // (`readable_holes`).
        //
        // gen r5w3/oldgen7: RE-COMMIT then, as `compact_walked` does, instead
        // of refusing (`gengc-r5w1-oldgen5-proposal-old-gen-interior-decommit`
        // blocker 1): holes are routine under `CRATONVM_GC_OLD_INTERIOR_DECOMMIT`,
        // and refusing here switched the pinned compaction — and the default
        // humongous compaction on the non-moving path — off for the rest of
        // the run. The compaction is about to fill free space low in the
        // generation anyway; what it leaves free a later resize can release
        // again. Refuse only when the OS says no. Nothing has moved yet, and a
        // commit changes no object, so a refusal still leaves the generation
        // exactly as the caller handed it over.
        if self.readable_holes {
            let end = self.readable_end.saturating_add(READ_SLACK).min(self.data.len());
            if !self.data.commit_range(0, end) {
                return Err(PinnedCompactRefusal::ReadableHoles);
            }
            self.readable_holes = false;
        }
        // The GCAUD-9 test `compact_walked` abandons on: a walk that does not
        // account for every allocated byte left an occupant nobody walked.
        let walked_bytes: usize = objects.iter().map(|&(_, size)| size).sum();
        if walked_bytes != self.used_bytes {
            return Err(PinnedCompactRefusal::WalkGap);
        }
        debug_assert!(
            objects.windows(2).all(|w| (w[0].0 as usize) < (w[1].0 as usize)),
            "compact_around_pins requires ascending object starts",
        );
        let walked = |addr: usize| {
            objects
                .binary_search_by_key(&addr, |&(p, _)| p as usize)
                .is_ok()
        };
        pins.sort_unstable();
        pins.dedup();
        pins.retain(|&p| walked(p));

        // Phase 0a: a pin is retained. Marking it BEFORE the closure makes the
        // closure trace it, so nothing a pinned object references is dropped.
        for &p in &pins {
            // SAFETY: `p` is an object base the walk yielded (filtered just
            // above), so its whole header lies inside an allocated region,
            // which is below `readable_end` and therefore committed; nothing
            // has moved yet.
            let h = unsafe { &*(p as *const ObjectHeader) };
            if h.gc_flags() & GC_FLAG_MARKED == 0 {
                h.add_gc_flags(GC_FLAG_MARKED);
            }
        }
        // Phase 0b: the compactor's dangling-reference guard, unchanged.
        let data_span = self.extent();
        if Self::close_live_set_over_old_gen(objects, data_span, &[]).1 {
            return Err(PinnedCompactRefusal::Escape);
        }

        // Committed from here on: every early return is above.
        let used_before = self.used_bytes;
        let largest_free_before = self.largest_free_block();
        self.reclaim_epoch = self.reclaim_epoch.wrapping_add(1);
        self.bot.get_mut().note_relayout();
        let base = self.data.as_mut_ptr();
        let base_addr = base as usize;

        // Phase 1: forwarding.
        let mut out = PinnedCompaction {
            used_before,
            largest_free_before,
            ..PinnedCompaction::default()
        };
        // (src, size, dest, saved mark word) per survivor — see
        // `compact_walked`'s `live_objects` for why the mark is snapshotted.
        let mut live: Vec<(*mut u8, usize, *mut u8, u32)> = Vec::new();
        // Every survivor's destination, pinned ones included. Phase 2 resolves
        // a referent here rather than through a header forward, for
        // `compact_walked`'s reason: since the 8-byte header a header forward
        // parks its target in the object's SECOND WORD, which is a compact
        // instance's first field, and Phase 2 reads exactly those words of
        // every live holder while the forwards would be installed.
        let mut destinations: rustc_hash::FxHashMap<usize, usize> =
            rustc_hash::FxHashMap::default();
        // Free gaps left in front of pinned objects, as (offset, size).
        let mut gaps: Vec<(usize, usize)> = Vec::new();
        let mut cursor: usize = 0;
        let mut pin_i = 0usize;
        for &(obj_ptr, total_size) in objects {
            let addr = obj_ptr as usize;
            // SAFETY: a base the walk yielded, not yet moved (Phase 3 is the
            // first write outside mark words); header inside allocated,
            // committed storage.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            if header.gc_flags() & GC_FLAG_MARKED == 0 {
                // Dead: the same reclamation record `compact_walked` keeps.
                crate::gen_heap::record_old_freed(
                    addr,
                    total_size,
                    header.class_id.as_u32(),
                    ObjectHeader::kind_tag(
                        header.mark_word.load(std::sync::atomic::Ordering::Relaxed),
                    ),
                    crate::gen_heap::OLD_FREED_SITE_COMPACT,
                    drop_flags.get(&addr).copied().unwrap_or(0),
                );
                // gcd d9/a: ascending walk, ascending list: `watch_dropped`
                // comes out ascending too.
                if !watch.is_empty() && watch.binary_search(&addr).is_ok() {
                    out.watch_dropped.push(addr);
                }
                continue;
            }
            let off = addr - base_addr;
            while pin_i < pins.len() && pins[pin_i] < addr {
                pin_i += 1;
            }
            let pinned = pins.get(pin_i) == Some(&addr);
            debug_assert!(
                cursor <= off && cursor % 8 == 0 && off % 8 == 0,
                "compact_around_pins: cursor {cursor:#x} above object {off:#x}, or unaligned",
            );
            let dest_off = if pinned {
                if off > cursor {
                    gaps.push((cursor, off - cursor));
                }
                out.pinned += 1;
                off
            } else {
                cursor
            };
            // SAFETY: `dest_off <= off < capacity`, so the pointer stays inside
            // the reservation (and inside committed storage: see the
            // invariant in the doc above).
            let dest = unsafe { base.add(dest_off) };
            let saved_mark = header.mark_word.load(std::sync::atomic::Ordering::Relaxed);
            destinations.insert(addr, dest as usize);
            if dest != obj_ptr {
                out.pointer_map.insert(addr, dest as usize);
                out.moved += 1;
                out.moved_bytes += total_size;
            } else if crate::gc_quiescence::is_watched_referent(addr) {
                // An object that stays put gets no map entry, and reference
                // processing reads "absent" as "died": the identity entry is
                // `compact_walked`'s rule for the same case.
                out.pointer_map.insert(addr, addr);
            }
            live.push((obj_ptr, total_size, dest, saved_mark));
            cursor = dest_off + total_size;
        }

        // Phase 2: forward every live object's references (pinned ones
        // included — a pin may point at a moved object), and note which
        // survivors hold a young reference: the survivor's FINAL base, or
        // (gcd d3/n, element-precise table) each young reference-array
        // element's final address, one per card.
        let mut young_cards: Vec<(usize, usize)> = Vec::new();
        for &(obj_ptr, _, dest, _) in &live {
            // SAFETY: a live walked object at its source address; Phase 1 wrote
            // nothing into it, so the layout reads below are sound.
            let header = unsafe { &*(obj_ptr as *const ObjectHeader) };
            let mut defer =
                crate::gen_heap::HolderCardDefer::new(dest as usize, header, element_card_base);
            Self::update_refs_noting_young(
                obj_ptr,
                header,
                data_span,
                young,
                &destinations,
                &mut defer,
                &mut young_cards,
            );
            defer.finish(&mut young_cards);
            out.young_ref_holders
                .extend(young_cards.drain(..).map(|(holder, off)| holder + off));
        }

        // Phase 3: slide, in ascending source order (see the invariant).
        for &(src, size, dest, saved_mark) in &live {
            if src != dest {
                // SAFETY: `[src, src + size)` is the object Phase 1 walked;
                // `[dest, dest + size)` lies at or below it, so below
                // `readable_end`, which is committed because any hole was
                // re-committed at entry; it overlaps no pinned object and no
                // source still to be copied (the doc's invariant); `copy` is a
                // `memmove`.
                unsafe { std::ptr::copy(src, dest, size) };
            }
            // SAFETY: `dest` now holds this object's header (copied, or never
            // moved).
            let final_header = unsafe { &*(dest as *const ObjectHeader) };
            final_header
                .mark_word
                .store(saved_mark, std::sync::atomic::Ordering::Relaxed);
            final_header.clear_gc_flags(GC_FLAG_MARKED);
        }

        // Phase 4: the free list is the gaps plus the tail.
        self.invalidate_sorted_free();
        #[cfg(debug_assertions)]
        self.unzeroed_buffer_carves.clear();
        for bucket in &mut self.buckets {
            bucket.clear();
        }
        for &(offset, size) in &gaps {
            // gen r4w6 (review6 finding 4), H2-CID0: a gap in front of a pin
            // can still hold a moved object's source bytes -- a header with
            // `is_forwarded()` set and a forwarding address to a live object.
            // A stale conservative word or dirty-card walk landing here must
            // read a zero header, as `compact`'s freed tail does. The gaps lie
            // below `readable_end`, committed because any hole was re-committed
            // at entry.
            self.data.fill_zero(offset, offset + size);
            self.buckets[bucket_for(size)].push(FreeBlock { offset, size });
        }
        let end = cursor;
        if end < self.data.len() {
            // `[end, high_water)` holds no survivor: every survivor ends at or
            // below `end`. Bytes at or above `high_water` were never written.
            let zero_end = self.high_water.min(self.data.len());
            self.zero_or_give_back(end, zero_end);
            let free_size = self.data.len() - end;
            self.buckets[bucket_for(free_size)].push(FreeBlock {
                offset: end,
                size: free_size,
            });
        }
        self.used_bytes = live.iter().map(|&(_, size, _, _)| size).sum();
        self.free_list_maximal = true;
        self.high_water = end;
        self.frag_pending_request = 0;
        self.frag_compaction_requested = false;
        self.humongous_refusal_pending = false;
        self.humongous_refused_bytes = 0;
        self.sizing.compactions = self.sizing.compactions.wrapping_add(1);
        self.sizing.pinned_compactions = self.sizing.pinned_compactions.wrapping_add(1);
        self.sizing.pinned_compaction_pins_last = out.pinned as u64;
        self.sizing.pinned_compaction_moved_bytes = self
            .sizing
            .pinned_compaction_moved_bytes
            .wrapping_add(out.moved_bytes as u64);
        out.used_after = self.used_bytes;
        out.largest_free_after = self.largest_free_block();
        Ok(out)
    }

    /// [`Self::update_refs_in_object`], plus: report each slot that holds a
    /// reference into `young` (`[lo, hi)`) to `defer` (gcd d3/n: by the slot's
    /// byte offset, so a reference array on an element-precise table records
    /// its element's card; every other holder owes its header card). Young
    /// references are left as they are (the young generation does not move
    /// here).
    fn update_refs_noting_young(
        obj_ptr: *mut u8,
        header: &ObjectHeader,
        data: (usize, usize),
        young: (usize, usize),
        destinations: &rustc_hash::FxHashMap<usize, usize>,
        defer: &mut crate::gen_heap::HolderCardDefer,
        young_cards: &mut Vec<(usize, usize)>,
    ) {
        let (data_start, data_end) = data;
        let (young_lo, young_hi) = young;
        // SAFETY: `obj_ptr`/`header` are a live walked old-gen object inside a
        // stop-the-world pause; `forward_ref_slots_at` reads only the slots its
        // layout names. A referent is dereferenced only when it lies inside
        // this generation's reservation, and Phase 0's closure guarantees such
        // a referent is a walked, marked object base, hence in `destinations`.
        unsafe {
            crate::gen_heap::forward_ref_slots_at(obj_ptr, header, |off, ref_ptr| {
                let r = ref_ptr as usize;
                if r >= young_lo && r < young_hi {
                    defer.slot_stays_young(off, young_cards);
                    return None;
                }
                if r < data_start || r >= data_end {
                    return None;
                }
                if let Some(&dest) = destinations.get(&r) {
                    Some(dest as *mut u8)
                } else {
                    debug_assert!(
                        false,
                        "compact_around_pins: live holder@0x{:x} -> unforwarded old-gen \
                         referent@0x{r:x}; the Phase 0 closure missed an edge",
                        obj_ptr as usize,
                    );
                    None
                }
            });
        }
    }

    /// gen r4w5/oldcompact5 — count a pinned compaction the collector declined
    /// (its gate, a walk gap, an escape); returns the count before this one.
    pub fn note_pinned_compaction_refusal(&mut self) -> u64 {
        let n = self.sizing.pinned_compaction_refusals;
        self.sizing.pinned_compaction_refusals = n.wrapping_add(1);
        n
    }
}

impl std::fmt::Debug for OldGen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OldGen")
            .field("used", &self.used_bytes)
            .field("capacity", &self.data.len())
            .field("free_blocks", &self.free_block_count())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out `count` fixed-size objects back to back in a fresh old gen
    /// and return their `(ptr, offset)` pairs. Headers must be real: the
    /// walker derives every object boundary by striding header-to-header.
    fn old_gen_with_objects(count: usize, slots: u32) -> (OldGen, Vec<usize>) {
        let mut og = OldGen::new(64 * 1024);
        let base = og.base_ptr() as usize;
        let mut offsets = Vec::new();
        for i in 0..count {
            let body = SLOT_SIZE * slots as usize;
            let ptr = og.alloc(HEADER_SIZE + body, 8).unwrap();
            // SAFETY: `ptr` is a fresh, correctly sized old-gen allocation.
            unsafe {
                std::ptr::write(
                    ptr as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        slots,
                    ),
                );
            }
            offsets.push(ptr as usize - base);
        }
        (og, offsets)
    }

    /// The reference semantics this walk had before it was given a monotone
    /// cursor: an object is collected iff its start offset lies in ANY dirty
    /// range. Kept here as the oracle the fast path is compared against.
    fn expected_in_ranges(offsets: &[usize], ranges: &[(usize, usize)]) -> Vec<usize> {
        offsets
            .iter()
            .copied()
            .filter(|off| ranges.iter().any(|&(s, e)| *off >= s && *off < e))
            .collect()
    }

    fn collected_offsets(og: &OldGen, ranges: &[(usize, usize)]) -> Vec<usize> {
        let base = og.base_ptr() as usize;
        og.walk_objects_in_card_ranges(ranges)
            .into_iter()
            .map(|(ptr, _)| ptr as usize - base)
            .collect()
    }

    #[test]
    fn card_range_walk_collects_exactly_the_objects_starting_in_a_dirty_range() {
        let (og, offsets) = old_gen_with_objects(8, 2);
        // One range covering objects 2..4 only.
        let ranges = vec![(offsets[2], offsets[4])];
        assert_eq!(
            collected_offsets(&og, &ranges),
            expected_in_ranges(&offsets, &ranges)
        );
        assert_eq!(
            collected_offsets(&og, &ranges),
            vec![offsets[2], offsets[3]]
        );
    }

    #[test]
    fn card_range_walk_handles_several_disjoint_ranges() {
        let (og, offsets) = old_gen_with_objects(10, 3);
        // Sorted, non-overlapping, with gaps — what the card table produces.
        let ranges = vec![
            (offsets[1], offsets[2]),
            (offsets[4], offsets[6]),
            (offsets[9], offsets[9] + 8),
        ];
        assert_eq!(
            collected_offsets(&og, &ranges),
            expected_in_ranges(&offsets, &ranges)
        );
    }

    #[test]
    fn card_range_walk_is_empty_when_no_object_starts_in_a_range() {
        let (og, offsets) = old_gen_with_objects(6, 2);
        // A range strictly inside object 3's body, so no object *starts* in it.
        let ranges = vec![(offsets[3] + 8, offsets[3] + 16)];
        assert!(collected_offsets(&og, &ranges).is_empty());
        assert!(expected_in_ranges(&offsets, &ranges).is_empty());
    }

    #[test]
    fn card_range_walk_matches_the_any_predicate_for_every_prefix_and_suffix() {
        // The monotone cursor added 2026-08-02 replaced a per-object
        // `ranges.iter().any(..)`, and returns early once the cursor passes
        // the last range. Sweep a family of range sets — including ones that
        // end before the last object, which is what exercises that early
        // return — and require the two to agree exactly.
        let (og, offsets) = old_gen_with_objects(12, 2);
        for lo in 0..offsets.len() {
            for hi in (lo + 1)..=offsets.len() {
                let end = if hi == offsets.len() {
                    offsets[hi - 1] + 8
                } else {
                    offsets[hi]
                };
                let ranges = vec![(offsets[lo], end)];
                assert_eq!(
                    collected_offsets(&og, &ranges),
                    expected_in_ranges(&offsets, &ranges),
                    "disagreement for objects {lo}..{hi}"
                );
            }
        }
    }

    /// A NESTED free block must not drag the card-range walk's cursor
    /// backwards, which would make it parse free bytes as object headers.
    ///
    /// gengc-round1 2026-09-20. `walk_objects` has taken `cursor.max(..)` since
    /// the `OLD_FREE_LIST_OVERLAPS` double-free diagnostic landed;
    /// `walk_objects_in_card_ranges` — the walk the minor GC's old→young card
    /// scan actually uses — did not. A phantom header on this path truncates
    /// the dirty-card root set rather than merely mis-reporting an object, and
    /// a lost old→young root is a live young object freed.
    ///
    /// The nesting is produced the way production can produce it: an over-free
    /// whose span is not object-aligned — the GCAUD-1 shape, where `free`'s
    /// rounding disagreed with `alloc`'s and left a sliver. `coalesce_free_blocks`
    /// deliberately refuses to collapse overlapping blocks, so the nesting
    /// survives to the next walk.
    #[test]
    fn card_range_walk_survives_a_nested_free_block() {
        let (mut og, offsets) = old_gen_with_objects(8, 2);
        let base = og.base_ptr() as usize;
        let stride = offsets[1] - offsets[0];
        // Outer: objects 2..6 freed as one span. Inner: a 24-byte span nested
        // inside it that ends in the MIDDLE of object 3's footprint.
        // SAFETY: both spans lie inside blocks this old gen allocated. The
        // overlap is the defect being modelled; nothing reads the freed bytes.
        unsafe {
            og.free((base + offsets[2]) as *mut u8, stride * 4);
            og.free((base + offsets[3]) as *mut u8, 24);
        }
        // Without the `max` the cursor rewinds from 6*stride back to
        // `offsets[3] + 24` — an address inside a freed object's zeroed
        // payload — and the walk parses those bytes as a run of phantom
        // 16-byte objects, so the objects that really are there (6 and 7) are
        // never yielded and a pile that is not there is.
        let full = vec![(0usize, offsets[7] + stride)];
        assert_eq!(
            collected_offsets(&og, &full),
            vec![offsets[0], offsets[1], offsets[6], offsets[7]],
            "a nested free block must not rewind the card-range walk's cursor",
        );
        // The real invariant: this walk and `walk_objects` (which has carried
        // the guard all along) must agree on an all-covering range.
        let all: Vec<usize> = og
            .walk_objects()
            .into_iter()
            .map(|(p, _)| p as usize - base)
            .collect();
        assert_eq!(
            collected_offsets(&og, &full),
            all,
            "the filtered and unfiltered old-gen walks disagree",
        );
    }

    #[test]
    fn card_range_walk_with_no_ranges_collects_nothing() {
        let (og, _offsets) = old_gen_with_objects(4, 2);
        assert!(collected_offsets(&og, &[]).is_empty());
    }

    /// gen-gc-five: a promotion buffer's unused tail goes back to the free
    /// list without stamping the reclaim epoch (it never held an object, so
    /// no remark snapshot can name it), and is servable again at once.
    #[test]
    fn releasing_an_unused_tail_keeps_the_reclaim_epoch() {
        let mut og = OldGen::new(64 * 1024);
        let epoch = og.reclaim_epoch();
        // gengc-round2: a promotion buffer is carved with the BUFFER form, so
        // the debug carve registry knows its tail may come back. See
        // `OldGen::unzeroed_buffer_carves`.
        let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
        let used = og.used();
        // SAFETY: `[p + 1024, p + 4096)` is the untouched tail of the block just carved.
        unsafe { og.release_unused_tail(p.add(1024), 3072) };
        assert_eq!(
            og.reclaim_epoch(),
            epoch,
            "a never-used tail is not a reclaim"
        );
        assert_eq!(og.used(), used - 3072);
        let again = og
            .alloc(3072, 8)
            .expect("the tail is a servable free block");
        assert_eq!(again as usize, p as usize + 1024);
    }

    /// gengc-round1 2026-09-20. `compact`'s Phase 4 zero is now bounded by the
    /// high-water mark instead of by CAPACITY, so the pause no longer carries
    /// a `memset` of the whole trailing free block (nor commits every page of
    /// a store `OldGen::new` deliberately leaves unmapped).
    ///
    /// Two properties have to survive that: every byte that ever held an
    /// object is still zeroed — H2-CID0's diagnostic depends on a stale
    /// reference reading an all-zero `ClassId(0)` header rather than plausible
    /// garbage — and the mark itself must track allocation, or the bound could
    /// silently shrink below what was written.
    #[test]
    fn compaction_still_zeroes_everything_that_ever_held_an_object() {
        let (mut og, offsets) = old_gen_with_objects(8, 2);
        let stride = offsets[1] - offsets[0];
        let top = offsets[7] + stride;
        assert_eq!(
            og.high_water(),
            top,
            "the high-water mark must follow the allocations",
        );
        assert!(
            og.capacity() > top * 8,
            "the whole point is a capacity far above the high-water mark",
        );

        // Object 0 is live; 1..8 are garbage this compaction drops.
        let base = og.base_ptr() as usize;
        // SAFETY: `old_gen_with_objects` wrote a valid header here.
        unsafe {
            (*((base + offsets[0]) as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
        }
        let _moved = og.compact();

        assert_eq!(og.used(), stride, "only the marked object survives");
        // SAFETY: `[stride, top)` is inside the backing store and now free.
        let dropped =
            unsafe { std::slice::from_raw_parts((base + stride) as *const u8, top - stride) };
        assert!(
            dropped.iter().all(|&b| b == 0),
            "a compacted-away object left non-zero bytes behind — a stale \
             reference to it would read plausible garbage instead of ClassId(0)",
        );
        assert_eq!(
            og.high_water(),
            og.used(),
            "the mark must come back down with the compacted end, or the next \
             compaction re-zeroes storage it has already proved clean",
        );
    }

    /// The bounded best-fit probe must never turn "there is a fit" into an OOM.
    ///
    /// gengc-round1 2026-09-20. `alloc_from_buckets` now stops REFINING the
    /// best fit after [`BEST_FIT_PROBE_LIMIT`] fitting candidates, because a
    /// bucket's length is unbounded even though its blocks' sizes are within a
    /// factor of two — a non-compacting sweep can leave one bucket holding
    /// every block it freed, reinstating the O(N) scan the segregated free
    /// list exists to remove. The cap counts FITTING candidates only, so a
    /// bucket with no fit is still scanned exhaustively; this pins that, which
    /// is the round-5 #2 "cannot spuriously OOM" property.
    ///
    /// gen r4w4/oldgen4: with four classes per power of two the pair is
    /// `256`/`312` (both in `[256, 320)`), and the generation is filled
    /// EXACTLY so no other free block exists: the non-fit deferral
    /// (`NONFIT_PROBE_LIMIT`) then finds no larger class and must finish the
    /// deferred one — the "cannot spuriously OOM" half of its contract.
    #[test]
    fn a_lone_fit_behind_a_long_run_of_too_small_blocks_is_still_found() {
        // All of these land in the same class (`[256, 320)`), which is also
        // the class a 300-byte request starts its search in.
        const TOO_SMALL: usize = 256;
        const FITS: usize = 312;
        const RUN: usize = BEST_FIT_PROBE_LIMIT * 2;
        assert!(RUN > NONFIT_PROBE_LIMIT, "the run must outlast the non-fit probe cap");
        assert_eq!(bucket_for(TOO_SMALL), bucket_for(FITS));
        assert_eq!(min_satisfying_bucket(300 + 7), bucket_for(TOO_SMALL));
        let mut og = OldGen::new(RUN * TOO_SMALL + FITS);

        let mut small = Vec::new();
        for _ in 0..RUN {
            small.push(og.alloc(TOO_SMALL, 8).expect("sized for exactly these"));
        }
        let big = og.alloc(FITS, 8).expect("and one larger block");
        assert_eq!(og.free_block_count(), 0, "no other free block to escalate to");
        for p in small {
            // SAFETY: each came from `alloc(TOO_SMALL, 8)` on this old gen and
            // is freed exactly once with its own size.
            unsafe { og.free(p, TOO_SMALL) };
        }
        // Freed LAST, so it sits at the end of the bucket — behind more
        // non-fitting blocks than the probe limit.
        // SAFETY: as above.
        unsafe { og.free(big, FITS) };

        let got = og
            .alloc(300, 8)
            .expect("the only fitting block in the bucket must still be found");
        assert_eq!(
            got as usize, big as usize,
            "the allocator served the request from somewhere other than the \
             one block in this bucket that fits",
        );
        let s = og.sizing_stats();
        assert_eq!(
            (s.nonfit_deferrals, s.nonfit_deferral_resumes),
            (1, 1),
            "the scan deferred after the cap and had to come back"
        );
    }

    /// gen r4w4/oldgen4 — the other half of the non-fit cap: when a LARGER
    /// class can serve the request, a class holding more than
    /// `NONFIT_PROBE_LIMIT` too-small blocks in front of its one fit costs
    /// exactly `NONFIT_PROBE_LIMIT` probes, not a walk of the class. The fit
    /// behind the run stays on the free list, still findable.
    #[test]
    fn a_long_run_of_too_small_blocks_is_deferred_when_a_larger_class_fits() {
        const TOO_SMALL: usize = 256;
        const FITS: usize = 312;
        const RUN: usize = NONFIT_PROBE_LIMIT * 4;
        let mut og = OldGen::new(128 * 1024);
        let mut small = Vec::new();
        for _ in 0..RUN {
            small.push(og.alloc(TOO_SMALL, 8).expect("128 KiB covers these"));
        }
        let fit = og.alloc(FITS, 8).expect("and the fit");
        for p in small {
            // SAFETY: each came from `alloc(TOO_SMALL, 8)` and is freed once.
            unsafe { og.free(p, TOO_SMALL) };
        }
        // SAFETY: as above.
        unsafe { og.free(fit, FITS) };

        let before = og.trigger_stats().nonfit_probes;
        let got = og.alloc(300, 8).expect("the trailing block serves it");
        assert_ne!(got, fit, "the larger class won: the deep hole was deferred");
        assert_eq!(
            og.trigger_stats().nonfit_probes - before,
            NONFIT_PROBE_LIMIT as u64,
            "bounded by the cap, not by the run"
        );
        assert_eq!(og.sizing_stats().nonfit_deferrals, 1);
        assert_eq!(og.sizing_stats().nonfit_deferral_resumes, 0);
        // The fit is still on the free list: the exhaustive fallback finds it
        // once nothing larger is left (exercised by the lone-fit test above);
        // here, a walk of the free list says so directly.
        let fit_off = fit as usize - og.base_ptr() as usize;
        assert!(og.with_sorted_free_blocks(|s| s
            .iter()
            .any(|b| b.offset == fit_off && b.size == FITS)));
    }

    /// gen r4w4/oldgen4 — the size classes: four per power of two, lower
    /// bounds strictly increasing, and every class above a request's start
    /// class holds only blocks larger than the request.
    #[test]
    fn size_classes_split_each_power_of_two_in_quarters() {
        assert_eq!(bucket_for(0), 0);
        assert_eq!(bucket_for(8), 0);
        assert_eq!(bucket_for(16), SUB_CLASSES);
        // Below 64 bytes every 8-aligned size has a class of its own.
        let small: Vec<usize> = (2..8).map(|k| bucket_for(8 * k)).collect();
        let mut dedup = small.clone();
        dedup.dedup();
        assert_eq!(small, dedup, "{small:?}");
        // [256, 320) [320, 384) [384, 448) [448, 512)
        assert_eq!(bucket_for(256), bucket_for(319));
        assert_eq!(bucket_for(320), bucket_for(256) + 1);
        assert_eq!(bucket_for(511), bucket_for(256) + 3);
        assert_eq!(bucket_for(512), bucket_for(256) + 4);
        // Monotone, and (from 32 bytes, where a class is at least 8 bytes
        // wide) consecutive 8-aligned sizes never skip a class.
        let mut prev = bucket_for(32);
        for size in (32..1 << 20).step_by(8) {
            let b = bucket_for(size);
            assert!(b >= prev && b <= prev + 1, "size {size}: {prev} -> {b}");
            prev = b;
        }
        assert_eq!(bucket_for(usize::MAX), NUM_BUCKETS - 1);
        assert_eq!(bucket_for(1 << 40), NUM_BUCKETS - 1);
    }

    #[test]
    fn new_old_gen() {
        let og = OldGen::new(4096);
        assert_eq!(og.used(), 0);
        assert_eq!(og.capacity(), 4096);
        // Round-5 #14: free_list is now segregated; only the count is observable.
        assert_eq!(og.free_block_count(), 1);
    }

    #[test]
    fn alloc_basic() {
        let mut og = OldGen::new(4096);
        let ptr = og.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(og.used(), 64);
        assert!(og.contains(ptr));
    }

    #[test]
    fn alloc_multiple() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(128, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(og.used(), 192);
    }

    #[test]
    fn alloc_alignment() {
        let mut og = OldGen::new(4096);
        // Allocate 3 bytes, then 64 with align 8
        let _p1 = og.alloc(3, 1).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        // p2 should be 8-byte aligned
        assert_eq!(p2 as usize % 8, 0);
    }

    #[test]
    fn alloc_alignment_reserves_compact_object_padding() {
        let mut og = OldGen::new(4096);
        let first = og.alloc(44, 8).unwrap();
        let second = og.alloc(8, 8).unwrap();

        assert_eq!(second as usize - first as usize, 48);
        // OldGen reserves at least one header for any request, so the second
        // 8-byte request occupies one production header after the 48-byte
        // aligned extent. Derived rather than restated: this number moved 80 ->
        // 72 with the 2026-08-06 header shrink, and a literal here would have
        // to be re-derived by hand every time the layout moves.
        assert_eq!(og.used(), 48 + cratonvm_types::HEADER_SIZE);
    }

    #[test]
    fn alloc_full_returns_none() {
        let mut og = OldGen::new(128);
        let _p1 = og.alloc(128, 1).unwrap();
        assert!(og.alloc(1, 1).is_none());
    }

    #[test]
    fn free_basic() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        assert_eq!(og.used(), 64);

        unsafe { og.free(p1, 64) };
        assert_eq!(og.used(), 0);
        // Should be able to allocate again
        let p2 = og.alloc(64, 8).unwrap();
        assert!(!p2.is_null());
    }

    /// xt-helper-window OOM regression: with compaction unavailable, freeing
    /// a run of adjacent blocks must not leave the generation unable to serve
    /// a request that the run's COMBINED bytes cover. Before the fix the size
    /// buckets held N isolated blocks of `S` bytes and an `S * 4` request
    /// returned `None` even though `N * S` contiguous bytes were free.
    ///
    /// The trailing free space is deliberately consumed first: without that,
    /// the request is served from the untouched tail and the test proves
    /// nothing.
    #[test]
    fn adjacent_frees_are_coalesced_so_a_larger_request_still_fits() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 256;
        const N: usize = 16;

        let mut ptrs = Vec::new();
        for _ in 0..N {
            ptrs.push(og.alloc(S, 8).expect("fresh old gen must serve S bytes"));
        }
        let tail = og.capacity() - og.used();
        og.alloc(tail, 8)
            .expect("the trailing free block must be allocatable");
        assert_eq!(og.free_block_count(), 0, "no free space must remain");

        for p in ptrs.drain(..) {
            // SAFETY: each `p` came from `alloc(S, 8)` on this OldGen and is
            // freed exactly once, with its own size.
            unsafe { og.free(p, S) };
        }
        assert_eq!(og.free_block_count(), N, "N isolated blocks, all adjacent");

        let big = S * 4;
        assert!(
            og.alloc(big, 8).is_some(),
            "a {big}-byte request must be served out of {N} adjacent {S}-byte \
             free blocks — this is the old-gen half of the xt-helper-window \
             OutOfMemoryError (the free list never coalesces without compaction)"
        );
    }

    /// gengc-round2. The mirror of
    /// `adjacent_frees_are_coalesced_so_a_larger_request_still_fits`, which is
    /// what `gengc-oldgen-fragmentation-has-no-trigger-FIXED-20260924.md` asks for:
    /// build the fragmented state, assert the POLICY PREDICATE fires, run the
    /// scheduled repair, assert the signal recovers.
    ///
    /// The point of the test is that the trigger no longer requires an
    /// allocation to have already failed.
    #[test]
    fn fragmentation_is_detected_before_an_allocation_fails_and_the_repair_fixes_it() {
        const BIG: usize = 2048;
        const SMALL: usize = 1024;
        const FREED: usize = 40;

        let mut og = OldGen::new(64 * 1024);
        // One larger object sets the scale the predicate judges against, then
        // the rest of the generation is filled with SMALL blocks so nothing is
        // left over — the free list this test builds must be the fragmented
        // middle, not an untouched tail.
        og.alloc(BIG, 8).unwrap();
        let small_count = (og.capacity() - og.used()) / SMALL;
        let mut ptrs = Vec::new();
        for _ in 0..small_count {
            ptrs.push(og.alloc(SMALL, 8).unwrap());
        }
        assert_eq!(og.free_block_count(), 0, "no free space must remain");
        assert_eq!(
            og.recent_max_request(),
            BIG,
            "the allocator must remember the largest request it served",
        );

        // A contiguous run of small blocks goes back: the SUM of free bytes is
        // large, every individual hole is smaller than the largest request.
        for p in ptrs.drain(..FREED) {
            // SAFETY: each `p` came from `alloc(SMALL, 8)` and is freed once.
            unsafe { og.free(p, SMALL) };
        }
        assert_eq!(og.free_bytes_and_largest(), (FREED * SMALL, SMALL));
        assert!(
            og.fragmentation_repair_due(),
            "{FREED} adjacent {SMALL}-byte holes with a {BIG}-byte recent \
             request is exactly the 'bytes but no room' state the pair was \
             added to detect (free={:?} recent_max={})",
            og.free_bytes_and_largest(),
            og.recent_max_request(),
        );

        let merged = og.repair_fragmentation();
        assert_eq!(merged, FREED - 1, "every hole was adjacent to the next");
        assert_eq!(og.fragmentation_repairs(), 1);
        assert_eq!(og.free_block_count(), 1);
        assert_eq!(og.largest_free_block(), FREED * SMALL);

        // The reset of `recent_max_request` is the hysteresis: an idle
        // generation cannot provoke a second repair.
        assert_eq!(og.recent_max_request(), 0);
        assert!(!og.fragmentation_repair_due());
    }

    /// The predicate is a pure function of four numbers so it can be tested
    /// without a heap, and each of its three clauses suppresses a specific
    /// false positive. Pin all of them.
    #[test]
    fn the_fragmentation_predicate_suppresses_each_false_positive() {
        let cap = 1024 * 1024;
        let fires = |free, largest, recent| {
            OldGen::fragmentation_warrants_repair(cap, free, largest, recent)
        };

        // The real thing: plenty free, no hole big enough for what is being
        // asked for.
        assert!(fires(cap / 2, 1024, 4096));

        // Nothing allocated since the last repair — no scale to judge against,
        // and the clause that stops a repair firing every cycle.
        assert!(!fires(cap / 2, 1024, 0));

        // Genuinely near-full for the sizes being asked for: coalescing cannot
        // conjure bytes. At the K boundary and just over it.
        assert!(!fires(FRAGMENTATION_FIT_FACTOR * 4096 - 1, 1024, 4096));
        assert!(fires(FRAGMENTATION_FIT_FACTOR * 4096, 1024, 4096));

        // Not fragmented: some single hole still serves the biggest recent
        // request. At the boundary and one byte below it.
        assert!(!fires(cap / 2, 4096, 4096));
        assert!(fires(cap / 2, 4095, 4096));

        // A negative control the gap page calls for: a generation allocated
        // once and never freed must never trigger.
        let mut og = OldGen::new(64 * 1024);
        for _ in 0..8 {
            og.alloc(512, 8).unwrap();
        }
        assert!(
            !og.fragmentation_repair_due(),
            "an unfragmented generation must not schedule a repair",
        );
    }

    /// gengc-round2. `release_unused_tail` skips the reclaim-epoch stamp; the
    /// two paths must stay pinned as DIFFERENT on purpose, which is the other
    /// half of `releasing_an_unused_tail_keeps_the_reclaim_epoch`.
    #[test]
    fn freeing_the_same_span_does_bump_the_reclaim_epoch() {
        let mut og = OldGen::new(64 * 1024);
        let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
        let epoch = og.reclaim_epoch();
        // SAFETY: `p` covers 4096 bytes carved from this generation.
        unsafe { og.free(p, 4096) };
        assert_ne!(
            og.reclaim_epoch(),
            epoch,
            "a `free` releases storage that HELD an object, so every \
             address-keyed snapshot taken before it is ambiguous — that is the \
             difference `release_unused_tail` is claiming not to have",
        );
    }

    /// gengc-round2: the carve registry must actually fire.
    ///
    /// An assertion that has never been seen to fail is not evidence. This is
    /// failure mode 2 from the gap page: a block that was handed out whole —
    /// `promote_alloc`'s "object on its own block" fallback carves exactly
    /// this way — with part of it later routed through `release_unused_tail`,
    /// which would silently skip the epoch stamp for a span that DID hold an
    /// object.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
    fn releasing_a_tail_of_a_block_that_was_not_a_buffer_is_caught() {
        let mut og = OldGen::new(64 * 1024);
        // Plain `alloc_unzeroed` — an object on its own block, NOT a buffer.
        let p = og.alloc_unzeroed(4096, 8).unwrap();
        // SAFETY (deliberately violated): this is the contract breach the
        // registry exists to catch, so the call is the test.
        unsafe { og.release_unused_tail(p.add(1024), 3072) };
    }

    /// And the other shape: a span that is inside a registered buffer but is
    /// not its tail. `retire_old_plab` only ever releases `[cursor, end)`, so
    /// a middle chunk means something changed that the epoch-skip argument was
    /// never made for.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
    fn releasing_a_middle_chunk_of_a_buffer_is_caught() {
        let mut og = OldGen::new(64 * 1024);
        let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
        // SAFETY (deliberately violated): see above.
        unsafe { og.release_unused_tail(p.add(1024), 1024) };
    }

    /// A buffer's tail can only be handed back once: the registry retires the
    /// entry, so a repeat release — which would double-free the span onto the
    /// free list — is caught too.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "outstanding alloc_unzeroed_buffer carve")]
    fn releasing_the_same_buffer_tail_twice_is_caught() {
        let mut og = OldGen::new(64 * 1024);
        let p = og.alloc_unzeroed_buffer(4096, 8).unwrap();
        // SAFETY: the first release is legitimate.
        unsafe { og.release_unused_tail(p.add(1024), 3072) };
        // SAFETY (deliberately violated): the second is not.
        unsafe { og.release_unused_tail(p.add(1024), 3072) };
    }

    /// The merge itself: adjacent blocks collapse, and a second call reports
    /// `0` so `alloc`'s coalesce-and-retry cannot loop.
    #[test]
    fn coalesce_free_blocks_merges_adjacent_and_is_idempotent() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 128;
        let mut ptrs = Vec::new();
        for _ in 0..8 {
            ptrs.push(og.alloc(S, 8).unwrap());
        }
        for p in ptrs.drain(..) {
            // SAFETY: see the test above.
            unsafe { og.free(p, S) };
        }
        let before = og.free_block_count();
        let merged = og.coalesce_free_blocks();
        assert!(merged > 0, "8 adjacent frees must merge (before={before})");
        assert!(
            og.free_block_count() < before,
            "the free-block count must drop ({before} -> {})",
            og.free_block_count()
        );
        assert_eq!(
            og.coalesce_free_blocks(),
            0,
            "a second merge must report no progress, so alloc's retry is one-shot"
        );
    }

    /// Two holes separated by a LIVE object must stay separate — merging them
    /// would hand out a span covering the live object.
    #[test]
    fn coalesce_free_blocks_never_merges_across_a_live_object() {
        let mut og = OldGen::new(64 * 1024);
        const S: usize = 128;
        let a = og.alloc(S, 8).unwrap();
        let live = og.alloc(S, 8).unwrap();
        let c = og.alloc(S, 8).unwrap();
        let tail = og.capacity() - og.used();
        og.alloc(tail, 8)
            .expect("the trailing free block must be allocatable");

        // SAFETY: `a` and `c` each came from `alloc(S, 8)` and are freed once;
        // `live` is deliberately left allocated between them.
        unsafe {
            og.free(a, S);
            og.free(c, S);
        }
        assert_eq!(og.free_block_count(), 2);

        og.coalesce_free_blocks();
        assert!(
            og.alloc(S * 2, 8).is_none(),
            "two {S}-byte holes separated by a LIVE object must not satisfy a \
             {}-byte request",
            S * 2
        );
        // The live object is untouched and its own hole is still usable.
        let _ = live;
        assert!(
            og.alloc(S, 8).is_some(),
            "each individual hole still serves S"
        );
    }

    #[test]
    fn free_coalesce_forward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();
        let _p3 = og.alloc(64, 8).unwrap();

        // Free p2, then p1.
        unsafe { og.free(p2, 64) };
        unsafe { og.free(p1, 64) };

        // Round-5 #14: free() defers coalescing to the next compact().
        // The big trailing free block (everything after p3) still
        // satisfies a 128-byte allocation directly.
        assert_eq!(og.used(), 64); // only p3 remains
        let big = og.alloc(128, 8).unwrap();
        assert!(!big.is_null());
    }

    #[test]
    fn free_coalesce_backward() {
        let mut og = OldGen::new(4096);
        let p1 = og.alloc(64, 8).unwrap();
        let p2 = og.alloc(64, 8).unwrap();

        // Free p1, then p2.
        unsafe { og.free(p1, 64) };
        unsafe { og.free(p2, 64) };
        assert_eq!(og.used(), 0);

        // Round-5 #14: free() no longer coalesces immediately — coalescing
        // is deferred to the next mark-compact. After freeing both blocks
        // the total free capacity is preserved and the heap can serve a
        // fresh allocation that exactly matches one of the freed blocks
        // (proving the bucketed freelist still hands back contiguous
        // memory).
        let p3 = og.alloc(64, 8).unwrap();
        assert!(!p3.is_null());
    }

    #[test]
    fn contains() {
        let og = OldGen::new(256);
        let base = og.base_ptr();
        assert!(og.contains(base));
        assert!(og.contains(unsafe { base.add(128) }));
        assert!(!og.contains(std::ptr::null()));
        assert!(!og.contains(unsafe { base.add(256) })); // exclusive end
    }

    #[test]
    fn walk_objects_empty() {
        let og = OldGen::new(4096);
        let objects = og.walk_objects();
        assert!(objects.is_empty());
    }

    #[test]
    fn walk_objects_with_allocations() {
        let mut og = OldGen::new(4096);

        // Allocate two objects manually with proper headers
        let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE; // 2 fields
        let p1 = og.alloc(obj_size1, 8).unwrap();
        unsafe {
            let header = &mut *(p1 as *mut ObjectHeader);
            header.set_num_slots(2);
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE; // 1 field
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.set_num_slots(1);
        }

        let objects = og.walk_objects();
        assert_eq!(objects.len(), 2);
        assert_eq!(objects[0].0, p1);
        assert_eq!(objects[0].1, obj_size1);
        assert_eq!(objects[1].0, p2);
        assert_eq!(objects[1].1, obj_size2);
    }

    /// `HIB-DCAST-LATEPHASE.1`: a walk that desyncs from real object
    /// boundaries lands on arbitrary bytes. Before this fix `scan_region`
    /// read those bytes as a typed `ObjectKind` unconditionally — instant UB
    /// for a tag outside `{0, 1, 2}`, which optimized code lowered to a
    /// `SIGILL` inside `OldGen::compact` (deterministic faulting RVA across
    /// repeated crashes against the real `DefaultCatalogAndSchemaTest`
    /// workload). This corrupts only the raw tag byte of an otherwise
    /// legitimately-allocated object, mirroring the regression style already
    /// used for the conservative-root validators (see
    /// `source-debug-jit-conservative-root-invalid-header-tag-sigill.md`),
    /// and asserts the walk stops at the corrupted header instead of
    /// trusting it.
    #[test]
    fn walk_objects_stops_at_a_desynced_invalid_kind_tag_instead_of_trapping() {
        let mut og = OldGen::new(4096);

        let obj_size1 = HEADER_SIZE + 2 * SLOT_SIZE;
        let p1 = og.alloc(obj_size1, 8).unwrap();
        unsafe {
            let header = &mut *(p1 as *mut ObjectHeader);
            header.set_num_slots(2);
        }

        let obj_size2 = HEADER_SIZE + SLOT_SIZE;
        let p2 = og.alloc(obj_size2, 8).unwrap();
        unsafe {
            let header = &mut *(p2 as *mut ObjectHeader);
            header.set_num_slots(1);
        }

        // 0xFF is not a declared ObjectKind discriminant (0=Object, 1=Array,
        // 2=HumongousFiller).
        // SAFETY: `p2 + cratonvm_types::KIND_TAGS_BYTE_OFFSET` is the `kind` byte of a live
        // allocation from this OldGen; writing a raw `u8` there does not
        // require the resulting value to be a valid `ObjectKind`.
        unsafe {
            std::ptr::write(p2.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
        }

        let before = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);
        let objects = og.walk_objects();
        let after = WALK_DESYNC_HITS.load(std::sync::atomic::Ordering::Relaxed);

        assert_eq!(
            objects,
            vec![(p1, obj_size1)],
            "the walk must stop at the corrupted header, not trust it"
        );
        assert!(
            after > before,
            "expected the desync guard to fire and bump WALK_DESYNC_HITS"
        );
    }

    /// Dangling-ref guard (Phase 0): a MARKED object that references an
    /// UNMARKED old-gen object must not be left pointing at floating garbage
    /// after compaction. The closure promotes the target to live and relocates
    /// it; the referrer's field must end up pointing at the target's *new*,
    /// still-valid location — never at zeroed/overwritten bytes.
    #[test]
    fn compact_promotes_unmarked_target_of_live_ref() {
        let mut og = OldGen::new(4096);

        // Object A: live, one reference field.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();

        // Dead filler between A and B so that, once it is reclaimed, B must
        // actually slide to a lower address during compaction (giving it a
        // pointer-map entry and exercising the relocation path).
        let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
        let filler = og.alloc(filler_size, 8).unwrap();
        unsafe {
            (*(filler as *mut ObjectHeader)).set_num_slots(4);
            // left UNMARKED -> dead -> reclaimed.
        }

        // Object B: the (initially unmarked) target. Tag it with a distinctive
        // identity hash so we can prove we land on real B data, not zeroes.
        const B_TAG: i32 = 0x5EED_BEEF_u32 as i32;
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();

        unsafe {
            // A is live, references B in field 0.
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            let a_field0 = a.add(HEADER_SIZE) as *mut Value;
            std::ptr::write(a_field0, Value::Object(Some(ObjectRef::from_raw(b))));

            // B is left UNMARKED (would be floating garbage without the guard).
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.set_num_slots(1);
            // A long (legacy) header keeps its identity hash in the aux word.
            b_hdr
                .mark_word
                .store(0, std::sync::atomic::Ordering::Relaxed);
            b_hdr.identity_hash(|| B_TAG).unwrap();
        }

        let map = og.compact();

        // A is the lowest-address live object, so it stays put (no map entry);
        // B must have been promoted and relocated, hence present in the map.
        let b_new = *map
            .get(&(b as usize))
            .expect("unmarked target reachable from a live ref must be promoted + relocated");

        unsafe {
            // A's field must now point at B's NEW location, in-bounds.
            let a_field0 = a.add(HEADER_SIZE) as *const Value;
            let field_val = std::ptr::read(a_field0);
            let Value::Object(Some(target)) = field_val else {
                panic!("A's reference field was clobbered: {field_val:?}");
            };
            assert_eq!(
                target.as_ptr() as usize,
                b_new,
                "A's field must be forwarded to B's new location, not left dangling",
            );

            // The forwarded B must still hold real B data (tag preserved),
            // proving we did not leave the slot pointing at zeroed memory.
            let b_hdr = &*(b_new as *const ObjectHeader);
            assert_eq!(
                b_hdr.installed_identity_hash(),
                B_TAG,
                "B data lost across compaction"
            );
            // GC metadata cleared on the survivor.
            assert!(!b_hdr.is_forwarded());
            assert_eq!(b_hdr.gc_flags() & GC_FLAG_MARKED, 0);
        }
    }

    /// GCAUD-1 — `free` must return the SAME extent `alloc` reserved.
    ///
    /// `alloc_from_buckets` rounds the reservation up to a multiple of `align`
    /// and charges `used_bytes` that rounded amount; `free` mirrored only the
    /// `max(HEADER_SIZE, 8)` half of that expression. The bytes between the
    /// unrounded and rounded ends were therefore never returned to the free
    /// list: `used_bytes` drifted up, and — the part that matters —
    /// `walk_objects` derives allocated extents as the GAPS between free
    /// blocks, so the sliver read as an allocated non-object-start. The walk
    /// then mis-parses it as a header and `break`s out of the region, dropping
    /// every later object in it from the walk that drives compaction.
    #[test]
    fn free_returns_the_whole_extent_alloc_reserved() {
        let mut og = OldGen::new(4096);
        // 44 is not a multiple of 8: `alloc` reserves 48 (see
        // `alloc_alignment_reserves_compact_object_padding`).
        let a = og.alloc(44, 8).expect("fresh old gen serves 44 bytes");
        let b = og.alloc(64, 8).expect("fresh old gen serves 64 bytes");
        assert_eq!(
            b as usize - a as usize,
            48,
            "alloc must reserve the align-rounded extent",
        );
        assert_eq!(og.used(), 48 + 64);

        // SAFETY: `a` is a live block of this OldGen and 44 is the size it was
        // requested with — exactly the call shape the accounting must survive.
        unsafe { og.free(a, 44) };
        assert_eq!(
            og.used(),
            64,
            "freeing with the requested size must decrement the RESERVED size",
        );

        // The recovered hole must be the full 48 bytes, not 44: a 48-byte
        // request has to be servable from it, at the same address.
        let c = og
            .alloc(48, 8)
            .expect("the freed 48-byte hole must be reusable");
        assert_eq!(
            c, a,
            "a 48-byte request must land back in the freed block, not past the \
             live neighbour — a short free leaves an unusable 4-byte sliver",
        );
    }

    /// GCAUD-2 — the compactor must refuse to slide a heap it cannot fully
    /// forward.
    ///
    /// Phase 0 closes the live set over old gen so Phase 1 can stamp a
    /// forwarding address on every object a live referrer can reach. It
    /// assumed `walk_objects` yields EVERY old-gen object. It does not: a
    /// block that is on the free list is invisible to the walk (allocated
    /// extents are the gaps between free blocks), which is exactly the state
    /// an under-marking in-place sweep leaves behind. Phase 0 used to `|=`
    /// `GC_FLAG_MARKED` into that freed block anyway, Phase 1 gave it no
    /// forwarding address, and Phase 3 slid a different object onto it —
    /// the compactor manufacturing the dangling pointer it exists to prevent.
    ///
    /// The fail-safe answer is to reclaim nothing this cycle.
    #[test]
    fn compact_is_abandoned_when_a_live_ref_escapes_the_object_walk() {
        let mut og = OldGen::new(4096);
        const C_TAG: i32 = 0x0BAD_F00D_u32 as i32;

        // A: the live referrer.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();
        // B: the target, freed below so the object walk stops yielding it.
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();
        // Dead filler, so a compaction that DID run would definitely move C.
        let filler_size = HEADER_SIZE + 4 * SLOT_SIZE;
        let filler = og.alloc(filler_size, 8).unwrap();
        // C: a second live object, the one whose non-movement proves the
        // compaction was abandoned rather than merely uneventful.
        let c_size = HEADER_SIZE + SLOT_SIZE;
        let c = og.alloc(c_size, 8).unwrap();

        // SAFETY: every pointer above is a live block of this OldGen, sized to
        // hold a header plus the slots written here.
        unsafe {
            (*(filler as *mut ObjectHeader)).set_num_slots(4);

            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );

            (*(b as *mut ObjectHeader)).set_num_slots(1);

            let c_hdr = &mut *(c as *mut ObjectHeader);
            c_hdr.set_num_slots(1);
            c_hdr.add_gc_flags(GC_FLAG_MARKED);
            // A long (legacy) header keeps its identity hash in the aux word.
            c_hdr
                .mark_word
                .store(0, std::sync::atomic::Ordering::Relaxed);
            c_hdr.identity_hash(|| C_TAG).unwrap();
        }

        // Return B's block to the free list WITHOUT zeroing it — the exact
        // shape the in-place old-gen sweep produces when the mark under-marks.
        // SAFETY: `(b, b_size)` is the pair `alloc` handed out.
        unsafe { og.free(b, b_size) };
        assert!(
            og.walk_objects().iter().all(|&(p, _)| p != b),
            "precondition: the freed block must be invisible to the object walk",
        );

        let used_before = og.used();
        let escapes_before = COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed);
        let map = og.compact();

        assert!(
            map.is_empty(),
            "a compaction that cannot forward every live reference must relocate \
             nothing, so it can report no relocations",
        );
        assert!(
            COMPACT_ESCAPE_HITS.load(std::sync::atomic::Ordering::Relaxed) > escapes_before,
            "the abandoned compaction must be counted, not silent",
        );
        assert_eq!(
            og.used(),
            used_before,
            "an abandoned compaction must not rewrite the occupancy",
        );

        // SAFETY: nothing moved, so every pointer above still names its object.
        unsafe {
            // A's slot is untouched — still naming B, which is still where it
            // was. A rewrite here (or a slide over B) is the use-after-free.
            let Value::Object(Some(target)) = std::ptr::read(a.add(HEADER_SIZE) as *const Value)
            else {
                panic!("A's reference field was clobbered by an abandoned compaction");
            };
            assert_eq!(target.as_ptr(), b);

            // C did not slide over the filler, and its mark bit was cleared so
            // the next cycle starts from a clean slate.
            let c_hdr = &*(c as *const ObjectHeader);
            assert_eq!(
                c_hdr.installed_identity_hash(),
                C_TAG,
                "C must not have moved"
            );
            assert_eq!(
                c_hdr.gc_flags() & GC_FLAG_MARKED,
                0,
                "marks must be cleared"
            );
            assert!(!c_hdr.is_forwarded());
        }
    }

    /// RandomizedContext WeakHashMap<Thread,...> fix regression: a live
    /// old-gen object that stays in place during sliding compaction (the
    /// lowest-address survivor, exactly like object A in
    /// `compact_promotes_unmarked_target_of_live_ref` above) gets NO
    /// `pointer_map` entry by design — but if it is a WATCHED referent
    /// (`gc_quiescence::set_watched_referents`), it must get an IDENTITY
    /// entry so post-GC reference processing's `is_marked` check can
    /// recognize it as alive. An unwatched object that also stays in place
    /// must still get no entry at all (bounded cost — this must not start
    /// recording every stationary survivor).
    #[test]
    fn compact_records_identity_map_for_watched_stationary_survivor() {
        let mut og = OldGen::new(4096);

        // Object A: lowest address, stays in place after compaction.
        let a_size = HEADER_SIZE + SLOT_SIZE;
        let a = og.alloc(a_size, 8).unwrap();
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
        }

        // Object B: also stays in place (contiguous with A, nothing to
        // reclaim between them) — left UNWATCHED as a control.
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();
        unsafe {
            let b_hdr = &mut *(b as *mut ObjectHeader);
            b_hdr.set_num_slots(1);
            b_hdr.add_gc_flags(GC_FLAG_MARKED);
        }

        crate::gc_quiescence::set_watched_referents(&[a as usize]);
        let map = og.compact();
        crate::gc_quiescence::set_watched_referents(&[]);

        assert_eq!(
            map.get(&(a as usize)),
            Some(&(a as usize)),
            "watched stationary survivor must get an identity pointer_map entry"
        );
        assert!(
            !map.contains_key(&(b as usize)),
            "unwatched stationary survivor must NOT get a pointer_map entry (bounded cost)"
        );
    }

    /// GCAUD-8 — [`OldGen::close_live_set`] is the in-place sweep's half of the
    /// guard the compactor has had since Phase 0 was written.
    ///
    /// Both halves must hold at once, so both are asserted here:
    ///
    /// * **the fix** — an UNMARKED object that a MARKED object still references
    ///   is promoted to live, so the sweep that runs next cannot free it under
    ///   a live pointer;
    /// * **the positive control** — an object that is unmarked *and*
    ///   unreferenced is left exactly as it was. A closure that promoted
    ///   everything would satisfy the first assertion and reclaim nothing ever
    ///   again, which is the failure mode this pairing exists to catch.
    #[test]
    fn close_live_set_promotes_a_referenced_target_and_leaves_real_garbage_dead() {
        let mut og = OldGen::new(4096);

        // A: live referrer.
        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // B: A's target, deliberately left unmarked — the object a mark-phase
        // gap loses and the sweep would otherwise free.
        let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // C: unmarked AND unreferenced — genuine garbage, the control.
        let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();

        // SAFETY: all three are live blocks of this OldGen, each sized for a
        // header plus the single slot written below.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );
            (*(b as *mut ObjectHeader)).set_num_slots(1);
            (*(c as *mut ObjectHeader)).set_num_slots(1);
        }

        let objects = og.walk_objects();
        let (rescued, escaped) = og.close_live_set(&objects);

        assert_eq!(
            rescued, 1,
            "exactly the one referenced-but-unmarked object must be promoted",
        );
        assert!(
            !escaped,
            "every referent here is a walked base — nothing escaped the grid",
        );

        // SAFETY: nothing moved; both pointers still name their object.
        unsafe {
            assert_ne!(
                (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "an unmarked object a LIVE object references must be retained",
            );
            assert_eq!(
                (*(c as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "POSITIVE CONTROL: unreferenced garbage must stay dead, or the \
                 sweep this feeds would stop reclaiming anything at all",
            );
        }

        // Idempotent: a second run has nothing left to do.
        let (rescued_again, _) = og.close_live_set(&objects);
        assert_eq!(rescued_again, 0, "the closure must reach a fixpoint");
    }

    /// gce e1/c — `close_live_set_collecting` names every object it rescued,
    /// both the ones the linear pass reaches ahead of its cursor and the ones
    /// its drain picks up behind it, and nothing else: the true-root major
    /// sends exactly these back through its mark body.
    #[test]
    fn gce_e1c_close_live_set_collecting_names_each_rescued_base() {
        let mut og = OldGen::new(4096);
        // Allocation order a, b, c, d: a (marked) -> c (ahead of the cursor),
        // c -> b (behind it, so the drain rescues it), d unreferenced garbage.
        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let d = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // SAFETY: four live blocks of this OldGen, each sized for a header
        // plus the one slot written below.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(c))),
            );
            (*(b as *mut ObjectHeader)).set_num_slots(1);
            (*(c as *mut ObjectHeader)).set_num_slots(1);
            std::ptr::write(
                c.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );
            (*(d as *mut ObjectHeader)).set_num_slots(1);
        }
        let objects = og.walk_objects();
        let mut out = Vec::new();
        let (rescued, escaped) = og.close_live_set_collecting(&objects, &[], &mut out);
        assert_eq!(rescued, 2);
        assert!(!escaped);
        let mut got = out.clone();
        got.sort_unstable();
        let mut want = vec![b as usize, c as usize];
        want.sort_unstable();
        assert_eq!(got, want, "exactly the rescued bases, each once");
        // SAFETY: nothing moved.
        unsafe {
            assert_eq!(
                (*(d as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "unreferenced garbage stays dead",
            );
        }
        out.clear();
        assert_eq!(og.close_live_set_collecting(&objects, &[], &mut out).0, 0);
        assert!(out.is_empty(), "a fixpoint rescues, and names, nothing");
    }

    /// GCAUD-8 — the closure must also report the escape the in-place sweep
    /// cannot repair: a live object pointing at a block that is ALREADY on the
    /// free list. It must not write a mark bit through that address (it is
    /// unallocated memory the allocator may reissue at any moment) and it must
    /// not promote it into the walked set.
    #[test]
    fn close_live_set_reports_a_referent_that_is_already_freed() {
        let mut og = OldGen::new(4096);

        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let b_size = HEADER_SIZE + SLOT_SIZE;
        let b = og.alloc(b_size, 8).unwrap();

        // SAFETY: both are live blocks of this OldGen.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(b))),
            );
            (*(b as *mut ObjectHeader)).set_num_slots(1);
        }

        // The state an under-marking sweep leaves behind: B freed, A still
        // naming it, B's bytes not zeroed.
        // SAFETY: `(b, b_size)` is exactly the pair `alloc` handed out.
        unsafe { og.free(b, b_size) };

        let objects = og.walk_objects();
        let (rescued, escaped) = og.close_live_set(&objects);

        assert!(
            escaped,
            "a referent outside the object walk must be reported"
        );
        assert_eq!(rescued, 0, "nothing in the walked set needed promoting");
        // SAFETY: `b`'s block is unallocated but still mapped inside `og`.
        unsafe {
            assert_eq!(
                (*(b as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED,
                0,
                "the closure must NOT write a mark bit into a free block",
            );
        }
    }

    // -----------------------------------------------------------------------
    // gen r4/oldgen (2026-09-23)
    // -----------------------------------------------------------------------

    /// The live-set closure as it was before round 4 — a re-scan-everything
    /// fixpoint — kept as the ORACLE the worklist form is held to. Same
    /// admission proof, same slot walk, same mark writes; only the iteration
    /// strategy differs.
    fn reference_fixpoint_closure(
        objects: &[(*mut u8, usize)],
        data: (usize, usize),
    ) -> (usize, bool) {
        let is_walked_base = |addr: usize| {
            objects
                .binary_search_by_key(&addr, |&(p, _)| p as usize)
                .is_ok()
        };
        let mut escaped = false;
        let mut promoted_total = 0usize;
        loop {
            let mut promoted_any = false;
            for &(obj_ptr, size) in objects {
                // SAFETY: `obj_ptr` is a walked object base.
                let is_marked =
                    unsafe { (*(obj_ptr as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 };
                if !is_marked {
                    continue;
                }
                OldGen::for_each_old_gen_ref(obj_ptr, size, data, |ref_ptr| {
                    if !is_walked_base(ref_ptr) {
                        escaped = true;
                        return;
                    }
                    // SAFETY: `ref_ptr` is a walked object base.
                    let ref_header = unsafe { &mut *(ref_ptr as *mut ObjectHeader) };
                    if ref_header.gc_flags() & GC_FLAG_MARKED == 0 {
                        ref_header.add_gc_flags(GC_FLAG_MARKED);
                        promoted_any = true;
                        promoted_total += 1;
                    }
                });
            }
            if !promoted_any {
                break;
            }
        }
        (promoted_total, escaped)
    }

    /// Slots per object in [`graph_old_gen`].
    const GRAPH_SLOTS: usize = 3;
    /// Footprint of one [`graph_old_gen`] object.
    const GRAPH_OBJ: usize = HEADER_SIZE + GRAPH_SLOTS * SLOT_SIZE;

    /// `n` legacy objects laid out back to back, slot `s` of object `i`
    /// holding a reference to object `edges[i][s]` (or null), and exactly the
    /// objects with `marked[i]` carrying `GC_FLAG_MARKED`.
    fn graph_old_gen(
        edges: &[[Option<usize>; GRAPH_SLOTS]],
        marked: &[bool],
    ) -> (OldGen, Vec<*mut u8>) {
        let n = edges.len();
        let mut og = OldGen::new(64 * 1024);
        let mut ptrs = Vec::with_capacity(n);
        for _ in 0..n {
            let p = og.alloc(GRAPH_OBJ, 8).expect("64 KiB holds the whole graph");
            // SAFETY: `p` is a fresh, zeroed allocation of `GRAPH_OBJ` bytes.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        GRAPH_SLOTS as u32,
                    ),
                );
            }
            ptrs.push(p);
        }
        for (i, (row, &live)) in edges.iter().zip(marked).enumerate() {
            for (s, target) in row.iter().enumerate() {
                // SAFETY: every `ptrs[t]` is a live, 8-aligned object base of
                // this generation, and slot `s < GRAPH_SLOTS` lies inside
                // object `i`'s body.
                unsafe {
                    let v = match target {
                        Some(t) => Value::Object(Some(ObjectRef::from_raw(ptrs[*t]))),
                        None => Value::Object(None),
                    };
                    std::ptr::write(ptrs[i].add(HEADER_SIZE + s * SLOT_SIZE) as *mut Value, v);
                }
            }
            if live {
                // SAFETY: `ptrs[i]` holds the header written above.
                unsafe { (*(ptrs[i] as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
            }
        }
        (og, ptrs)
    }

    /// xorshift64 — deterministic, dependency-free.
    struct Xorshift(u64);
    impl Xorshift {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    fn is_marked(p: *mut u8) -> bool {
        // SAFETY: callers pass object bases of a live test generation.
        unsafe { (*(p as *const ObjectHeader)).gc_flags() & GC_FLAG_MARKED != 0 }
    }

    /// gen r4/oldgen — design item 5: the worklist closure must produce the
    /// SAME marked set, the same rescue count and the same escape verdict as
    /// the fixpoint it replaced, on graphs chosen to hit every shape the
    /// fixpoint handled differently: sparse and dense random graphs with
    /// self-loops, a chain whose every link points BACKWARDS in address order
    /// (the fixpoint's worst case — one full pass per link), a backward chain
    /// with random cross edges, and all of those with a referenced block
    /// already on the free list so the escape arm is compared too.
    #[test]
    fn the_worklist_closure_matches_the_old_fixpoint_exactly() {
        const N: usize = 120;
        for seed in 1..=64u64 {
            let mut rng = Xorshift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut edges = vec![[None; GRAPH_SLOTS]; N];
            let mut marked = vec![false; N];
            let shape = seed % 4;
            for (i, (row, live)) in edges.iter_mut().zip(marked.iter_mut()).enumerate() {
                for (s, slot) in row.iter_mut().enumerate() {
                    *slot = match shape {
                        0 => {
                            if rng.below(3) == 0 {
                                Some(rng.below(N))
                            } else {
                                None
                            }
                        }
                        1 => Some(rng.below(N)),
                        2 => {
                            if s == 0 && i > 0 {
                                Some(i - 1)
                            } else {
                                None
                            }
                        }
                        _ => {
                            if s == 0 && i > 0 {
                                Some(i - 1)
                            } else if rng.below(4) == 0 {
                                Some(rng.below(N))
                            } else {
                                None
                            }
                        }
                    };
                }
                *live = if shape == 2 {
                    i == N - 1
                } else {
                    rng.below(8) == 0
                };
            }
            // Half the cases: one unmarked object is already free, so a
            // referrer that reaches it is an ESCAPE in both algorithms.
            let freed = if seed % 2 == 0 {
                (0..N).rev().find(|&i| !marked[i])
            } else {
                None
            };
            let build = || {
                let (mut og, ptrs) = graph_old_gen(&edges, &marked);
                if let Some(f) = freed {
                    // SAFETY: `(ptrs[f], GRAPH_OBJ)` is exactly what `alloc`
                    // handed out.
                    unsafe { og.free(ptrs[f], GRAPH_OBJ) };
                }
                (og, ptrs)
            };
            let (oracle_og, oracle_ptrs) = build();
            let (og, ptrs) = build();
            let oracle_walk = oracle_og.walk_objects();
            let walk = og.walk_objects();
            assert_eq!(oracle_walk.len(), walk.len());

            let expected = reference_fixpoint_closure(&oracle_walk, oracle_og.extent());
            let got = og.close_live_set(&walk);
            assert_eq!(
                got, expected,
                "seed {seed} (shape {shape}): (rescued, escaped) differs from the fixpoint",
            );
            for (i, (&p, &q)) in ptrs.iter().zip(&oracle_ptrs).enumerate() {
                if Some(i) == freed {
                    continue;
                }
                assert_eq!(
                    is_marked(p),
                    is_marked(q),
                    "seed {seed} (shape {shape}): object {i}'s mark differs from the fixpoint",
                );
            }
        }
    }

    /// The fixpoint's worst case, directly: a chain whose every link points to
    /// the object BELOW it, with only the top marked. The fixpoint needed one
    /// full pass over the generation per link; the worklist rescues the whole
    /// chain in its single linear pass plus one drain.
    #[test]
    fn a_backward_chain_is_closed_by_one_pass_and_a_drain() {
        const N: usize = 200;
        let mut edges = vec![[None; GRAPH_SLOTS]; N];
        for (i, e) in edges.iter_mut().enumerate().skip(1) {
            e[0] = Some(i - 1);
        }
        let mut marked = vec![false; N];
        marked[N - 1] = true;
        let (og, ptrs) = graph_old_gen(&edges, &marked);
        let walk = og.walk_objects();
        let deep_before = CLOSE_LIVE_SET_DEEP_CALLS.load(std::sync::atomic::Ordering::Relaxed);
        let scans_before = CLOSE_LIVE_SET_WORKLIST_SCANS.load(std::sync::atomic::Ordering::Relaxed);

        assert_eq!(og.close_live_set(&walk), (N - 1, false));
        assert!(ptrs.iter().all(|&p| is_marked(p)), "the whole chain is live");
        // Process-global and monotone, so `>=` is exact enough under a
        // parallel test harness: other tests can only ADD.
        assert!(
            CLOSE_LIVE_SET_DEEP_CALLS.load(std::sync::atomic::Ordering::Relaxed) > deep_before,
            "a backward chain is precisely what the old fixpoint needed 3+ passes for",
        );
        assert!(
            CLOSE_LIVE_SET_WORKLIST_SCANS.load(std::sync::atomic::Ordering::Relaxed)
                >= scans_before + (N - 1) as u64,
            "every rescued link behind the cursor is scanned by the drain",
        );
        // Idempotent.
        assert_eq!(og.close_live_set(&walk), (0, false));
    }

    /// gen r4/oldgen — a merge that has already run is not re-run until
    /// something is freed, and skipping it never costs an allocation: a free
    /// re-arms it, and the allocation-failure retry then merges as before.
    #[test]
    fn coalesce_is_skipped_until_something_is_freed_and_still_merges_after() {
        let mut og = OldGen::new(1024);
        let a = og.alloc(256, 8).unwrap();
        let b = og.alloc(256, 8).unwrap();
        let c = og.alloc(256, 8).unwrap();
        let _d = og.alloc(256, 8).unwrap();
        assert_eq!(og.free_block_count(), 0, "four 256-byte blocks fill 1 KiB");
        // SAFETY: `a` and `c` are blocks `alloc` handed out, freed whole.
        unsafe {
            og.free(a, 256);
            og.free(c, 256);
        }
        assert!(!og.free_list_maximal, "a free must re-arm the merge");
        assert_eq!(og.coalesce_free_blocks(), 0, "a and c are separated by live b");
        assert!(og.free_list_maximal);

        // Nothing can serve 512 bytes. The failure's coalesce retry is now
        // skipped rather than re-sorting the list to learn the same thing.
        assert!(og.alloc(512, 8).is_none());
        assert!(og.free_list_maximal, "an allocation cannot create adjacency");
        assert_eq!(og.free_block_count(), 2);

        // SAFETY: as above.
        unsafe { og.free(b, 256) };
        assert!(!og.free_list_maximal);
        let big = og
            .alloc(700, 8)
            .expect("a, b and c are contiguous once b is free: the retry must merge them");
        assert_eq!(big, a);
    }

    /// gen r4/oldgen — a promotion BUFFER must not become the scale the
    /// fragmentation predicate judges holes by; a real request still does.
    #[test]
    fn a_promotion_buffer_carve_does_not_set_the_fragmentation_scale() {
        let mut og = OldGen::new(1 << 20);
        assert!(og.alloc_unzeroed_buffer(64 * 1024, 8).is_some());
        assert_eq!(og.recent_max_request(), 0, "a buffer is not a request");
        assert!(og.alloc_unzeroed(4096, 8).is_some());
        assert_eq!(og.recent_max_request(), 4096, "an object-sized block is");
        assert!(og.alloc_unzeroed_buffer(128 * 1024, 8).is_some());
        assert_eq!(og.recent_max_request(), 4096);
    }

    /// Three one-slot objects `a`, `b`, `c`, with `a` and `c` marked and `b`
    /// dead between them.
    fn marked_dead_marked() -> (OldGen, *mut u8, *mut u8, *mut u8) {
        let mut og = OldGen::new(4096);
        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let b = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let c = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        // SAFETY: all three are zeroed live blocks sized for one slot.
        unsafe {
            for p in [a, b, c] {
                (*(p as *mut ObjectHeader)).set_num_slots(1);
            }
            (*(a as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
            (*(c as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
        }
        (og, a, b, c)
    }

    /// gen r4/oldgen — pause-path redundant walks item 3: compacting over the
    /// grid the caller already derived is the same compaction as walking again.
    #[test]
    fn compacting_over_a_fresh_grid_matches_walking_again() {
        let (mut og1, _a1, _b1, c1) = marked_dead_marked();
        let map1 = og1.compact();
        let (mut og2, _a2, _b2, c2) = marked_dead_marked();
        let walk = og2.walk_objects();
        let seq = og2.free_list_seq();
        let map2 = og2.compact_with_walked_objects(walk, seq, &HashMap::new());

        let rel = |base: *const u8, p: usize| p - base as usize;
        let c1_to = *map1.get(&(c1 as usize)).expect("c slides down over b");
        let c2_to = *map2.get(&(c2 as usize)).expect("c slides down over b");
        assert_eq!(rel(og1.base_ptr(), c1_to), rel(og2.base_ptr(), c2_to));
        assert_eq!(og1.used(), og2.used());
        assert_eq!(og1.used(), 2 * (HEADER_SIZE + SLOT_SIZE));
    }

    /// gen r4/oldgen — a grid taken before the free list changed is not
    /// trusted. Compacting over this one would still list `b`, whose block is
    /// back on the free list, so `walked_bytes != used_bytes` would abandon
    /// the compaction and relocate nothing; the re-walk sees the real layout
    /// and slides `c` into `b`'s hole.
    #[test]
    fn compacting_over_a_stale_grid_rewalks_instead() {
        let (mut og, _a, b, c) = marked_dead_marked();
        let stale = og.walk_objects();
        let seq = og.free_list_seq();
        // SAFETY: `(b, HEADER_SIZE + SLOT_SIZE)` is exactly what `alloc`
        // handed out.
        unsafe { og.free(b, HEADER_SIZE + SLOT_SIZE) };
        assert_ne!(og.free_list_seq(), seq, "a free must move the stamp");

        let map = og.compact_with_walked_objects(stale, seq, &HashMap::new());
        assert_eq!(
            map.get(&(c as usize)),
            Some(&(b as usize)),
            "the compaction must run against the current layout",
        );
        assert_eq!(og.used(), 2 * (HEADER_SIZE + SLOT_SIZE));
    }

    /// gen r4/oldgen — the in-place sweep now returns each run of ADJACENT
    /// dead objects as one block instead of one block per object. That is
    /// only sound if the result is indistinguishable from the old per-object
    /// frees once the sweep's own trailing `coalesce_free_blocks` has run:
    /// same occupancy, same free list, same object walk.
    #[test]
    fn freeing_a_contiguous_dead_run_as_one_block_equals_freeing_each_then_coalescing() {
        let size = HEADER_SIZE + 2 * SLOT_SIZE;
        let dead = [1usize, 2, 3, 5, 7, 8];
        let runs = [(1usize, 3usize), (5, 1), (7, 2)];

        let (mut each, offs_each) = old_gen_with_objects(10, 2);
        let base_each = each.base_ptr() as usize;
        for &i in &dead {
            // SAFETY: each is exactly one `alloc`ed block.
            unsafe { each.free((base_each + offs_each[i]) as *mut u8, size) };
        }
        each.coalesce_free_blocks();

        let (mut batched, offs_batched) = old_gen_with_objects(10, 2);
        let base_batched = batched.base_ptr() as usize;
        for &(first, len) in &runs {
            // SAFETY: objects `first..first + len` are adjacent `alloc`ed
            // blocks (`old_gen_with_objects` lays them out back to back), so
            // their union is exactly the span they were handed.
            unsafe { batched.free((base_batched + offs_batched[first]) as *mut u8, len * size) };
        }
        batched.coalesce_free_blocks();

        assert_eq!(offs_each, offs_batched, "identical layouts to start from");
        assert_eq!(each.used(), batched.used());
        let free_list = |og: &OldGen| {
            og.with_sorted_free_blocks(|s| s.iter().map(|b| (b.offset, b.size)).collect::<Vec<_>>())
        };
        assert_eq!(free_list(&each), free_list(&batched));
        let walked = |og: &OldGen| {
            let base = og.base_ptr() as usize;
            og.walk_objects()
                .into_iter()
                .map(|(p, s)| (p as usize - base, s))
                .collect::<Vec<_>>()
        };
        assert_eq!(walked(&each), walked(&batched));
    }

    // -----------------------------------------------------------------
    // gen r4w2/oldgen2 (2026-09-23): the old-gen trigger
    // -----------------------------------------------------------------

    /// With the hysteresis OFF — the default — the verdict RUNS exactly when
    /// the pre-2026-09-23 expression `(cap > 0 && used >= cap * 75 / 100) ||
    /// requested` did, over every combination of the inputs the hysteresis
    /// could have looked at. This is the "default behaviour is byte-for-byte
    /// unchanged" claim, checked rather than argued.
    #[test]
    fn the_trigger_with_hysteresis_off_is_the_bare_occupancy_test() {
        let caps = [0usize, 1, 100, 1600, 4096, 1 << 20];
        for &cap in &caps {
            let mut useds: Vec<usize> = vec![0, 1, cap / 2, cap.saturating_sub(1), cap];
            let floor = cap * 75 / 100;
            useds.extend([floor.saturating_sub(1), floor, floor + 1, floor + cap / 16]);
            for &used in &useds {
                let lasts = [None, Some(0), Some(floor), Some(used), Some(cap)];
                for &last in &lasts {
                    for failures in [0u64, 1, 7] {
                        for requested in [false, true] {
                            let old = (cap > 0 && used >= cap * 75 / 100) || requested;
                            let v = OldGen::major_trigger_verdict(
                                cap, used, last, failures, requested, false,
                            );
                            assert_eq!(
                                v.runs(),
                                old,
                                "cap={cap} used={used} last={last:?} failures={failures} \
                                 requested={requested}: {v:?}",
                            );
                            assert_ne!(v, MajorTrigger::Suppressed, "OFF never suppresses");
                        }
                    }
                }
            }
        }
    }

    /// The hysteresis, clause by clause, at a capacity where the numbers are
    /// easy to read: 1600 bytes, floor 1200, `cap / 16` = 100.
    #[test]
    fn the_hysteresis_skips_a_repeat_collection_until_growth_or_a_failure() {
        let cap = 1600;
        let v = |used, last, failures, requested| {
            OldGen::major_trigger_verdict(cap, used, last, failures, requested, true)
        };
        // Below the floor: never, whatever else is true.
        assert_eq!(v(1199, Some(1190), 3, false), MajorTrigger::NotDue);
        // A request always runs, even below the floor.
        assert_eq!(v(0, Some(1500), 0, true), MajorTrigger::Requested);
        // Never collected before: the floor alone decides.
        assert_eq!(v(1200, None, 0, false), MajorTrigger::Occupancy);
        // Last collection left 1250 used, i.e. 350 free: the next one is due
        // after max(100, 350 / 2) = 175 more bytes, at 1425.
        assert_eq!(v(1424, Some(1250), 0, false), MajorTrigger::Suppressed);
        assert_eq!(v(1425, Some(1250), 0, false), MajorTrigger::Occupancy);
        // ...unless an allocation has failed since: then at once.
        assert_eq!(v(1300, Some(1250), 1, false), MajorTrigger::AllocationFailure);
        // Last collection left the generation nearly full (1500 used, 100
        // free): `cap / 16` is the binding term (max(100, 50)), so a
        // generation that is legitimately tight is not collected on every
        // cycle either — only once it is full, or once something fails.
        assert_eq!(v(1599, Some(1500), 0, false), MajorTrigger::Suppressed);
        assert_eq!(v(1600, Some(1500), 0, false), MajorTrigger::Occupancy);
        assert_eq!(v(1510, Some(1500), 1, false), MajorTrigger::AllocationFailure);
        // Used went DOWN since (a concurrent sweep freed): no growth at all.
        assert_eq!(v(1300, Some(1400), 0, false), MajorTrigger::Suppressed);
        // And the same states with the hysteresis off report, but run.
        let off = OldGen::major_trigger_verdict(cap, 1424, Some(1250), 0, false, false);
        assert_eq!(off, MajorTrigger::WouldSuppress);
        assert!(off.runs());
    }

    /// The live form: a collection's end is what arms the hysteresis, a
    /// refused allocation is what disarms it, and a refused optional BUFFER
    /// carve is not a refused allocation.
    #[test]
    fn a_collection_arms_the_hysteresis_and_a_refused_allocation_disarms_it() {
        let (mut og, _) = old_gen_with_objects(1, 1);
        let cap = og.capacity();
        // Fill to just past the floor with plain blocks.
        while og.used() < cap * 75 / 100 {
            og.alloc(256, 8).expect("room below the floor");
        }
        assert_eq!(og.major_trigger(false, true), MajorTrigger::Occupancy);
        // A collection that reclaimed nothing.
        let used = og.used();
        og.note_collection_end(used);
        let s = og.trigger_stats();
        assert_eq!((s.collections, s.low_yield, s.occupancy), (1, 1, 1));
        assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);
        assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);

        // A promotion BUFFER that does not fit is optional: not a failure.
        assert!(og.alloc_unzeroed_buffer(cap, 8).is_none());
        assert_eq!(og.trigger_stats().alloc_failures, 0);
        assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);

        // An object that does not fit is.
        assert!(og.alloc(cap, 8).is_none());
        assert_eq!(og.trigger_stats().alloc_failures, 1);
        assert_eq!(og.major_trigger(false, true), MajorTrigger::AllocationFailure);

        // The next collection's end re-arms it.
        og.note_collection_end(og.used());
        assert_eq!(og.major_trigger(false, true), MajorTrigger::Suppressed);
        let s = og.trigger_stats();
        assert_eq!(
            (s.suppressed, s.would_suppress, s.allocation_failure, s.collections),
            (3, 1, 1, 2)
        );
    }

    // -----------------------------------------------------------------
    // gen r4w2/oldgen2 (2026-09-23): the reserve/commit backing store
    // -----------------------------------------------------------------

    /// A generation big enough to reserve (at least one granule) and a way to
    /// tell which store the process gave it: `CRATONVM_GC_RESERVE=0`, or a
    /// refused reservation, leaves it on the wholly committed arm, where every
    /// assertion below has a different (and simpler) right answer.
    fn big_old_gen(granules: usize) -> (OldGen, bool) {
        let og = OldGen::new(granules * GRANULE);
        let reserved = og.commit_bits().is_some();
        (og, reserved)
    }

    #[test]
    fn a_reserved_old_gen_commits_only_what_allocation_reaches() {
        let (mut og, reserved) = big_old_gen(8);
        let base = og.base_ptr() as usize;
        let cap = og.capacity();
        assert_eq!(cap, 8 * GRANULE, "reserving must not round the capacity");
        if !reserved {
            // Wholly committed: the historical behaviour, byte for byte.
            assert_eq!(og.committed_bytes(), cap);
            assert!(og.contains((base + cap - 8) as *const u8));
            return;
        }
        assert_eq!(og.committed_bytes(), 0, "construction commits nothing");
        assert!(
            !og.contains(base as *const u8),
            "nothing is backed yet, so nothing is readable old-gen storage",
        );

        let p = og.alloc(HEADER_SIZE + 64, 8).expect("first allocation");
        assert_eq!(p as usize, base);
        assert_eq!(og.committed_bytes(), GRANULE, "one granule, on demand");
        assert!(og.contains(p));
        // SAFETY: `alloc` handed out `HEADER_SIZE + 64` committed bytes at `p`.
        let bytes = unsafe { std::slice::from_raw_parts(p, HEADER_SIZE + 64) };
        assert!(bytes.iter().all(|&b| b == 0), "alloc still zeroes");
        assert!(
            !og.contains((base + 3 * GRANULE) as *const u8),
            "reserved-but-uncommitted space is not readable old-gen storage",
        );

        // A request that crosses granules commits the whole run it needs.
        let q = og.alloc(2 * GRANULE, 8).expect("a multi-granule allocation");
        assert!(og.committed_bytes() >= 3 * GRANULE);
        // SAFETY: the allocation spans `2 * GRANULE` committed bytes at `q`;
        // touching its last byte is what would fault if the commit were short.
        unsafe { std::ptr::write_volatile(q.add(2 * GRANULE - 1), 0xAB) };
        assert!(og.contains(unsafe { q.add(2 * GRANULE - 1) }));

        // `-Xms`: an explicit prefix commit raises the readable mark too.
        let committed = og.commit_initial_prefix(6 * GRANULE);
        assert_eq!(committed, 6 * GRANULE);
        assert!(og.committed_bytes() >= 6 * GRANULE);
        assert!(og.contains((base + 6 * GRANULE - 8) as *const u8));
        assert!(!og.contains((base + 6 * GRANULE) as *const u8));
        assert_eq!(og.trigger_stats().commit_refusals, 0);
    }

    /// Compaction's tail zero becomes a give-back of whole granules where the
    /// store can, and the range still reads as zero afterwards — the H2-CID0
    /// contract the `memset` existed for.
    #[test]
    fn compaction_gives_whole_tail_granules_back_and_the_tail_still_reads_zero() {
        let (mut og, reserved) = big_old_gen(8);
        let base = og.base_ptr() as usize;
        // Six 1 MiB-bodied objects: about three granules of storage.
        let slots = (1024 * 1024 / SLOT_SIZE) as u32;
        let mut ptrs = Vec::new();
        for _ in 0..6 {
            let p = og
                .alloc(HEADER_SIZE + slots as usize * SLOT_SIZE, 8)
                .expect("room");
            // SAFETY: `p` is a fresh, correctly sized allocation.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        slots,
                    ),
                );
            }
            ptrs.push(p);
        }
        let top = og.high_water();
        // Only the first survives.
        // SAFETY: a header was written there above.
        unsafe { (*(ptrs[0] as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
        let _ = og.compact();
        let end = og.used();
        assert!(end < GRANULE, "one object survives");
        // SAFETY: `[end, top)` is inside the store and was committed by the
        // allocations above; a reset granule stays mapped and readable.
        let tail = unsafe { std::slice::from_raw_parts((base + end) as *const u8, top - end) };
        assert!(tail.iter().all(|&b| b == 0), "the compacted tail must read as zero");
        let whole = (top / GRANULE) * GRANULE - end.div_ceil(GRANULE) * GRANULE;
        let resets_to_zero = cfg!(any(target_os = "linux", target_os = "windows"));
        if reserved && resets_to_zero {
            assert_eq!(
                og.bytes_given_back(),
                whole as u64,
                "every whole granule of the dropped range goes back to the OS",
            );
        } else {
            assert_eq!(
                og.bytes_given_back(),
                0,
                "the memset arm gives nothing back"
            );
        }
        // The storage is still usable: the next allocation lands on the
        // compacted end and reads as zero.
        let p = og
            .alloc(HEADER_SIZE + 64, 8)
            .expect("reuse after compaction");
        assert_eq!(p as usize, base + end);
        assert!(og.contains(p));
    }

    // -----------------------------------------------------------------
    // gen r4w3/oldgen3 (2026-09-23)
    // -----------------------------------------------------------------

    /// `n` isolated `hole`-byte holes — each separated from the next by a live
    /// 16-byte keeper so nothing can coalesce them — plus the big trailing
    /// block.
    fn og_with_isolated_holes(n: usize, hole: usize) -> OldGen {
        let mut og = OldGen::new(1 << 20);
        let mut holes = Vec::new();
        for _ in 0..n {
            holes.push(og.alloc(hole, 8).expect("1 MiB covers the holes"));
            og.alloc(HEADER_SIZE, 8).expect("and the keepers");
        }
        for p in holes {
            // SAFETY: each came from `alloc(hole, 8)` on this old gen and is
            // freed exactly once with its own size.
            unsafe { og.free(p, hole) };
        }
        og
    }

    /// `gengc-r4w2-oldgen2-best-fit-scan-is-unbounded-over-non-fitting-blocks`:
    /// a request a little larger than a class full of holes used to walk every
    /// hole on every call. The bucket's size bound skips the class in O(1)
    /// whenever it proves nothing there fits, tightens itself after a scan
    /// that found nothing, and never hides a block that DOES fit.
    ///
    /// gen r4w4/oldgen4: 48/56 no longer share a class (below 64 bytes every
    /// 8-aligned size has its own), so the pair is 1040/1100 in `[1024,
    /// 1280)`; and `N` stays under `NONFIT_PROBE_LIMIT` so the full walks this
    /// test counts are not deferred (the deferral has its own test).
    #[test]
    fn a_class_that_cannot_fit_the_request_is_skipped_without_a_scan() {
        const N: usize = 40;
        const HOLE: usize = 1040;
        const REQ: usize = 1100;
        assert!(N < NONFIT_PROBE_LIMIT);
        let mut og = og_with_isolated_holes(N, HOLE);
        assert_eq!(bucket_for(HOLE), bucket_for(REQ), "one class holds both sizes");
        assert_eq!(
            min_satisfying_bucket(REQ + 7),
            bucket_for(HOLE),
            "and the request starts there"
        );

        // Every hole is 1040 bytes, so the bound is 1040 < 1100: skip at once.
        let s0 = og.trigger_stats();
        let r1 = og.alloc(REQ, 8).expect("the trailing block serves it");
        let s1 = og.trigger_stats();
        assert_eq!(s1.nonfit_probes, s0.nonfit_probes, "no hole was walked");
        assert_eq!(s1.bucket_skips, s0.bucket_skips + 1);

        // A FITTING block raises the bound, and the next request finds it —
        // behind every hole, exactly where the unbounded scan found it.
        // SAFETY: `r1` came from `alloc(REQ, 8)` above.
        unsafe { og.free(r1, REQ) };
        let r2 = og.alloc(REQ, 8).expect("the freed block");
        assert_eq!(
            r2, r1,
            "placement is unchanged: the fitting block is reused"
        );
        let s2 = og.trigger_stats();
        assert_eq!(s2.nonfit_probes - s1.nonfit_probes, N as u64);

        // The bound is now stale-high (1100, with no such block left): one
        // full scan finds nothing and TIGHTENS it...
        let _r3 = og.alloc(REQ, 8).expect("the trailing block again");
        let s3 = og.trigger_stats();
        assert_eq!(s3.nonfit_probes - s2.nonfit_probes, N as u64);
        assert_eq!(
            og.buckets[bucket_for(HOLE)].size_bound,
            HOLE,
            "tightened to the real maximum"
        );
        // ...so the next one skips again.
        let _r4 = og.alloc(REQ, 8).expect("and again");
        let s4 = og.trigger_stats();
        assert_eq!(s4.nonfit_probes, s3.nonfit_probes);
        assert_eq!(s4.bucket_skips, s3.bucket_skips + 1);

        // A request the holes DO fit is served from them, as before.
        let before = og.free_block_count();
        let small = og.alloc(1000, 8).expect("a hole fits 1000");
        assert!(
            og.free_block_count() <= before,
            "served from a hole, not the tail"
        );
        assert!(
            (small as usize) < (r1 as usize),
            "the holes all lie below the first tail carve"
        );
    }

    /// The bound is an UPPER bound on every block in its bucket after every
    /// kind of mutation — the property that makes the skip unable to hide a
    /// fit (and so unable to turn into a spurious OOM).
    #[test]
    fn bucket_size_bounds_stay_upper_bounds_through_every_mutation() {
        let check = |og: &OldGen, what: &str| {
            for (k, b) in og.buckets.iter().enumerate() {
                let max = b.iter().map(|f| f.size).max().unwrap_or(0);
                assert!(
                    b.size_bound >= max,
                    "bucket {k} bound {} < block {max} after {what}",
                    b.size_bound
                );
            }
        };
        let mut og = OldGen::new(256 * 1024);
        let mut live: Vec<(*mut u8, usize)> = Vec::new();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..4000 {
            let r = next();
            if r % 3 != 0 || live.is_empty() {
                let size = 16 + (r as usize >> 8) % 600;
                if let Some(p) = og.alloc(size, 8) {
                    live.push((p, size));
                }
                check(&og, "alloc");
            } else {
                let i = (r as usize >> 16) % live.len();
                let (p, size) = live.swap_remove(i);
                // SAFETY: `(p, size)` is exactly what `alloc` returned.
                unsafe { og.free(p, size) };
                check(&og, "free");
            }
            if round % 500 == 499 {
                og.coalesce_free_blocks();
                check(&og, "coalesce");
            }
        }
        // And a compaction's rebuild, on a generation with real headers (the
        // blocks above are header-less zeros, which a walk would stride as
        // 16-byte objects): nothing is marked, so it drops all of them.
        let (mut og2, _) = old_gen_with_objects(8, 2);
        check(&og2, "setup");
        let _ = og2.compact();
        check(&og2, "compact");
        assert!(og2.alloc(1024, 8).is_some());
    }

    /// Lay out `n` two-slot objects, free the ones `dead` names (without
    /// coalescing), and return the old gen and every object offset.
    fn og_with_holes(n: usize, dead: &[usize]) -> (OldGen, Vec<usize>) {
        let (mut og, offsets) = old_gen_with_objects(n, 2);
        let base = og.base_ptr() as usize;
        for &i in dead {
            // SAFETY: each is exactly one `alloc`ed block.
            unsafe { og.free((base + offsets[i]) as *mut u8, HEADER_SIZE + 2 * SLOT_SIZE) };
        }
        (og, offsets)
    }

    fn rel_walk(og: &OldGen) -> Vec<(usize, usize)> {
        let base = og.base_ptr() as usize;
        og.walk_objects()
            .into_iter()
            .map(|(p, s)| (p as usize - base, s))
            .collect()
    }

    /// `walk_objects_from` in slices of any size, and `for_each_object`, visit
    /// exactly what `walk_objects` returns, in the same order — including
    /// across region boundaries (the freed objects split the generation into
    /// several allocated regions).
    #[test]
    fn a_budgeted_walk_in_slices_visits_exactly_what_walk_objects_returns() {
        let (og, _offsets) = og_with_holes(12, &[0, 3, 4, 8, 11]);
        let base = og.base_ptr() as usize;
        let expected = rel_walk(&og);
        assert_eq!(expected.len(), 7);

        let mut each = Vec::new();
        og.for_each_object(|p, s| each.push((p as usize - base, s)));
        assert_eq!(
            each, expected,
            "for_each_object is walk_objects without the Vec"
        );

        for budget in 1..=8 {
            let mut got = Vec::new();
            let mut from = 0usize;
            let mut calls = 0;
            loop {
                calls += 1;
                assert!(calls < 100, "a budgeted walk must make progress");
                let next =
                    og.walk_objects_from(from, budget, |p, s| got.push((p as usize - base, s)));
                match next {
                    Some(n) => {
                        assert!(
                            expected.iter().any(|&(o, _)| o == n),
                            "budget {budget}: resume offset {n:#x} is not an object start",
                        );
                        from = n;
                    }
                    None => break,
                }
            }
            assert_eq!(got, expected, "budget {budget}");
        }
    }

    /// The two things that may happen between two slices of the concurrent
    /// sweep, and what `walk_objects_from` must do about each: an allocation
    /// into a hole BELOW the resume offset (this sweep's own frees, coalesced)
    /// is not revisited, and the budget running out exactly at a region's end
    /// resumes at the NEXT region's first object, not at the free block — so
    /// an allocation into that free block cannot straddle the resume point.
    #[test]
    fn a_resumed_walk_is_not_confused_by_allocations_between_slices() {
        let size = HEADER_SIZE + 2 * SLOT_SIZE;
        // Objects 0..4, a hole of two (4, 5), objects 6..10.
        let (mut og, offsets) = og_with_holes(10, &[4, 5]);
        let base = og.base_ptr() as usize;

        // Four objects end exactly at the hole: resume at object 6.
        let mut first = Vec::new();
        let resume = og
            .walk_objects_from(0, 4, |p, _| first.push(p as usize - base))
            .expect("more to walk");
        assert_eq!(first, offsets[0..4].to_vec());
        assert_eq!(
            resume, offsets[6],
            "the next REGION's first object, not the free block"
        );

        // "This sweep" frees objects 1..3 (below the resume offset); an
        // allocation failure coalesces them with nothing further; a new object
        // is then allocated into the hole at 4 and one into the freed run.
        // SAFETY: objects 1 and 2 are adjacent `alloc`ed blocks.
        unsafe { og.free((base + offsets[1]) as *mut u8, 2 * size) };
        og.coalesce_free_blocks();
        let hole = og
            .alloc(2 * size, 8)
            .expect("the old hole at 4..6 or the freed run");
        let other = og.alloc(size, 8).expect("room");
        for p in [hole, other] {
            // SAFETY: fresh allocations of at least one two-slot object.
            unsafe {
                std::ptr::write(
                    p as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(9),
                        ObjectKind::Object,
                        ArrayElementType::Reference,
                        0,
                        if p == hole { 2 * 2 + 1 } else { 2 },
                    ),
                );
            }
        }
        assert!((hole as usize - base) < resume && (other as usize - base) < resume);

        let mut rest = Vec::new();
        assert_eq!(
            og.walk_objects_from(resume, usize::MAX, |p, _| rest.push(p as usize - base)),
            None
        );
        assert_eq!(
            rest,
            offsets[6..10].to_vec(),
            "exactly the objects at and after the resume point"
        );
    }

    /// `gengc-r4w2-oldgen2-in-place-sweep-never-returns-pages-and-old-gen-cannot-grow`
    /// item 1: the candidate sample is taken whether or not the flag is on;
    /// the give-back itself is opt-in, bounded to large free blocks below the
    /// high-water mark, refused above the trigger floor, and rate-limited.
    #[test]
    fn an_in_place_give_back_returns_large_free_granules_only_when_asked() {
        let (mut og, reserved) = big_old_gen(16);
        let base = og.base_ptr() as usize;
        let mut blocks = Vec::new();
        for _ in 0..12 {
            blocks.push(og.alloc(GRANULE, 8).expect("room for twelve granules"));
        }
        assert_eq!(og.high_water(), 12 * GRANULE);
        // Free granules 2..9 as one run (7 whole granules), and granule 10 on
        // its own (1 granule: below `GIVE_BACK_MIN_GRANULES`).
        for &p in &blocks[2..9] {
            // SAFETY: each is exactly one `alloc(GRANULE, 8)` block; nothing
            // was written into it that anything reads.
            unsafe { og.free(p, GRANULE) };
        }
        // SAFETY: as above.
        unsafe { og.free(blocks[10], GRANULE) };
        og.coalesce_free_blocks();
        if reserved {
            // Make the run's pages resident and non-zero, so a give-back is
            // observable as zeros.
            // SAFETY: `[2G, 9G)` is a committed free block of this store.
            unsafe { std::ptr::write_bytes((base + 2 * GRANULE) as *mut u8, 0xA5, 7 * GRANULE) };
        }

        // Flag off: only the sample. The trailing block is above high water
        // and the single-granule hole is too small, so the run is all of it.
        assert_eq!(og.after_in_place_sweep(false), 0);
        assert_eq!(
            og.trigger_stats().give_back_candidate_bytes,
            (7 * GRANULE) as u64
        );
        assert_eq!(og.bytes_given_back(), 0);

        let given = og.after_in_place_sweep(true);
        if !reserved {
            assert_eq!(
                given, 0,
                "the wholly committed store has nothing to give back"
            );
            return;
        }
        assert_eq!(given, 7 * GRANULE);
        assert_eq!(og.bytes_given_back(), (7 * GRANULE) as u64);
        assert_eq!(og.trigger_stats().in_place_give_backs, 1);
        if cfg!(any(
            target_os = "linux",
            target_os = "windows",
            target_os = "macos"
        )) {
            // SAFETY: a reset granule stays mapped and readable.
            let run = unsafe {
                std::slice::from_raw_parts((base + 2 * GRANULE) as *const u8, 7 * GRANULE)
            };
            assert!(
                run.iter().all(|&b| b == 0),
                "a given-back granule reads back as zero"
            );
        }
        assert!(
            og.contains((base + 5 * GRANULE) as *const u8),
            "and is still readable old-gen storage"
        );

        // Rate limit: not again until `GIVE_BACK_EVERY` collections have ended.
        assert_eq!(og.after_in_place_sweep(true), 0);
        for _ in 0..GIVE_BACK_EVERY {
            og.note_collection_end(og.used());
        }
        assert_eq!(og.after_in_place_sweep(true), 7 * GRANULE);

        // At or above the trigger floor nothing is given back — the
        // generation is about to reuse those pages — even with a candidate on
        // hand and the rate limit satisfied. Fill exactly to the floor
        // (24 MiB of 32): the 8 MiB tail, the 2 MiB hole, then 6 MiB off the
        // front of the run, which leaves [5G, 9G) — still four whole granules.
        for _ in 0..GIVE_BACK_EVERY {
            og.note_collection_end(og.used());
        }
        let tail = og.alloc(4 * GRANULE, 8).expect("the exact-fit tail");
        assert_eq!(tail as usize, base + 12 * GRANULE);
        let hole = og.alloc(GRANULE, 8).expect("the exact-fit hole");
        assert_eq!(hole as usize, base + 10 * GRANULE);
        let front = og.alloc(3 * GRANULE, 8).expect("the front of the run");
        assert_eq!(front as usize, base + 2 * GRANULE);
        assert_eq!(og.used(), og.capacity() * 75 / 100);
        assert_eq!(og.after_in_place_sweep(true), 0);
        assert_eq!(
            og.trigger_stats().give_back_candidate_bytes,
            (4 * GRANULE) as u64
        );
        assert_eq!(og.trigger_stats().in_place_give_backs, 2);

        // The storage stays usable after a give-back.
        assert!(og.alloc(64, 8).is_some());
    }

    /// The wave-3 trigger instruments: a peek decides nothing and counts
    /// nothing; a collection the hysteresis WOULD have skipped reports what it
    /// reclaimed; a concurrent collection is a collection (and is not charged
    /// to a pending STW verdict).
    #[test]
    fn the_trigger_instruments_attribute_each_collection_to_its_reason() {
        let (mut og, _) = old_gen_with_objects(1, 1);
        let cap = og.capacity();
        let mut blocks = Vec::new();
        // A margin above the floor, so freeing a few blocks below does not
        // drop the generation under it.
        while og.used() < cap * 75 / 100 + 2048 {
            blocks.push(og.alloc(256, 8).expect("room below the floor"));
        }
        // Peeking is free.
        let s0 = og.trigger_stats();
        assert!(og.major_trigger_peek(false, false).runs());
        assert_eq!(og.trigger_stats(), s0, "a peek records nothing");

        // Arm the hysteresis with a collection that reclaimed nothing.
        assert_eq!(og.major_trigger(false, false), MajorTrigger::Occupancy);
        og.note_collection_end(og.used());
        assert_eq!(og.trigger_stats().would_suppress_freed_bytes, 0);

        // The next decision is one the hysteresis would skip; the collection
        // it runs frees two blocks.
        assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);
        let before = og.used();
        for p in blocks.drain(..2) {
            // SAFETY: exactly the `(p, 256)` pairs `alloc` returned.
            unsafe { og.free(p, 256) };
        }
        og.note_collection_end(before);
        let s = og.trigger_stats();
        assert_eq!(s.would_suppress_freed_bytes, 512);
        assert_eq!(s.would_suppress_low_yield, 1, "512 bytes is under cap / 16");
        assert_eq!(s.freed_bytes, 512);
        assert_eq!(s.collections, 2);
        assert!(s.largest_free_after_last > 0 && s.free_blocks_after_last > 0);

        // A concurrent collection: counted, not attributed to a stale verdict.
        assert_eq!(og.major_trigger(false, false), MajorTrigger::WouldSuppress);
        for p in blocks.drain(..1) {
            // SAFETY: as above.
            unsafe { og.free(p, 256) };
        }
        og.note_concurrent_collection_end(256);
        let s = og.trigger_stats();
        assert_eq!(s.concurrent_collections, 1);
        assert_eq!(s.collections, 3);
        assert_eq!(s.freed_bytes, 768);
        assert_eq!(
            s.would_suppress_freed_bytes, 512,
            "the concurrent cycle was not a WouldSuppress run"
        );
    }

    // -----------------------------------------------------------------
    // gen r4w4/oldgen4 (2026-09-24)
    // -----------------------------------------------------------------

    /// The shrink: the first shrinking collection only records (damped),
    /// the next decommits the free tail down to the live set, the generation
    /// grows back on demand and reads zero, and a committed end that advanced
    /// since the last collection damps the next shrink again.
    #[test]
    fn a_shrink_after_collection_decommits_the_free_tail_and_the_generation_regrows() {
        let (mut og, reserved) = big_old_gen(16);
        let base = og.base_ptr() as usize;
        let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
        let mut big = Vec::new();
        for _ in 0..10 {
            big.push(og.alloc(GRANULE, 8).expect("ten granules"));
        }
        for p in big {
            // SAFETY: each is exactly one `alloc(GRANULE, 8)` block.
            unsafe { og.free(p, GRANULE) };
        }
        og.coalesce_free_blocks();
        let committed_before = og.committed_bytes();

        let r0 = og.resize_after_collection(true);
        assert_eq!(r0.released, 0, "the first shrinking collection releases nothing");
        if !reserved {
            // The wholly committed store has nothing to decommit, ever.
            assert!(!r0.damped);
            assert_eq!(og.sizing_stats().shrinks, 0);
            return;
        }
        assert!(r0.damped, "...because the generation has only grown so far");
        assert!(committed_before >= 10 * GRANULE);
        assert_eq!(og.committed_bytes(), committed_before);

        let r1 = og.resize_after_collection(true);
        assert!(!r1.damped);
        assert!(
            r1.released >= 9 * GRANULE,
            "the free tail above the survivor's granule goes back ({} bytes)",
            r1.released
        );
        assert_eq!(og.committed_bytes(), committed_before - r1.released);
        assert!(og.committed_bytes() <= GRANULE, "down to the survivor's granule");
        assert!(og.contains(keep), "the survivor is still readable");
        assert!(
            !og.contains((base + 3 * GRANULE) as *const u8),
            "a decommitted granule is no longer readable old-gen storage"
        );
        // The mark screen reads a whole header at any address `contains`
        // admits: the committed prefix must extend `READ_SLACK` past it.
        assert!(
            og.readable_end + READ_SLACK <= og.committed_bytes(),
            "a shrink must keep READ_SLACK committed above readable_end"
        );
        assert!(!og.contains((base + og.committed_bytes() - 1) as *const u8));
        assert_eq!(og.committed_capacity(), og.committed_bytes());

        // Growth is on demand: the next allocation re-commits, from the
        // committed end, and reads zero.
        let q = og.alloc(5 * GRANULE, 8).expect("the generation grows back");
        // SAFETY: `alloc` handed out `5 * GRANULE` committed, zeroed bytes.
        let bytes = unsafe { std::slice::from_raw_parts(q, 5 * GRANULE) };
        assert!(bytes.iter().all(|&b| b == 0));
        // SAFETY: the last byte of the allocation; faults if the commit is short.
        unsafe { std::ptr::write_volatile(q.add(5 * GRANULE - 1), 0x5A) };
        // SAFETY: pointer arithmetic inside the same allocation.
        assert!(og.contains(unsafe { q.add(5 * GRANULE - 1) }));

        // The committed end advanced since the last resize: damped...
        // SAFETY: `(q, 5 * GRANULE)` is what `alloc` returned.
        unsafe { og.free(q, 5 * GRANULE) };
        og.coalesce_free_blocks();
        let r2 = og.resize_after_collection(true);
        assert!(r2.damped && r2.released == 0);
        // ...and the next collection, with no growth in between, shrinks.
        let r3 = og.resize_after_collection(true);
        assert!(r3.released >= 4 * GRANULE, "{r3:?}");
        let s = og.sizing_stats();
        assert_eq!((s.shrinks, s.shrinks_damped), (2, 2));
        assert!(s.committed_peak >= 10 * GRANULE as u64);
        assert_eq!(og.trigger_stats().commit_refusals, 0);

        // The kill switch: `allow_shrink = false` never releases anything.
        let q2 = og.alloc(6 * GRANULE, 8).expect("again");
        // SAFETY: as above.
        unsafe { og.free(q2, 6 * GRANULE) };
        og.coalesce_free_blocks();
        let _ = og.resize_after_collection(false);
        assert_eq!(og.resize_after_collection(false).released, 0);
    }

    /// gen r4w6/oldpin6 — a concurrent sweep's end arms the resize for the
    /// young pauses that follow: nothing is due before it; a damped attempt is
    /// kept for the next pause; the undamped one shrinks (READ_SLACK kept above
    /// `readable_end`, the survivor readable) and answers it; a stop-the-world
    /// resize answers it too; and a generation that keeps growing between
    /// young pauses runs out of attempts instead of sorting its free list on
    /// every pause.
    #[test]
    fn a_concurrent_sweep_end_arms_a_bounded_resize_for_the_young_pauses() {
        let (mut og, reserved) = big_old_gen(16);
        assert_eq!(og.resize_after_concurrent_sweep_if_due(true), None, "nothing due yet");
        let keep = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor");
        let mut big = Vec::new();
        for _ in 0..10 {
            big.push(og.alloc(GRANULE, 8).expect("ten granules"));
        }
        let committed_peak = og.committed_bytes();
        for p in big {
            // SAFETY: each is exactly one `alloc(GRANULE, 8)` block.
            unsafe { og.free(p, GRANULE) };
        }
        og.coalesce_free_blocks();
        og.note_concurrent_collection_end(10 * GRANULE);
        assert_eq!(og.concurrent_resize_attempts(), CONCURRENT_RESIZE_ATTEMPTS);
        let r0 = og
            .resize_after_concurrent_sweep_if_due(true)
            .expect("due after a concurrent end");
        assert_eq!(r0.released, 0);
        if !reserved {
            assert!(!r0.damped, "the wholly committed store has nothing to damp");
            assert_eq!(og.concurrent_resize_attempts(), 0, "an undamped resize answers it");
            return;
        }
        assert!(r0.damped, "the generation has only grown so far");
        assert_eq!(
            og.concurrent_resize_attempts(),
            CONCURRENT_RESIZE_ATTEMPTS - 1,
            "a damped attempt is kept for the next young pause"
        );
        let r1 = og.resize_after_concurrent_sweep_if_due(true).expect("still due");
        assert!(r1.released >= 9 * GRANULE, "the free tail goes back: {r1:?}");
        assert_eq!(og.concurrent_resize_attempts(), 0);
        assert_eq!(og.resize_after_concurrent_sweep_if_due(true), None, "answered");
        assert!(og.contains(keep), "the survivor stays readable");
        assert!(
            og.readable_end + READ_SLACK <= og.committed_bytes(),
            "READ_SLACK stays committed above readable_end"
        );
        assert!(og.committed_bytes() < committed_peak);
        let s = og.sizing_stats();
        assert_eq!((s.concurrent_sweep_resizes, s.concurrent_sweep_shrinks), (2, 1));
        assert_eq!(s.shrinks, 1);

        // A stop-the-world resize in the same pause answers a pending one.
        og.note_concurrent_collection_end(0);
        let _ = og.resize_after_collection(true);
        assert_eq!(og.concurrent_resize_attempts(), 0);

        // Bounded: growth between every pair of young pauses keeps each
        // attempt damped, and the attempts run out.
        og.note_concurrent_collection_end(0);
        for i in 0..usize::from(CONCURRENT_RESIZE_ATTEMPTS) {
            let n = 5 + i;
            let q = og.alloc(n * GRANULE, 8).expect("growth between young pauses");
            // SAFETY: exactly what `alloc` returned.
            unsafe { og.free(q, n * GRANULE) };
            og.coalesce_free_blocks();
            let r = og
                .resize_after_concurrent_sweep_if_due(true)
                .expect("within the bound");
            assert!(r.damped && r.released == 0, "attempt {i}: {r:?}");
        }
        assert_eq!(og.resize_after_concurrent_sweep_if_due(true), None, "bounded");
    }

    /// gen r4w6/oldpin6 — the shrink edges the round asked for: a COMPLETELY
    /// full generation (no free tail: nothing to release, nothing read past
    /// the reservation), and one whose last object sits at the very end of
    /// the reservation after everything below it died (the tail run is empty,
    /// so the shrink keeps everything: it may only release a FREE tail).
    #[test]
    fn a_shrink_of_a_full_generation_or_one_with_its_last_object_at_the_end_releases_nothing() {
        let (mut og, reserved) = big_old_gen(8);
        let cap = og.capacity();
        // Completely full: one block covering the whole reservation.
        let all = og.alloc(cap, 8).expect("the whole generation");
        assert_eq!(og.used(), cap);
        assert_eq!(og.tail_free_start(), cap, "no free tail");
        let _ = og.resize_after_collection(true);
        let r = og.resize_after_collection(true);
        assert_eq!(r.released, 0, "a full generation has no free tail to release");
        // SAFETY: the last byte of a live, committed block.
        unsafe { std::ptr::write_volatile(all.add(cap - 1), 0x5A) };
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(all, cap) };
        og.coalesce_free_blocks();

        // The last object at the very end of the reservation, everything
        // below it dead.
        let low = og.alloc(cap - GRANULE, 8).expect("everything but the last granule");
        let last = og.alloc(GRANULE, 8).expect("the last granule");
        assert_eq!(last as usize + GRANULE, og.base_ptr() as usize + cap);
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(low, cap - GRANULE) };
        og.coalesce_free_blocks();
        let _ = og.resize_after_collection(true);
        let r = og.resize_after_collection(true);
        assert_eq!(r.released, 0, "only a trailing FREE run is released");
        assert!(og.contains(last), "the last object stays readable");
        // SAFETY: the last byte of the reservation, inside `last`.
        unsafe { std::ptr::write_volatile(last.add(GRANULE - 1), 0xA5) };
        if reserved {
            assert!(og.committed_bytes() >= cap, "nothing below the last object was decommitted");
        }
    }

    /// A shrink never goes below the `-Xms` prefix, nor below the last
    /// allocated byte, nor below `used / 0.3`.
    #[test]
    fn a_shrink_keeps_the_xms_floor_the_last_object_and_the_free_ratio() {
        let (mut og, reserved) = big_old_gen(16);
        if !reserved {
            return;
        }
        assert_eq!(og.commit_initial_prefix(6 * GRANULE), 6 * GRANULE);
        assert_eq!(og.sizing_stats().commit_floor, (6 * GRANULE) as u64);
        let _ = og.resize_after_collection(true); // the damped first one
        let _ = og.resize_after_collection(true);
        assert!(
            og.committed_bytes() >= 6 * GRANULE,
            "never below the -Xms prefix ({} committed)",
            og.committed_bytes()
        );

        // An object near the top pins everything below it.
        let (mut og, _) = big_old_gen(16);
        let low = og.alloc(GRANULE, 8).expect("low");
        let mid = og.alloc(8 * GRANULE, 8).expect("middle");
        let high = og.alloc(HEADER_SIZE + 64, 8).expect("high");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(mid, 8 * GRANULE) };
        let _ = low;
        let _ = og.resize_after_collection(true);
        let _ = og.resize_after_collection(true);
        assert!(og.contains(high), "the last allocated object stays committed");

        // The free ratio: 4 granules live need `4 / 0.3` = 13.3 granules.
        let (mut og, _) = big_old_gen(16);
        let live = og.alloc(4 * GRANULE, 8).expect("live");
        let dead = og.alloc(11 * GRANULE, 8).expect("dead");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(dead, 11 * GRANULE) };
        og.coalesce_free_blocks();
        let _ = og.resize_after_collection(true);
        let r = og.resize_after_collection(true);
        assert_eq!(r.released, 0, "releasing 1-2 granules is below SHRINK_MIN_GRANULES");
        assert!(og.contains(live));
    }

    // -----------------------------------------------------------------
    // gen r5w3/oldgen7 (2026-09-26)
    // -----------------------------------------------------------------

    /// The interior decommit
    /// (`gengc-r5w1-oldgen5-proposal-old-gen-interior-decommit`): a survivor
    /// near the top pins the tail shrink, and the large dead run below it is
    /// released instead — after one damped pass, down to the free-ratio
    /// target — leaving holes that `contains` / `contains_range` refuse, that
    /// the walk never reads, and that an allocation re-commits (the hole flag
    /// dropping once every hole is refilled).
    #[test]
    fn an_interior_decommit_releases_a_dead_run_below_a_survivor_at_the_top() {
        let (mut og, reserved) = big_old_gen(32);
        let base = og.base_ptr() as usize;
        let low = og.alloc(HEADER_SIZE + 64, 8).expect("a low survivor");
        let mid = og.alloc(20 * GRANULE, 8).expect("a large run that dies");
        let high = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor near the top");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(mid, 20 * GRANULE) };
        og.coalesce_free_blocks();
        if !reserved {
            assert_eq!(og.decommit_interior_free_runs(), 0, "nothing to decommit");
            return;
        }
        // The tail shrink cannot help: `high` pins the committed end.
        let _ = og.resize_after_collection(true);
        let _ = og.resize_after_collection(true);
        let committed_before = og.committed_bytes();
        assert!(committed_before >= 20 * GRANULE, "{committed_before}");

        assert_eq!(og.decommit_interior_free_runs(), 0, "the first pass is damped");
        assert_eq!(og.sizing_stats().interior_decommits_damped, 1);
        let released = og.decommit_interior_free_runs();
        assert!(released >= 18 * GRANULE, "the dead run's interior goes back: {released}");
        assert_eq!(og.committed_bytes(), committed_before - released);
        let s = og.sizing_stats();
        assert_eq!((s.interior_decommits, s.interior_decommitted_bytes), (1, released as u64));
        assert_eq!(s.shrinks, 0, "the tail shrink released nothing");

        // Readers: the survivors stay readable; the hole is not old-gen
        // storage, for a point, a header's width at its lower edge, or an
        // extent across it.
        assert!(og.contains(low) && og.contains(high));
        assert!(og.contains_range(low, HEADER_SIZE + 64));
        assert!(!og.contains((base + 5 * GRANULE) as *const u8), "a released granule");
        assert!(
            !og.contains((base + GRANULE - 8) as *const u8),
            "a header read at the hole's lower edge would straddle into it"
        );
        let run_start = mid as *const u8;
        assert!(!og.contains_range(run_start, 20 * GRANULE), "an extent across the hole");
        assert_eq!(og.walk_objects().iter().map(|&(_, s)| s).sum::<usize>(), og.used());
        assert_eq!(og.decommit_interior_free_runs(), 0, "below the target now");

        // An allocation into the hole re-commits what it hands out and reads
        // zero; the flag stays while holes remain...
        let q = og.alloc(13 * GRANULE, 8).expect("fits only the run below `high`");
        assert!((q as usize) < high as usize, "placed in the hole, not the tail");
        // SAFETY: `alloc` handed out `13 * GRANULE` committed, zeroed bytes.
        let bytes = unsafe { std::slice::from_raw_parts(q, 13 * GRANULE) };
        assert!(bytes.iter().all(|&b| b == 0));
        // SAFETY: the last byte of the allocation; faults if the commit is short.
        unsafe { std::ptr::write_volatile(q.add(13 * GRANULE - 1), 0x5A) };
        assert!(og.sizing_stats().hole_recommits >= 1);
        assert!(og.readable_holes, "the rest of the hole is still released");
        // ...and drops once the last hole is refilled.
        let rest = (high as usize) - (q as usize + 13 * GRANULE);
        let r = og.alloc(rest, 8).expect("the rest of the run, an exact fit");
        // SAFETY: the last byte of the allocation.
        unsafe { std::ptr::write_volatile(r.add(rest - 1), 0xA5) };
        assert!(!og.readable_holes, "every hole refilled: back to the one-number test");
        assert!(og.contains(r));
    }

    /// gen r5w6/old10 — the post-concurrent resize keeps an attempt when its
    /// INTERIOR half is damped. The shape `GenR4W6OldShrinkProbe` makes: the
    /// tail resize is undamped (nothing advanced since the last resize) and
    /// releases nothing (a survivor pins the top), which used to end the
    /// episode on the same pause whose interior pass was damped (its first
    /// pass: the committed size "grew"). Now the next young pause runs both
    /// halves again and the dead run goes back; the episode then ends. A
    /// stop-the-world resize leaves nothing to keep.
    #[test]
    fn a_damped_interior_pass_keeps_the_post_concurrent_resize_for_the_next_pause() {
        let (mut og, reserved) = big_old_gen(32);
        let _low = og.alloc(HEADER_SIZE + 64, 8).expect("a low survivor");
        let mid = og.alloc(20 * GRANULE, 8).expect("a large run that dies");
        let high = og.alloc(HEADER_SIZE + 64, 8).expect("a survivor near the top");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(mid, 20 * GRANULE) };
        og.coalesce_free_blocks();
        if !reserved {
            return;
        }
        // The tail damping's baseline: nothing advances from here on.
        let _ = og.resize_after_collection(true);
        let _ = og.resize_after_collection(true);
        let committed_before = og.committed_bytes();

        og.note_concurrent_collection_end(20 * GRANULE);
        let r0 = og.resize_after_concurrent_sweep_if_due(true).expect("due");
        assert!(!r0.damped && r0.released == 0, "`high` pins the tail: {r0:?}");
        assert_eq!(og.concurrent_resize_attempts(), 0, "the tail half alone ends the episode");
        assert_eq!(og.decommit_interior_after_concurrent_resize(), 0, "the first pass is damped");
        assert_eq!(
            og.concurrent_resize_attempts(),
            CONCURRENT_RESIZE_ATTEMPTS - 1,
            "...so the episode keeps an attempt"
        );

        let r1 = og.resize_after_concurrent_sweep_if_due(true).expect("kept for this pause");
        assert!(!r1.damped, "{r1:?}");
        let released = og.decommit_interior_after_concurrent_resize();
        assert!(released >= 18 * GRANULE, "the dead run's interior goes back: {released}");
        assert_eq!(og.committed_bytes(), committed_before - released);
        assert_eq!(og.concurrent_resize_attempts(), 0, "an undamped pass ends the episode");
        assert_eq!(og.resize_after_concurrent_sweep_if_due(true), None, "answered");
        assert!(og.contains(high), "the survivor stays readable");

        // A stop-the-world resize answers an episode: nothing left to keep.
        og.note_concurrent_collection_end(0);
        let _ = og.resize_after_collection(true);
        assert!(!og.keep_concurrent_resize_attempt());
        assert_eq!(og.concurrent_resize_attempts(), 0);
    }

    /// The pinned compaction RE-COMMITS holes an interior decommit left
    /// instead of refusing (blocker 1 of the interior-decommit proposal),
    /// and completes.
    #[test]
    fn a_pinned_compaction_recommits_interior_holes_instead_of_refusing() {
        let (mut og, reserved) = big_old_gen(32);
        let a = pc_obj(&mut og, 2, true);
        // A dead `int[]` of 20 granules, freed in place.
        let len = (20 * GRANULE / 4) as u32;
        let dead = pc_int_array(&mut og, len, false);
        let dead_size = (ARRAY_DATA_OFFSET + len as usize * 4 + 7) & !7;
        let b = pc_obj(&mut og, 2, true);
        // SAFETY: the dead array's whole reserved extent.
        unsafe { og.free(dead, dead_size) };
        og.coalesce_free_blocks();
        if !reserved {
            return;
        }
        let _ = og.decommit_interior_free_runs(); // damped
        assert!(og.decommit_interior_free_runs() >= 18 * GRANULE);
        assert!(og.readable_holes);
        let walk = og.walk_objects();
        assert_eq!(walk.len(), 2, "the two survivors");
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new())
            .expect("re-commits and compacts");
        assert!(!og.readable_holes, "the holes were re-committed");
        let b_new = done.pointer_map[&(b as usize)];
        assert!(b_new < b as usize, "`b` slid down into the freed run");
        assert_eq!(b_new, a as usize + HEADER_SIZE + 2 * SLOT_SIZE);
        assert_eq!(og.used(), 2 * (HEADER_SIZE + 2 * SLOT_SIZE));
    }

    /// Growing into the young budget
    /// (`gengc-r4w4-oldgen4-proposal-old-gen-borrows-the-young-budget`): a
    /// generation built with headroom grows IN PLACE after a refused request
    /// the collection did not make room for, to `MinHeapFreeRatio` capped at
    /// its maximum; a promotion-buffer refusal and a request beyond the
    /// maximum grow nothing; a generation without headroom never grows.
    #[test]
    fn a_refused_allocation_grows_a_generation_with_headroom_in_place() {
        let (mut plain, _) = big_old_gen(8);
        assert!(plain.alloc(9 * GRANULE, 8).is_none());
        assert_eq!(plain.grow_after_refusal(), 0, "no headroom, no growth");
        assert_eq!(plain.growth_max(), plain.capacity());

        let mut og = OldGen::with_growth_headroom(8 * GRANULE, 12 * GRANULE);
        let base = og.base_ptr() as usize;
        assert_eq!(og.capacity(), 8 * GRANULE, "the capacity starts at its share");
        if og.commit_bits().is_none() {
            assert_eq!(og.growth_max(), 8 * GRANULE, "a fallback store cannot grow in place");
            return;
        }
        assert_eq!(og.growth_max(), 12 * GRANULE);
        assert_eq!(og.grow_after_refusal(), 0, "nothing refused");
        let live = og.alloc(7 * GRANULE, 8).expect("seven granules of live data");
        // An optional buffer carve is not a reason to grow.
        assert!(og.alloc_unzeroed_buffer(2 * GRANULE, 8).is_none());
        assert_eq!(og.grow_after_refusal(), 0, "a buffer refusal grows nothing");
        assert!(og.alloc(2 * GRANULE, 8).is_none(), "full at its share");
        let epoch = og.reclaim_epoch();
        // used 7 granules: `used / 0.6` = 11.7 granules, rounded up to 12.
        assert_eq!(og.grow_after_refusal(), 4 * GRANULE);
        assert_eq!(og.capacity(), 12 * GRANULE);
        assert_eq!(og.base_ptr() as usize, base, "grown in place");
        assert_ne!(og.reclaim_epoch(), epoch, "an open concurrent cycle must abandon");
        let q = og.alloc(2 * GRANULE, 8).expect("the refused request fits now");
        // SAFETY: the last byte of the allocation; faults if the commit is short.
        unsafe { std::ptr::write_volatile(q.add(2 * GRANULE - 1), 0x5A) };
        assert!(og.contains(q) && og.contains(live));
        assert_eq!(og.grow_after_refusal(), 0, "answered");
        // Beyond the maximum: the refusal stands.
        assert!(og.alloc(4 * GRANULE, 8).is_none());
        assert_eq!(og.grow_after_refusal(), 0);
        let s = og.sizing_stats();
        assert_eq!((s.borrow_growths, s.borrowed_bytes), (1, (4 * GRANULE) as u64));
        assert_eq!(s.growth_max_bytes, (12 * GRANULE) as u64);
        assert_eq!(og.used(), 9 * GRANULE);
    }

    /// gen r5w6/old10 — a FRAGMENTATION refusal no compaction answered (a
    /// survivor pinned between the holes, as on the non-moving path's pinned
    /// compaction) grows the generation so its trailing free run holds the
    /// request. Before, `used + refused` fitted the capacity in total, so the
    /// generation did not grow and the refusal stood although the reservation
    /// had room; and a growth to `used + refused` would have left the tail
    /// too short anyway.
    #[test]
    fn a_fragmentation_refusal_grows_the_tail_until_it_holds_the_request() {
        let mut og = OldGen::with_growth_headroom(8 * GRANULE, 16 * GRANULE);
        if og.commit_bits().is_none() {
            return;
        }
        let low = og.alloc(GRANULE, 8).expect("a low survivor");
        let mid = og.alloc(3 * GRANULE, 8).expect("a run that dies");
        let high = og.alloc(GRANULE, 8).expect("a survivor between the holes");
        // SAFETY: exactly what `alloc` returned.
        unsafe { og.free(mid, 3 * GRANULE) };
        og.coalesce_free_blocks();
        // 6 of 8 granules free, in two runs of 3: a 5-granule request fails.
        assert_eq!(og.used(), 2 * GRANULE);
        assert!(og.alloc(5 * GRANULE, 8).is_none(), "fragmented");
        // The tail run starts at 5 granules: 5 + 5 = 10 granules.
        assert_eq!(og.grow_after_refusal(), 2 * GRANULE);
        assert_eq!(og.capacity(), 10 * GRANULE);
        let q = og.alloc(5 * GRANULE, 8).expect("the grown tail holds the request");
        assert!((q as usize) > high as usize, "placed in the tail, above the survivor");
        // SAFETY: the last byte of the allocation; faults if the commit is short.
        unsafe { std::ptr::write_volatile(q.add(5 * GRANULE - 1), 0x5A) };
        assert!(og.contains(low) && og.contains(high));
        assert_eq!(og.grow_after_refusal(), 0, "answered");
    }

    /// Humongous arrays from the top
    /// (`gengc-r5w1-oldgen5-proposal-humongous-arrays-from-the-top-of-old-gen`,
    /// its unit test): alternating humongous and small requests put every
    /// small object below the first humongous one, the humongous ones back to
    /// back from the reservation's end, only their own pages committed (the
    /// gap is a hole); freed, they merge with the gap into ONE block, which
    /// the tail shrink then returns.
    #[test]
    fn humongous_arrays_placed_from_the_top_merge_into_the_free_tail_when_they_die() {
        let (mut og, reserved) = big_old_gen(16);
        let base = og.base_ptr() as usize;
        let cap = og.capacity();
        if !reserved {
            // No holes on the wholly committed store: the ordinary best fit.
            let p = og.alloc_from_top(2 * GRANULE, 8, true).expect("fits");
            assert_eq!(p as usize, base);
            assert_eq!(og.sizing_stats().humongous_top_fallbacks, 1);
            return;
        }
        let small_size = HEADER_SIZE + 64;
        let mut small = Vec::new();
        let mut big = Vec::new();
        for _ in 0..3 {
            small.push(og.alloc(small_size, 8).expect("small"));
            big.push(og.alloc_from_top(2 * GRANULE, 8, true).expect("humongous"));
        }
        small.push(og.alloc(small_size, 8).expect("small"));
        assert_eq!(big[0] as usize + 2 * GRANULE, base + cap, "the first at the very top");
        assert_eq!(big[1] as usize + 2 * GRANULE, big[0] as usize, "back to back");
        assert_eq!(big[2] as usize + 2 * GRANULE, big[1] as usize);
        let lowest_big = big[2] as usize;
        assert!(small.iter().all(|&p| p as usize + small_size <= lowest_big));
        assert_eq!(og.sizing_stats().humongous_top_allocs, 3);
        // Only the small objects' granule and the arrays' are committed.
        assert!(og.committed_bytes() <= 7 * GRANULE, "{}", og.committed_bytes());
        assert!(og.readable_holes, "the gap below the arrays is a hole");
        assert!(!og.contains((base + 5 * GRANULE) as *const u8));
        assert!(og.contains(big[2]) && og.contains(small[3]));
        // SAFETY: the last byte of the topmost array; faults if uncommitted.
        unsafe { std::ptr::write_volatile(big[0].add(2 * GRANULE - 1), 0x5A) };

        for p in big {
            // SAFETY: exactly what `alloc_from_top` returned.
            unsafe { og.free(p, 2 * GRANULE) };
        }
        og.coalesce_free_blocks();
        let (_, largest) = og.free_bytes_and_largest();
        assert_eq!(largest, cap - 4 * small_size, "the dead arrays merged with the gap");
        let _ = og.resize_after_collection(true);
        let r = og.resize_after_collection(true);
        assert!(r.released >= 6 * GRANULE, "the tail shrink returns them: {r:?}");
        assert!(og.committed_bytes() <= GRANULE);
        assert!(og.contains(small[0]));
        assert_eq!(og.walk_objects().iter().map(|&(_, s)| s).sum::<usize>(), og.used());
    }

    /// The fragmentation trigger's refusal half: an allocation refused while
    /// the free list holds 4x its size arms ONE compaction request, a full
    /// generation's refusal and a promotion buffer's do not, and a completed
    /// compaction answers it (and makes the request fit).
    #[test]
    fn a_fragmentation_refusal_arms_one_compaction_request_and_a_compaction_answers_it() {
        // 1024 three-slot (64-byte) objects fill 64 KiB exactly.
        let (mut og, offsets) = old_gen_with_objects(1024, 3);
        assert_eq!(og.free_block_count(), 0);
        let base = og.base_ptr() as usize;
        // A FULL generation: a refusal, but not a fragmentation one.
        assert!(og.alloc(64, 8).is_none());
        assert_eq!(og.sizing_stats().fragmentation_refusals, 0);
        assert!(!og.arm_fragmentation_compaction());

        // Every other object dies: 32 KiB free, no hole larger than 64 bytes.
        for (i, &off) in offsets.iter().enumerate() {
            if i % 2 == 0 {
                // SAFETY: each is exactly one `alloc`ed 64-byte object.
                unsafe { og.free((base + off) as *mut u8, 64) };
            } else {
                // SAFETY: a header `old_gen_with_objects` wrote.
                unsafe { (*((base + off) as *mut ObjectHeader)).add_gc_flags(GC_FLAG_MARKED) };
            }
        }
        // The ladder's single-request form: room in total, no hole.
        assert!(og.fragmented_for(1024));
        assert!(!og.fragmented_for(64), "a 64-byte hole serves 64");
        assert!(!og.fragmented_for(40_000), "32 KiB free cannot serve 40 000");
        // An optional promotion BUFFER that does not fit is not the signal.
        assert!(og.alloc_unzeroed_buffer(1024, 8).is_none());
        assert_eq!(og.fragmentation_pending_request(), 0);
        // An object that does not fit is.
        assert!(og.alloc(1024, 8).is_none());
        assert_eq!(og.fragmentation_pending_request(), 1024);
        assert_eq!(og.sizing_stats().fragmentation_refusals, 1);

        // One request per episode.
        assert!(og.arm_fragmentation_compaction());
        assert!(!og.arm_fragmentation_compaction());
        assert!(og.take_fragmentation_compaction_request());
        assert!(!og.take_fragmentation_compaction_request());
        assert_eq!(og.sizing_stats().fragmentation_compactions_requested, 1);

        // The compaction the request asks for makes the request fit.
        let _ = og.compact();
        assert_eq!(og.used(), 512 * 64);
        assert_eq!(og.sizing_stats().compactions, 1);
        assert!(og.alloc(1024, 8).is_some());
    }

    /// The fragmentation trigger's post-collection half: the existing
    /// `fragmentation_repair_due` state arms a request, once, and consumes the
    /// scale it judged against.
    #[test]
    fn a_fragmented_free_list_after_a_collection_arms_the_request_once() {
        let mut og = OldGen::new(64 * 1024);
        og.alloc(2048, 8).unwrap();
        let small_count = (og.capacity() - og.used()) / 1024;
        let mut ptrs = Vec::new();
        for _ in 0..small_count {
            ptrs.push(og.alloc(1024, 8).unwrap());
        }
        for p in ptrs.iter().step_by(2).take(20) {
            // SAFETY: each `p` came from `alloc(1024, 8)` and is freed once.
            unsafe { og.free(*p, 1024) };
        }
        assert!(og.fragmentation_repair_due());
        assert!(og.note_fragmentation_after_collection());
        assert_eq!(og.fragmentation_pending_request(), 2048);
        assert_eq!(og.recent_max_request(), 0, "the scale is consumed");
        assert!(!og.note_fragmentation_after_collection());
    }

    /// `walk_objects_with_gaps` reports the unwalked tail of a region whose
    /// scan broke, the walked and gap bytes add up to `used`, and a reference
    /// INTO the gap is not an escape for the gap-aware closure (it is for the
    /// plain one, which cannot tell it from a freed block).
    #[test]
    fn a_broken_region_is_reported_as_a_gap_and_a_reference_into_it_is_not_an_escape() {
        let mut og = OldGen::new(4096);
        let a = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let g = og.alloc(HEADER_SIZE + SLOT_SIZE, 8).unwrap();
        let base = og.base_ptr() as usize;
        // SAFETY: both are live blocks sized for a header and one slot.
        unsafe {
            let a_hdr = &mut *(a as *mut ObjectHeader);
            a_hdr.set_num_slots(1);
            a_hdr.add_gc_flags(GC_FLAG_MARKED);
            std::ptr::write(
                a.add(HEADER_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(g))),
            );
            (*(g as *mut ObjectHeader)).set_num_slots(1);
            // Not a valid `ObjectKind`: the walk breaks here.
            std::ptr::write(g.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET), 0xFFu8);
        }
        let (walk, gaps) = og.walk_objects_with_gaps();
        assert_eq!(walk, vec![(a, HEADER_SIZE + SLOT_SIZE)]);
        let g_off = g as usize - base;
        assert_eq!(gaps, vec![(g_off, g_off + HEADER_SIZE + SLOT_SIZE)]);
        let walked: usize = walk.iter().map(|&(_, s)| s).sum();
        let gap: usize = gaps.iter().map(|&(s, e)| e - s).sum();
        assert_eq!(walked + gap, og.used());

        assert!(og.close_live_set(&walk).1, "the plain closure calls it an escape");
        assert!(
            !og.close_live_set_with_gaps(&walk, &gaps).1,
            "the gap-aware closure knows the gap is retained"
        );
        assert_eq!(og.note_walk_gap_recovery(gap), 0);
        assert_eq!(og.sizing_stats().walk_gap_recoveries, 1);
        assert_eq!(og.sizing_stats().walk_gap_bytes_last, gap as u64);
        // A complete walk has no gaps.
        let (clean, _) = old_gen_with_objects(4, 2);
        assert!(clean.walk_objects_with_gaps().1.is_empty());
    }

    // --- gen r4w5/oldcompact5: compaction around pinned objects -------------

    /// A legacy object with `slots` reference slots; marked (live) or not.
    fn pc_obj(og: &mut OldGen, slots: u32, marked: bool) -> *mut u8 {
        let p = og.alloc(HEADER_SIZE + SLOT_SIZE * slots as usize, 8).unwrap();
        // SAFETY: a fresh, zeroed block sized for a header and `slots` slots.
        unsafe {
            (*(p as *mut ObjectHeader)).set_num_slots(slots);
            if marked {
                (*(p as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
            }
        }
        p
    }

    /// An `int[len]`; marked (live) or not.
    fn pc_int_array(og: &mut OldGen, len: u32, marked: bool) -> *mut u8 {
        let p = og.alloc(ARRAY_DATA_OFFSET + len as usize * 4, 8).unwrap();
        // SAFETY: a fresh, zeroed block sized for the array header and body.
        unsafe {
            std::ptr::write(
                p as *mut ObjectHeader,
                ObjectHeader::new(
                    cratonvm_types::ClassId::new(1),
                    ObjectKind::Array,
                    ArrayElementType::Int,
                    len,
                    len,
                ),
            );
            if marked {
                (*(p as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
            }
        }
        p
    }

    fn pc_set_ref(holder: *mut u8, slot: usize, target: usize) {
        // SAFETY: test objects are legacy objects with at least `slot + 1`
        // slots; `target` is non-null and 8-aligned.
        unsafe {
            std::ptr::write(
                holder.add(HEADER_SIZE + slot * SLOT_SIZE) as *mut Value,
                Value::Object(Some(ObjectRef::from_raw(target as *mut u8))),
            );
        }
    }

    fn pc_get_ref(holder: usize, slot: usize) -> usize {
        let slot_addr = holder + HEADER_SIZE + slot * SLOT_SIZE;
        // SAFETY: as for `pc_set_ref`.
        match unsafe { std::ptr::read(slot_addr as *const Value) } {
            Value::Object(Some(r)) => r.as_ptr() as usize,
            _ => 0,
        }
    }

    fn pc_size(slots: usize) -> usize {
        HEADER_SIZE + SLOT_SIZE * slots
    }

    /// The pinned object keeps its address; the live objects above it slide
    /// down to its end and are forwarded; every reference is rewritten — the
    /// pinned object's own included — and a young reference is left alone
    /// but its holder is reported at its NEW address; the walk, the
    /// accounting and the free list all describe the new layout.
    #[test]
    fn a_pinned_object_keeps_its_address_and_the_rest_slides_around_it() {
        let mut og = OldGen::new(64 * 1024);
        let d0 = pc_obj(&mut og, 4, false);
        let p = pc_obj(&mut og, 2, true);
        let d1 = pc_obj(&mut og, 3, false);
        let m1 = pc_obj(&mut og, 2, true);
        let m2 = pc_obj(&mut og, 1, true);
        let _d2 = pc_obj(&mut og, 5, false);
        // A stand-in "young generation": four words outside old gen.
        let young_words = [0u64; 4];
        let y = young_words.as_ptr() as usize;
        pc_set_ref(p, 0, m1 as usize);
        pc_set_ref(m1, 0, m2 as usize);
        pc_set_ref(m1, 1, p as usize);
        pc_set_ref(m2, 0, y);

        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walk, seq, vec![p as usize], (y, y + 32), &HashMap::new())
            .expect("a healthy grid compacts");

        assert_eq!(done.pinned, 1);
        assert_eq!(done.moved, 2);
        assert_eq!(
            done.pointer_map.get(&(p as usize)).copied().unwrap_or(p as usize),
            p as usize,
            "the pin does not move",
        );
        let m1_new = done.pointer_map[&(m1 as usize)];
        let m2_new = done.pointer_map[&(m2 as usize)];
        assert_eq!(m1_new, p as usize + pc_size(2), "m1 slides to the pin's end");
        assert_eq!(m1_new, d1 as usize, "...which is where the dead d1 was");
        assert_eq!(m2_new, m1_new + pc_size(2));
        assert_eq!(pc_get_ref(p as usize, 0), m1_new, "the pin's own slot is forwarded");
        assert_eq!(pc_get_ref(m1_new, 0), m2_new);
        assert_eq!(pc_get_ref(m1_new, 1), p as usize, "a reference to the pin is unchanged");
        assert_eq!(pc_get_ref(m2_new, 0), y, "a young reference is not touched");
        assert_eq!(done.young_ref_holders, vec![m2_new]);
        for q in [p as usize, m1_new, m2_new] {
            // SAFETY: each is a survivor's header after the slide.
            let h = unsafe { &*(q as *const ObjectHeader) };
            assert!(!h.is_forwarded(), "the forward is retired");
            assert_eq!(h.gc_flags() & GC_FLAG_MARKED, 0, "the mark is cleared");
        }
        let after: Vec<(usize, usize)> = og
            .walk_objects()
            .iter()
            .map(|&(q, s)| (q as usize, s))
            .collect();
        assert_eq!(
            after,
            vec![(p as usize, pc_size(2)), (m1_new, pc_size(2)), (m2_new, pc_size(1))]
        );
        assert_eq!(og.used(), 2 * pc_size(2) + pc_size(1));
        let (free, largest) = og.free_bytes_and_largest();
        assert_eq!(free + og.used(), og.capacity(), "used + free is the capacity");
        assert_eq!(og.free_block_count(), 2, "the hole in front of the pin, and the tail");
        assert!(!og.is_allocated_addr(d0), "the hole in front of the pin is free");
        assert_eq!(largest, og.capacity() - (m2_new + pc_size(1) - og.base_ptr() as usize));
        assert_eq!(og.high_water(), m2_new + pc_size(1) - og.base_ptr() as usize);
        let s = og.sizing_stats();
        assert_eq!((s.pinned_compactions, s.compactions, s.pinned_compaction_pins_last), (1, 1, 1));
        // The hole in front of the pin is reusable, and the pin is untouched.
        assert_eq!(og.alloc(pc_size(4), 8), Some(d0));
        assert_eq!(pc_get_ref(p as usize, 0), m1_new);
    }

    /// gcd d4/n (`gcd-d3m-phase5-compacts-old-gen-under-unmapped-compiled-words`):
    /// the veto counter counts, returns the previous count, and is reported by
    /// `sizing_stats`.
    #[test]
    fn gcd_d4n_the_moving_major_veto_is_counted() {
        let mut og = OldGen::new(64 * 1024);
        assert_eq!(og.sizing_stats().moving_major_compaction_vetoes, 0);
        assert_eq!(og.note_moving_major_compaction_veto(), 0);
        assert_eq!(og.note_moving_major_compaction_veto(), 1);
        assert_eq!(og.sizing_stats().moving_major_compaction_vetoes, 2);
    }

    /// gcd d3/n (`gengc-r4w3-cards3-precise-array-cards-residuals`, item 3
    /// producer (d)). A surviving reference array with young elements reports
    /// each young element's FINAL address, one per card, on an element-precise
    /// table, and its final base (the header card) on a header-grained one; an
    /// ordinary object reports its final base either way. The young slots are
    /// left as they were.
    #[test]
    fn gcd_d3n_a_compacted_reference_array_reports_its_young_elements_cards() {
        // The stand-in young words below are raw process addresses; a narrow
        // geometry could not encode them.
        if cratonvm_types::narrow_oop::narrow_oops_enabled() {
            return;
        }
        let esz = ref_element_size();
        let card = crate::card_table::CARD_SIZE;
        let off = |i: usize| ARRAY_DATA_OFFSET + i * esz;
        let young_words = [0u64; 4];
        let y = young_words.as_ptr() as usize;
        const LEN: u32 = 1000;
        let young_elements = [3usize, 900, 901];
        for element_cards in [false, true] {
            let mut og = OldGen::new(64 * 1024);
            let _dead = pc_obj(&mut og, 4, false);
            let arr = og.alloc(off(LEN as usize), 8).expect("room");
            // SAFETY: a fresh, zeroed block sized for the array header and
            // `LEN` reference elements; the stores stay inside it.
            unsafe {
                std::ptr::write(
                    arr as *mut ObjectHeader,
                    ObjectHeader::new(
                        cratonvm_types::ClassId::new(1),
                        ObjectKind::Array,
                        ArrayElementType::Reference,
                        LEN,
                        LEN,
                    ),
                );
                (*(arr as *const ObjectHeader)).add_gc_flags(GC_FLAG_MARKED);
                for &i in &young_elements {
                    cratonvm_types::narrow_oop::write_ref_slot(arr.add(off(i)), (y + 8) as u64);
                }
            }
            let m = pc_obj(&mut og, 2, true);
            pc_set_ref(m, 1, y);

            let walk = og.walk_objects();
            let seq = og.free_list_seq();
            let card_base = og.base_ptr() as usize;
            let done = og
                .compact_around_pins_with_element_cards(
                    &walk,
                    seq,
                    Vec::new(),
                    (y, y + 32),
                    &HashMap::new(),
                    element_cards.then_some(card_base),
                )
                .expect("a healthy grid compacts");
            let arr_new = done.pointer_map[&(arr as usize)];
            let m_new = done.pointer_map[&(m as usize)];
            assert!(arr_new < arr as usize, "the array slid onto the dead object");
            let mut expected = Vec::new();
            if element_cards {
                let mut last = usize::MAX;
                for &i in &young_elements {
                    let c = (arr_new + off(i) - card_base) / card;
                    if c != last {
                        last = c;
                        expected.push(arr_new + off(i));
                    }
                }
                assert!(
                    !expected.contains(&arr_new) && expected.len() >= 2,
                    "element addresses, not the base",
                );
            } else {
                expected.push(arr_new);
            }
            expected.push(m_new);
            assert_eq!(done.young_ref_holders, expected, "element_cards={element_cards}");
            for &i in &young_elements {
                // SAFETY: a slot of the survivor at its final address.
                let v = unsafe { read_ref_slot(arr_new.wrapping_add(off(i)) as *const u8) };
                assert_eq!(v as usize, y + 8, "a young element is not touched");
            }
            assert_eq!(pc_get_ref(m_new, 1), y);
        }
    }

    /// The false-`OutOfMemoryError` shape at the `OldGen` level: the free
    /// bytes hold a request, no hole does, and one live object sits between
    /// the holes. With the object in front PINNED, the live array behind it
    /// slides down onto it and the request fits. The object-start table then
    /// finds both survivors where they are now.
    #[test]
    fn a_humongous_request_fits_after_a_pinned_compaction() {
        let mut og = OldGen::new(64 * 1024);
        let pin = pc_obj(&mut og, 1, true);
        let _a = pc_int_array(&mut og, 2048, false);
        let b = pc_int_array(&mut og, 2048, true);
        let _c = pc_int_array(&mut og, 2048, false);
        // SAFETY: element 0 of the live `int[2048]` `b`.
        unsafe { std::ptr::write(b.add(ARRAY_DATA_OFFSET) as *mut i32, 0x5EED_1234) };
        // The dead arrays go back to the free list, as the in-place sweep
        // leaves them: two holes and the tail, none big enough.
        let walk = og.walk_objects();
        for &(q, size) in &walk {
            // SAFETY: a walked object's header.
            let marked = unsafe { &*(q as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED != 0;
            if !marked {
                // SAFETY: `(q, size)` is exactly a walked allocation.
                unsafe { og.free(q, size) };
            }
        }
        let _ = og.coalesce_free_blocks();
        let (free, largest) = og.free_bytes_and_largest();
        let need = largest + 8;
        assert!(free >= need, "precondition: the bytes are there in total");
        assert!(og.fragmented_for(need), "precondition: no single hole fits");

        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walk, seq, vec![pin as usize], (0, 0), &HashMap::new())
            .expect("compacts");
        let b_new = done.pointer_map[&(b as usize)];
        assert_eq!(b_new, pin as usize + pc_size(1), "b slides onto the pin's end");
        assert_eq!(
            // SAFETY: element 0 of `b` at its new address.
            unsafe { std::ptr::read((b_new + ARRAY_DATA_OFFSET) as *const i32) },
            0x5EED_1234,
            "b's payload moved with it",
        );
        assert!(done.largest_free_after >= need);
        assert!(!og.fragmented_for(need));
        // Object starts are coherent: the anchored card walk finds each
        // survivor at its new address.
        let base = og.base_ptr() as usize;
        let b_off = b_new - base;
        let hits = og.walk_card_ranges(&[(0, 8), (b_off, b_off + 8)], false);
        let found: Vec<usize> = hits.iter().map(|h| h.ptr as usize).collect();
        assert_eq!(found, vec![pin as usize, b_new]);
        assert!(og.alloc(need, 8).is_some(), "the request fits now");
    }

    /// A refusal moves nothing: an escaping reference (a live object naming a
    /// FREED block) leaves the free list, the accounting and every address as
    /// they were, and the marks set for the in-place sweep to use.
    #[test]
    fn a_pinned_compaction_refuses_an_escape_and_changes_nothing() {
        let mut og = OldGen::new(4096);
        let a = pc_obj(&mut og, 1, true);
        let gone = pc_obj(&mut og, 1, false);
        let c = pc_obj(&mut og, 1, true);
        pc_set_ref(a, 0, gone as usize);
        // SAFETY: `gone` is exactly one allocation of this size.
        unsafe { og.free(gone, pc_size(1)) };
        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let used = og.used();
        let r = og.compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new());
        assert_eq!(r.err(), Some(PinnedCompactRefusal::Escape));
        assert_eq!(og.free_list_seq(), seq, "the free list is untouched");
        assert_eq!(og.used(), used);
        assert_eq!(og.walk_objects(), walk);
        // SAFETY: `c` is a live walked object.
        assert_ne!(unsafe { &*(c as *const ObjectHeader) }.gc_flags() & GC_FLAG_MARKED, 0);
        assert_eq!(og.sizing_stats().pinned_compactions, 0);
        // A stale grid is refused too.
        let (mut og2, _a2, b2, _c2) = marked_dead_marked();
        let stale = og2.walk_objects();
        let seq2 = og2.free_list_seq();
        // SAFETY: `b2` is exactly one allocation of this size.
        unsafe { og2.free(b2, HEADER_SIZE + SLOT_SIZE) };
        assert_eq!(
            og2.compact_around_pins(&stale, seq2, Vec::new(), (0, 0), &HashMap::new()).err(),
            Some(PinnedCompactRefusal::StaleGrid),
        );
    }

    /// With no pins the pinned compaction IS the sliding compaction: the same
    /// destinations and the same accounting as `compact`.
    #[test]
    fn with_no_pins_the_pinned_compaction_matches_compact() {
        let (mut og1, _a1, _b1, c1) = marked_dead_marked();
        let map1 = og1.compact();
        let (mut og2, _a2, _b2, c2) = marked_dead_marked();
        let walk = og2.walk_objects();
        let seq = og2.free_list_seq();
        let done = og2
            .compact_around_pins(&walk, seq, Vec::new(), (0, 0), &HashMap::new())
            .expect("compacts");
        let rel = |base: *const u8, q: usize| q - base as usize;
        assert_eq!(
            rel(og1.base_ptr(), map1[&(c1 as usize)]),
            rel(og2.base_ptr(), done.pointer_map[&(c2 as usize)]),
        );
        assert_eq!(og1.used(), og2.used());
        assert_eq!(og1.free_block_count(), og2.free_block_count());
        assert_eq!(og1.high_water(), og2.high_water());
    }

    /// gcd d9/a: the pinned compaction says which entries of its `watch` list
    /// it DROPPED (the true-root major's dead promotion destinations: after
    /// the slide a moved object may sit at a dropped address, so the caller
    /// cannot ask the free list). A live watched object is not reported, and
    /// without a list nothing is.
    #[test]
    fn gcd_d9a_a_pinned_compaction_reports_the_watched_objects_it_drops() {
        let (mut og, a, b, c) = marked_dead_marked();
        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let mut watch = vec![a as usize, b as usize, c as usize];
        watch.sort_unstable();
        let done = og
            .compact_around_pins_watching(
                &walk,
                seq,
                Vec::new(),
                (0, 0),
                &HashMap::new(),
                None,
                &watch,
            )
            .expect("compacts");
        assert_eq!(done.watch_dropped, vec![b as usize], "only the dead watched object");
        assert!(
            done.pointer_map.contains_key(&(c as usize)),
            "precondition: c slid onto the dropped b's address",
        );

        let (mut og2, _a2, _b2, _c2) = marked_dead_marked();
        let walk2 = og2.walk_objects();
        let seq2 = og2.free_list_seq();
        let done2 = og2
            .compact_around_pins(&walk2, seq2, Vec::new(), (0, 0), &HashMap::new())
            .expect("compacts");
        assert!(done2.watch_dropped.is_empty(), "no list, no report");
    }

    /// gcd d9/a: the true-root fallbacks are counted per reason, into the
    /// total, and every reason has its own slot and label.
    #[test]
    fn gcd_d9a_true_root_fallbacks_are_counted_per_reason() {
        let mut og = OldGen::new(64 * 1024);
        assert_eq!(og.note_true_root_fallback(TrueRootFallback::Promoted), 1);
        assert_eq!(og.note_true_root_fallback(TrueRootFallback::WalkGap), 2);
        assert_eq!(og.note_true_root_fallback(TrueRootFallback::Promoted), 3);
        let s = og.sizing_stats();
        assert_eq!(s.true_root_fallbacks, 3);
        assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::Promoted), 2);
        assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::WalkGap), 1);
        assert_eq!(s.true_root_fallbacks_for(TrueRootFallback::PinnedPlan), 0);
        og.note_true_root_promotions(5, 2);
        let s = og.sizing_stats();
        assert_eq!((s.true_root_promotions, s.true_root_promotions_dead), (5, 2));
        assert_eq!(s.true_root_young_freed_bytes, 0, "step 1 was removed (gce e2/c)");
        let mut labels: Vec<&str> = TrueRootFallback::ALL.iter().map(|r| r.label()).collect();
        let idx: Vec<usize> = TrueRootFallback::ALL.iter().map(|r| r.index()).collect();
        assert_eq!(idx, (0..TrueRootFallback::COUNT).collect::<Vec<_>>(), "slots in order");
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), TrueRootFallback::COUNT, "labels are unique");
    }

    // --- gen r4w6/oldpin6: compaction edges ---------------------------------

    fn pc_int_at(arr: usize, i: usize) -> i32 {
        // SAFETY: callers pass a live `int[]` base and an in-bounds index.
        unsafe { std::ptr::read((arr + ARRAY_DATA_OFFSET + 4 * i) as *const i32) }
    }

    fn pc_set_int(arr: *mut u8, i: usize, v: i32) {
        // SAFETY: callers pass a live `int[]` base and an in-bounds index.
        unsafe { std::ptr::write(arr.add(ARRAY_DATA_OFFSET + 4 * i) as *mut i32, v) };
    }

    /// A COMPLETELY full generation whose last object — pinned — ends exactly
    /// at the end of the reservation: the pin keeps its address and payload,
    /// the live objects below it slide to the bottom, the dead bytes become
    /// ONE gap in front of the pin, there is no tail block (the cursor ends at
    /// the reservation's end, so nothing is zeroed or given back past it), and
    /// the gap is allocatable.
    #[test]
    fn a_pinned_object_at_the_very_end_of_a_full_generation_stays_put() {
        let mut og = OldGen::new(64 * 1024);
        let cap = og.capacity();
        let base = og.base_ptr() as usize;
        let mut arrays = Vec::new();
        for i in 0..6 {
            let a = pc_int_array(&mut og, 1024, i % 2 == 1);
            pc_set_int(a, 0, 100 + i);
            arrays.push(a);
        }
        let size = ARRAY_DATA_OFFSET + 1024 * 4;
        let rem = cap - og.used();
        assert!(rem > ARRAY_DATA_OFFSET && (rem - ARRAY_DATA_OFFSET) % 8 == 0, "rem {rem}");
        let last_len = u32::try_from((rem - ARRAY_DATA_OFFSET) / 4).expect("small");
        let last = pc_int_array(&mut og, last_len, true);
        pc_set_int(last, 0, 0x1A57);
        pc_set_int(last, last_len as usize - 1, 0x0E7D);
        assert_eq!(og.used(), cap, "precondition: completely full");
        assert_eq!(last as usize + rem, base + cap, "precondition: the pin ends the reservation");

        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walk, seq, vec![last as usize], (0, 0), &HashMap::new())
            .expect("a full generation compacts around its last object");
        assert_eq!((done.pinned, done.moved), (1, 3));
        assert!(!done.pointer_map.contains_key(&(last as usize)), "the pin does not move");
        for (k, &i) in [1usize, 3, 5].iter().enumerate() {
            let new = done.pointer_map[&(arrays[i] as usize)];
            assert_eq!(new, base + k * size, "live array {i} slides to slot {k}");
            assert_eq!(pc_int_at(new, 0), 100 + i as i32, "its payload moved with it");
        }
        assert_eq!(pc_int_at(last as usize, 0), 0x1A57, "the pin's payload is untouched");
        assert_eq!(pc_int_at(last as usize, last_len as usize - 1), 0x0E7D);
        // gen r4w6 (review6 finding 4): arrays 3 and 5 moved out of what is
        // now the gap in front of the pin; their old images read zero, header
        // and payload, not a forwarded header and the old ints.
        for i in [3usize, 5] {
            let old = arrays[i] as usize;
            // SAFETY: `old` is inside the committed gap `[base + 3 * size,
            // last)`; the first word is the old header's mark word.
            let mark = unsafe { std::ptr::read(old as *const u64) };
            assert_eq!(mark, 0, "array {i}'s old header is zeroed");
            assert_eq!(pc_int_at(old, 0), 0, "array {i}'s old payload is zeroed");
        }
        assert_eq!(og.used(), 3 * size + rem);
        assert_eq!(og.free_block_count(), 1, "one gap in front of the pin, no tail block");
        assert_eq!(og.largest_free_block(), 3 * size);
        assert_eq!(og.high_water(), cap, "the last survivor ends at the reservation's end");
        assert_eq!(og.alloc(3 * size, 8), Some((base + 3 * size) as *mut u8), "the gap is reusable");
        assert_eq!(og.used(), cap, "full again");
    }

    /// A pinned HUMONGOUS object (a 64 KiB `int[]` in a 256 KiB generation,
    /// dead objects on both sides): it keeps its address and every byte of its
    /// payload; the survivor below it slides to the bottom, the survivors
    /// above it slide down onto its end, and the free list is the gap in front
    /// of it plus the tail.
    #[test]
    fn a_pinned_humongous_object_keeps_its_address_and_its_neighbours_slide() {
        let mut og = OldGen::new(256 * 1024);
        let base = og.base_ptr() as usize;
        let _d0 = pc_obj(&mut og, 4, false);
        let m0 = pc_obj(&mut og, 2, true);
        let _d1 = pc_int_array(&mut og, 1024, false);
        let hum_len = 16 * 1024u32;
        let hum = pc_int_array(&mut og, hum_len, true);
        pc_set_int(hum, 0, 0x4855);
        pc_set_int(hum, hum_len as usize - 1, 0x4D47);
        let _d2 = pc_int_array(&mut og, 512, false);
        let m1 = pc_obj(&mut og, 1, true);
        let m2 = pc_int_array(&mut og, 256, true);
        pc_set_int(m2, 255, 0x0202);
        pc_set_ref(m1, 0, hum as usize);
        pc_set_ref(m0, 0, m2 as usize);
        let hum_size = ARRAY_DATA_OFFSET + hum_len as usize * 4;

        let walk = og.walk_objects();
        let seq = og.free_list_seq();
        let done = og
            .compact_around_pins(&walk, seq, vec![hum as usize], (0, 0), &HashMap::new())
            .expect("compacts around a humongous pin");
        assert_eq!((done.pinned, done.moved), (1, 3));
        assert!(!done.pointer_map.contains_key(&(hum as usize)));
        let m0_new = done.pointer_map[&(m0 as usize)];
        let m1_new = done.pointer_map[&(m1 as usize)];
        let m2_new = done.pointer_map[&(m2 as usize)];
        assert_eq!(m0_new, base, "the survivor below the pin slides to the bottom");
        assert_eq!(m1_new, hum as usize + hum_size, "the survivors above land on the pin's end");
        assert_eq!(m2_new, m1_new + pc_size(1));
        assert_eq!(pc_int_at(hum as usize, 0), 0x4855, "the humongous payload is untouched");
        assert_eq!(pc_int_at(hum as usize, hum_len as usize - 1), 0x4D47);
        assert_eq!(pc_int_at(m2_new, 255), 0x0202);
        assert_eq!(pc_get_ref(m1_new, 0), hum as usize, "a reference to the pin is unchanged");
        assert_eq!(pc_get_ref(m0_new, 0), m2_new, "a reference to a moved object is forwarded");
        assert_eq!(og.free_block_count(), 2, "the gap in front of the pin, and the tail");
        let (free, _) = og.free_bytes_and_largest();
        assert_eq!(free + og.used(), og.capacity());
        assert_eq!(og.high_water(), m2_new + ARRAY_DATA_OFFSET + 256 * 4 - base);
        let walked: Vec<usize> = og.walk_objects().iter().map(|&(q, _)| q as usize).collect();
        assert_eq!(walked, vec![m0_new, hum as usize, m1_new, m2_new]);
    }

    /// gen r5w1/oldgen5 — a refused humongous request arms ONE compaction
    /// request whatever the free list holds right now (the collection it
    /// provokes is what frees the dead neighbours), a request larger than the
    /// generation arms nothing, and taking the request clears the humongous
    /// mark with it; a compaction answers both.
    #[test]
    fn a_humongous_refusal_arms_a_compaction_request_until_it_is_taken() {
        let mut og = OldGen::new(64 * 1024);
        assert!(!og.humongous_refusal_pending());
        assert!(!og.note_humongous_refusal(64 * 1024 + 8), "larger than the generation");
        assert!(!og.take_fragmentation_compaction_request());
        // A FULL generation: `note_refused_request` records no fragmentation,
        // the humongous refusal still arms.
        let (mut full, _) = old_gen_with_objects(1024, 3);
        assert!(full.alloc(32 * 1024, 8).is_none());
        assert_eq!(full.fragmentation_pending_request(), 0);
        assert!(full.note_humongous_refusal(32 * 1024));
        assert!(full.humongous_refusal_pending());
        let s = full.sizing_stats();
        assert_eq!((s.humongous_compaction_requests, s.fragmentation_compactions_requested), (1, 1));
        assert!(full.take_fragmentation_compaction_request());
        assert!(!full.humongous_refusal_pending(), "taken with the request");
        assert!(!full.take_fragmentation_compaction_request(), "once");
        // A completed compaction answers a pending one.
        assert!(og.note_humongous_refusal(1024));
        let _ = og.compact();
        assert!(!og.humongous_refusal_pending());
        assert!(!og.take_fragmentation_compaction_request());
    }

    /// gen r5w2/alloc6 — the refused request's size rides the request: the
    /// largest of several refusals, 0 once the request is taken or answered,
    /// and 0 while no humongous refusal is pending (a fragmentation request
    /// armed another way does not report one).
    #[test]
    fn the_refused_humongous_size_rides_the_request_and_leaves_with_it() {
        let mut og = OldGen::new(64 * 1024);
        assert_eq!(og.humongous_refused_request(), 0);
        og.request_fragmentation_compaction();
        assert_eq!(og.humongous_refused_request(), 0, "not a humongous request");
        assert!(og.take_fragmentation_compaction_request());
        assert!(og.note_humongous_refusal(8 * 1024));
        assert!(og.note_humongous_refusal(24 * 1024));
        assert!(og.note_humongous_refusal(16 * 1024));
        assert_eq!(og.humongous_refused_request(), 24 * 1024, "the largest refusal");
        assert!(og.take_fragmentation_compaction_request());
        assert_eq!(og.humongous_refused_request(), 0, "taken with the request");
        assert!(og.note_humongous_refusal(4 * 1024));
        let _ = og.compact();
        assert_eq!(og.humongous_refused_request(), 0, "answered by a compaction");
        // A new refusal after the answer starts from its own size.
        assert!(og.note_humongous_refusal(2 * 1024));
        assert_eq!(og.humongous_refused_request(), 2 * 1024);
    }
}
