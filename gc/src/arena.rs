// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Linear arena allocator for the semi-space copying GC.
//!
//! Each arena is a contiguous block of memory (`Vec<u8>`) with a cursor
//! that advances on each allocation. Objects cannot be individually freed;
//! instead, the entire arena is reset when GC swaps the semi-spaces.

/// A reclaimed (swept) region inside the arena, available for reuse by
/// the non-moving young-gen mark-sweep collector.
///
/// Free blocks are produced only by [`Arena::add_free_block`], which the
/// non-moving sweep calls for every dead object it reclaims. The bump
/// `cursor` continues to govern the high-water mark; free blocks let the
/// allocator satisfy requests from holes *below* the cursor without
/// relocating any survivor (which is what makes the collection
/// JIT-frame-safe — see `gen_heap::sweep_young_non_moving`).
use crate::gc_flags;
#[derive(Debug, Clone, Copy)]
pub struct FreeBlock {
    /// Byte offset from the start of the backing buffer.
    pub offset: usize,
    /// Size of the free region in bytes.
    pub size: usize,
}

/// A linear (bump-pointer) arena allocator with an optional free list.
///
/// Allocates primarily via a monotonically advancing `cursor`. In
/// addition, the non-moving young-gen collector may hand reclaimed
/// regions back via [`Arena::add_free_block`]; subsequent allocations
/// prefer those holes (first sufficiently-large block) before bumping
/// the cursor. A full-arena [`Arena::reset`] clears both the cursor and
/// the free list.
/// Tier boundary for the segregated free lists: blocks smaller than this go
/// on `free_small`, the rest on `free_large`. Motivation (binarytrees-18
/// profile, 2026-07-06): a single first-fit free list mixes ~100k node-sized
/// holes with a handful of big coalesced spans, so TLAB-refill-sized requests
/// scanned the entire small-hole prefix before reaching a span (70% of wall
/// clock split between `Arena::alloc` and the young allocation probe). With
/// the split, object-sized requests first-fit the small list at ~index 0
/// (holes are node-sized) and chunk-sized requests consult ONLY the short
/// span list. 4 KiB sits far above any normal object (72 B nodes) and far
/// below the minimum TLAB refill (16 KiB+).
///
/// The boundary now also separates two DIFFERENT structures, not just two
/// lists: below it, exact 8-byte size classes plus an occupancy bitmap (see
/// [`Arena::free_small`]); at or above it, a short first-fit span list.
const LARGE_BLOCK_MIN: usize = 4096;

/// `log2` of the small tier's size-class granularity. The whole heap runs on
/// an 8-byte object grid (`Arena::alloc` rounds every request up to 8, the
/// sweep only ever publishes 8-aligned spans), so one class per 8 bytes gives
/// the small tier EXACT size classes with no rounding loss.
const SMALL_GRAIN_SHIFT: u32 = 3;

/// Small-tier size-class granularity (8 bytes).
const SMALL_GRAIN: usize = 1 << SMALL_GRAIN_SHIFT;

/// Number of small-tier size classes: every size below [`LARGE_BLOCK_MIN`],
/// one class per [`SMALL_GRAIN`] bytes (512 classes).
const SMALL_BUCKETS: usize = LARGE_BLOCK_MIN >> SMALL_GRAIN_SHIFT;

/// `u64` words in the small-tier occupancy bitmap (512 buckets → 8 words).
const SMALL_MASK_WORDS: usize = SMALL_BUCKETS / 64;

/// Size class for a small block: bucket `k` holds every block whose size is
/// in `[k * SMALL_GRAIN, (k + 1) * SMALL_GRAIN)`. Indexing by the FLOOR means
/// every block in bucket `k` is guaranteed to be at least `k * SMALL_GRAIN`
/// bytes — that guarantee is what makes the escalating search in
/// [`Arena::small_fit`] sound without re-reading block sizes.
#[inline]
fn small_bucket_for(size: usize) -> usize {
    (size >> SMALL_GRAIN_SHIFT).min(SMALL_BUCKETS - 1)
}

#[inline]
fn mask_set(mask: &mut [u64; SMALL_MASK_WORDS], k: usize) {
    mask[k >> 6] |= 1u64 << (k & 63);
}

#[inline]
fn mask_clear(mask: &mut [u64; SMALL_MASK_WORDS], k: usize) {
    mask[k >> 6] &= !(1u64 << (k & 63));
}

/// Lowest occupied bucket index `>= from`, or `None`.
#[inline]
fn mask_first_from(mask: &[u64; SMALL_MASK_WORDS], from: usize) -> Option<usize> {
    if from >= SMALL_BUCKETS {
        return None;
    }
    let mut w = from >> 6;
    // `from & 63` is in 0..=63, so the shift never overflows.
    let mut word = mask[w] & (!0u64 << (from & 63));
    loop {
        if word != 0 {
            return Some((w << 6) | word.trailing_zeros() as usize);
        }
        w += 1;
        if w >= SMALL_MASK_WORDS {
            return None;
        }
        word = mask[w];
    }
}

/// Highest occupied bucket index, or `None` when the small tier is empty.
#[inline]
fn mask_last(mask: &[u64; SMALL_MASK_WORDS]) -> Option<usize> {
    for w in (0..SMALL_MASK_WORDS).rev() {
        if mask[w] != 0 {
            return Some((w << 6) | (63 - mask[w].leading_zeros() as usize));
        }
    }
    None
}

pub struct Arena {
    /// Backing storage. Pre-allocated to `capacity` bytes.
    data: Vec<u8>,
    /// Next free byte offset within `data` (bump-allocation high-water mark).
    cursor: usize,
    /// Reclaimed regions below `cursor` SMALLER than [`LARGE_BLOCK_MIN`],
    /// produced by the non-moving sweep (typically object-sized holes),
    /// **segregated by exact 8-byte size class** ([`small_bucket_for`]).
    /// Empty unless a JIT-frame-safe mark-sweep has run.
    ///
    /// WHY SEGREGATED (perf/gc-allocation-fastpath, 2026-07-26). This tier was
    /// a single flat `Vec` walked first-fit with a 16-block scan budget. Under
    /// the non-moving sweep — the collector that actually runs in steady state
    /// — `swap_remove` back-fills the scan prefix with freshly split "dust",
    /// so a same-shaped request stopped first-fitting at index 0 and the
    /// bounded scan started missing satisfiable blocks; the misses fell
    /// through to the bump tail (gone, once the cursor pins at capacity) and
    /// then to a full unbounded rescue scan. Worse, `largest_free_block` —
    /// consulted by `gen_heap::refill_tlab`'s fragmentation fallback on every
    /// mini-TLAB refill, i.e. once per ~85 objects in the degraded mode — was
    /// an O(free-list) walk of exactly this list (measured live: 33 MB of
    /// uniform 4080-byte remnants, ~8500 blocks, rescanned per refill).
    ///
    /// One bucket per 8-byte size class removes all three at once: a request
    /// lands on its own class (exact fit, NO remainder, so the allocator stops
    /// manufacturing dust), a miss escalates through a 512-bit occupancy
    /// bitmap instead of a linear walk, and the largest-block query is the
    /// bitmap's high bit.
    free_small: Vec<Vec<FreeBlock>>,
    /// Occupancy bitmap over [`Self::free_small`]: bit `k` set iff bucket `k`
    /// is non-empty. Kept in lockstep at every push/pop so the "smallest
    /// class that can serve this request" and "largest available block"
    /// queries are a handful of word operations rather than a list walk.
    small_mask: [u64; SMALL_MASK_WORDS],
    /// Reclaimed regions of at least [`LARGE_BLOCK_MIN`] bytes — the
    /// coalesced spans TLAB refills and array allocations carve from. Stays
    /// short (the post-sweep coalescer merges adjacent holes into a handful
    /// of spans), so scanning it is cheap.
    free_large: Vec<FreeBlock>,
    /// Conservative UPPER BOUND on the size of the largest free-list block
    /// (`actual_max <= max_free_upper` always). Maintained so hot callers can
    /// answer "no block of >= size exists" in O(1) instead of scanning the
    /// whole free list — the young-gen allocation probe runs once per JIT
    /// slow-path allocation and its former full `largest_free_block` scan was
    /// ~7% of a binarytrees-18 run (and 55% when probed with TLAB-refill
    /// sizes).
    ///
    /// Soundness of the bound:
    /// * [`Self::add_free_block`] raises it to at least the new block's size.
    /// * [`Self::alloc`] only shrinks/splits/removes blocks (the split
    ///   remainders it pushes are strictly smaller than the consumed block),
    ///   so the true max never grows there and the bound stays valid.
    /// * Failed full scans ([`Self::has_free_block_at_least`],
    ///   [`Self::largest_free_block_tightening`]) tighten it to the exact max
    ///   observed, so repeated "no" answers become O(1).
    /// * [`Self::clear_free_list`] / [`Self::reset`] / [`Self::reset_no_zero`]
    ///   zero it alongside the list.
    max_free_upper: usize,
    /// Memoised EXACT maximum over [`Self::free_large`], or `None` when it
    /// must be recomputed. Unlike [`Self::max_free_upper`] (a bound that may
    /// over-estimate) this is the precise value [`Self::largest_free_block`]
    /// owes its callers — `gen_heap::refill_tlab` sizes a mini-TLAB from it
    /// and then *allocates* that many bytes, so an over-estimate turns into a
    /// failed refill and an allocation wedge.
    ///
    /// Maintained rather than recomputed because the span tier is the only
    /// part of the free list a scan still has to walk: a push can only raise
    /// the maximum (folded in on the spot), and only consuming a block can
    /// lower it (invalidates). `Cell` because the query takes `&self` —
    /// `Arena` always lives inside a `Mutex`, which needs `Send`, not `Sync`.
    large_max_exact: std::cell::Cell<Option<usize>>,
    /// Running total of bytes currently held across both free-list tiers,
    /// maintained incrementally at every mutation site (`push_block_routed`
    /// adds a pushed block's size; `small_fit`/`large_fit` subtract the
    /// whole consumed block's size before its remainder is re-routed;
    /// `clear_free_list` / `reset` / `reset_no_zero` zero it alongside the
    /// list). This replaced an epoch-gated cache (2026-07-15) that was only
    /// O(1) between calls with no free-list mutation in between — under
    /// steady allocation churn (an object-heavy workload where the young
    /// sweep keeps reclaiming into the list while `alloc` keeps consuming
    /// from it) the list content changes on nearly every call, so the old
    /// cache degraded back to O(free-list-size) per call. Maintaining the
    /// sum incrementally instead of gating a from-scratch recompute makes
    /// `free_list_bytes()` unconditionally O(1), including under churn.
    free_bytes_total: usize,
    /// perf/gc-oracle-anchors (2026-07-25): allocator-recorded object starts,
    /// one per `1 << anchor_shift`-byte bucket of this arena. `usize::MAX`
    /// means "nothing recorded in this bucket during the current epoch".
    ///
    /// WHY the allocator and not a GC-time walk. Both consumers of an "object
    /// grid split point" — the parallel sweep's chunk anchors and the mark
    /// oracle's exact-base lookup for CONSERVATIVE candidates — used to get
    /// their split points from one sequential linear header chase over the
    /// whole young arena (`report_phase("mark-oracle-walk")`), which on a
    /// 2 GiB from-space measured 233–368 ms, the single largest young-GC
    /// phase left after the sweep went parallel. But the allocator already
    /// KNOWS every boundary it hands out; rediscovering them by chasing
    /// `gen_object_total_size` over 36M objects is redundant work. Recording
    /// them here costs a shift, a bounds-checked load, a compare and a
    /// per-bucket-once store on the arena slow path (TLAB refill + non-TLAB
    /// young object), all of which already hold this arena's lock.
    ///
    /// COVERAGE IS A HINT, NEVER A CORRECTNESS INPUT. The JIT's inline TLAB
    /// fast path bumps a pointer without ever entering this file, so the
    /// objects inside a TLAB are not individually recorded — only the TLAB's
    /// start is (a TLAB is 256 KiB–1 MiB, see `tlab::DEFAULT_TLAB_SIZE`), and
    /// regions holding survivors of an earlier collection are not re-recorded
    /// at all. That is fine by construction: every consumer re-proves an
    /// anchor by chaining to the next one, so a MISSING anchor only means a
    /// longer chunk / a longer local walk, and a WRONG one only means the
    /// chain fails to land and the caller falls back to its sequential path.
    alloc_anchors: Vec<usize>,
    /// `log2` of the anchor bucket width. See [`Arena::rearm_alloc_anchors`].
    anchor_shift: u32,
}

/// Alignment tripwire (perf/halfgap residuals, 2026-07-18): every free-list
/// block must sit on the 8-aligned object grid — the non-moving sweep's walk
/// and the mark oracle both assume it. An unaligned block is upstream-bug
/// evidence (an alloc with an unrounded size minted an unaligned split
/// remainder, or a walk desync published an off-grid span); it later derails
/// the linear walk ("cursor overshot into free block" at +4 offsets). Warn
/// loudly (bounded) at the birth site so the producer is identifiable.
static FL_ALIGN_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cold]
fn warn_unaligned_block(site: &str, offset: usize, size: usize) {
    use std::sync::atomic::Ordering;
    let n = FL_ALIGN_HITS.fetch_add(1, Ordering::Relaxed);
    if n < 16 {
        tracing::warn!(
            "free-list ALIGNMENT tripwire [{site}]: unaligned block off={offset}              (mod8={}) size={size} (mod8={}) — walk-grid hazard",
            offset & 7,
            size & 7,
        );
    }
}

impl Arena {
    /// Create a new arena with the given capacity in bytes.
    pub fn new(capacity: usize) -> Self {
        // Alignment invariant (perf/halfgap residuals, 2026-07-18): the
        // capacity must be a multiple of 8 — the heap-ergonomics young size
        // can arrive unaligned (observed live: 1 GiB - 4), and an unaligned
        // bump tail makes `remaining()` carry a permanent mod-8 dreg that
        // `refill_tlab`'s `requested.min(available)` then mints into
        // unaligned TLAB sizes and +4 free-list split remnants — the
        // trigger-ON bt18 walk-grid corruption. Rounding down loses at most
        // 7 bytes of arena.
        let capacity = capacity & !7;
        // We need the Vec to have length == capacity so we can
        // hand out pointers into it. We zero-initialize for safety.
        let data = vec![0u8; capacity];
        let mut a = Self {
            data,
            cursor: 0,
            free_small: (0..SMALL_BUCKETS).map(|_| Vec::new()).collect(),
            small_mask: [0u64; SMALL_MASK_WORDS],
            free_large: Vec::new(),
            max_free_upper: 0,
            large_max_exact: std::cell::Cell::new(Some(0)),
            free_bytes_total: 0,
            alloc_anchors: Vec::new(),
            anchor_shift: 0,
        };
        a.rearm_alloc_anchors();
        a
    }

    /// (Re)size the allocator-anchor bucket table for the current capacity.
    ///
    /// Called from [`Arena::new`] and [`Arena::grow`] — the only two places
    /// `data.len()` changes. Armed on EVERY arena rather than only the young
    /// from-space so that the moving collector's `mem::swap` of from/to (and
    /// the `Arena::new(0)` + `grow` reuse path in the major collector) cannot
    /// hand out an un-armed arena; recording only happens where a caller
    /// explicitly calls [`Arena::note_object_start`], so an unused table costs
    /// nothing but its (bounded) allocation.
    fn rearm_alloc_anchors(&mut self) {
        const MAX_BUCKETS: usize = 1 << 16;
        let cap = self.data.len().max(1);
        // Bucket width. Deliberately NOT `sweep_anchor_stride()`: this table
        // feeds the mark oracle's per-candidate local walk as well as the
        // sweep's chunk split, and those want opposite things — the oracle
        // wants the FINEST grid it can get (its walk is `O(bucket width)` per
        // candidate cluster), the sweep wants ~one chunk per few MiB. So
        // record fine here and let `sweep_young_non_moving` subsample down to
        // the sweep stride. It also keeps the GC-stress knob
        // `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` (legal down to 64 bytes) from
        // sizing this table: at 64 bytes a 2 GiB arena would want 256 MB of
        // buckets.
        //
        // 4 KiB is already finer than the smallest TLAB (`MIN_TLAB_SIZE`,
        // 8 KiB), so the recording rate — not the bucket width — is what
        // actually bounds anchor density; widening only kicks in to cap the
        // table at MAX_BUCKETS (512 KiB of table on a 2 GiB from-space).
        let mut shift = 12u32;
        while (cap >> shift) >= MAX_BUCKETS {
            shift += 1;
        }
        self.anchor_shift = shift;
        self.alloc_anchors = vec![usize::MAX; (cap >> shift) + 1];
    }

    /// Record `offset` as a VERIFIED object start (the allocator just handed
    /// this exact offset out as the base of an object or of a TLAB, whose
    /// first byte is an object base too — a TLAB is tail-filled at retire, so
    /// it is object-covered end to end).
    ///
    /// One store per bucket per epoch; every later offset in the same bucket
    /// is a load + compare + not-taken branch.
    #[inline]
    pub fn note_object_start(&mut self, offset: usize) {
        if let Some(slot) = self.alloc_anchors.get_mut(offset >> self.anchor_shift) {
            if *slot == usize::MAX {
                *slot = offset;
            }
        }
    }

    /// Drain the epoch's anchors as a strictly-increasing offset list, leaving
    /// the table empty for the next epoch.
    ///
    /// STRICTLY INCREASING BY CONSTRUCTION: bucket `i` only ever stores an
    /// offset in `[i << shift, (i+1) << shift)`, so iterating buckets in order
    /// yields offsets in order with no duplicates. Consumers rely on that (a
    /// non-monotonic anchor list would make the sweep's chunk split invalid).
    ///
    /// DRAINING IS MANDATORY, NOT AN OPTIMISATION: an anchor describes the
    /// object grid of the epoch that just ended. The collection about to run
    /// reclaims dead spans into the free list, so an offset kept across it can
    /// end up INSIDE a free block, where a chunk walk resyncs past its own
    /// upper bound and the whole parallel attempt aborts. Cheap to be wrong,
    /// but pointlessly so.
    pub fn take_alloc_anchors(&mut self) -> Vec<usize> {
        let mut out = Vec::new();
        for slot in self.alloc_anchors.iter_mut() {
            let v = std::mem::replace(slot, usize::MAX);
            if v != usize::MAX {
                out.push(v);
            }
        }
        out
    }

    /// Forget every recorded anchor without collecting them.
    pub fn clear_alloc_anchors(&mut self) {
        self.alloc_anchors.fill(usize::MAX);
    }

    /// Route a block to its size tier. Does NOT touch `max_free_upper` — the
    /// callers that can GROW the true maximum ([`Self::add_free_block`]) bump
    /// it themselves; split remainders are strictly smaller than the block
    /// they came from, so routing them leaves the bound valid.
    #[inline]
    fn push_block_routed(&mut self, block: FreeBlock) {
        if block.size == 0 {
            return;
        }
        if (block.offset | block.size) & 7 != 0 {
            warn_unaligned_block("route", block.offset, block.size);
        }
        if block.size < LARGE_BLOCK_MIN {
            let k = small_bucket_for(block.size);
            self.free_small[k].push(block);
            mask_set(&mut self.small_mask, k);
        } else {
            self.free_large.push(block);
            // A push can only RAISE the span-tier maximum; fold it in so the
            // memo survives (invalidating here would make every sweep's
            // reclaim cost the next `largest_free_block` a full rescan).
            if let Some(m) = self.large_max_exact.get() {
                self.large_max_exact.set(Some(m.max(block.size)));
            }
        }
        self.free_bytes_total += block.size;
    }

    /// True when both tiers are empty.
    #[inline]
    fn free_is_empty(&self) -> bool {
        self.free_large.is_empty() && self.small_mask.iter().all(|&w| w == 0)
    }

    /// GUARANTEED-available largest small block: every block in the highest
    /// occupied bucket `k` is at least `k * SMALL_GRAIN` bytes, so this value
    /// is always actually allocatable. Used where an over-estimate would turn
    /// into a failed allocation (see [`Self::largest_free_block`]).
    #[inline]
    fn small_max_floor(&self) -> usize {
        mask_last(&self.small_mask).map_or(0, |k| k * SMALL_GRAIN)
    }

    /// UPPER bound on the largest small block: bucket `k` caps at
    /// `k * SMALL_GRAIN + SMALL_GRAIN - 1`. Used where soundness requires the
    /// bound never to under-state the truth ([`Self::max_free_upper`]).
    ///
    /// Floor and ceiling coincide for every block the heap actually produces
    /// (all spans sit on the 8-byte object grid; the `warn_unaligned_block`
    /// tripwire fires at the producer otherwise), so the 7-byte spread only
    /// exists to keep both directions sound under a grid violation.
    #[inline]
    fn small_max_ceil(&self) -> usize {
        mask_last(&self.small_mask).map_or(0, |k| k * SMALL_GRAIN + (SMALL_GRAIN - 1))
    }

    /// Carve `size` bytes out of `block` at `padding`, yielding the allocation
    /// offset plus the head-padding and tail remainders for re-routing.
    #[inline]
    fn split(block: FreeBlock, padding: usize, size: usize) -> (usize, [Option<FreeBlock>; 2]) {
        let alloc_offset = block.offset + padding;
        let remaining = block.size - padding - size;
        let head = (padding > 0).then_some(FreeBlock {
            offset: block.offset,
            size: padding,
        });
        let tail = (remaining > 0).then_some(FreeBlock {
            offset: alloc_offset + size,
            size: remaining,
        });
        (alloc_offset, [head, tail])
    }

    /// Serve `size` bytes at `align` from the segregated small tier.
    ///
    /// Two steps, both bounded:
    ///
    /// 1. **Exact size class.** Bucket `size / SMALL_GRAIN` holds blocks of
    ///    at least `size` bytes, so a zero-padding block there is an exact
    ///    fit that leaves NO remainder — the steady state for a workload that
    ///    keeps recycling one node shape, and the reason this tier stops
    ///    generating dust. Padding is still checked per block (the free list
    ///    is keyed by size, not by alignment), but padding is zero for every
    ///    block on the 8-byte grid, so the loop exits at index 0.
    /// 2. **Escalate.** Otherwise take the first block of the lowest occupied
    ///    class that is provably big enough — `ceil((size + align - 1) /
    ///    SMALL_GRAIN)`, and never the class already scanned in step 1. Every
    ///    block in that class covers `size` plus the worst-case alignment
    ///    padding, so the head of the bucket is taken without a scan.
    ///
    /// `size` must already be rounded up to a multiple of [`SMALL_GRAIN`] —
    /// [`Self::alloc`] does that for every request before calling in. Without
    /// it the floor-indexed class in step 1 could sit below the class holding
    /// an exactly-sized block.
    fn small_fit(
        &mut self,
        base: usize,
        size: usize,
        align: usize,
    ) -> Option<(usize, [Option<FreeBlock>; 2])> {
        debug_assert_eq!(
            size % SMALL_GRAIN,
            0,
            "small_fit requires a grid-rounded size (got {size})",
        );
        let exact = size >> SMALL_GRAIN_SHIFT;
        if exact < SMALL_BUCKETS {
            let list = &mut self.free_small[exact];
            for i in 0..list.len() {
                let block = list[i];
                let block_addr = base + block.offset;
                let aligned_addr = (block_addr + align - 1) & !(align - 1);
                let padding = aligned_addr - block_addr;
                // Overflow means this block can't satisfy the request; skip
                // it rather than aborting the whole search.
                let Some(total_needed) = padding.checked_add(size) else {
                    continue;
                };
                if total_needed <= block.size {
                    list.swap_remove(i);
                    if list.is_empty() {
                        mask_clear(&mut self.small_mask, exact);
                    }
                    self.free_bytes_total -= block.size;
                    return Some(Self::split(block, padding, size));
                }
            }
        }
        let from = if align <= SMALL_GRAIN {
            // Padding is at most `align - 1 <= SMALL_GRAIN - 1`, and every
            // block in bucket `exact + 1` is at least
            // `(exact + 1) * SMALL_GRAIN == size + SMALL_GRAIN` bytes — so
            // the very next occupied class always covers size + padding, and
            // no satisfiable class is skipped.
            exact.saturating_add(1)
        } else {
            // Alignments wider than the grid need a conservative start (no
            // production caller uses one; the arena is an 8-aligned world).
            let need = size.checked_add(align - 1)?;
            need.div_ceil(SMALL_GRAIN).max(exact.saturating_add(1))
        };
        let k = mask_first_from(&self.small_mask, from)?;
        let list = &mut self.free_small[k];
        let block = list.swap_remove(0);
        if list.is_empty() {
            mask_clear(&mut self.small_mask, k);
        }
        self.free_bytes_total -= block.size;
        let block_addr = base + block.offset;
        let aligned_addr = (block_addr + align - 1) & !(align - 1);
        let padding = aligned_addr - block_addr;
        debug_assert!(
            padding + size <= block.size,
            "escalated small-tier bucket must cover size + worst-case padding",
        );
        Some(Self::split(block, padding, size))
    }

    /// First-fit scan of the span tier. On a fit: removes the block (O(1)
    /// `swap_remove`), returns the aligned allocation offset plus up to two
    /// remainder blocks (head alignment padding, tail leftover) for the
    /// caller to re-route by size.
    ///
    /// Unbounded on purpose: this tier stays short (the post-sweep coalescer
    /// merges adjacent holes into a handful of spans) and, unlike the small
    /// tier, a miss here has to be authoritative — [`Self::alloc`] and
    /// [`Self::has_free_block_at_least`] tighten [`Self::max_free_upper`]
    /// from it. The O(1) `max_free_upper` fail-fast keeps hopeless requests
    /// from reaching the loop at all.
    fn large_fit(
        &mut self,
        base: usize,
        size: usize,
        align: usize,
    ) -> Option<(usize, [Option<FreeBlock>; 2])> {
        for i in 0..self.free_large.len() {
            let block = self.free_large[i];
            let block_addr = base + block.offset;
            let aligned_addr = (block_addr + align - 1) & !(align - 1);
            let padding = aligned_addr - block_addr;
            let Some(total_needed) = padding.checked_add(size) else {
                continue;
            };
            if total_needed <= block.size {
                self.free_large.swap_remove(i);
                self.free_bytes_total -= block.size;
                // The consumed block may have been the maximum.
                self.large_max_exact.set(None);
                return Some(Self::split(block, padding, size));
            }
        }
        None
    }

    /// Bump-allocate `size` bytes with the given alignment.
    ///
    /// Returns a pointer to the allocated region, or `None` if there
    /// isn't enough space.
    pub fn alloc(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        if size & 7 != 0 {
            // An unrounded size mints an unaligned split remainder / bump
            // cursor — the free-list alignment hazard above.
            warn_unaligned_block("alloc-size", self.cursor, size);
        }
        // Alignment invariant: consume whole 8-byte grid units so split
        // remainders and the bump cursor stay on the object grid no matter
        // what a caller passes. Every walker size formula already rounds the
        // same way, so this matches how the walk will stride the region.
        let size = size.checked_add(7)? & !7;

        // Reserve the allocation's aligned footprint, not merely an aligned
        // start. Compact object bodies can end 1..7 bytes before the next
        // object boundary; leaving that tail outside the allocation makes
        // linear heap walks see a phantom region between valid objects.
        let alloc_size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

        // Free-list fast path: if a prior non-moving sweep reclaimed any
        // holes, satisfy the request from the first block large enough to
        // hold `size` plus the alignment padding. The leftover (head
        // padding and/or tail) is returned to the free list so no space
        // is silently lost. This is checked first because the cursor may
        // already be at the arena's high-water mark after a sweep that
        // could not move survivors.
        // O(1) fail-fast: padding >= 0 means every block needs
        // `total_needed >= size`; if even the (upper bound of the) largest
        // block is smaller than `size`, no block can satisfy the request —
        // skip the scans entirely and go straight to the bump path.
        if !self.free_is_empty() && alloc_size <= self.max_free_upper {
            let base = self.data.as_ptr() as usize;
            // Tier selection: a request whose worst-case need (size + max
            // alignment padding) reaches LARGE_BLOCK_MIN can never be served
            // by a small block — skip the small tier entirely. Smaller
            // requests try the small tier first: its blocks are object-sized
            // holes, so a same-shaped request lands on its own size class.
            let worst_need = alloc_size.saturating_add(align - 1);
            let mut hit = if worst_need < LARGE_BLOCK_MIN {
                self.small_fit(base, alloc_size, align)
            } else {
                None
            };
            if hit.is_none() {
                hit = self.large_fit(base, alloc_size, align);
            }
            if let Some((alloc_offset, remainders)) = hit {
                for r in remainders.into_iter().flatten() {
                    self.push_block_routed(r);
                }
                // SAFETY: `alloc_offset + alloc_size` lies within the consumed
                // block, which came from a region inside the buffer.
                return Some(unsafe { self.data.as_mut_ptr().add(alloc_offset) });
            }
            // No fit anywhere, and BOTH tiers were viewed in full (the
            // segregated small tier has no scan budget to hide behind any
            // more), so the miss is a proof: tighten the bound to the exact
            // span-tier maximum folded with the small tier's ceiling.
            let large_max = self.free_large.iter().map(|b| b.size).max().unwrap_or(0);
            self.large_max_exact.set(Some(large_max));
            let small_cap = self.small_max_ceil();
            self.max_free_upper = self.max_free_upper.min(large_max.max(small_cap));
        }

        // Bump-allocation path: align the cursor up (checked to prevent
        // overflow near usize::MAX).
        let aligned = self.cursor.checked_add(align - 1).map(|v| v & !(align - 1));
        let end = aligned.and_then(|a| a.checked_add(alloc_size));
        if let (Some(aligned), Some(end)) = (aligned, end) {
            if end <= self.data.len() {
                // SAFETY: `aligned` is within `[0, self.data.len())` because
                // `end <= self.data.len()` was just checked.
                let ptr = unsafe { self.data.as_mut_ptr().add(aligned) };
                self.cursor = end;
                return Some(ptr);
            }
        }

        // The bump tail is exhausted too, so the free list is the only home
        // left for this object. The guarded search above is skipped whenever
        // the cached `max_free_upper` says no block can fit — that bound only
        // ever over-estimates, so a skip is a proof of no fit and this retry
        // is (now) redundant. It is kept because it costs nothing on the
        // common path: reaching here already means the allocation was about
        // to fail. Historically this arm also covered a real blind spot — the
        // small tier's bounded first-fit could bury a satisfiable block
        // behind a prefix of `swap_remove`-shuffled dust — which the
        // segregated size classes have since eliminated (see `small_fit`).
        let base = self.data.as_ptr() as usize;
        let mut hit = self.small_fit(base, alloc_size, align);
        if hit.is_none() {
            hit = self.large_fit(base, alloc_size, align);
        }
        if let Some((alloc_offset, remainders)) = hit {
            for r in remainders.into_iter().flatten() {
                self.push_block_routed(r);
            }
            // SAFETY: `alloc_offset + alloc_size` lies within the consumed
            // block, which came from a region inside the buffer.
            return Some(unsafe { self.data.as_mut_ptr().add(alloc_offset) });
        }
        None
    }

    /// Register a reclaimed `[offset, offset+size)` region as a free block.
    ///
    /// Called by the non-moving young-gen sweep for every dead object it
    /// reclaims. The region is **not** zeroed here — the sweep zeroes the
    /// reclaimed span itself so a later conservative root scan cannot
    /// observe a stale object header inside the hole.
    ///
    /// # Panics (debug only)
    /// Debug-asserts the block lies fully within the live (`< cursor`)
    /// region of the arena.
    pub fn add_free_block(&mut self, offset: usize, size: usize) {
        debug_assert!(
            offset + size <= self.cursor,
            "free block must lie within the bump region",
        );
        if size == 0 {
            return;
        }
        if (offset | size) & 7 != 0 {
            warn_unaligned_block("add_free_block", offset, size);
        }
        self.max_free_upper = self.max_free_upper.max(size);
        self.push_block_routed(FreeBlock { offset, size });
    }

    /// Drop every reclaimed region. Called when the arena is about to be
    /// swapped or reset so the next collection cycle starts clean.
    pub fn clear_free_list(&mut self) {
        // Clear the buckets in place (keeping their allocations) rather than
        // dropping the outer Vec — the table is rebuilt on every cycle.
        for bucket in self.free_small.iter_mut() {
            bucket.clear();
        }
        self.small_mask = [0u64; SMALL_MASK_WORDS];
        self.free_large.clear();
        self.max_free_upper = 0;
        self.large_max_exact.set(Some(0));
        self.free_bytes_total = 0;
    }

    /// Total bytes currently held on the free lists (reclaimed but unallocated).
    ///
    /// PERF (2026-07-15): this used to sum both tiers from scratch on EVERY
    /// call. Its only caller, `GenerationalHeap::needs_gc`, runs on every
    /// allocation attempt (not just ones that touch the free list), so a
    /// live workload with many allocations between free-list-changing events
    /// (sweeps, or an allocation actually consuming a free block) paid this
    /// O(free-list-size) cost far more often than the list's contents
    /// actually changed — and the list only grows over a session (the
    /// non-moving young sweep reclaims into it without ever fully draining
    /// it under steady churn). Measured contribution: `needs_gc` (which
    /// inlines this) was 24.5% of ALL sampled CPU time during a live
    /// `RequestMappingMessageConversionIntegrationTests` run — see
    /// docs/known-issues/CRATONVM-SPRING-GENUINE-BUGLIST.md. The
    /// epoch-gated cache below makes repeated calls between real changes
    /// O(1); the summation itself is unchanged (same tiers, same order),
    /// so a cache miss recomputes byte-identically to the old behavior.
    pub fn free_list_bytes(&self) -> usize {
        self.free_bytes_total
    }

    /// Size of the largest single free-list block (0 if the free list is
    /// empty). Used by the young-gen allocation probe to decide whether a
    /// request can be satisfied from reclaimed space when the bump cursor has
    /// reached capacity (a non-moving sweep cannot retreat the cursor, so a
    /// cursor-only probe would wrongly report OOM with the free list full).
    /// Unlike [`Self::free_list_bytes`], this reflects what a *single*
    /// allocation can actually use (the free list is non-coalescing across
    /// blocks within one request).
    ///
    /// PERF (perf/gc-allocation-fastpath, 2026-07-26): this was a full walk of
    /// BOTH tiers. `gen_heap::refill_tlab`'s fragmentation fallback calls it on
    /// every refill that the main path could not serve, and in the degraded
    /// mode that fallback hands out `FRAG_TLAB_FLOOR`-sized mini-TLABs — a few
    /// dozen objects each — so the walk was effectively per-allocation over a
    /// free list of thousands of uniform remnants. It is now a bitmap probe
    /// for the small tier and a memoised value for the span tier.
    ///
    /// The answer is a value that can actually be ALLOCATED, never an
    /// over-estimate: callers size a subsequent `alloc` from it.
    pub fn largest_free_block(&self) -> usize {
        let large = match self.large_max_exact.get() {
            Some(m) => m,
            None => {
                let m = self.free_large.iter().map(|b| b.size).max().unwrap_or(0);
                self.large_max_exact.set(Some(m));
                m
            }
        };
        // Any span is >= LARGE_BLOCK_MIN, which is larger than every block in
        // the small tier by construction — so a non-zero span maximum is the
        // overall maximum and the small tier need not be consulted.
        if large > 0 {
            return large;
        }
        self.small_max_floor()
    }

    /// Early-exit probe: is there any single free block of at least `size`
    /// bytes?
    ///
    /// Semantically `largest_free_block() >= size`, but cheap on the hot
    /// allocation-probe path:
    /// * O(1) "no" when `size > max_free_upper` (the cached upper bound).
    /// * O(1) "yes" for any sub-[`LARGE_BLOCK_MIN`] request while the span
    ///   tier is non-empty (every span is bigger by definition).
    /// * `size >= LARGE_BLOCK_MIN` consults ONLY the short span tier — the
    ///   long small-hole tier can never satisfy it.
    /// * A full failed scan tightens `max_free_upper` to the exact maximum,
    ///   so subsequent same-or-larger probes become O(1).
    ///
    /// PERF (perf/gc-allocation-fastpath, 2026-07-26): the sub-`LARGE_BLOCK_MIN`
    /// arm used to walk the whole small tier on a miss. It is now a bitmap
    /// probe — the highest occupied size class answers directly.
    pub fn has_free_block_at_least(&mut self, size: usize) -> bool {
        if size == 0 {
            return !self.free_is_empty();
        }
        if size > self.max_free_upper {
            return false;
        }
        if size < LARGE_BLOCK_MIN {
            if !self.free_large.is_empty() {
                return true; // every span is >= LARGE_BLOCK_MIN > size
            }
            // Every block in the highest occupied class is at least
            // `small_max_floor()` bytes, so this "yes" is a guarantee.
            if self.small_max_floor() >= size {
                return true;
            }
            let ceil = self.small_max_ceil();
            self.max_free_upper = self.max_free_upper.min(ceil);
            // Ambiguity window: `size` lands strictly inside the highest
            // occupied size class, so only that class's blocks can decide it.
            // Unreachable while every span sits on the 8-byte object grid
            // (floor == ceil then), which the `warn_unaligned_block` tripwire
            // enforces at every producer — and bounded to a single class even
            // when it is not.
            if size <= ceil {
                if let Some(k) = mask_last(&self.small_mask) {
                    return self.free_small[k].iter().any(|b| b.size >= size);
                }
            }
            false
        } else {
            let mut scan_max = 0usize;
            for b in &self.free_large {
                if b.size >= size {
                    return true;
                }
                scan_max = scan_max.max(b.size);
            }
            // The whole span tier was viewed, so its maximum is now exact.
            self.large_max_exact.set(Some(scan_max));
            // The small tier caps below LARGE_BLOCK_MIN <= size; fold in its
            // (bitmap-derived) ceiling rather than scanning it.
            let ceil = self.small_max_ceil();
            self.max_free_upper = scan_max.max(ceil);
            false
        }
    }

    /// Snapshot of the current free list as `(offset, size)` pairs,
    /// sorted by ascending offset. Used by the non-moving sweep's object
    /// walker to skip holes the same way `OldGen::walk_objects` does.
    pub fn free_blocks_sorted(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self
            .free_small
            .iter()
            .flatten()
            .chain(self.free_large.iter())
            .map(|b| (b.offset, b.size))
            .collect();
        v.sort_by_key(|&(off, _)| off);
        v
    }

    /// Reset the arena, logically freeing all allocations.
    /// The backing memory is zeroed for safety.
    ///
    /// # When is it safe to use [`Self::reset_no_zero`] instead?
    ///
    /// The unsafe `reset_no_zero` variant is only sound when **both** of the
    /// following hold:
    ///
    /// 1. The allocator path zero-initialises every byte before handing the
    ///    pointer to the caller (e.g. `try_alloc_young` calls
    ///    `ptr::write_bytes(ptr, 0, size)`), OR the immediate caller fully
    ///    overwrites the region (e.g. Cheney `copy_nonoverlapping` writing
    ///    `total_size` bytes into the to-space slot). This guarantees no
    ///    legitimate read ever observes a stale byte.
    /// 2. No external code can read bytes past the cursor between the reset
    ///    and the next allocation. In CratonVM this is **not** true for
    ///    young-gen arenas: `GenerationalHeap::is_object_address`
    ///    (gen_heap.rs:465) performs conservative pointer validation that
    ///    accepts any 8-byte-aligned address within `[base, base+capacity)`
    ///    of either young space and then dereferences it as an
    ///    `ObjectHeader`. The VM's conservative root scanner
    ///    (vm/src/vm/vm_exec.rs:679) feeds ambiguous JVM long values through
    ///    this check, so leftover bytes in a reset young arena can be
    ///    misidentified as live objects and forwarded as garbage.
    ///
    /// Therefore: do **not** swap `reset` for `reset_no_zero` on the young
    /// from-space hot path until `is_object_address` is bounded by the live
    /// cursor (or the conservative root scanner is replaced with a precise
    /// stack map). The audit-flagged "perf bug" is a real cost, but the
    /// correctness hazard outweighs it.
    pub fn reset(&mut self) {
        // stw-residual-close forensics: record the wipe range before zeroing
        // (site 2 = from-space reset). Gated; no-op unless the env is set.
        crate::zero_forensics::record(2, 0, self.data.as_ptr() as usize, self.cursor);
        // Zero out used region for safety (prevents stale data reads)
        self.data[..self.cursor].fill(0);
        self.cursor = 0;
        self.clear_free_list();
        // Every recorded object start just became zeroed bytes.
        self.clear_alloc_anchors();
    }

    /// Reset the arena without zeroing memory.
    ///
    /// # Safety
    /// Callers must ensure all subsequent allocations are fully
    /// initialized before any reads, AND that no external scanner can
    /// observe bytes past the (now-zero) cursor before they are
    /// re-allocated. See the doc comment on [`Self::reset`] for the
    /// full safety contract.
    ///
    /// Currently unused: the young-gen reset path in `gen_heap.rs` cannot
    /// satisfy condition (2) because conservative root scanning may
    /// dereference any 8-byte-aligned address in an arena's capacity
    /// range. Retained for future use when the GC switches to precise
    /// stack maps or when `is_object_address` is tightened to honour the
    /// cursor bound.
    #[allow(dead_code)]
    pub unsafe fn reset_no_zero(&mut self) {
        self.cursor = 0;
        self.clear_free_list();
        self.clear_alloc_anchors();
    }

    /// Returns true if the given pointer falls within this arena's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// The number of bytes currently allocated (cursor position).
    pub fn used(&self) -> usize {
        self.cursor
    }

    /// The total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// Get the base pointer of the arena's backing storage.
    pub fn base_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Get a mutable base pointer of the arena's backing storage.
    pub fn base_ptr_mut(&mut self) -> *mut u8 {
        self.data.as_mut_ptr()
    }

    /// Grow the arena to at least `new_capacity` bytes, preserving existing data.
    ///
    /// If `new_capacity` <= current capacity, this is a no-op.
    ///
    /// # Panics
    ///
    /// `Vec::resize` may reallocate the backing buffer, which **invalidates
    /// every raw pointer** previously handed out by [`Self::alloc`]. Nothing
    /// in the type system enforces that callers fix up those pointers, so
    /// growing a non-empty arena is almost always a silent heap-corruption
    /// bug. To make the "safe by accident" usage explicit, this method
    /// **panics** if the arena has any live allocations (`cursor != 0`).
    ///
    /// Only an empty arena (cursor at 0, e.g. a freshly-reset to-space) may
    /// be grown. If a future caller genuinely needs to grow a populated
    /// arena it must first relocate every object and reset the cursor.
    ///
    /// Returns the old base pointer so callers can compute relocation offsets.
    pub fn grow(&mut self, new_capacity: usize) -> *const u8 {
        // Same alignment invariant as `new()` — see the constructor note.
        let new_capacity = new_capacity & !7;
        if new_capacity <= self.data.len() {
            return self.data.as_ptr();
        }
        assert_eq!(
            self.cursor, 0,
            "Arena::grow called on a non-empty arena ({} bytes live): \
             Vec::resize may reallocate and invalidate every pointer into \
             the arena. Only an empty (reset) arena may be grown.",
            self.cursor,
        );
        let old_base = self.data.as_ptr();
        let old_capacity = self.data.len();
        self.data.resize(new_capacity, 0);
        // The bucket table is indexed by capacity; `grow` is the only other
        // place `data.len()` moves. `cursor == 0` was just asserted, so there
        // is nothing recorded to preserve.
        self.rearm_alloc_anchors();
        if gc_flags().dbg_youngstate {
            eprintln!("[youngstate] arena-grow {old_capacity} -> {new_capacity} bytes");
        }
        old_base
    }

    /// The remaining free bytes in this arena.
    ///
    /// Counts both the un-bumped tail (`capacity - cursor`) and any holes
    /// reclaimed by a non-moving sweep. Note that free-list space is
    /// fragmented: a single allocation can only use one block, so this is
    /// an upper bound on the largest satisfiable request.
    pub fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.cursor) + self.free_list_bytes()
    }
}

impl std::fmt::Debug for Arena {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Arena")
            .field("used", &self.cursor)
            .field("capacity", &self.data.len())
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
    fn arena_basic_alloc() {
        let mut arena = Arena::new(1024);
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.capacity(), 1024);

        let ptr = arena.alloc(64, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 64);
    }

    /// dohead-oom (2026-07-19): a satisfiable hole must be found no matter
    /// how much dust was reclaimed ahead of it, once the bump tail is also
    /// exhausted. Simulate the non-moving young collector's steady state
    /// (cursor pinned at capacity, so every further allocation must come from
    /// the free list): register a long run of too-small holes followed by one
    /// genuinely-sized hole, then request that size. This used to depend on a
    /// post-bump rescue scan because the small tier was a flat first-fit list
    /// with a 16-block budget; with segregated size classes the fit-sized
    /// hole is found directly in its own class, and the dust is never even
    /// visited.
    #[test]
    fn arena_alloc_finds_fit_past_scan_budget_when_bump_tail_exhausted() {
        let dust_count = 20usize;
        let dust_size = 8usize;
        let fit_size = 32usize;
        let capacity = dust_count * dust_size + fit_size + 256;
        let mut arena = Arena::new(capacity);

        // Carve out `dust_count` tiny live objects, then a fit-sized one.
        let mut dust_offsets = Vec::new();
        for _ in 0..dust_count {
            let ptr = arena.alloc(dust_size, 8).unwrap();
            dust_offsets.push(unsafe { ptr.offset_from(arena.base_ptr_mut()) } as usize);
        }
        let fit_ptr = arena.alloc(fit_size, 8).unwrap();
        let fit_offset = unsafe { fit_ptr.offset_from(arena.base_ptr_mut()) } as usize;

        // Reclaim them in the same order a real sweep would walk them: dust
        // first (fills the `swap_remove`-backed scan prefix), fit-sized hole
        // last (lands past the scan budget).
        for &off in &dust_offsets {
            arena.add_free_block(off, dust_size);
        }
        arena.add_free_block(fit_offset, fit_size);

        // Exhaust the bump tail so the free list is the only remaining path.
        let remaining = arena.capacity() - arena.used();
        arena.alloc(remaining, 8).unwrap();
        assert_eq!(arena.used(), arena.capacity());

        // A bounded scan alone would only see `dust_size`-sized holes here
        // and report failure; the fallback full scan must find the
        // fit-sized hole regardless of its position in the list.
        let request_size = 24;
        assert!(
            fit_size >= request_size,
            "test fixture invariant: the reclaimed hole must satisfy the request"
        );
        assert!(
            arena.alloc(request_size, 8).is_some(),
            "alloc must fall back to a full free-list scan once the bump tail \
             is exhausted, instead of trusting a bounded scan's miss"
        );
    }

    #[test]
    fn arena_multiple_allocs() {
        let mut arena = Arena::new(256);

        let p1 = arena.alloc(32, 8).unwrap();
        let p2 = arena.alloc(32, 8).unwrap();
        assert_ne!(p1, p2);
        assert_eq!(arena.used(), 64);
    }

    #[test]
    fn arena_alignment() {
        let mut arena = Arena::new(256);

        // Allocate 3 bytes with alignment 1 — the arena consumes whole
        // 8-byte grid units (alignment invariant), so used() advances to 8.
        arena.alloc(3, 1).unwrap();
        assert_eq!(arena.used(), 8);

        // Next allocation with alignment 8 lands on the grid at offset 8.
        let p2 = arena.alloc(8, 8).unwrap();
        let offset = unsafe { p2.offset_from(arena.base_ptr_mut()) } as usize;
        assert_eq!(offset, 8); // aligned to 8
        assert_eq!(arena.used(), 16);
    }

    #[test]
    fn arena_full_returns_none() {
        let mut arena = Arena::new(64);
        assert!(arena.alloc(64, 8).is_some());
        assert!(arena.alloc(1, 1).is_none());
    }

    #[test]
    fn arena_alignment_reserves_trailing_padding() {
        let mut arena = Arena::new(64);
        let first = arena.alloc(44, 8).unwrap();
        let second = arena.alloc(8, 8).unwrap();

        assert_eq!(unsafe { first.offset_from(arena.base_ptr_mut()) }, 0);
        assert_eq!(unsafe { second.offset_from(arena.base_ptr_mut()) }, 48);
        assert_eq!(arena.used(), 56);
    }

    #[test]
    fn arena_reset() {
        let mut arena = Arena::new(128);
        arena.alloc(64, 8).unwrap();
        assert_eq!(arena.used(), 64);

        arena.reset();
        assert_eq!(arena.used(), 0);

        // Can allocate again
        assert!(arena.alloc(128, 8).is_some());
    }

    #[test]
    fn arena_contains() {
        let mut arena = Arena::new(256);
        let ptr = arena.alloc(32, 8).unwrap();

        assert!(arena.contains(ptr));
        assert!(arena.contains(unsafe { ptr.add(16) }));
        // Just outside the arena
        assert!(!arena.contains(std::ptr::null()));
    }

    #[test]
    fn arena_zero_size_alloc() {
        let mut arena = Arena::new(64);
        let ptr = arena.alloc(0, 8).unwrap();
        assert!(!ptr.is_null());
        assert_eq!(arena.used(), 0);
    }

    #[test]
    fn arena_alignment_overflow_returns_none() {
        let mut arena = Arena::new(64);
        // Push cursor near usize::MAX so alignment arithmetic would overflow
        arena.cursor = usize::MAX - 2;
        // align=8 means cursor + 7 would overflow usize
        assert!(arena.alloc(1, 8).is_none());
    }

    #[test]
    fn arena_free_list_alloc_and_probe() {
        let mut arena = Arena::new(256);
        // Consume the whole bump region so only the free list can serve.
        assert!(arena.alloc(256, 8).is_some());
        assert!(arena.alloc(8, 8).is_none());

        // Reclaim two holes: 24B and 64B.
        arena.add_free_block(0, 24);
        arena.add_free_block(64, 64);
        assert_eq!(arena.largest_free_block(), 64);
        assert!(arena.has_free_block_at_least(24));
        assert!(arena.has_free_block_at_least(64));
        assert!(!arena.has_free_block_at_least(65));

        // Allocation from the 64B hole succeeds and leaves the remainder.
        assert!(arena.alloc(48, 8).is_some());
        assert_eq!(arena.largest_free_block(), 24);
    }

    #[test]
    fn arena_max_free_upper_bound_tightens_and_fails_fast() {
        let mut arena = Arena::new(256);
        assert!(arena.alloc(256, 8).is_some()); // exhaust bump tail
        arena.add_free_block(0, 32);
        arena.add_free_block(64, 16);

        // Upper bound reflects the largest add (32).
        assert!(!arena.has_free_block_at_least(33)); // full scan → tightens to 32
        assert!(arena.has_free_block_at_least(32));

        // Consume the 32B block entirely; the cached bound (32) is now an
        // over-estimate but stays a SOUND upper bound: a 20B probe scans,
        // finds only the 16B block, answers false, and tightens to 16.
        assert!(arena.alloc(32, 8).is_some());
        assert!(!arena.has_free_block_at_least(20));
        assert!(arena.has_free_block_at_least(16));

        // A fresh add raises the bound again.
        arena.add_free_block(96, 40);
        assert!(arena.has_free_block_at_least(40));

        // clear/reset zero the bound → O(1) false.
        arena.clear_free_list();
        assert!(!arena.has_free_block_at_least(1));
        assert!(arena.has_free_block_at_least(0) == false);
    }

    #[test]
    fn arena_tiered_free_lists_route_and_serve_by_size() {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(arena.alloc(cap, 8).is_some()); // exhaust bump tail

        // A big coalesced span + a pile of node-sized holes.
        arena.add_free_block(0, 32 * 1024); // → free_large
        for i in 0..100 {
            arena.add_free_block(40 * 1024 + i * 72, 72); // → free_small
        }

        // Chunk-sized request is served from the span tier (the small holes
        // can never satisfy it and must not be scanned — behaviorally: it
        // just succeeds).
        assert!(arena.alloc(16 * 1024, 8).is_some());
        // Object-sized requests are served from the hole tier.
        assert!(arena.alloc(72, 8).is_some());
        assert!(arena.alloc(72, 8).is_some());

        // Probes: sub-tier-boundary probe answers O(1)-yes while a span
        // remains; chunk-sized probe consults spans only.
        assert!(arena.has_free_block_at_least(72));
        assert!(arena.has_free_block_at_least(8 * 1024));
        assert!(!arena.has_free_block_at_least(33 * 1024));

        // Consume the span remainder; chunk probes now answer false while
        // hole-sized probes still answer true.
        assert!(arena.alloc(16 * 1024 - 16, 8).is_some());
        assert!(!arena.has_free_block_at_least(LARGE_BLOCK_MIN));
        assert!(arena.has_free_block_at_least(72));

        // Splitting a span routes a sub-tier remainder to the small list:
        // reclaim a 5 KiB block, carve 4.5 KiB → ~0.5 KiB remainder must
        // still be findable by a small request.
        let mut a2 = Arena::new(8 * 1024);
        let c2 = a2.capacity();
        assert!(a2.alloc(c2, 8).is_some());
        a2.add_free_block(0, 5 * 1024);
        assert!(a2.alloc(4 * 1024 + 512, 8).is_some());
        assert!(a2.has_free_block_at_least(256));
        assert!(a2.alloc(256, 8).is_some());
    }

    #[test]
    fn arena_alloc_fail_fast_skips_scan_for_oversized_requests() {
        let mut arena = Arena::new(128);
        assert!(arena.alloc(128, 8).is_some()); // exhaust bump tail
        arena.add_free_block(0, 24);
        // Larger than any block AND no bump room → None (via the O(1)
        // fail-fast; behaviorally identical to the historical full scan).
        assert!(arena.alloc(64, 8).is_none());
        // Fits the hole → allocates from the free list.
        assert!(arena.alloc(16, 8).is_some());
    }

    // ----- Allocator-recorded sweep anchors (perf/gc-oracle-anchors) --------

    #[test]
    fn alloc_anchors_are_one_per_bucket_and_strictly_increasing() {
        let mut arena = Arena::new(64 * 1024);
        let bucket = 1usize << arena.anchor_shift;
        // Three offsets in bucket 0, one in bucket 1, one in bucket 3 —
        // recorded out of bucket order on purpose.
        arena.note_object_start(3 * bucket + 24);
        arena.note_object_start(0);
        arena.note_object_start(64);
        arena.note_object_start(128);
        arena.note_object_start(bucket + 8);
        let a = arena.take_alloc_anchors();
        // First-in-bucket wins, and draining yields ascending offsets.
        assert_eq!(a, vec![0, bucket + 8, 3 * bucket + 24]);
        assert!(a.windows(2).all(|w| w[0] < w[1]));
        // Draining leaves the table empty for the next epoch.
        assert!(arena.take_alloc_anchors().is_empty());
    }

    #[test]
    fn alloc_anchors_bucket_table_stays_bounded_and_in_range() {
        // A 2 GiB arena must not mint a per-4 KiB table (that would be 512 K
        // entries); the shift widens until the bucket count fits the cap.
        let mut arena = Arena::new(64 * 1024);
        arena.rearm_alloc_anchors();
        assert!(arena.alloc_anchors.len() <= (1 << 16) + 1);
        // Out-of-range offsets are ignored rather than panicking (a caller
        // handing over a stale offset must never take the VM down).
        arena.note_object_start(usize::MAX);
        arena.note_object_start(arena.capacity() * 4);
        assert!(arena.take_alloc_anchors().is_empty());
    }

    #[test]
    fn reset_clears_alloc_anchors() {
        // An anchor describes the object grid of the epoch that just ended;
        // `reset` zeroes the arena, so every recorded offset is now dead.
        let mut arena = Arena::new(64 * 1024);
        assert!(arena.alloc(64, 8).is_some());
        arena.note_object_start(0);
        arena.reset();
        assert!(arena.take_alloc_anchors().is_empty());
    }

    // ----- Segregated small tier (perf/gc-allocation-fastpath) --------------

    /// The bitmap helpers are the load-bearing part of the small tier: a wrong
    /// answer either loses a satisfiable block (spurious OOM / forced GC) or
    /// claims one that does not exist (failed refill).
    #[test]
    fn small_bucket_mask_probes_are_exact() {
        let mut mask = [0u64; SMALL_MASK_WORDS];
        assert_eq!(mask_first_from(&mask, 0), None);
        assert_eq!(mask_last(&mask), None);

        // One bit in each word, including the last.
        for k in [0usize, 1, 63, 64, 65, 127, 200, SMALL_BUCKETS - 1] {
            mask_set(&mut mask, k);
        }
        assert_eq!(mask_first_from(&mask, 0), Some(0));
        assert_eq!(mask_first_from(&mask, 1), Some(1));
        assert_eq!(mask_first_from(&mask, 2), Some(63));
        assert_eq!(mask_first_from(&mask, 64), Some(64));
        assert_eq!(mask_first_from(&mask, 66), Some(127));
        assert_eq!(mask_first_from(&mask, 128), Some(200));
        assert_eq!(mask_first_from(&mask, 201), Some(SMALL_BUCKETS - 1));
        // A start past the table must not index out of bounds.
        assert_eq!(mask_first_from(&mask, SMALL_BUCKETS), None);
        assert_eq!(mask_first_from(&mask, usize::MAX), None);
        assert_eq!(mask_last(&mask), Some(SMALL_BUCKETS - 1));

        mask_clear(&mut mask, SMALL_BUCKETS - 1);
        assert_eq!(mask_last(&mask), Some(200));
        for k in [0usize, 1, 63, 64, 65, 127, 200] {
            mask_clear(&mut mask, k);
        }
        assert_eq!(mask_last(&mask), None);
        assert_eq!(mask_first_from(&mask, 0), None);
    }

    /// A same-shaped request must land in its own size class and consume the
    /// hole WHOLE — no split, therefore no dust for the next request to walk
    /// past. This is the property the flat first-fit list could not hold.
    #[test]
    fn small_tier_exact_class_reuses_holes_without_splitting() {
        let node = 72usize;
        let count = 500usize;
        let mut arena = Arena::new(count * node + 4096);
        let mut offsets = Vec::new();
        for _ in 0..count {
            let p = arena.alloc(node, 8).unwrap();
            offsets.push(unsafe { p.offset_from(arena.base_ptr_mut()) } as usize);
        }
        // Exhaust the bump tail so only the free list can serve.
        let tail = arena.capacity() - arena.used();
        arena.alloc(tail, 8).unwrap();
        for &off in &offsets {
            arena.add_free_block(off, node);
        }
        let reclaimed = arena.free_list_bytes();
        assert_eq!(reclaimed, count * node);

        // Every re-allocation is an exact-class hit: the free list shrinks by
        // exactly one node each time and never grows a remainder block.
        for i in 0..count {
            assert!(
                arena.alloc(node, 8).is_some(),
                "exact-class reuse failed at iteration {i}"
            );
            assert_eq!(arena.free_list_bytes(), reclaimed - (i + 1) * node);
        }
        assert_eq!(arena.free_list_bytes(), 0);
        assert_eq!(arena.largest_free_block(), 0);
        assert!(arena.alloc(node, 8).is_none());
    }

    /// `largest_free_block` must stay EXACTLY allocatable: `refill_tlab` sizes
    /// a mini-TLAB from it and immediately allocates that many bytes, so an
    /// over-estimate turns into a wedge. Check it against a brute-force
    /// maximum across both tiers, through pushes, splits and clears.
    #[test]
    fn largest_free_block_is_allocatable_across_tiers() {
        let mut arena = Arena::new(256 * 1024);
        let cap = arena.capacity();
        arena.alloc(cap, 8).unwrap(); // exhaust the bump tail
        assert_eq!(arena.largest_free_block(), 0);

        // Small tier only.
        arena.add_free_block(0, 40);
        arena.add_free_block(64, 4088); // top small class
        assert_eq!(arena.largest_free_block(), 4088);
        // ...and it really is allocatable.
        assert!(arena.alloc(4088, 8).is_some());
        assert_eq!(arena.largest_free_block(), 40);

        // Span tier dominates whenever it is non-empty.
        arena.add_free_block(8192, 64 * 1024);
        assert_eq!(arena.largest_free_block(), 64 * 1024);
        assert!(arena.alloc(64 * 1024, 8).is_some());
        // The span was consumed whole, so the small tier is the maximum again.
        assert_eq!(arena.largest_free_block(), 40);

        arena.clear_free_list();
        assert_eq!(arena.largest_free_block(), 0);
        assert_eq!(arena.free_list_bytes(), 0);
    }

    /// The `max_free_upper` fail-fast gate must never under-state the truth:
    /// an under-estimate makes `alloc` skip a free list that could have served
    /// the request and report a false OOM (which the JIT slow path turns into
    /// a forced GC). Drive it through the small-tier tightening path.
    #[test]
    fn max_free_upper_stays_a_sound_upper_bound() {
        let mut arena = Arena::new(1024);
        arena.alloc(1024, 8).unwrap(); // exhaust the bump tail
        arena.add_free_block(0, 128);
        arena.add_free_block(256, 64);

        // A miss tightens the bound; the bound must still cover every block.
        assert!(!arena.has_free_block_at_least(129));
        assert!(arena.max_free_upper >= 128);
        assert!(arena.alloc(128, 8).is_some());

        // After the 128 block is gone the bound may still read 128 (it only
        // ever over-estimates) but a 64-byte request must still be served.
        assert!(arena.alloc(64, 8).is_some());
        assert!(arena.alloc(8, 8).is_none());
        assert_eq!(arena.free_list_bytes(), 0);
    }

    /// A reclaimed hole smaller than the request must not be handed out, and
    /// the escalating class search must not skip a class that could serve it.
    #[test]
    fn small_tier_escalates_to_the_next_occupied_class() {
        let mut arena = Arena::new(1024);
        arena.alloc(1024, 8).unwrap();
        arena.add_free_block(0, 24);
        arena.add_free_block(64, 56);

        // 32 bytes: class 4 is empty, class 7 (56) is the next occupied one.
        let p = arena.alloc(32, 8).unwrap();
        assert_eq!(unsafe { p.offset_from(arena.base_ptr_mut()) }, 64);
        // The 24-byte remainder went back as its own class, alongside the
        // untouched 24-byte hole.
        assert_eq!(arena.free_list_bytes(), 24 + 24);
        assert_eq!(arena.largest_free_block(), 24);
        // Nothing can serve 32 any more.
        assert!(arena.alloc(32, 8).is_none());
        // But two 24s can still be served.
        assert!(arena.alloc(24, 8).is_some());
        assert!(arena.alloc(24, 8).is_some());
        assert!(arena.alloc(8, 8).is_none());
    }

    /// The sweep's hole map must see every block regardless of which class it
    /// landed in, in ascending offset order.
    #[test]
    fn free_blocks_sorted_spans_every_size_class() {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        arena.alloc(cap, 8).unwrap();
        arena.add_free_block(16 * 1024, 8 * 1024); // span tier
        arena.add_free_block(0, 4080); // top small class
        arena.add_free_block(8 * 1024, 40); // low small class
        let sorted = arena.free_blocks_sorted();
        assert_eq!(
            sorted,
            vec![(0, 4080), (8 * 1024, 40), (16 * 1024, 8 * 1024)]
        );
    }

    #[test]
    fn grow_rearms_alloc_anchors_for_the_new_capacity() {
        let mut arena = Arena::new(4 * 1024);
        arena.grow(1024 * 1024);
        // The table must cover the grown capacity, or every offset past the
        // old end would silently fail to record.
        let last = arena.capacity() - 8;
        arena.note_object_start(last);
        assert_eq!(arena.take_alloc_anchors(), vec![last]);
    }
}
