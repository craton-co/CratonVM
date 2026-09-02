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
use std::collections::BTreeMap;
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
    /// Backing storage for `capacity` bytes.
    ///
    /// Reserved address space committed in 2 MiB granules as the cursors reach
    /// them, or -- under `CRATONVM_GC_RESERVE=0`, on a platform whose syscalls
    /// this crate does not declare, or when the OS refuses the reservation --
    /// the wholly-committed `alloc_zeroed` block this used to be. See
    /// [`crate::reservation`] for why the difference is visible in a process's
    /// commit charge and its resident set but nowhere else.
    data: crate::reservation::HeapStore,
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
    /// Reclaimed regions of at least [`LARGE_BLOCK_MIN`] bytes — the spans TLAB
    /// refills and array allocations carve from — **keyed by exact size**.
    ///
    /// WHY A MAP AND NOT A VEC. This was a `Vec<FreeBlock>` scanned first-fit,
    /// on the stated grounds that "this tier stays short (the post-sweep
    /// coalescer merges adjacent holes into a handful of spans), so scanning it
    /// is cheap". That premise is false for any workload whose holes are walled
    /// by live data: the coalescer merges what is ADJACENT, and one survivor
    /// between two holes keeps them apart however often it runs. The tier then
    /// holds thousands of spans and every request scanned all of them — and
    /// `max_free_upper`'s O(1) fail-fast does not save it, because
    /// `add_free_block` raises that bound again on every block the sweep
    /// reclaims.
    ///
    /// Measured on `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest`
    /// (2026-08-11, Azure Linux, default collector, `--Xmx 1500m`):
    /// `Arena::alloc` was **76% of all CPU samples**, and `perf annotate`
    /// attributed 47% of the entire process to one source line — the first line
    /// of that scan's loop body. The class takes 54s on real HotSpot on the
    /// same box and was taking ~2300s.
    ///
    /// Keyed by size, the same question is `range(need..).next()`: O(log n) for
    /// a hit, O(log n) for an AUTHORITATIVE miss, and no scan either way. Every
    /// block inside one bucket has the same size, which is what makes taking
    /// the head exact rather than merely first-fit.
    free_large: BTreeMap<usize, Vec<FreeBlock>>,
    /// Blocks across every [`Self::free_large`] bucket. The map's `len()` counts
    /// buckets, not blocks, and several invariants here are about blocks.
    free_large_blocks: usize,
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
    // NOTE: the old `large_max_exact: Cell<Option<usize>>` memo is gone. It
    // existed because the span tier was a Vec whose maximum could only be found
    // by walking it, so every consuming take invalidated the memo and the next
    // query re-walked. A size-keyed map answers the same question with its last
    // key — always exact, never stale, no walk. See `Arena::large_max`.
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
    /// Blocks pushed since the last [`Arena::coalesce_free_list`], i.e. how
    /// much adjacency a merge could possibly have to collapse. Bumped by every
    /// push (`push_block_routed`), zeroed by the merge.
    /// Serve from the BUMP cursor before the free list.
    ///
    /// # Why a collector would ask for this
    ///
    /// `alloc` checks the free list first, and the comment there says why: after
    /// a sweep that could not move survivors the cursor may already be at the
    /// high-water mark, so reusing holes is the only thing keeping the arena from
    /// ratcheting. That is the right default and it stays the default.
    ///
    /// A generational collector needs the opposite, and only for its nursery.
    /// ZGC's young cycle bounds its sweep below by the cursor the last whole-heap
    /// collection ended on (`ZgcRealHeap::gen_young_floor`), so an object the free
    /// list places BELOW that floor is inside the old region and no young cycle
    /// will reclaim it — it waits for a major. Bump-first puts every new object
    /// above the floor, which is what makes the nursery capture all of the
    /// allocation it is supposed to.
    ///
    /// # It is a preference, not a wall — and that is what makes it safe
    ///
    /// This only skips the free-list FAST path. `alloc`'s post-bump retry
    /// searches both tiers in full and then coalesces and searches again, so a
    /// request the bump tail cannot serve still gets every hole in the arena.
    /// Turning this on cannot turn a servable allocation into an
    /// `OutOfMemoryError`; it can only change which space serves it. The holes
    /// below the floor are recovered by the compacting slide, which is default-on
    /// and is also what promotes the nursery's survivors out.
    ///
    /// `free_list_after_bump` counts the fall-throughs, so "the nursery is
    /// leaking into the old region because the bump tail is exhausted" is a
    /// number rather than an inference.
    prefer_bump: bool,
    /// Allocations that took the free list after the bump tail refused them,
    /// while [`Self::prefer_bump`] was on.
    free_list_after_bump: usize,
    free_pushed: usize,
    /// Downward bump cursor for the HIGH region: allocations enter at
    /// `capacity` and grow towards [`Self::cursor`]. `capacity` means the
    /// region has never been used, which is the state every arena but ZGC's
    /// stays in forever.
    ///
    /// # Why an arena needs two ends
    ///
    /// On a collector that does not compact, the largest servable request is
    /// the largest gap between two survivors — so the allocator's own layout
    /// decisions set the ceiling. Measured on Tomcat's
    /// `TestNonBlockingAPI` under ZGC (2026-08-13), a 2,101,264-byte `char[]`
    /// failed with 1.99 GB free because the biggest hole was 524,192 bytes:
    /// one TLAB chunk. The report that found it named the walls exactly —
    /// **544 live bytes in four runs (four `AQS$ConditionNode`s and two
    /// `AQS$ExclusiveNode`s) standing inside 2,621,264 bytes of otherwise
    /// contiguous arena**. Each parked thread leaves one small, long-lived
    /// node inside its own 512 KiB private chunk, and one survivor per chunk
    /// caps every hole in the heap at one chunk, permanently.
    ///
    /// Raising the chunk size only moves that ceiling (it was already raised
    /// once, 64 KiB -> 512 KiB, when a 65,552-byte `DFAState[8192]` hit the
    /// same wall). Separating the two populations removes it: a request too
    /// big for any TLAB is served from the opposite end of the arena, where
    /// the only neighbours it can ever have are other large objects — objects
    /// that are orders of magnitude rarer, so their holes stay large.
    ///
    /// The two ends share the middle: neither has a fixed reservation, and
    /// whichever population grows faster gets the space. When they meet,
    /// `cursor == high_cursor` and the partition is fixed and exact for the
    /// rest of the run, which is what makes the region test on a free block
    /// (`offset >= high_cursor`) permanently correct.
    high_cursor: usize,
    /// Reclaimed regions at or above [`Self::high_cursor`] — the large-object
    /// end's free list.
    ///
    /// A plain `Vec` scanned best-fit, deliberately, where the low end needed
    /// a size-keyed map and a bitmap: the population here is large objects, of
    /// which a running VM has thousands at most against the low end's tens of
    /// millions. The scan is also on the *rarest* allocation path there is —
    /// an object no TLAB will ever hold. Buying a `BTreeMap` here would be
    /// paying the maintenance cost of the low end's structures for a list that
    /// is three orders of magnitude shorter.
    free_high: Vec<FreeBlock>,
    /// Exact size of the largest block on [`Self::free_high`], or 0.
    /// Recomputed by the scans that can lower it; raised by every push.
    high_max: usize,
    /// Bytes at the top of the arena the LOW bump will not consume while the
    /// high end still has a claim on them. 0 disables the reserve, which is
    /// what every arena but ZGC's uses.
    ///
    /// # Why a reserve, when the two ends already share the middle
    ///
    /// Sharing sounds fair and is not, because the two populations do not
    /// compete at comparable rates. Measured on `TestNonBlockingAPI` under ZGC
    /// (2026-08-13, `-Xmx 2g`) with the split in place but no reserve: the free
    /// list at the failing allocation held **3,829 spans of ~512 KiB** — that
    /// is 1.96 GB of a 2.15 GB arena consumed as thread-private TLAB chunks
    /// alone, one per thread the workload created. The two cursors met while
    /// the large-object end had claimed almost nothing, so the split existed
    /// and bought nothing: a 2 MB `char[]` still had nowhere but the walled
    /// low end to go.
    ///
    /// The reserve is what makes the split load-bearing rather than
    /// theoretical. It is a **preference, not a wall**: the low end may still
    /// take reserved bytes as its last resort before failing (see
    /// [`Self::alloc`]), so a workload that allocates no large objects at all
    /// loses nothing, and one that would otherwise OOM at the low end still
    /// gets the memory.
    ///
    /// It also SHRINKS as it is used — `remaining_high_reserve` subtracts what
    /// the high end has already bumped — so the reserve is a floor on the
    /// large-object region's size, not an additional tax on top of it.
    high_reserve: usize,
    /// Blocks pushed to [`Self::free_high`] since the last high-end merge.
    /// Same role as [`Self::free_pushed`] for the low end.
    high_pushed: usize,
    /// How many pushes [`Arena::alloc`]'s last-resort merge waits for before it
    /// will sort the free list again.
    ///
    /// A merge is `O(n log n)` over the whole list, and `alloc` reaches that
    /// arm once per allocation on a heap that has wedged — so an ungated merge
    /// is per-allocation. Measured: `Arena::free_blocks_sorted` was 9.1% of a
    /// `type.temporal.ZonedDateTimeTest` profile the day the merge landed.
    ///
    /// So: back off when merging does not pay. A merge that removes nothing
    /// quadruples this (capped); one that removes something resets it. On a
    /// genuinely fragmented heap — holes walled by live objects, nothing
    /// adjacent — the arm switches itself off within a few attempts, while a
    /// list that really is merely split still gets merged on the first ask.
    coalesce_threshold: usize,
}

/// Starting (and post-productive-merge) value of [`Arena::coalesce_threshold`]:
/// one push is enough to justify looking.
const COALESCE_THRESHOLD_MIN: usize = 1;
/// Ceiling for the backoff. At this point the merge arm is effectively off,
/// which is the right answer for a heap whose holes are walled by live data.
const COALESCE_THRESHOLD_MAX: usize = 1 << 20;

/// Alignment tripwire (perf/halfgap residuals, 2026-07-18): every free-list
/// block must sit on the 8-aligned object grid — the non-moving sweep's walk
/// and the mark oracle both assume it. An unaligned block is upstream-bug
/// evidence (an alloc with an unrounded size minted an unaligned split
/// remainder, or a walk desync published an off-grid span); it later derails
/// the linear walk ("cursor overshot into free block" at +4 offsets). Warn
/// loudly (bounded) at the birth site so the producer is identifiable.
static FL_ALIGN_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Region tripwire: a SMALL-object allocation that lands at or above
/// [`Arena::high_cursor`], i.e. inside the large-object region.
///
/// The two ends exist so that "at the high end a large object's only possible
/// neighbours are other large objects, which are rarer by orders of magnitude,
/// so the holes they leave stay large" (`ZGC_LARGE_OBJECT_MIN`). One long-lived
/// 80-byte object up there caps every hole in that region at the distance to
/// its neighbour — which is the whole failure the split was introduced to
/// prevent, re-created from the other side.
///
/// MEASURED, which is why this exists rather than a comment: on
/// `org.h2.test.store.TestKillProcessWhileWriting` at `--Xmx 1g`, a
/// 1,048,592-byte `ByteBuffer.allocate` raised `OutOfMemoryError` with 97 % of
/// the heap free, and the collector's own fragmentation report named the wall:
/// `104 live bytes in 1 run(s) are all that stand between 1204192 free bytes
/// spread over 1204296 bytes of contiguous arena`, occupants
/// `java/lang/String` (80 B) and `java/lang/Object` (24 B) — inside the high
/// region.
///
/// # Where it is armed, and where it was NOT (2026-08-29)
///
/// [`Arena::alloc`]'s LOW free-list exits cannot fire it: a low tier only ever
/// returns an offset below `high_cursor`, so `is_high` is false there by
/// construction. For a week the three sites it was armed at were exactly those
/// three, and the page above recorded the resulting zero as "an untriggered
/// instrument, not evidence". The one exit that can return a high offset —
/// `alloc`'s last-resort `high_fit`, which spends the large-object end's own
/// free list rather than raise `OutOfMemoryError` — is now armed too, under
/// the site name `high-free-list-last-resort`.
///
/// It is still not a refusal, and should not become one: refusing would trade
/// a fragmentation hazard for an `OutOfMemoryError` on a heap that has bytes.
/// What changed the balance is that the damage is no longer PERMANENT —
/// `ZgcRealHeap::compact_high_region` packs a small survivor up there against
/// the top with everything else, so it stops walling the region at the next
/// relocating cycle.
static REGION_LEAK_HITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Count of small allocations placed in the large-object region, for a test or
/// a caller that wants the fact rather than the log line.
pub fn small_allocations_in_large_region() -> usize {
    REGION_LEAK_HITS.load(std::sync::atomic::Ordering::Relaxed)
}

#[cold]
fn warn_small_alloc_in_high_region(site: &str, offset: usize, size: usize, high_cursor: usize) {
    use std::sync::atomic::Ordering;
    let n = REGION_LEAK_HITS.fetch_add(1, Ordering::Relaxed);
    if n < 8 {
        tracing::warn!(
            "arena REGION tripwire [{site}]: a {size}-byte allocation landed at \
             offset={offset}, at or above high_cursor={high_cursor} — i.e. inside the \
             large-object region, whose whole purpose is to hold only objects too big \
             for a TLAB. One small survivor there caps every hole in that region and \
             is how a multi-megabyte request fails on a mostly-free heap."
        );
    }
}

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

/// Order free blocks by arena offset.
///
/// This used to be `v.sort_by_key(|&(off, _)| off)`. On the H2 UPDATE shape
/// that single line was **43 % of a 25-thread profile** (`quicksort` 21.8 % +
/// `drift::sort` 21.1 %), with driftsort's scratch buffer showing up as another
/// 3.4 % of `memmove`. The cause is not the call frequency — an earlier fix
/// already collapsed three rebuilds per sweep into one — but the input size: on
/// a process wedged onto the non-moving sweep the young arena reaches
/// `used == capacity` with a free list holding ~300 MB of holes, and every
/// sweep comparison-sorts all of them.
///
/// The key is an arena offset: unique (no two free blocks start at the same
/// place), non-negative, and bounded by the arena capacity. That is a radix
/// key, so an LSD radix sort replaces `O(n log n)` comparisons with a fixed
/// number of linear passes — three for a 512 MB arena at 11 bits per pass.
/// Below `RADIX_MIN` the pass overhead and the scratch allocation are not worth
/// it and a comparison sort wins; `sort_unstable_by_key` is used there because
/// unique keys make stability meaningless.
///
/// The radix path is itself stable, so both branches agree with the previous
/// `sort_by_key` on every input, not merely on inputs with unique keys.
fn sort_by_offset(v: &mut Vec<(usize, usize)>) {
    /// Below this, a comparison sort beats the radix passes.
    const RADIX_MIN: usize = 512;
    /// Bits consumed per pass. 11 keeps the histogram (2048 × usize = 16 KB)
    /// inside L1/L2 while covering a 512 MB arena in three passes.
    const BITS: u32 = 11;
    const BUCKETS: usize = 1 << BITS;
    const MASK: usize = BUCKETS - 1;

    if v.len() < RADIX_MIN {
        v.sort_unstable_by_key(|&(off, _)| off);
        return;
    }
    let max = v.iter().map(|&(off, _)| off).max().unwrap_or(0);
    let mut scratch: Vec<(usize, usize)> = vec![(0, 0); v.len()];
    let mut counts = [0usize; BUCKETS];
    let mut shift = 0u32;
    while (max >> shift) > 0 {
        counts.fill(0);
        for &(off, _) in v.iter() {
            counts[(off >> shift) & MASK] += 1;
        }
        let mut running = 0usize;
        for c in counts.iter_mut() {
            let n = *c;
            *c = running;
            running += n;
        }
        for &entry in v.iter() {
            let bucket = (entry.0 >> shift) & MASK;
            scratch[counts[bucket]] = entry;
            counts[bucket] += 1;
        }
        std::mem::swap(v, &mut scratch);
        shift += BITS;
    }
}

/// One contiguous stretch of the arena measured against a target request: the
/// window of free spans — and the live "walls" standing between them — that
/// would serve `request` at the least cost in bytes that would have to be
/// moved out of the way.
///
/// This is the number that decides what a fragmentation OOM actually needs.
/// `largest_free_block < request` says the request cannot be served; it does
/// not say whether the heap is a mosaic of thousands of live objects (nothing
/// short of relocation helps) or whether a *handful* of small survivors stands
/// between gigabytes of otherwise contiguous free space (which a targeted fix
/// can address). Those want opposite work, and the guard could not tell them
/// apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FragWindow {
    /// Arena offset the window starts at (the base of its first free span).
    pub start: usize,
    /// Arena offset the window ends at, exclusive (the end of its last span).
    pub end: usize,
    /// Free bytes inside the window.
    pub free_bytes: usize,
    /// Occupied bytes inside the window — what stands between the holes.
    pub wall_bytes: usize,
    /// How many separate occupied runs those bytes form.
    pub walls: usize,
}

/// What a fragmented free list actually looks like, laid out across the arena.
///
/// Produced by [`Arena::frag_profile`] on the allocation-failure path only —
/// it sorts the whole free list, so it is far too expensive for any path that
/// is not already about to raise `OutOfMemoryError`.
#[derive(Debug, Clone, Default)]
pub struct FragProfile {
    /// Bump high-water mark at the time of the failure.
    pub cursor: usize,
    /// Arena capacity.
    pub capacity: usize,
    /// Total free-list bytes (both tiers).
    pub free_bytes: usize,
    /// Free-list blocks (both tiers).
    pub spans: usize,
    /// Largest single free block.
    pub largest_span: usize,
    /// `(2^i, count)` — spans whose size is in `[2^i, 2^(i+1))`. Only
    /// non-empty buckets are listed.
    pub span_hist: Vec<(usize, usize)>,
    /// Occupied runs strictly between two free spans.
    pub walls: usize,
    /// Bytes in those runs.
    pub wall_bytes: usize,
    /// `(2^i, count)` over wall sizes, non-empty buckets only.
    pub wall_hist: Vec<(usize, usize)>,
    /// The cheapest contiguous window that could serve the request, if the
    /// swept region contains one at all.
    pub cheapest: Option<FragWindow>,
}

/// `(2^i, count)` buckets over `sizes`, non-empty buckets only, ascending.
fn log2_hist(sizes: impl Iterator<Item = usize>) -> Vec<(usize, usize)> {
    let mut buckets = [0usize; 48];
    for s in sizes {
        let b = (usize::BITS - 1 - s.max(1).leading_zeros()) as usize;
        buckets[b.min(47)] += 1;
    }
    buckets
        .iter()
        .enumerate()
        .filter(|&(_, &n)| n != 0)
        .map(|(b, &n)| (1usize << b, n))
        .collect()
}

/// Render a `(2^i, count)` histogram compactly: `4K:12 512K:3892`.
pub fn format_log2_hist(hist: &[(usize, usize)]) -> String {
    fn unit(v: usize) -> String {
        const NAMES: [&str; 5] = ["", "K", "M", "G", "T"];
        let mut v = v;
        let mut i = 0;
        while v >= 1024 && i + 1 < NAMES.len() {
            v /= 1024;
            i += 1;
        }
        format!("{v}{}", NAMES[i])
    }
    hist.iter()
        .map(|&(lo, n)| format!("{}:{n}", unit(lo)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Allocate a zero-filled heap backing store of `capacity` bytes, reporting an
/// unsatisfiable *reservation* as such instead of as a bare allocator abort.
///
/// `vec![0u8; capacity]` is infallible: when the OS refuses the reservation,
/// `Vec` routes into `std::alloc::handle_alloc_error`, whose entire output is
///
/// ```text
/// memory allocation of 12884901888 bytes failed
/// note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
/// ```
///
/// with an empty stdout and no mention of `-Xmx`, of the heap, or of the
/// machine. That is actively misleading, and it has already cost real time: the
/// Tomcat non-passed census filed the two `*LargeHeap` classes as an allocator
/// defect on the strength of that line, when the harness runs them at
/// `-Xmx12g` by design and the box simply could not reserve 12 GiB while eight
/// shards were resident. Both pass at `-Xmx12g` on an idle box.
///
/// HotSpot says "Could not reserve enough space for object heap" and names the
/// size; so does this. `region` names which store failed, because a VM can
/// stand up several (young semi-spaces, G1's region array) and "which one" is
/// the first thing worth knowing.
///
/// Only the *reservation* path belongs here. A Java-level allocation that
/// cannot be satisfied inside an existing heap must still raise a catchable
/// `java.lang.OutOfMemoryError`, which it does — verified against
/// `TestByteChunkLargeHeap` at `-Xmx2g`:
/// `OutOfMemoryError: Java heap space (alloc_array length 1610612736)`.
pub fn alloc_zeroed_heap(capacity: usize, region: &str) -> Vec<u8> {
    if capacity == 0 {
        return Vec::new();
    }
    // `u8` is align-1, so the only way `from_size_align` can fail is a size
    // that rounds past `isize::MAX`; report that as the reservation failure it
    // effectively is rather than unwrapping.
    let layout = match std::alloc::Layout::from_size_align(capacity, 1) {
        Ok(l) => l,
        Err(_) => heap_reservation_failed(capacity, region),
    };
    // SAFETY: `layout` has non-zero size (the `capacity == 0` early return
    // above is the only zero case).
    let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
    if ptr.is_null() {
        heap_reservation_failed(capacity, region);
    }
    // SAFETY: `ptr` is a live allocation from the global allocator for exactly
    // `layout` — size `capacity`, align 1, which is `align_of::<u8>()` — and
    // `alloc_zeroed` initialised all `capacity` bytes, so every element is a
    // valid `u8`. Passing `capacity` as both length and capacity matches the
    // allocation exactly, so the `Vec`'s own deallocation reconstructs the same
    // layout. This is the allocation `vec![0u8; capacity]` would have made; the
    // only difference is that a null return is handled instead of aborting.
    unsafe { Vec::from_raw_parts(ptr, capacity, capacity) }
}

/// Report a heap reservation the OS refused, and exit.
///
/// Exits rather than panics: this runs before there is a Java thread to throw
/// on, and the state it replaces was already a process abort
/// (`handle_alloc_error`), so nothing that previously unwound stops doing so.
fn heap_reservation_failed(capacity: usize, region: &str) -> ! {
    let mib = capacity / (1024 * 1024);
    eprintln!("#");
    eprintln!("# There is insufficient memory for the Java Runtime Environment to continue.");
    eprintln!("# Could not reserve enough space for object heap ({region})");
    eprintln!("#   requested {capacity} bytes ({mib} MiB) — this size comes from -Xmx");
    eprintln!("#   The OS refused the reservation. Either lower -Xmx, or free physical");
    eprintln!("#   memory / page-file (commit charge) on this machine and retry.");
    eprintln!("#");
    std::process::exit(1);
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
        let data = crate::reservation::HeapStore::new(capacity, "arena");
        let mut a = Self {
            data,
            cursor: 0,
            free_small: (0..SMALL_BUCKETS).map(|_| Vec::new()).collect(),
            small_mask: [0u64; SMALL_MASK_WORDS],
            free_large: BTreeMap::new(),
            free_large_blocks: 0,
            max_free_upper: 0,
            free_bytes_total: 0,
            alloc_anchors: Vec::new(),
            anchor_shift: 0,
            prefer_bump: false,
            free_list_after_bump: 0,
            free_pushed: 0,
            coalesce_threshold: COALESCE_THRESHOLD_MIN,
            high_cursor: capacity,
            free_high: Vec::new(),
            high_max: 0,
            high_pushed: 0,
            high_reserve: 0,
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

    /// The epoch's anchors as a strictly-increasing offset list, WITHOUT
    /// consuming them (gc-genpause F1).
    ///
    /// [`Self::take_alloc_anchors`] is the sweep's reader and it drains, which
    /// is right for a path that owns the epoch. The moving young cycle's
    /// object-start walk wants the same grid a phase earlier and must not
    /// disturb it -- a moving cycle can still divert to the non-moving sweep
    /// mid-flight, and that sweep would then find the anchors gone and fall
    /// back to a full sequential walk for no reason.
    ///
    /// Same contents, same order, same `usize::MAX`-means-empty encoding.
    pub fn alloc_anchors_snapshot(&self) -> Vec<usize> {
        self.alloc_anchors
            .iter()
            .copied()
            .filter(|&v| v != usize::MAX)
            .collect()
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
        // A block from the large-object region published on a LOW tier is the
        // other half of the region tripwire: from there `alloc` will hand it to
        // a small object, and the split the two ends exist to enforce is gone.
        // `add_free_block` routes by region before it gets here; the direct
        // callers (split remainders) do not, which is why the check is here and
        // not only there.
        if self.is_high(block.offset) {
            warn_small_alloc_in_high_region(
                "route-low-listed-high-block",
                block.offset,
                block.size,
                self.high_cursor,
            );
        }
        if block.size < LARGE_BLOCK_MIN {
            let k = small_bucket_for(block.size);
            self.free_small[k].push(block);
            mask_set(&mut self.small_mask, k);
        } else {
            self.free_large.entry(block.size).or_default().push(block);
            self.free_large_blocks += 1;
        }
        self.free_bytes_total += block.size;
        // A new block may sit next to one already on the list.
        self.free_pushed += 1;
    }

    /// True when both tiers are empty.
    #[inline]
    fn free_is_empty(&self) -> bool {
        self.free_large.is_empty() && self.small_mask.iter().all(|&w| w == 0)
    }

    /// How many spans the free list holds, and how many distinct sizes they
    /// come in.
    ///
    /// The SHAPE of a fragmented heap, not just its size. `free_list_bytes` and
    /// `largest_free_block` together say "1.13 GiB free, biggest hole 65528" —
    /// which leaves open whether that is two dozen holes or twenty thousand,
    /// and those want completely different fixes. Reported by the ZGC
    /// allocation-failure guard for exactly that reason.
    pub fn free_span_shape(&self) -> (usize, usize) {
        (
            self.free_large_blocks + self.free_high.len(),
            self.free_large.len() + usize::from(!self.free_high.is_empty()),
        )
    }

    /// The EXACT largest span, or 0 when the tier is empty.
    ///
    /// The map is keyed by size, so this is its last key — no walk, and never
    /// stale. See the `large_max_exact` note on [`Self::free_large`] for the
    /// memo this replaced.
    #[inline]
    fn large_max(&self) -> usize {
        self.free_large.keys().next_back().copied().unwrap_or(0)
    }

    /// Remove one block from bucket `size`, dropping the bucket when it empties
    /// so [`Self::large_max`] and every `range` query stay exact.
    #[inline]
    fn take_from_bucket(&mut self, size: usize, idx: usize) -> FreeBlock {
        let list = self
            .free_large
            .get_mut(&size)
            .expect("bucket exists: the caller just found it");
        let block = list.swap_remove(idx);
        if list.is_empty() {
            self.free_large.remove(&size);
        }
        self.free_large_blocks -= 1;
        self.free_bytes_total -= block.size;
        block
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
    /// BEST fit over the span tier, in `O(log n)` — and a miss here is
    /// authoritative, which [`Self::alloc`] and
    /// [`Self::has_free_block_at_least`] rely on to tighten
    /// [`Self::max_free_upper`].
    ///
    /// Two arms, and the split is the whole soundness argument:
    ///
    /// * **`size + align - 1` and up.** Every block in such a bucket covers the
    ///   request plus the worst-case alignment padding, so the head of the
    ///   FIRST such bucket fits with no per-block test. Smallest-first, so this
    ///   is best fit and it stops manufacturing dust the way a first-fit scan
    ///   over a size-mixed Vec did.
    /// * **`[size, size + align - 1)`.** Only reachable for `align > 8`: every
    ///   block on this heap sits on the 8-byte object grid (`alloc` rounds, the
    ///   sweep only publishes 8-aligned spans, `warn_unaligned_block` fires at
    ///   any producer that breaks it), so for `align <= 8` the padding is zero
    ///   and the exact-size bucket is already covered by the arm above. When it
    ///   IS reachable the buckets in that window are searched per block, because
    ///   there the fit depends on the block's own offset — and they must be
    ///   searched, or a miss would stop being authoritative.
    ///
    /// This replaced an unbounded first-fit scan of a `Vec<FreeBlock>` whose
    /// justification ("this tier stays short") does not hold on a fragmented
    /// heap: see the `free_large` field doc for the 76%-of-CPU profile that
    /// found it.
    fn large_fit(
        &mut self,
        base: usize,
        size: usize,
        align: usize,
    ) -> Option<(usize, [Option<FreeBlock>; 2])> {
        let worst = size.checked_add(align - 1)?;
        // Arm 1 — a bucket that covers size + worst-case padding. Head fits.
        if let Some((&sz, list)) = self.free_large.range(worst..).next() {
            let block = list[list.len() - 1];
            let block_addr = base + block.offset;
            let padding = ((block_addr + align - 1) & !(align - 1)) - block_addr;
            debug_assert!(
                padding + size <= sz,
                "a bucket at or above size+align-1 must cover size plus any padding",
            );
            let block = self.take_from_bucket(sz, list.len() - 1);
            return Some(Self::split(block, padding, size));
        }
        // Arm 2 — the alignment window. Empty for every `align <= 8` caller.
        if worst > size {
            let candidate = self.free_large.range(size..worst).find_map(|(&sz, list)| {
                list.iter()
                    .position(|block| {
                        let block_addr = base + block.offset;
                        let padding = ((block_addr + align - 1) & !(align - 1)) - block_addr;
                        padding.checked_add(size).is_some_and(|need| need <= sz)
                    })
                    .map(|idx| (sz, idx))
            });
            if let Some((sz, idx)) = candidate {
                let block = self.free_large[&sz][idx];
                let block_addr = base + block.offset;
                let padding = ((block_addr + align - 1) & !(align - 1)) - block_addr;
                let block = self.take_from_bucket(sz, idx);
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
        //
        // SKIPPED ENTIRELY under `prefer_bump` — see that field. The retry below
        // the bump path is a strictly more thorough search, so skipping this
        // cannot fail an allocation that would otherwise have succeeded.
        if !self.prefer_bump && !self.free_is_empty() && alloc_size <= self.max_free_upper {
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
                self.note_region_leak("free-list", alloc_offset, alloc_size);
                return self.hand_out(alloc_offset, alloc_size);
            }
            // No fit anywhere, and BOTH tiers were viewed in full (the
            // segregated small tier has no scan budget to hide behind any
            // more), so the miss is a proof: tighten the bound to the exact
            // span-tier maximum folded with the small tier's ceiling.
            let large_max = self.large_max();
            let small_cap = self.small_max_ceil();
            self.max_free_upper = self.max_free_upper.min(large_max.max(small_cap));
        }

        // Bump-allocation path: align the cursor up (checked to prevent
        // overflow near usize::MAX).
        let aligned = self.cursor.checked_add(align - 1).map(|v| v & !(align - 1));
        let end = aligned.and_then(|a| a.checked_add(alloc_size));
        if let (Some(aligned), Some(end)) = (aligned, end) {
            // `high_cursor`, not `data.len()`: the two ends share one middle,
            // and bumping past the descending cursor would hand out memory the
            // large-object end has already allocated. On an arena that never
            // used the high end and set no reserve this is `data.len()`
            // verbatim.
            //
            // The reserve is subtracted here and NOT on the failure path below,
            // which is the whole of what "preference, not a wall" means: an
            // allocation that can be served any other way leaves the
            // large-object floor alone, and one that cannot takes it rather
            // than raising `OutOfMemoryError`.
            if end
                <= self
                    .high_cursor
                    .saturating_sub(self.remaining_high_reserve())
            {
                // COMMIT BEFORE HANDING OUT. See `commit_bump`: the two
                // cursors are the only places the arena's writable envelope
                // grows, so committing here is what makes every FREE-LIST
                // block below the cursor writable without a check of its own.
                let ptr = self.hand_out(aligned, end - aligned)?;
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
            if self.prefer_bump {
                // The nursery could not serve this and a hole below the floor
                // did. Counted, because that object is now in the old region and
                // no young cycle will reclaim it — see `prefer_bump`.
                self.free_list_after_bump += 1;
            }
            self.note_region_leak("free-list-retry", alloc_offset, alloc_size);
            return self.hand_out(alloc_offset, alloc_size);
        }

        // LAST RESORT BEFORE OOM: merge adjacent holes and look once more.
        //
        // Coalescing used to happen ONLY inside a collector's post-sweep hook
        // (`zgc::collect_garbage`, `gen_heap`'s non-moving sweep). Between two
        // sweeps the free list is mutated constantly by paths that MINT
        // adjacency and never merge it: every `split` remainder, and — on ZGC —
        // every TLAB retire, which hands back the unused tail of a chunk whose
        // used part the sweep has already free-listed object by object. So a
        // heap could sit on a free list that was *bytes-wise* enormous and
        // *block-wise* capped, and fail an allocation the merged list would
        // have served without collecting at all.
        //
        // That is not theoretical either. On the 2026-08-11 Azure Linux
        // Hibernate run, `sql.exec.SmokeTests` and
        // `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` both
        // died with `OutOfMemoryError: Java heap space` while the guard line
        // reported `free_list_bytes=1211378512 largest_free_block=65528` — 1.13
        // GiB free, no hole big enough for one 65552-byte `DFAState[8192]`.
        // Both classes pass on the same heap with the TLAB fast path disabled
        // (`CRATONVM_ZGC_TLAB=0`) and on G1, which evacuates.
        //
        // Placed HERE, on the path that has already exhausted both tiers and
        // the bump tail, so it costs nothing until the alternative is failing.
        // `coalesce_threshold`'s backoff keeps a hopeless request from
        // re-sorting the list once per attempt — see its field doc.
        if self.free_bytes_total >= alloc_size
            && self.free_pushed >= self.coalesce_threshold
            && self.coalesce_free_list() != 0
        {
            let base = self.data.as_ptr() as usize;
            let mut hit = self.small_fit(base, alloc_size, align);
            if hit.is_none() {
                hit = self.large_fit(base, alloc_size, align);
            }
            if let Some((alloc_offset, remainders)) = hit {
                for r in remainders.into_iter().flatten() {
                    self.push_block_routed(r);
                }
                self.note_region_leak("free-list-coalesced", alloc_offset, alloc_size);
                return self.hand_out(alloc_offset, alloc_size);
            }
        }

        // ABSOLUTELY last, in two steps. First take from the large-object
        // end's FREE LIST if it has anything: those bytes are already claimed
        // by the high end, so spending them costs the reserve nothing.
        //
        // THIS IS THE ARM THAT PUTS A SMALL OBJECT IN THE LARGE-OBJECT REGION,
        // and until 2026-08-29 it was the one arm of this function with no
        // region tripwire on it.
        //
        // The `bug-h2-testkillprocess-zgc-oom-at-97-percent-free` page carried
        // "the two small objects in the large-object region" as an open
        // residual for a week: the fragmentation report placed an 80-byte
        // `String` and a 24-byte `Object` above `high_cursor`, where
        // `ZGC_LARGE_OBJECT_MIN`'s design says only large objects live, and the
        // tripwire "fired ZERO times across a full failing run". It could not
        // have fired. `note_region_leak` sat on the three LOW free-list exits,
        // every one of which returns an offset below `high_cursor` by
        // construction, so all three were unfireable by definition — and the
        // only exit that can return a high offset had none. An untriggered
        // instrument reading zero is not evidence, which that page said about
        // this very counter; this is what it was missing.
        if !self.free_high.is_empty() {
            if let Some(off) = self.high_fit(alloc_size, align) {
                self.note_region_leak("high-free-list-last-resort", off, alloc_size);
                return self.hand_out(off, alloc_size);
            }
        }

        // Then, and only then, eat into the UNCLAIMED reserve rather than fail.
        //
        // The reserve is a preference, not a wall — a workload that allocates
        // no large objects must not be made to raise `OutOfMemoryError` over
        // space nothing was ever going to claim. But it is the last preference
        // to give up, after the free list, the un-reserved bump tail and the
        // merge have all said no. Placing it earlier (its first home) made the
        // reserve worthless: the low end reached it on ordinary TLAB churn and
        // drained the whole reserve straight through it.
        if self.high_reserve != 0 {
            let aligned = self.cursor.checked_add(align - 1).map(|v| v & !(align - 1));
            let end = aligned.and_then(|a| a.checked_add(alloc_size));
            if let (Some(aligned), Some(end)) = (aligned, end) {
                if end <= self.high_cursor {
                    let ptr = self.hand_out(aligned, end - aligned)?;
                    self.cursor = end;
                    return Some(ptr);
                }
            }
        }
        None
    }

    /// Is `offset` in the high (large-object) region?
    ///
    /// The test is a comparison against [`Self::high_cursor`] and nothing
    /// else, which is exact for every block that exists: a block is only ever
    /// published for memory that has been handed out, the high end only ever
    /// hands out memory at or above its cursor, and that cursor only descends.
    /// An arena that never called [`Self::alloc_high`] has
    /// `high_cursor == capacity`, so nothing is ever high — the low end
    /// behaves byte-for-byte as it did before the region existed.
    #[inline]
    /// Turn an offset into a pointer the caller may WRITE to, committing
    /// whatever granules the range needs.
    ///
    /// # Every hand-out goes through here, and that is the point
    ///
    /// The backing store commits lazily ([`crate::reservation`]), so a write
    /// into an uncommitted granule faults rather than reading zero. A first
    /// version committed only where the two CURSORS move, on the argument that
    /// every free-list block lies inside `[0, cursor)` or
    /// `[high_cursor, capacity)` and is therefore already committed.
    ///
    /// The argument is nearly true and the exception is a segfault:
    /// `compact_high_to` publishes vacated spans onto the high free list, and
    /// the high bump's descent leaves committed and uncommitted granules
    /// interleaved inside the region it has passed over. `large_array_survives_gc`
    /// found it as `STATUS_ACCESS_VIOLATION` -- which is what an induction over
    /// an allocator with two cursors and three free lists earns.
    ///
    /// So the obligation is LOCAL: nine hand-out sites, one helper, and the
    /// question "is this range writable?" is answered where the pointer is
    /// produced rather than three functions away. The cost is a granule-bitmap
    /// test -- a load and a not-taken branch for an already-committed small
    /// object -- against a free-list scan that has already happened.
    ///
    /// `None` means the OS refused the commit, and the caller must treat that
    /// as an allocation failure: returning the pointer anyway would hand out
    /// memory that faults on first write.
    #[must_use = "a refused commit must fail the allocation"]
    #[inline]
    fn hand_out(&mut self, offset: usize, size: usize) -> Option<*mut u8> {
        if !self.data.commit_range(offset, size) {
            return None;
        }
        // SAFETY: `commit_range` returned `true`, which it only does for a
        // range wholly inside the reservation -- so `offset + size` is in
        // bounds and the bytes are mapped read-write.
        Some(unsafe { self.data.as_mut_ptr().add(offset) })
    }

    /// Hand whole granules of `[lo, hi)` back to the OS.
    ///
    /// **The caller must have proved the span holds nothing live and is on no
    /// free list.** Both callers satisfy that by construction: they pass space
    /// a cursor has just retracted past, which is un-bumped by definition.
    fn decommit_span(&mut self, lo: usize, hi: usize) -> usize {
        if hi <= lo {
            return 0;
        }
        self.data.decommit_range(lo, hi - lo)
    }

    fn is_high(&self, offset: usize) -> bool {
        offset >= self.high_cursor
    }

    /// Report a LOW-path allocation that landed in the large-object region.
    ///
    /// One comparison against a field already in cache, on paths that are
    /// already off the bump fast path. An arena that never called
    /// [`Self::alloc_high`] has `high_cursor == capacity`, so this can never
    /// fire for the generational or G1 backends.
    #[inline]
    fn note_region_leak(&self, site: &str, offset: usize, size: usize) {
        if self.is_high(offset) {
            warn_small_alloc_in_high_region(site, offset, size, self.high_cursor);
        }
    }

    /// **Large-object allocation: bump DOWN from the top of the arena.**
    ///
    /// Same contract as [`Self::alloc`] — `size` bytes at `align`, `None` when
    /// it cannot be served — over the opposite end of the same buffer. See
    /// [`Self::high_cursor`] for why the two ends exist.
    ///
    /// A caller that gets `None` may still fall back to [`Self::alloc`]: the
    /// two ends share one middle, so "the high end cannot serve this" does not
    /// mean the low end cannot. ZGC does exactly that, because serving a large
    /// object out of the small-object end is merely bad for future
    /// fragmentation, while failing is an `OutOfMemoryError`.
    pub fn alloc_high(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        if size & 7 != 0 {
            warn_unaligned_block("alloc-high-size", self.high_cursor, size);
        }
        // Same grid rounding as `alloc`: whole 8-byte units, so split
        // remainders and both cursors stay on the object grid.
        let size = size.checked_add(7)? & !7;
        let alloc_size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

        if let Some(off) = self.high_fit(alloc_size, align) {
            return self.hand_out(off, alloc_size);
        }

        // Bump path: descend. Align DOWN, because the allocation's base is
        // what has to be aligned and the cursor is its end.
        if let Some(base) = self.high_cursor.checked_sub(alloc_size) {
            let base = base & !(align - 1);
            if base >= self.cursor {
                // The whole span the cursor passed over, not just this
                // object: the descent leaves `[base, old_high_cursor)` inside
                // the region, and `compact_high_to` may free-list any of it.
                let ptr = self.hand_out(base, self.high_cursor - base)?;
                self.high_cursor = base;
                return Some(ptr);
            }
        }

        // Last resort before failing: merge this end's adjacent holes and look
        // once more — the mirror of `alloc`'s last-resort merge, reached on the
        // same terms (both the free list and the bump space have said no).
        if self.high_pushed != 0 && self.coalesce_high() != 0 {
            if let Some(off) = self.high_fit(alloc_size, align) {
                return self.hand_out(off, alloc_size);
            }
        }
        None
    }

    /// Best fit over [`Self::free_high`]: the smallest block that covers the
    /// request plus its alignment padding. Removes the block, re-publishes the
    /// head and tail remainders, and returns the allocation offset.
    ///
    /// Best fit rather than first fit for the reason the low end's span tier
    /// gives: first fit over a size-mixed list carves large spans up for
    /// requests a smaller block would have served, and dust at THIS end is
    /// exactly the failure the region exists to prevent.
    fn high_fit(&mut self, alloc_size: usize, align: usize) -> Option<usize> {
        if self.free_high.is_empty() || alloc_size > self.high_max {
            return None;
        }
        let base = self.data.as_ptr() as usize;
        // (index, head padding, block size)
        let mut best: Option<(usize, usize, usize)> = None;
        for (i, block) in self.free_high.iter().enumerate() {
            let block_addr = base + block.offset;
            let padding = ((block_addr + align - 1) & !(align - 1)) - block_addr;
            let Some(need) = padding.checked_add(alloc_size) else {
                continue;
            };
            if need <= block.size && best.is_none_or(|(_, _, s)| block.size < s) {
                best = Some((i, padding, block.size));
            }
        }
        let (idx, padding, _) = best?;
        let block = self.free_high.swap_remove(idx);
        if block.size >= self.high_max {
            // The block taken may have been the maximum; re-derive rather than
            // leave a bound that over-states what is available. This list is
            // short by construction — see the field doc.
            self.high_max = self.free_high.iter().map(|b| b.size).max().unwrap_or(0);
        }
        self.free_bytes_total -= block.size;
        let (offset, remainders) = Self::split(block, padding, alloc_size);
        for r in remainders.into_iter().flatten() {
            self.push_high(r);
        }
        Some(offset)
    }

    /// Publish a block to the high end's free list.
    fn push_high(&mut self, block: FreeBlock) {
        if block.size == 0 {
            return;
        }
        if (block.offset | block.size) & 7 != 0 {
            warn_unaligned_block("route-high", block.offset, block.size);
        }
        self.high_max = self.high_max.max(block.size);
        self.free_bytes_total += block.size;
        self.high_pushed += 1;
        self.free_high.push(block);
    }

    /// Merge adjacent holes at the high end. Returns blocks removed.
    ///
    /// Separate from [`Self::coalesce_free_list`] rather than folded into it,
    /// so that no merge can ever produce a block straddling
    /// `cursor == high_cursor` once the two ends meet. Such a block would be
    /// genuinely contiguous free memory, but it would also be routed by its own
    /// offset into ONE of the two lists — silently moving the partition, and
    /// with it the region test every other method here relies on.
    pub fn coalesce_high(&mut self) -> usize {
        self.high_pushed = 0;
        if self.free_high.len() < 2 {
            return 0;
        }
        let before = self.free_high.len();
        let mut v: Vec<(usize, usize)> =
            self.free_high.iter().map(|b| (b.offset, b.size)).collect();
        sort_by_offset(&mut v);
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(before);
        for (off, sz) in v {
            if let Some(last) = merged.last_mut() {
                let last_end = last.0 + last.1;
                if off <= last_end {
                    last.1 = last_end.max(off + sz) - last.0;
                    continue;
                }
            }
            merged.push((off, sz));
        }
        if merged.len() == before {
            return 0;
        }
        let removed = before - merged.len();
        for b in self.free_high.drain(..) {
            self.free_bytes_total -= b.size;
        }
        self.high_max = 0;
        for (off, sz) in merged {
            self.push_high(FreeBlock {
                offset: off,
                size: sz,
            });
        }
        self.high_pushed = 0;
        removed
    }

    /// Un-bump a wholly-free head of the high region: the mirror of
    /// [`Self::retract_cursor_into_free_tail`].
    ///
    /// The low end's version exists because objects die young, so the top of
    /// its bump region is usually all garbage. The same is true here from the
    /// other side — a burst of large buffers that all die leaves the LOWEST
    /// part of the high region (the most recently bumped) free — and without
    /// this the downward cursor is the one-way ratchet the upward one was
    /// before it got its retraction: once a process has cumulatively allocated
    /// its capacity, every later request must fit an existing hole however
    /// little is live.
    ///
    /// Callers must coalesce first, so the lowest block is already maximal.
    /// Returns the bytes handed back (0 when a live object sits at the bottom
    /// of the region, the ordinary case).
    pub fn retract_high_cursor_into_free_head(&mut self) -> usize {
        let Some((idx, block)) = self
            .free_high
            .iter()
            .enumerate()
            .min_by_key(|(_, b)| b.offset)
            .map(|(i, b)| (i, *b))
        else {
            return 0;
        };
        if block.size == 0 || block.offset != self.high_cursor {
            return 0;
        }
        // Refuse to retract below the reserve, and this is the whole reason the
        // check exists rather than a tidiness bound.
        //
        // Retracting moves bytes out of `free_high` into the SHARED middle,
        // where the low end can take them — and the low end, on a thread-heavy
        // workload, is carving 512 KiB TLAB chunks continuously. Measured on
        // `TestNonBlockingAPI` with this ungated: the large-object region
        // claimed 92 MB, then every sweep handed its freed head back to the
        // middle, the low end took it, and the region reached the failing
        // allocation holding **3,224 free bytes** — drained to its own live
        // set, one sweep at a time. The low end's own retraction has no such
        // hazard because there is no third party to lose the bytes to.
        //
        // So retraction survives only for what it is actually good for: a
        // region that over-claimed relative to its floor handing the excess
        // back.
        //
        // Partial, not all-or-nothing: hand back exactly the excess. A merged
        // free head is frequently larger than the excess, and refusing the
        // whole thing would mean a region that over-claimed early never gave
        // any of it back.
        let region = self.data.len() - self.high_cursor;
        let give = region.saturating_sub(self.high_reserve).min(block.size) & !7;
        if give == 0 {
            return 0;
        }
        // Those bytes are becoming un-bumped space; leaving them listed would
        // hand them out twice. The part that stays reserved stays listed.
        self.free_high.swap_remove(idx);
        self.free_bytes_total -= block.size;
        self.high_max = self.free_high.iter().map(|b| b.size).max().unwrap_or(0);
        let old_high = self.high_cursor;
        self.high_cursor += give;
        // As the low retraction: `[old_high, high_cursor)` is un-bumped again
        // and off the free list, so its whole granules go back to the OS.
        self.decommit_span(old_high, self.high_cursor);
        if give < block.size {
            self.push_high(FreeBlock {
                offset: block.offset + give,
                size: block.size - give,
            });
        }
        give
    }

    /// The high region's downward bump cursor. Diagnostics and tests.
    pub fn high_cursor(&self) -> usize {
        self.high_cursor
    }

    /// Set the large-object bump reserve. See [`Self::high_reserve`].
    ///
    /// Clamped to a quarter of capacity: past that the reserve stops being a
    /// floor under one population and becomes a ceiling over the other.
    pub fn set_high_reserve(&mut self, bytes: usize) {
        self.high_reserve = bytes.min(self.data.len() / 4);
    }

    /// How much of [`Self::high_reserve`] the high end has not yet claimed.
    ///
    /// Subtracting what it has already bumped is what makes the reserve a
    /// FLOOR on the region rather than a permanent tax: once the large-object
    /// end holds `high_reserve` bytes of its own, the reserve is zero and the
    /// low end may bump right up to it again.
    #[inline]
    fn remaining_high_reserve(&self) -> usize {
        self.high_reserve
            .saturating_sub(self.data.len() - self.high_cursor)
    }

    /// Current size of the large-object region (`capacity - high_cursor`).
    /// Diagnostic/test helper — `capacity` and `high_cursor` say the same
    /// thing but read as arithmetic at every call site.
    pub fn high_region_bytes(&self) -> usize {
        self.data.len() - self.high_cursor
    }

    /// The exact largest block on the LOW end alone.
    ///
    /// [`Self::largest_free_block`] folds in the large-object end, which is the
    /// right answer for "what could this arena hand out" and the wrong one for
    /// a caller sizing a small-object allocation: acting on a high-end block's
    /// size would size a TLAB chunk that the low end then cannot serve.
    pub fn largest_low_free_block(&self) -> usize {
        let large = self.large_max();
        if large > 0 {
            large
        } else {
            self.small_max_floor()
        }
    }

    /// How many LOW free blocks are at least `size` bytes, counted up to `cap`.
    ///
    /// Not `has_free_block_at_least(size)` with a different name. That answers
    /// "can the arena serve one request of this size"; this answers "would it
    /// still be able to after somebody took one", which is the question a
    /// caller about to CONSUME the largest block has to ask and the boolean
    /// cannot.
    ///
    /// LOW tier only, and `size` is expected at or above [`LARGE_BLOCK_MIN`] —
    /// the large tier is a `BTreeMap` keyed by size, so the range walk touches
    /// only the buckets that qualify and stops at `cap`. A `size` below the
    /// boundary would need the small tier too and is not what this exists for;
    /// it saturates at `cap` rather than lying, so a caller asking about a
    /// small size gets a conservative answer rather than a wrong one.
    pub fn low_free_blocks_at_least(&self, size: usize, cap: usize) -> usize {
        let mut n = 0usize;
        for (_, blocks) in self.free_large.range(size..) {
            n += blocks.len();
            if n >= cap {
                return cap;
            }
        }
        n
    }

    /// Bytes the LOW bump may still take without eating the large-object
    /// reserve — i.e. the headroom [`Self::alloc`]'s ordinary bump path has
    /// before the last-resort arm at the bottom of that function starts
    /// spending the reserve to avoid an `OutOfMemoryError`.
    ///
    /// Published because the TLAB refill needs it to make a decision the free
    /// list alone cannot inform. "Is there a recycled chunk worth taking?" has
    /// a different answer depending on what the ALTERNATIVE is: with headroom,
    /// the alternative is a clean full-size bump and a short chunk is mere
    /// churn; without it, the alternative is eating the reserve, and a short
    /// chunk is then strictly better than the large-object end losing the space
    /// it was promised. See `zgc::recycled_chunk_size`.
    pub fn low_bump_headroom(&self) -> usize {
        self.high_cursor
            .saturating_sub(self.remaining_high_reserve())
            .saturating_sub(self.cursor)
    }

    /// Bytes of [`Self::high_reserve`] the large-object end has not claimed
    /// yet — i.e. how much of the shared middle is spoken for.
    ///
    /// Published because a GC trigger sized against the WHOLE arena's un-bumped
    /// tail cannot see it, and that is the difference between collecting while
    /// the reserve is intact and collecting after the small-object end has
    /// bumped straight through it.
    pub fn unclaimed_high_reserve(&self) -> usize {
        self.remaining_high_reserve()
    }

    /// Free-list state of the large-object end: `(blocks, bytes, largest)`.
    /// Diagnostics only — the allocation-failure report prints it beside the
    /// low end's, because "the split is in place" and "the split has any space
    /// in it" are different claims and only the second one predicts an OOM.
    pub fn high_free_shape(&self) -> (usize, usize, usize) {
        (
            self.free_high.len(),
            self.free_high.iter().map(|b| b.size).sum(),
            self.high_max,
        )
    }

    /// Merge adjacent (and defensively overlapping) free blocks into maximal
    /// spans. Returns the number of blocks the merge removed — `0` means the
    /// list was already maximal, or nothing has changed since the last merge.
    ///
    /// This is the *shared* implementation of what every non-moving collector
    /// in this crate has to do after a sweep: the sweep returns one
    /// object-sized hole per dead object, and a heap that never merges them
    /// can only ever serve object-sized requests again. It is also called from
    /// [`Self::alloc`]'s last-resort arm, so an allocation never fails while
    /// the bytes are present and merely split.
    ///
    /// A call with nothing pushed since the last one is free: the list it
    /// produced was maximal and nothing has touched it.
    ///
    /// Every call also re-aims [`Self::coalesce_threshold`], the backoff
    /// `Arena::alloc`'s last-resort arm consults — see that field.
    pub fn coalesce_free_list(&mut self) -> usize {
        if self.free_pushed == 0 {
            return 0;
        }
        self.free_pushed = 0;
        // LOW blocks only. The high end merges through `coalesce_high`, and
        // keeping the two apart is what guarantees no span can straddle the
        // point where the two cursors meet — see `coalesce_high`.
        let sorted = self.low_blocks_sorted();
        if sorted.len() < 2 {
            return 0;
        }
        let before = sorted.len();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(before);
        for (off, sz) in sorted {
            if let Some(last) = merged.last_mut() {
                let last_end = last.0 + last.1;
                if off <= last_end {
                    // Adjacent or overlapping: extend to the farther end so no
                    // span is ever double-served.
                    let new_end = last_end.max(off + sz);
                    last.1 = new_end - last.0;
                    continue;
                }
            }
            merged.push((off, sz));
        }
        if merged.len() == before {
            // Nothing was adjacent: this heap's holes are walled by live data,
            // not merely split. Ask for four times as much churn before paying
            // for the next sort.
            self.coalesce_threshold = self
                .coalesce_threshold
                .saturating_mul(4)
                .clamp(COALESCE_THRESHOLD_MIN, COALESCE_THRESHOLD_MAX);
            return 0;
        }
        let removed = before - merged.len();
        self.clear_low_free_list();
        for (off, sz) in merged {
            self.add_free_block(off, sz);
        }
        // The rebuild above counted every re-added block through
        // `add_free_block`; the list it produced IS maximal, so zero it back.
        self.free_pushed = 0;
        // Merging paid: be willing to look again immediately.
        self.coalesce_threshold = COALESCE_THRESHOLD_MIN;
        removed
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
            offset + size <= self.cursor || self.is_high(offset),
            "free block must lie within one of the two bump regions",
        );
        if size == 0 {
            return;
        }
        if (offset | size) & 7 != 0 {
            warn_unaligned_block("add_free_block", offset, size);
        }
        // Region routing. A reclaimed span goes back to the end it was carved
        // from, and `max_free_upper` deliberately does NOT see the high end:
        // that bound exists to let the LOW tiers skip their scans, the high
        // end has its own exact `high_max`, and folding the two would make a
        // large free buffer at the top cause pointless small-tier scans on
        // every small allocation.
        if self.is_high(offset) {
            self.push_high(FreeBlock { offset, size });
            return;
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
        self.free_large_blocks = 0;
        self.max_free_upper = 0;
        self.free_high.clear();
        self.high_max = 0;
        self.high_pushed = 0;
        self.free_bytes_total = 0;
        // An empty list holds no adjacency.
        self.free_pushed = 0;
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
    /// CRATONVM-SPRING-GENUINE-BUGLIST. The
    /// epoch-gated cache below makes repeated calls between real changes
    /// O(1); the summation itself is unchanged (same tiers, same order),
    /// so a cache miss recomputes byte-identically to the old behavior.
    /// Serve from the bump cursor before the free list — see [`Self::prefer_bump`].
    pub fn set_prefer_bump(&mut self, on: bool) {
        self.prefer_bump = on;
    }

    /// Is bump-first on?
    pub fn prefer_bump(&self) -> bool {
        self.prefer_bump
    }

    /// Allocations that fell through the bump tail to the free list while
    /// bump-first was on — see [`Self::prefer_bump`].
    pub fn free_list_after_bump(&self) -> usize {
        self.free_list_after_bump
    }

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
        let large = self.large_max();
        // Any span is >= LARGE_BLOCK_MIN, which is larger than every block in
        // the small tier by construction — so a non-zero span maximum is the
        // overall maximum of the LOW end and the small tier need not be
        // consulted. The high end is a separate list and is always folded in:
        // every caller of this is asking "what is the biggest thing this arena
        // could hand out", and after the two-ended split that answer is
        // frequently a large-object hole.
        let low = if large > 0 {
            large
        } else {
            self.small_max_floor()
        };
        low.max(self.high_max)
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
            return !self.free_is_empty() || !self.free_high.is_empty();
        }
        // The high end first: `high_max` is exact, so this is one compare and
        // it cannot be answered by the low-tier bound below (which deliberately
        // never sees high blocks — see `add_free_block`).
        if self.high_max >= size {
            return true;
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
            // O(log n) and exact: the map is keyed by size, so "is there a span
            // at least this big" is one range probe and the maximum is the last
            // key. This used to be a full walk of the span tier, whose miss then
            // had to publish the maximum it happened to observe.
            if self.free_large.range(size..).next().is_some() {
                return true;
            }
            let scan_max = self.large_max();
            // The small tier caps below LARGE_BLOCK_MIN <= size; fold in its
            // (bitmap-derived) ceiling rather than scanning it.
            let ceil = self.small_max_ceil();
            self.max_free_upper = scan_max.max(ceil);
            false
        }
    }

    /// Lay the free list out across the arena and measure what stands between
    /// its holes, against one target `request`.
    ///
    /// **Failure path only.** This sorts the entire free list (the same
    /// `free_blocks_sorted` a sweep pays for once a cycle) and then makes a
    /// few linear passes over it, so it costs what a sweep's merge costs.
    /// Every caller is one step from raising `OutOfMemoryError`.
    ///
    /// The interesting output is [`FragProfile::cheapest`]: the contiguous
    /// window of arena that would serve `request` while displacing the fewest
    /// live bytes. `wall_bytes == 0` there would mean the free list was merely
    /// split and a coalesce would have served the request; a small
    /// `wall_bytes` over a small `walls` count means a handful of survivors is
    /// holding the whole window hostage; a `wall_bytes` comparable to
    /// `request` means the region really is a live/dead mosaic and only
    /// relocation can serve it. Those three want completely different fixes,
    /// and `largest_free_block < request` cannot tell them apart.
    pub fn frag_profile(&self, request: usize) -> FragProfile {
        let blocks = self.free_blocks_sorted();
        let mut profile = FragProfile {
            cursor: self.cursor,
            capacity: self.data.len(),
            free_bytes: self.free_bytes_total,
            spans: blocks.len(),
            largest_span: self.largest_free_block(),
            span_hist: log2_hist(blocks.iter().map(|&(_, s)| s)),
            ..FragProfile::default()
        };
        if blocks.is_empty() {
            return profile;
        }
        // `gap[k]` is the occupied run between block `k` and block `k + 1`.
        // Adjacent spans (a list the coalescer has not merged yet) give a zero
        // gap, which is deliberately not counted as a wall — it is not
        // standing in anybody's way.
        let gaps: Vec<usize> = blocks
            .windows(2)
            .map(|w| w[1].0.saturating_sub(w[0].0 + w[0].1))
            .collect();
        profile.walls = gaps.iter().filter(|&&g| g != 0).count();
        profile.wall_bytes = gaps.iter().sum();
        profile.wall_hist = log2_hist(gaps.iter().copied().filter(|&g| g != 0));

        // Cheapest window. `blocks[i].0 .. blocks[j].0 + blocks[j].1` is one
        // contiguous stretch of arena; it can hold `request` once it is that
        // wide, and the price of using it is every occupied byte strictly
        // inside it. Widening a window can only add wall bytes, so for each
        // left edge the FIRST `j` that reaches `request` is also the cheapest,
        // and `j` only ever moves right as `i` does — one pass, not a
        // quadratic search.
        //
        // Both quantities come from prefix sums rather than incremental
        // add/remove bookkeeping: the wall bytes inside a window are its total
        // width minus its free bytes, which needs no per-edge fixups to stay
        // correct.
        if request != 0 {
            // `free_prefix[k]` = free bytes in `blocks[..k]`.
            let mut free_prefix = Vec::with_capacity(blocks.len() + 1);
            free_prefix.push(0usize);
            for &(_, s) in &blocks {
                let last = *free_prefix.last().expect("seeded with 0");
                free_prefix.push(last + s);
            }
            // `wallcount_prefix[k]` = non-zero gaps in `gaps[..k]`.
            let mut wallcount_prefix = Vec::with_capacity(gaps.len() + 1);
            wallcount_prefix.push(0usize);
            for &g in &gaps {
                let last = *wallcount_prefix.last().expect("seeded with 0");
                wallcount_prefix.push(last + usize::from(g != 0));
            }
            let mut best: Option<FragWindow> = None;
            let mut j = 0usize;
            for i in 0..blocks.len() {
                if j < i {
                    j = i;
                }
                while j < blocks.len()
                    && (blocks[j].0 + blocks[j].1).saturating_sub(blocks[i].0) < request
                {
                    j += 1;
                }
                if j >= blocks.len() {
                    // No window with this left edge is wide enough, and every
                    // later left edge starts further right, so none is either.
                    break;
                }
                let start = blocks[i].0;
                let end = blocks[j].0 + blocks[j].1;
                let free_bytes = free_prefix[j + 1] - free_prefix[i];
                let cand = FragWindow {
                    start,
                    end,
                    free_bytes,
                    wall_bytes: (end - start).saturating_sub(free_bytes),
                    walls: wallcount_prefix[j] - wallcount_prefix[i],
                };
                if best.is_none_or(|b| (cand.wall_bytes, cand.walls) < (b.wall_bytes, b.walls)) {
                    best = Some(cand);
                }
            }
            profile.cheapest = best;
        }
        profile
    }

    /// The LOW end's blocks as `(offset, size)`, sorted by ascending offset.
    ///
    /// The low-region twin of [`Self::free_blocks_sorted`]. Used by the two
    /// operations that rebuild the low list ([`Self::coalesce_free_list`],
    /// [`Self::retract_cursor_into_free_tail`]) — both of which must not see a
    /// high block, because both re-publish what they read through
    /// [`Self::add_free_block`], and a merge that straddled the two ends would
    /// be re-routed into one of them by its own offset alone.
    fn low_blocks_sorted(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self
            .free_small
            .iter()
            .flatten()
            .chain(self.free_large.values().flatten())
            .map(|b| (b.offset, b.size))
            .collect();
        sort_by_offset(&mut v);
        v
    }

    /// Drop every LOW-end block, leaving the high end untouched. The rebuild
    /// half of the two low-list operations above.
    fn clear_low_free_list(&mut self) {
        let dropped: usize = self
            .free_small
            .iter()
            .flatten()
            .chain(self.free_large.values().flatten())
            .map(|b| b.size)
            .sum();
        self.free_bytes_total -= dropped;
        for bucket in self.free_small.iter_mut() {
            bucket.clear();
        }
        self.small_mask = [0u64; SMALL_MASK_WORDS];
        self.free_large.clear();
        self.free_large_blocks = 0;
        self.max_free_upper = 0;
        self.free_pushed = 0;
    }

    /// Snapshot of the current free list as `(offset, size)` pairs,
    /// sorted by ascending offset. Used by the non-moving sweep's object
    /// walker to skip holes the same way `OldGen::walk_objects` does.
    pub fn free_blocks_sorted(&self) -> Vec<(usize, usize)> {
        let mut v: Vec<(usize, usize)> = self
            .free_small
            .iter()
            .flatten()
            .chain(self.free_large.values().flatten())
            .chain(self.free_high.iter())
            .map(|b| (b.offset, b.size))
            .collect();
        sort_by_offset(&mut v);
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
    /// Drop the low bump cursor to `new_cursor` after an external compaction
    /// has slid every low-end survivor below it. Returns the bytes reclaimed.
    ///
    /// # Why this is not `retract_cursor_into_free_tail`
    ///
    /// That method is careful and incremental: it hands back only a free span
    /// that ends exactly AT the cursor, because on a non-moving heap anything
    /// else may still be live. This one is the compacting twin — the caller
    /// asserts that everything above `new_cursor` at the low end is now dead,
    /// which is a claim only a relocator that has just moved the survivors can
    /// make. It is `pub(crate)` so that claim stays inside this crate.
    ///
    /// `touched` is the half-open offset range the slide actually WROTE into --
    /// its destination window. Free blocks that overlap it are dropped, because
    /// the slide places survivors without consulting this list and may have put
    /// one on top of a hole. Everything else below `new_cursor` is kept.
    ///
    /// # The list used to be dropped wholesale, and that was the churn OOM
    ///
    /// The premise was "after a slide every low hole is inside the reclaimed
    /// span by construction, so a surviving entry would name bytes that are now
    /// un-bumped tail and would hand them out twice". True of a slide that
    /// compacts the whole low region; this one compacts the pages the
    /// relocation-set selector picked, into `[slide_floor, dest)`, and leaves
    /// every other byte exactly where it was. A hole outside that window is a
    /// real free block below the cursor, and dropping it loses the memory
    /// permanently -- the sweep only ever free-lists objects that DIE, so a
    /// hole that was already free when the slide ran is never re-discovered.
    ///
    /// Measured on `repros/frag-churn` (512 MiB heap, ~2 MiB live): the sweep
    /// free-listed 400 MB, the slide cleared it, and from then on the bump
    /// cursor advanced by EXACTLY the bytes allocated -- not one byte came from
    /// the free list -- until it reached capacity and the VM threw
    /// `OutOfMemoryError` with 99% of the heap dead. It is also why the arm
    /// with the JIT ON survives: `relocate_stw` declines to relocate while a
    /// compiled frame is live, so on that arm `compact_low_to` is barely
    /// called and the free list is left alone.
    ///
    /// The **high end is untouched**. Large objects live above `high_cursor`
    /// with their own free list, and this compaction does not move them; that
    /// is a deliberate first cut, not an oversight — see
    /// `ZgcRealHeap::relocate_stw`.
    /// Bytes of the low region a compaction may consider -- the bump cursor.
    ///
    /// Not [`Self::used`], which folds in the large-object region at the other
    /// end and would send a compactor walking addresses above `high_cursor`.
    pub(crate) fn used_low_for_compaction(&self) -> usize {
        self.cursor
    }

    /// `vacated` is the third argument and the reason it exists is the whole
    /// of `Follow-up 2026-08-29` on
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821.md`:
    /// **the space a slide empties is only reclaimed when the CURSOR can drop
    /// to it, and otherwise it was lost forever.**
    ///
    /// The sweep free-lists dead objects by walking the object-start REGISTRY.
    /// A slide rebuilds that registry with the survivors' NEW bases, so the old
    /// ones are gone from it and no later sweep can ever discover them. If
    /// anything live sits above the compacted region — one survivor on an
    /// unselected dense page is enough — `new_cursor` is pinned above the
    /// vacated span, the span is neither below the cursor nor on the free list,
    /// and it is invisible to the allocator for the rest of the process.
    ///
    /// Measured on `org.h2.test.store.TestMVStoreTool` at `--Xmx 1g`: four
    /// compaction cycles relocated **885 793 objects** and the largest free
    /// block at the failing 262 160-byte request was **8 184 bytes**. The
    /// slide's own output is 2 MiB-granular and contiguous by construction,
    /// which is exactly the shape that request needed and exactly what was
    /// being thrown away.
    ///
    /// So the caller now names the offset spans it emptied, and they are added
    /// to the free list here. A kept block that overlaps one is dropped rather
    /// than kept beside it: the span is a superset of that dead space, and
    /// publishing both would hand the same bytes out twice.
    pub(crate) fn compact_low_to(
        &mut self,
        new_cursor: usize,
        touched: std::ops::Range<usize>,
        vacated: &[(usize, usize)],
    ) -> usize {
        assert!(
            new_cursor <= self.cursor,
            "compaction must not raise the cursor: {new_cursor} > {}",
            self.cursor
        );
        let reclaimed = self.cursor - new_cursor;
        // Zero the vacated span. A slid-down survivor leaves its old bytes
        // behind verbatim, including a valid-looking `ObjectHeader`, and a
        // conservative scanner that met one would resurrect a corpse.
        self.data.fill_zero(new_cursor, self.cursor);
        let old_cursor = self.cursor;
        self.cursor = new_cursor;
        // Keep the holes the slide did not write into. A block is dropped if it
        // overlaps the destination window (a survivor may be sitting on it) or
        // reaches above the new cursor (those bytes are un-bumped tail now, and
        // serving them from both the list and the cursor is the double-hand-out
        // the wholesale clear was guarding against). A block that straddles the
        // cursor is truncated rather than dropped.
        let keep: Vec<(usize, usize)> = self
            .low_blocks_sorted()
            .into_iter()
            .filter_map(|(off, size)| {
                if off >= new_cursor {
                    return None;
                }
                let size = size.min(new_cursor - off);
                if size == 0 {
                    return None;
                }
                let overlaps_touched = off < touched.end && touched.start < off + size;
                // ...and a block inside a span the caller is about to publish
                // is superseded by it. By construction there are no straddlers
                // to worry about (a block starting below `touched.end` is
                // already dropped above, and every span starts at or above it),
                // so an overlap here is a containment; dropping on the weaker
                // test costs at most a few bytes and can never double-publish.
                let inside_vacated = vacated
                    .iter()
                    .any(|&(s, e)| off < e && s < off + size);
                (!overlaps_touched && !inside_vacated).then_some((off, size))
            })
            .collect();
        self.clear_low_free_list();
        for (off, size) in keep {
            self.add_free_block(off, size);
        }
        for &(s, e) in vacated {
            // Clamp: bytes at or above the new cursor are un-bumped tail now,
            // and serving them from both the list and the cursor is the
            // double-hand-out this function's own history records.
            let e = e.min(new_cursor);
            if e > s {
                // ZERO IT FIRST, for the reason the span above `new_cursor` is
                // zeroed and the reason the sweep zeroes a dead object's header
                // before free-listing it: a slid-away survivor leaves its old
                // bytes behind verbatim, including a valid-looking
                // `ObjectHeader`, and a conservative scanner that met one would
                // resurrect a corpse. The whole span rather than the headers,
                // because unlike the sweep this pass does not know where inside
                // it the object grid fell.
                self.data.fill_zero(s, e);
                self.add_free_block(s, e - s);
            }
        }
        // Every recorded low object start just moved.
        self.clear_alloc_anchors();
        // THE VACATED TAIL IS NOT HANDED BACK HERE, and the reason is a
        // collision worth stating rather than a limit of the arena.
        //
        // `ZgcRealHeap::stamp_forwarding_words` writes a forwarding record into
        // exactly this span -- `[new_cursor, old_cursor)` is the only part of
        // the vacated region a slide can leave a record in, because everything
        // below the new cursor now holds a different live object. Decommitting
        // it here destroys those records the instant they are written, and on
        // Windows a later read of one is a `STATUS_ACCESS_VIOLATION` rather
        // than a miss. That is not hypothetical: it is what the first version
        // of this line did, and `a_compiled_frame_forbids_relocation_only_...`
        // found it.
        //
        // So the give-back moves to `Arena::decommit_unbumped_middle`, which
        // the collector calls at the START of the NEXT collection -- by which
        // point the records have served their purpose. A stale reference that
        // survives a whole collection is unrepairable anyway, which is the same
        // reasoning `ZgcRealHeap::prune_relocations` already applies to the
        // table beside them.
        let _ = old_cursor;
        reclaimed
    }

    /// Publish the result of an external slide over the LARGE-OBJECT end.
    ///
    /// The mirror of [`Self::compact_low_to`], and the geometry is mirrored
    /// too: the high region bumps DOWN from `capacity`, so compacting it packs
    /// survivors against the TOP and leaves its holes at the bottom.
    ///
    /// Two arguments rather than one cursor, because the high end cannot be
    /// described by a cursor the way the low end can:
    ///
    /// * `floor` — the lowest offset the caller's slide is AUTHORITATIVE about.
    ///   At or above it, this method owns the free list; below it, nothing has
    ///   changed and every existing block is kept. It is `high_cursor` when the
    ///   slide walked the whole region, and the floor of the packed region when
    ///   the slide stopped early on a header it could not size.
    /// * `vacated` — the offset spans inside `[floor, capacity)` that the slide
    ///   emptied. Every byte in them is dead by the caller's assertion, a claim
    ///   only a relocator that has just moved every survivor out of them can
    ///   make; every other byte at or above `floor` is a live object or the
    ///   grid padding beside one. A LIST rather than a single span because one
    ///   pinned large object splits the region in two, and dropping the free
    ///   space on the far side of it would lose that memory permanently — the
    ///   failure [`Self::compact_low_to`]'s own history records.
    ///
    /// Both are `pub(crate)`-only claims, which is why this is not `pub`.
    ///
    /// Returns the bytes that were live-object space before the slide and are
    /// free after it. Bytes already on `free_high` are not counted: they were
    /// free before and are free after, merely contiguous — and contiguity is
    /// what this exists for, not reclaim.
    ///
    /// # The bytes stay on THIS end's free list
    ///
    /// Deliberately not `high_cursor += hole`. Raising the cursor moves the
    /// bytes into the SHARED MIDDLE, where the low end carves TLAB chunks out
    /// of them — the drain [`Self::retract_high_cursor_into_free_head`]
    /// documents (`TestNonBlockingAPI` reached its failing allocation with the
    /// large-object region holding 3,224 free bytes, handed back one sweep at a
    /// time). Publishing merged blocks instead leaves the span where only large
    /// objects can take it, and `high_max` becomes the whole span, which is the
    /// number [`Self::alloc_high`]'s best fit consults. A caller that wants the
    /// excess handed back has the retraction, whose reserve policy already
    /// decides how much.
    pub(crate) fn compact_high_to(&mut self, floor: usize, vacated: &[(usize, usize)]) -> usize {
        assert!(
            floor >= self.high_cursor,
            "high compaction floor is below the high cursor: {floor} < {}",
            self.high_cursor
        );
        assert!(
            floor <= self.data.len(),
            "high compaction floor past capacity: {floor} > {}",
            self.data.len()
        );
        // Zero every vacated span, for the reason `compact_low_to` gives: a
        // slid-away survivor leaves its old bytes behind verbatim, including a
        // valid-looking `ObjectHeader`, and a conservative scanner that met one
        // would resurrect a corpse.
        for &(start, end) in vacated {
            assert!(
                start >= floor && end <= self.data.len() && start <= end,
                "vacated span {start}..{end} is outside the authoritative region \
                 {floor}..{}",
                self.data.len()
            );
            self.data.fill_zero(start, end);
        }
        // Rebuild this end's free list: everything wholly below `floor` is
        // kept verbatim, a straddler is truncated to its part below it, and
        // everything else is replaced by `vacated` — because at or above
        // `floor` a pre-existing block either lies inside a span the slide just
        // emptied (so `vacated` already names it) or names bytes a survivor was
        // packed into (so keeping it would hand out occupied memory).
        let old_total: usize = self.free_high.iter().map(|b| b.size).sum();
        let mut kept: Vec<FreeBlock> = Vec::with_capacity(self.free_high.len());
        for b in self.free_high.drain(..) {
            if b.offset >= floor {
                continue;
            }
            let size = b.size.min(floor - b.offset);
            if size != 0 {
                kept.push(FreeBlock {
                    offset: b.offset,
                    size,
                });
            }
        }
        self.free_bytes_total -= old_total;
        self.high_max = 0;
        self.high_pushed = 0;
        let mut new_total = 0usize;
        for b in kept {
            new_total += b.size;
            self.push_high(b);
        }
        for &(start, end) in vacated {
            if end > start {
                new_total += end - start;
                self.push_high(FreeBlock {
                    offset: start,
                    size: end - start,
                });
            }
        }
        // A vacated span is very often adjacent to a kept block just below the
        // floor, and the whole point of this pass is one BIG block rather than
        // two touching ones.
        self.coalesce_high();
        new_total.saturating_sub(old_total)
    }

    pub fn reset(&mut self) {
        // stw-residual-close forensics: record the wipe range before zeroing
        // (site 2 = from-space reset). Gated; no-op unless the env is set.
        crate::zero_forensics::record(2, 0, self.data.as_ptr() as usize, self.cursor);
        // Zero out used region for safety (prevents stale data reads)
        self.data.fill_zero(0, self.cursor);
        // The high region too, or a reset arena hands out bytes that still
        // hold a previous object's header — the exact hazard `reset`'s own
        // contract exists to close.
        { let n = self.data.len(); self.data.fill_zero(self.high_cursor, n); }
        self.cursor = 0;
        self.high_cursor = self.data.len();
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
        self.high_cursor = self.data.len();
        self.clear_free_list();
        self.clear_alloc_anchors();
    }

    /// Returns true if the given pointer falls within this arena's storage.
    pub fn contains(&self, ptr: *const u8) -> bool {
        let base = self.data.as_ptr();
        let end = unsafe { base.add(self.data.len()) };
        ptr >= base && ptr < end
    }

    /// The number of bytes currently allocated: both bump regions together.
    ///
    /// `capacity - used()` is therefore still exactly the un-bumped middle,
    /// which is what every consumer of this actually means by "used" (the ZGC
    /// headroom trigger sizes its margin from it, and the allocation-failure
    /// guard prints it beside `capacity` to say whether the heap is full).
    pub fn used(&self) -> usize {
        self.cursor + (self.data.len() - self.high_cursor)
    }

    /// The total capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.data.len()
    }

    /// The LOW bump tail as `(first free address, byte count)` — the span a
    /// parallel evacuation may carve into per-worker buffers.
    ///
    /// # Why an evacuator cannot just call `alloc`
    ///
    /// [`Self::alloc`] takes `&mut self`, which is precisely what makes a
    /// copying collector's copy phase single-threaded. Handing the un-bumped
    /// tail out as two plain words lets N workers bump a shared atomic cursor
    /// inside it instead, with the arena itself untouched until
    /// [`Self::commit_parallel_evacuation`] publishes the result.
    ///
    /// The tail is returned rather than the whole arena on purpose: the free
    /// list holds spans whose neighbours are live objects, and an evacuator
    /// bumping through those would overwrite them.
    ///
    /// # This reports the tail; it does NOT make it writable
    ///
    /// The backing store commits lazily, so the span named here is RESERVED
    /// and mostly not yet mapped. The caller must pass the bytes it will
    /// actually use to [`Self::commit_parallel_evacuation_region`] before any
    /// worker writes into it — this is the tenth hand-out site the `hand_out`
    /// helper's note counts, and the only one that does not go through it,
    /// because it hands out a span for N threads to sub-allocate rather than a
    /// single object.
    pub fn parallel_evacuation_region(&self) -> (usize, usize) {
        (
            self.data.as_ptr() as usize + self.cursor,
            self.low_bump_headroom(),
        )
    }

    /// Map the first `bytes` of the tail so N workers may write into it.
    ///
    /// Returns `false` if the OS refused the commit, which the caller must
    /// treat exactly as `hand_out` does — as an allocation failure, here
    /// meaning "run the serial copy phase instead". Returning `true` anyway
    /// would hand the workers memory that faults on first write.
    ///
    /// # Why this is separate from the region query
    ///
    /// `bytes` is the evacuation's own reservation (survivors + per-worker
    /// buffers + its abandoned-tail allowance), which is computed AFTER the
    /// tail is known. Committing the whole tail instead would work and would
    /// throw away what the lazy backing store is for: on a 128 MB to-space
    /// whose cycle copies 400 KB, the difference is the whole arena.
    #[must_use = "a refused commit must send the cycle down the serial path"]
    pub fn commit_parallel_evacuation_region(&mut self, bytes: usize) -> bool {
        debug_assert!(bytes <= self.low_bump_headroom());
        self.data.commit_range(self.cursor, bytes)
    }

    /// Publish the outcome of a parallel evacuation: `bytes` were consumed
    /// from the tail [`Self::parallel_evacuation_region`] handed out.
    ///
    /// The caller must already have made every byte below the new cursor
    /// walkable — object copies, and a filler over every retired per-worker
    /// buffer's tail (`gen_evac::install_gap_filler`). This method deliberately
    /// does NOT take the gaps and push them on the free list instead: a free
    /// block is invisible to `walk_objects` and friends, and this arena is
    /// about to become the next cycle's FROM-space, where several walks
    /// reconstruct the object grid without consulting the free list at all.
    ///
    /// # Panics
    /// If `end_addr` is outside the tail that was handed out — below its start
    /// would lose live copies, above it would put the cursor past the arena's
    /// own capacity.
    pub fn commit_parallel_evacuation(&mut self, end_addr: usize) {
        let (start, len) = self.parallel_evacuation_region();
        assert!(
            end_addr >= start && end_addr <= start + len,
            "parallel evacuation ended at {end_addr:#x}, outside the tail [{start:#x},{:#x})",
            start + len,
        );
        self.cursor += end_addr - start;
    }

    /// Retract the bump cursor into a free span that ends exactly at it,
    /// returning the bytes handed back to the un-bumped tail.
    ///
    /// # Why a non-moving collector needs this
    ///
    /// The cursor is otherwise a **one-way ratchet**. Where nothing compacts,
    /// reclaimed space returns only as free-list holes — so once a process has
    /// cumulatively allocated its whole capacity it can never again serve a
    /// request larger than the biggest hole, for the rest of its life, however
    /// little is live.
    ///
    /// Not a theoretical corner. On `ZipContentTests` at `-Xmx 2g`, with the
    /// sweep having driven live down to **325 MB of 2.1 GB (15%)**, a 16 MB
    /// `byte[]` still failed: `free_list_bytes` 1.81 GB against a
    /// `largest_free_block` of 1.05 MB, cursor parked at 2,130,722,576 of
    /// 2,147,483,648 — an un-bumped tail 16 KB short of the request, and able
    /// only to shrink.
    ///
    /// Objects die young, so the top of the arena is very often entirely
    /// garbage. Handing that span back costs one comparison per sweep and
    /// restores a large CONTIGUOUS tail — precisely what a free list of
    /// scattered holes cannot offer — without relocating a single object.
    ///
    /// Callers must coalesce first, so the topmost span is already maximal;
    /// only the single highest block is considered. Returns 0 when that block
    /// does not reach the cursor (a live object sits above it), the ordinary
    /// mid-heap case.
    pub fn retract_cursor_into_free_tail(&mut self) -> usize {
        // LOW blocks only. Reading the whole list here would take the highest
        // block in the ARENA — which, once the large-object end has been used,
        // is a high-region block that by construction never ends at `cursor`.
        // The retraction would then silently answer 0 for the rest of the run,
        // re-arming the one-way-ratchet failure it exists to prevent.
        let blocks = self.low_blocks_sorted();
        let Some(&(off, size)) = blocks.last() else {
            return 0;
        };
        if size == 0 || off.saturating_add(size) != self.cursor {
            return 0;
        }
        // Rebuild the list without the tail block: those bytes are becoming
        // un-bumped space, and leaving them listed would hand them out twice.
        self.clear_low_free_list();
        for (o, s) in blocks.iter().take(blocks.len() - 1) {
            self.add_free_block(*o, *s);
        }
        let old_cursor = self.cursor;
        self.cursor = off;
        // GIVE THE PAGES BACK. `[off, old_cursor)` was just proved free (it was
        // one free-list block ending exactly at the cursor) and has been
        // removed from the list, so it is un-bumped space that nothing can
        // reach until a later bump commits it again. Whole granules only; the
        // partial ones at either end still neighbour live data.
        //
        // This is the only thing on a non-compacting heap that returns memory
        // to the OS: without it a process that peaks and then idles holds its
        // peak forever, because `retract_cursor_into_free_tail` moved a number
        // and nothing else.
        self.decommit_span(off, old_cursor);
        size
    }

    /// Hand the un-bumped middle back to the OS.
    ///
    /// `[cursor, high_cursor)` is the space between the two ends: below the
    /// low cursor is bump-allocated or free-listed, above the high cursor is
    /// the large-object region, and the middle belongs to neither. Nothing can
    /// reach it without moving a cursor, and a cursor move commits what it
    /// passes over -- so releasing its whole granules is sound with no
    /// liveness question asked.
    ///
    /// **Call at a safepoint, and after anything that reads a vacated span.**
    /// See `compact_low_to` for the record this would otherwise destroy.
    ///
    /// Returns the bytes released. Zero on the wholly-committed fallback store,
    /// and zero on a heap whose middle is smaller than a granule.
    pub fn decommit_unbumped_middle(&mut self) -> usize {
        let (lo, hi) = (self.cursor, self.high_cursor);
        self.decommit_span(lo, hi)
    }

    /// Hand the whole granules inside every FREE-LIST block back to the OS.
    ///
    /// # Why the un-bumped middle is not enough
    ///
    /// [`Self::decommit_unbumped_middle`] releases the space between the two
    /// cursors, which is the space no allocation has ever reached. It cannot
    /// touch memory that WAS allocated and has since been freed -- and on a
    /// heap whose peak was large objects, that is all of it: they are served
    /// from the high end, the sweep returns them to the high free list, and
    /// `retract_high_cursor_into_free_head` gives back only the excess over
    /// the reserve. Measured on `probes/HeapGiveBack.java` at a 64 MiB peak:
    /// the middle-only give-back returned **2 MiB of 64**.
    ///
    /// A free-list block is by definition not live, so its granules can go
    /// back. The bytes come with no obligation either: every path that hands
    /// one out again goes through [`Self::hand_out`], which commits before it
    /// returns a pointer, and a re-committed granule reads as zero -- which is
    /// what a caller of a reused block is entitled to and what `Arena::alloc`'s
    /// consumers already zero for themselves.
    ///
    /// WHOLE granules only, rounded INWARD: a block's ends usually share a
    /// granule with a live object, and rounding outward would take it too --
    /// silently, because a decommitted page reads as zero rather than
    /// faulting on the platforms that map it back on touch.
    ///
    /// **Call at a safepoint**, after the sweep has coalesced: the lists are
    /// then a handful of maximal spans rather than one entry per dead object,
    /// so this is a walk of tens rather than millions.
    ///
    /// Returns the bytes released.
    pub fn decommit_free_blocks(&mut self) -> usize {
        let mut released = 0usize;
        let low: Vec<(usize, usize)> = self.low_blocks_sorted();
        for (off, size) in low {
            released += self.decommit_span(off, off + size);
        }
        let high: Vec<(usize, usize)> = self
            .free_high
            .iter()
            .map(|b| (b.offset, b.size))
            .collect();
        for (off, size) in high {
            released += self.decommit_span(off, off + size);
        }
        released
    }

    /// Is `offset` inside a granule that is currently committed, i.e. safe to
    /// read?
    ///
    /// Always `true` on the wholly-committed fallback store. Used by the one
    /// writer that deliberately puts something in a span the allocator has
    /// retracted past -- see `ZgcRealHeap::stamp_forwarding_words`.
    pub fn is_readable_at(&self, offset: usize) -> bool {
        self.data.is_committed_at(offset)
    }

    /// Bytes of this arena's capacity that are actually committed.
    ///
    /// Equal to `capacity()` on the wholly-committed fallback store; below it,
    /// often far below, on a reserving one.
    pub fn committed_bytes(&self) -> usize {
        self.data.committed_bytes()
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
        let _ = self.data.grow_to(new_capacity);
        // The high end anchors at capacity, so growing moves it. Safe for the
        // same reason the assert above allows the grow at all: `cursor == 0`
        // means the arena is empty, so there is nothing at the old top to
        // strand. (`assert_eq!(self.cursor, 0)` fires above otherwise.)
        debug_assert_eq!(
            self.high_cursor, old_capacity,
            "an empty arena must have an un-bumped high region",
        );
        self.high_cursor = new_capacity;
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
        self.high_cursor.saturating_sub(self.cursor) + self.free_list_bytes()
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
mod prefer_bump_tests {
    use super::*;

    /// **Bump-first must skip a hole the default policy would have taken.**
    ///
    /// This is the whole behavioural claim, and it is the one that makes a
    /// generational nursery possible: an object placed in a hole BELOW the young
    /// floor is in the old region and no young cycle reclaims it. Asserted by
    /// address, against the same arena in both policies, because "it allocated
    /// somewhere" is satisfied by either.
    #[test]
    fn bump_first_skips_a_hole_the_default_policy_takes() {
        // Two identical arenas, one hole each, one request each.
        let mut with_holes = |prefer_bump: bool| -> (usize, usize) {
            let mut a = Arena::new(64 * 1024);
            let base = a.base_ptr() as usize;
            let first = a.alloc(256, 8).expect("first") as usize;
            let _second = a.alloc(256, 8).expect("second") as usize;
            let cursor_before = a.used();
            // Free the FIRST block, making a hole below the cursor.
            a.add_free_block(first - base, 256);
            a.set_prefer_bump(prefer_bump);
            let next = a.alloc(256, 8).expect("third") as usize;
            (next - base, cursor_before)
        };

        let (default_off, hole_off) = with_holes(false);
        assert_eq!(
            default_off, 0,
            "the DEFAULT policy must reuse the hole at offset 0 -- if it does not, \
             this test is not comparing the two policies"
        );

        let (bump_off, cursor_before) = with_holes(true);
        assert!(
            bump_off >= cursor_before,
            "bump-first must allocate at or above the cursor ({cursor_before}), not \
             in the hole at 0 -- got {bump_off}. An object in that hole is below a \
             generational nursery floor and waits for a major"
        );
        let _ = hole_off;
    }

    /// **Bump-first must NOT be able to cause an OutOfMemoryError.**
    ///
    /// It skips only the free-list FAST path; `alloc`'s post-bump retry searches
    /// both tiers in full and then coalesces and searches again. So once the bump
    /// tail is exhausted, every hole is still reachable. If that were not true,
    /// turning the nursery on would turn a servable allocation into an OOM, which
    /// is the one outcome a layout preference must never have -- on a
    /// non-compacting heap the layout policy IS the OOM policy.
    #[test]
    fn bump_first_still_serves_from_the_free_list_once_the_tail_is_gone() {
        let mut a = Arena::new(16 * 1024);
        let base = a.base_ptr() as usize;
        a.set_prefer_bump(true);

        // Fill the arena by bumping until it refuses.
        let mut blocks: Vec<usize> = Vec::new();
        while let Some(p) = a.alloc(512, 8) {
            blocks.push(p as usize - base);
        }
        assert!(
            blocks.len() > 8,
            "the fixture must fill the arena: {}",
            blocks.len()
        );
        assert!(
            a.alloc(512, 8).is_none(),
            "the arena is full, so this must refuse"
        );
        assert_eq!(
            a.free_list_after_bump(),
            0,
            "nothing has fallen through yet"
        );

        // Free two blocks in the middle and ask again. The bump tail is gone, so
        // only the free list can serve it.
        a.add_free_block(blocks[3], 512);
        a.add_free_block(blocks[4], 512);
        let got = a
            .alloc(512, 8)
            .expect("bump-first must fall through to the free list, not OOM");
        let off = got as usize - base;
        assert!(
            off == blocks[3] || off == blocks[4],
            "it must come from one of the freed holes, got {off}"
        );
        assert_eq!(
            a.free_list_after_bump(),
            1,
            "and the fall-through must be COUNTED -- otherwise 'the nursery is \
             leaking into the old region' is an inference rather than a number"
        );
    }

    /// **A pre-merged run coalesces to the same free list as its members do.**
    ///
    /// This is the claim the ZGC young sweep's run merge rests on
    /// (`zgc_gen_dead_runs`): handing one span per run of adjacent dead objects
    /// must leave the arena in the state that handing one span per object leaves
    /// it in, because `coalesce_free_list` merges exactly those spans into
    /// exactly that run a few statements later either way.
    ///
    /// It is not self-evident, and the reason is `add_free_block`'s **region
    /// routing**: a span goes to the small or the large tier by its own size, so
    /// a merged run routes differently on the way IN than its members did. What
    /// makes the result identical is that the coalescer rebuilds the list from
    /// `low_blocks_sorted` rather than merging in place — so the routing on the
    /// way in cannot survive it. Asserted rather than assumed, because "the
    /// coalescer normalises it" is the entire safety argument for the merge and
    /// it is one refactor away from stopping being true.
    #[test]
    fn an_arena_coalesces_pre_merged_runs_to_the_same_shape() {
        const N: usize = 16;
        const SZ: usize = 256;

        // Same arena, same bumped region, same total span -- the only difference
        // is how many calls it arrives in.
        let shape = |per_object: bool| -> (Vec<(usize, usize)>, usize, usize) {
            let mut a = Arena::new(64 * 1024);
            let base = a.base_ptr() as usize;
            let p = a.alloc(N * SZ, 8).expect("one bumped region") as usize;
            let off = p - base;
            if per_object {
                for i in 0..N {
                    a.add_free_block(off + i * SZ, SZ);
                }
            } else {
                a.add_free_block(off, N * SZ);
            }
            a.coalesce_free_list();
            (
                a.free_blocks_sorted(),
                a.free_list_bytes(),
                a.largest_free_block(),
            )
        };

        let (blocks_each, bytes_each, largest_each) = shape(true);
        let (blocks_run, bytes_run, largest_run) = shape(false);

        assert_eq!(
            blocks_each, blocks_run,
            "the free list must have the same SPANS either way"
        );
        assert_eq!(bytes_each, bytes_run, "and the same total");
        assert_eq!(
            largest_each, largest_run,
            "and the same largest block -- which is the figure an allocation \
             decision is actually taken on"
        );
        assert_eq!(
            bytes_run,
            N * SZ,
            "and the fixture must have freed the whole region, or this compares \
             two empty lists and passes for nothing"
        );
    }

    /// **Off by default**, so no other collector's layout changes.
    #[test]
    fn bump_first_is_off_unless_asked_for() {
        let a = Arena::new(4 * 1024);
        assert!(!a.prefer_bump(), "the default policy must be unchanged");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A compaction may only drop the holes it wrote into.**
    ///
    /// `compact_low_to` used to clear the whole low free list, on the premise
    /// that a slide leaves no hole below the cursor. That holds for a slide
    /// which compacts the entire low region; ZGC's compacts the pages its
    /// relocation-set selector picked and leaves the rest untouched, so every
    /// hole outside the destination window is a real free block — and the
    /// sweep never re-discovers it, because the sweep only free-lists objects
    /// that DIE.
    ///
    /// The cost was total: on `repros/frag-churn` the sweep free-listed 400 MB,
    /// the slide cleared it, and from then on the bump cursor advanced by
    /// exactly the bytes allocated until it hit capacity and the VM threw
    /// `OutOfMemoryError` with 99% of the heap dead.
    ///
    /// Three blocks, one of each kind, so the test cannot pass by halves:
    /// below the window (must survive), inside it (must go — a survivor may be
    /// sitting on it), and straddling the new cursor (must be truncated, not
    /// dropped, or the bytes below the cursor are lost too).
    #[test]
    fn compaction_keeps_the_free_holes_it_did_not_write_into() {
        let mut arena = Arena::new(64 * 1024);
        // Bump the cursor out so every offset below is inside the live region.
        let _ = arena.alloc(32 * 1024, 8).expect("fresh arena has room");

        arena.add_free_block(1024, 512); // below the window
        arena.add_free_block(8192, 512); // inside the window
        arena.add_free_block(20_480, 4096); // straddles the new cursor

        let before = arena.free_list_bytes();
        assert_eq!(
            before,
            512 + 512 + 4096,
            "the fixture must set up three blocks"
        );

        // The slide wrote into [4096, 12288) and left the cursor at 22528.
        let reclaimed = arena.compact_low_to(22_528, 4096..12_288, &[]);
        assert!(reclaimed > 0, "the cursor must actually retract");

        let kept = arena.free_blocks_sorted();
        assert!(
            kept.iter().any(|&(off, sz)| off == 1024 && sz == 512),
            "a hole below the destination window is untouched memory and must \
             survive: {kept:?}"
        );
        assert!(
            !kept.iter().any(|&(off, _)| off == 8192),
            "a hole inside the destination window may have a survivor on it and \
             must be dropped: {kept:?}"
        );
        assert!(
            kept.iter().any(|&(off, sz)| off == 20_480 && sz == 2048),
            "a hole straddling the new cursor must be TRUNCATED to the part \
             below it, not dropped: {kept:?}"
        );
        assert_eq!(
            arena.free_list_bytes(),
            512 + 2048,
            "the accounting must match the blocks that survived"
        );
    }

    /// The `ZipContentTests` shape, in miniature: the cursor is a one-way
    /// ratchet, so a request larger than the biggest hole is unservable even
    /// when most of the heap is free.
    ///
    /// Structure matters here. The free tail is deliberately NOT the largest
    /// block, and a live object is left above the small holes, so the test
    /// cannot pass by accident: a `retract` that returned any free block rather
    /// than specifically the one ending AT the cursor would hand out bytes that
    /// are still live, and one that merely coalesced would not move the cursor
    /// at all. The RED it pins is the real one — `alloc` fails before the
    /// retraction and succeeds after, with nothing else changed.
    /// The defect this whole region exists for, reduced to eleven lines.
    ///
    /// One small survivor inside each of four chunk-sized runs caps every hole
    /// at one chunk, so a request larger than a chunk cannot be served however
    /// much of the arena is free. This is the `TestNonBlockingAPI` shape
    /// exactly: four AQS nodes, 544 bytes, holding 2.6 MB hostage.
    ///
    /// Built so it cannot pass by accident: the survivors are deliberately
    /// placed so that no two adjacent free runs can merge into anything as
    /// large as the request, and the assertion on the LOW end is that the
    /// request genuinely fails there — if that ever starts succeeding, the
    /// second half of the test is proving nothing.
    #[test]
    fn a_survivor_in_every_chunk_caps_the_low_end_but_not_the_high_end() {
        // One "chunk" of small-object churn plus one large object, repeated —
        // which is what a running VM does, and the interleaving is the whole
        // point: it is what puts a survivor between every pair of large-object
        // holes when the two populations share one bump region.
        const CHUNK: usize = 4096;
        const LARGE: usize = 4096;
        const REQUEST: usize = LARGE * 2;
        const ROUNDS: usize = 4;

        let mut arena = Arena::new((CHUNK + LARGE) * ROUNDS);
        let base = arena.base_ptr() as usize;
        let mut low_churn = Vec::new();
        let mut large_dead = Vec::new();
        for _ in 0..ROUNDS {
            // A survivor that is never freed, then churn, then a large object.
            let _survivor = arena.alloc(64, 8).unwrap();
            let churn = arena.alloc(CHUNK - 64, 8).unwrap();
            let large = arena.alloc_high(LARGE, 8).unwrap();
            low_churn.push((churn as usize - base, CHUNK - 64));
            large_dead.push((large as usize - base, LARGE));
        }
        assert_eq!(
            arena.used(),
            (CHUNK + LARGE) * ROUNDS,
            "both ends are fully bumped: nothing can come from un-bumped space",
        );
        for (off, size) in &low_churn {
            arena.add_free_block(*off, *size);
        }
        arena.coalesce_free_list();

        // The low end is walled: its holes are `CHUNK - 64` apart, so however
        // many bytes it holds, none of them is a contiguous `REQUEST`. Asserted
        // while the HIGH free list is still empty, so the low end's
        // raid-the-other-end last resort cannot answer for it.
        assert!(
            arena.alloc(REQUEST, 8).is_none(),
            "precondition: the low end is walled by one survivor per chunk —              if this succeeds the assertion below proves nothing",
        );

        // The large-object end never saw a survivor, so its holes merge into
        // one contiguous run.
        for (off, size) in &large_dead {
            arena.add_free_block(*off, *size);
        }
        arena.coalesce_high();
        assert!(
            arena.alloc_high(REQUEST, 8).is_some(),
            "the large-object end coalesced into a contiguous run",
        );
    }

    /// The two ends must never hand out the same byte, and the shared middle
    /// is where that would happen.
    #[test]
    fn the_two_ends_meet_without_overlapping() {
        let mut arena = Arena::new(4096);
        let base = arena.base_ptr() as usize;
        // Deliberately asymmetric: an allocator that ignored the high end and
        // simply bumped upwards would put this at 1024, not 3072.
        let low = arena.alloc(1024, 8).unwrap() as usize - base;
        let high = arena.alloc_high(1024, 8).unwrap() as usize - base;
        assert_eq!(low, 0);
        assert_eq!(high, 3072, "the high end bumps DOWN from capacity");
        assert_eq!(arena.used(), 2048, "both regions count as used");
        assert_eq!(arena.remaining(), 2048, "and the middle is what is left");
        // Close the middle from the low side and check neither can cross.
        let _fill = arena.alloc(2048, 8).unwrap();
        assert_eq!(arena.remaining(), 0);
        assert!(arena.alloc(8, 8).is_none(), "the low end cannot cross over");
        assert!(
            arena.alloc_high(8, 8).is_none(),
            "and neither can the high end",
        );
    }

    /// The high end's own one-way-ratchet fix: a wholly-free head goes back to
    /// the descending cursor. Watched to fail before it passed — with the
    /// retraction stubbed to 0 the final allocation returns `None`, because
    /// the two halves are separate free blocks and neither alone is big
    /// enough.
    #[test]
    fn a_wholly_free_high_head_is_handed_back_to_the_descending_cursor() {
        let mut arena = Arena::new(8192);
        let base = arena.base_ptr() as usize;
        // From the top down: [live 1024][dead 1024][dead 1024]
        let _live = arena.alloc_high(1024, 8).unwrap();
        let dead_a = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let dead_b = arena.alloc_high(1024, 8).unwrap() as usize - base;
        assert_eq!(arena.high_cursor(), 8192 - 3072);
        arena.add_free_block(dead_a, 1024);
        arena.add_free_block(dead_b, 1024);
        assert_eq!(
            arena.coalesce_high(),
            1,
            "the two dead spans are adjacent and must merge into one",
        );

        let handed_back = arena.retract_high_cursor_into_free_head();
        assert_eq!(handed_back, 2048, "the merged span starts at the cursor");
        assert_eq!(arena.high_cursor(), 8192 - 1024, "cursor climbed back");
        assert_eq!(
            arena.free_list_bytes(),
            0,
            "the retracted span must leave the free list, or it is served twice",
        );
        assert_eq!(arena.used(), 1024, "only the live object is still charged");
    }

    /// The other half of that contract: a hole with a live object BELOW it (at
    /// a lower address, i.e. bumped more recently) must not move the cursor.
    /// Getting this wrong hands out live bytes.
    /// The reserve has to survive ordinary low-end churn, or it buys nothing.
    ///
    /// This is the shape that made the first version of the reserve useless:
    /// the low end asks for space it could get from the reserve, gets it, and
    /// the large-object region is drained before it can claim its floor. The
    /// low end must be pushed onto its own free list instead, and the reserved
    /// span must still be there for a large request afterwards.
    #[test]
    fn ordinary_low_end_churn_does_not_drain_the_reserve() {
        let mut arena = Arena::new(16384);
        arena.set_high_reserve(4096);
        let base = arena.base_ptr() as usize;

        // Fill the un-reserved low region, then free half of it so the low end
        // has a free list to fall back on.
        let mut freed = Vec::new();
        for i in 0..12 {
            let p = arena.alloc(1024, 8).unwrap() as usize - base;
            if i % 2 == 0 {
                freed.push(p);
            }
        }
        assert_eq!(arena.used(), 12288, "the un-reserved low region is full");
        for p in &freed {
            arena.add_free_block(*p, 1024);
        }
        // Six 1 KiB holes are now on the low free list. Six more requests must
        // all come from there and none from the reserve, which is exactly what
        // ordering the reserve override AFTER the free list buys.
        for _ in 0..6 {
            assert!(arena.alloc(1024, 8).is_some(), "served from the free list");
        }
        assert_eq!(
            arena.used(),
            12288,
            "no bump happened: every request came out of the free list",
        );
        assert!(
            arena.alloc_high(4096, 8).is_some(),
            "the reserve survived the churn and the large request fits",
        );
    }

    /// ...but the reserve is a preference, not a wall: a low-end request that
    /// cannot be served any other way takes it rather than failing.
    #[test]
    fn the_reserve_yields_rather_than_raising_oom() {
        let mut arena = Arena::new(16384);
        arena.set_high_reserve(4096);
        // Consume everything the low end is allowed to bump.
        while arena.alloc(1024, 8).is_some() {
            if arena.used() > 16384 {
                panic!("bumped past capacity");
            }
        }
        assert_eq!(
            arena.used(),
            16384,
            "the low end took the reserve rather than failing with it free",
        );
    }

    /// The drain the gate exists to stop: a freed large object at the bottom of
    /// the high region must NOT be handed back to the shared middle while the
    /// region is at or under its floor, because the low end would take it and
    /// the large-object region would shrink to its own live set one sweep at a
    /// time (measured: 92 MB claimed, 3,224 bytes left).
    #[test]
    fn the_high_retraction_does_not_drain_the_region_below_its_floor() {
        let mut arena = Arena::new(16384);
        arena.set_high_reserve(4096);
        let base = arena.base_ptr() as usize;
        let dead = arena.alloc_high(2048, 8).unwrap() as usize - base;
        let cursor_before = arena.high_cursor();
        arena.add_free_block(dead, 2048);
        arena.coalesce_high();
        assert_eq!(
            arena.retract_high_cursor_into_free_head(),
            0,
            "the region holds 2 KiB against a 4 KiB floor — nothing to give back",
        );
        assert_eq!(arena.high_cursor(), cursor_before, "cursor must not move");
        assert_eq!(
            arena.high_free_shape().1,
            2048,
            "and the bytes stay on the HIGH free list, where the low end \
             cannot reach them",
        );

        // Over-claim relative to the floor and the EXCESS — only the excess —
        // is handed back.
        let over = arena.alloc_high(8192, 8).unwrap() as usize - base;
        arena.add_free_block(over, 8192);
        assert_eq!(arena.coalesce_high(), 1, "the two dead spans are adjacent");
        assert_eq!(
            arena.retract_high_cursor_into_free_head(),
            6144,
            "10 KiB claimed against a 4 KiB floor: 6 KiB goes back, 4 KiB stays",
        );
        assert_eq!(
            arena.high_region_bytes(),
            4096,
            "the region settled exactly on its floor",
        );
        assert_eq!(
            arena.high_free_shape().1,
            4096,
            "and the retained half is still on the HIGH free list",
        );
    }

    /// `compact_high_to` publishes ONE maximal block where the slide left a
    /// mosaic — which is the entire reason the large-object end grew a
    /// compactor. A best fit over three 1 KiB holes cannot serve 3 KiB; over
    /// their merge it can, and that is the assertion.
    #[test]
    fn compacting_the_high_end_merges_its_holes_into_one_servable_block() {
        // Capacity exactly four allocations wide, so the two ends meet and the
        // bump path is out -- the state a fragmented heap is actually in, and
        // the only one in which a free-list miss is an `OutOfMemoryError`.
        let mut arena = Arena::new(4096);
        let base = arena.base_ptr() as usize;
        // Four 1 KiB large objects, alternating live and dead. Addresses
        // descend, so `a` is the highest.
        let a = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let b = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let c = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let d = arena.alloc_high(1024, 8).unwrap() as usize - base;
        assert!(a > b && b > c && c > d, "the high end bumps DOWN");
        // `b` and `d` die; the sweep free-lists them, leaving two 1 KiB holes
        // with a live kilobyte between them.
        arena.add_free_block(b, 1024);
        arena.add_free_block(d, 1024);
        assert_eq!(arena.high_free_shape(), (2, 2048, 1024));
        assert!(
            arena.alloc_high(2048, 8).is_none(),
            "the precondition: 2 KiB free in two blocks cannot serve 2 KiB"
        );

        // The slide the collector would have performed: `c` packs against `a`
        // (into `b`'s span), `a` stays. Everything below `c`'s new base is
        // then dead.
        //
        // SAFETY: the test owns the arena and nothing else reads these bytes.
        unsafe {
            std::ptr::copy(
                (base + c) as *const u8,
                (base + b) as *mut u8,
                1024,
            )
        };
        let new_floor = b;
        let reclaimed = arena.compact_high_to(d, &[(d, new_floor)]);

        assert_eq!(
            reclaimed, 0,
            "and the pass buys CONTIGUITY, not bytes: the same two kilobytes
             were free before and after, which is why `reclaimed` is the wrong
             number to judge this pass by"
        );
        let (blocks, bytes, largest) = arena.high_free_shape();
        assert_eq!(
            (blocks, bytes, largest),
            (1, 2048, 2048),
            "the holes must come back as ONE block, not two touching ones"
        );
        assert!(
            arena.alloc_high(2048, 8).is_some(),
            "the request that could not be served before the compaction must \
             be servable after it -- that is the whole feature"
        );
    }

    /// A free block BELOW the authoritative floor is kept verbatim.
    ///
    /// This is the case a pinned large object creates: the slide compacts the
    /// region above it and knows nothing about the region below, so a wholesale
    /// replacement of the high free list would lose every byte down there
    /// permanently. `Arena::compact_low_to` records having made exactly that
    /// mistake at the other end (the `repros/frag-churn` OOM at 99 % dead), and
    /// this is the check that it was not repeated here.
    #[test]
    fn compacting_the_high_end_keeps_the_free_space_below_its_floor() {
        let mut arena = Arena::new(16384);
        let base = arena.base_ptr() as usize;
        let _top = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let hole_above = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let pinned = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let hole_below = arena.alloc_high(1024, 8).unwrap() as usize - base;
        arena.add_free_block(hole_above, 1024);
        arena.add_free_block(hole_below, 1024);
        assert_eq!(arena.high_free_shape().1, 2048);

        // The slide stopped at `pinned`: it is authoritative only from
        // `pinned + 1024` upwards, and it emptied nothing new up there.
        let reclaimed = arena.compact_high_to(pinned + 1024, &[]);

        assert_eq!(reclaimed, 0, "nothing new was freed");
        let (blocks, bytes, _largest) = arena.high_free_shape();
        assert_eq!(
            (blocks, bytes),
            (1, 1024),
            "the hole above the floor is inside the packed region and goes; \
             the one BELOW it must survive untouched"
        );
        assert_eq!(
            arena.free_blocks_sorted()[0],
            (hole_below, 1024),
            "and it must be the same block, at the same offset"
        );
    }

    /// A vacated span adjacent to a kept block comes back as one block, not
    /// two touching ones. Contiguity is the only thing this pass buys, so
    /// leaving the seam in would make it buy nothing.
    #[test]
    fn a_vacated_high_span_merges_with_the_hole_below_the_floor() {
        let mut arena = Arena::new(16384);
        let base = arena.base_ptr() as usize;
        let _top = arena.alloc_high(2048, 8).unwrap() as usize - base;
        let vacated = arena.alloc_high(2048, 8).unwrap() as usize - base;
        let below = arena.alloc_high(2048, 8).unwrap() as usize - base;
        arena.add_free_block(below, 2048);

        let reclaimed = arena.compact_high_to(vacated, &[(vacated, vacated + 2048)]);

        assert_eq!(reclaimed, 2048);
        assert_eq!(
            arena.high_free_shape(),
            (1, 4096, 4096),
            "the vacated span and the pre-existing hole below it are adjacent \
             and must be published as one 4 KiB block"
        );
    }

    /// The vacated span is ZEROED. A slid-away survivor leaves a valid-looking
    /// `ObjectHeader` behind, and a conservative scanner that met one would
    /// resurrect a corpse — the same contract `compact_low_to` and `reset`
    /// state at the other end.
    #[test]
    fn compacting_the_high_end_zeroes_what_it_frees() {
        let mut arena = Arena::new(16384);
        let base = arena.base_ptr() as usize;
        let _live = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let corpse = arena.alloc_high(1024, 8).unwrap() as usize - base;
        // SAFETY: the test owns the arena; this is the "header left behind"
        // the zeroing exists to erase.
        unsafe { std::ptr::write_bytes((base + corpse) as *mut u8, 0xAB, 1024) };

        arena.compact_high_to(corpse, &[(corpse, corpse + 1024)]);

        // SAFETY: same span, still inside the arena.
        let bytes = unsafe { std::slice::from_raw_parts((base + corpse) as *const u8, 1024) };
        assert!(
            bytes.iter().all(|b| *b == 0),
            "the vacated span must be zeroed, or a conservative scan can \
             resurrect what was there"
        );
    }

    /// **The region tripwire fires on the ONE arm that can trip it.**
    ///
    /// `Arena::alloc`'s low free-list exits return offsets below `high_cursor`
    /// by construction, so arming them was arming nothing: the counter read
    /// zero for a week on a workload whose fragmentation report was naming an
    /// 80-byte `String` and a 24-byte `Object` above `high_cursor` at the same
    /// time. The last-resort `high_fit` is the exit that can put a small object
    /// in the large-object region, and this is the test that says so.
    ///
    /// The delta, not the absolute: the counter is process-global and bounded
    /// logging shares it with every other test in the binary.
    #[test]
    fn a_small_object_served_from_the_high_free_list_trips_the_region_wire() {
        let mut arena = Arena::new(8192);
        // Claim and release 4 KiB at the high end, so its free list has a block
        // and the two cursors can meet.
        let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
        arena.add_free_block(big, 4096);
        assert_eq!(arena.high_free_shape(), (1, 4096, 4096));

        // Bump the low end right up to `high_cursor`, so the ordinary paths are
        // all exhausted and only the last resort is left.
        arena.alloc(2048, 8).unwrap();
        arena.alloc(2048, 8).unwrap();
        assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");

        let before = small_allocations_in_large_region();
        let p = arena.alloc(64, 8).expect(
            "the last resort must still serve this rather than raise OutOfMemoryError",
        );
        let off = p as usize - arena.base_ptr() as usize;
        assert!(
            off >= arena.high_cursor(),
            "the only space left was the large-object end's own free list"
        );
        assert_eq!(
            small_allocations_in_large_region(),
            before + 1,
            "and the region tripwire must SEE it -- the residual this closes is \
             an untriggered instrument reading zero"
        );
    }

    #[test]
    fn a_high_hole_above_a_live_object_does_not_move_the_high_cursor() {
        let mut arena = Arena::new(8192);
        let base = arena.base_ptr() as usize;
        let dead = arena.alloc_high(1024, 8).unwrap() as usize - base;
        let _live_below = arena.alloc_high(512, 8).unwrap();
        let cursor_before = arena.high_cursor();
        arena.add_free_block(dead, 1024);
        assert_eq!(
            arena.retract_high_cursor_into_free_head(),
            0,
            "a hole with a live object below it is not the region's head",
        );
        assert_eq!(arena.high_cursor(), cursor_before, "cursor must not move");
        assert_eq!(arena.free_list_bytes(), 1024, "and the hole stays listed");
    }

    /// A reclaimed high-end span must come back to the HIGH free list, and a
    /// low-end one to the low tiers. Routing by offset is the whole basis of
    /// the region test, so a block landing in the wrong list would let a
    /// small-object allocation eat the large-object region (and would trip the
    /// low end's `offset + size <= cursor` invariant).
    #[test]
    fn reclaimed_spans_return_to_the_end_they_were_carved_from() {
        let mut arena = Arena::new(8192);
        let base = arena.base_ptr() as usize;
        let low = arena.alloc(1024, 8).unwrap() as usize - base;
        let high = arena.alloc_high(1024, 8).unwrap() as usize - base;
        assert_eq!(low, 0, "the low end starts at the bottom");
        assert_eq!(high, 8192 - 1024, "the high end starts at the top");
        arena.add_free_block(low, 1024);
        arena.add_free_block(high, 1024);
        assert_eq!(arena.free_list_bytes(), 2048);

        // The low tiers must not be able to serve a request out of the high
        // block: `alloc` never consults `free_high`.
        let served = arena.alloc(1024, 8).unwrap() as usize - base;
        assert_eq!(served, low, "the low end reused its own hole");
        let served_high = arena.alloc_high(1024, 8).unwrap() as usize - base;
        assert_eq!(served_high, high, "and the high end reused its own");
        assert_eq!(arena.free_list_bytes(), 0);
    }

    /// `retract_cursor_into_free_tail` reads the free list to find the block
    /// that ends at the LOW cursor. Once the high end has been used, the
    /// highest block in the arena is a high-region block that by construction
    /// never ends there — so a version that looked at the whole list would
    /// answer 0 forever and silently re-arm the ratchet it exists to prevent.
    #[test]
    fn a_high_region_block_does_not_blind_the_low_retraction() {
        let mut arena = Arena::new(8192);
        let base = arena.base_ptr() as usize;
        let _live_low = arena.alloc(1024, 8).unwrap();
        let tail = arena.alloc(1024, 8).unwrap() as usize - base;
        let dead_high = arena.alloc_high(2048, 8).unwrap() as usize - base;
        arena.add_free_block(dead_high, 2048);
        arena.add_free_block(tail, 1024);

        assert_eq!(
            arena.retract_cursor_into_free_tail(),
            1024,
            "the low tail must still be found with a high block on the list",
        );
        assert_eq!(arena.used(), 1024 + 2048, "low cursor moved, high did not");
    }

    #[test]
    fn a_wholly_free_tail_is_handed_back_to_the_bump_cursor() {
        let mut arena = Arena::new(4096);
        // Layout: [live 512][dust 256][live 512][free tail 2816]
        let _live_a = arena.alloc(512, 8).unwrap();
        let dust = arena.alloc(256, 8).unwrap();
        let _live_b = arena.alloc(512, 8).unwrap();
        let tail = arena.alloc(2816, 8).unwrap();
        assert_eq!(arena.used(), 4096, "the arena is now fully bumped");

        let base = arena.base_ptr() as usize;
        arena.add_free_block(dust as usize - base, 256);
        arena.add_free_block(tail as usize - base, 2816);

        // A 1 KiB request cannot be served from the 256-byte hole, and the
        // cursor is at capacity — this is the ratchet.
        assert!(
            arena.largest_free_block() >= 1024,
            "precondition: the tail block itself is big enough",
        );

        let handed_back = arena.retract_cursor_into_free_tail();
        assert_eq!(handed_back, 2816, "the tail ends exactly at the cursor");
        assert_eq!(arena.used(), 1280, "cursor retreated to the tail's start");

        // The retracted span must not still be on the free list, or these
        // bytes would be handed out twice.
        assert_eq!(
            arena.free_list_bytes(),
            256,
            "only the interior dust hole should remain listed",
        );

        // And the bytes are usable again as ONE contiguous run.
        assert!(
            arena.alloc(2048, 8).is_some(),
            "a 2 KiB request must now be servable from the recovered tail",
        );
    }

    /// The other half of the contract: a free block that does NOT reach the
    /// cursor must leave it alone. Without this, the retraction could hand back
    /// a hole with live objects above it — heap corruption rather than a
    /// missed optimisation.
    #[test]
    fn a_free_hole_below_a_live_object_does_not_move_the_cursor() {
        let mut arena = Arena::new(4096);
        let dead = arena.alloc(1024, 8).unwrap();
        let _live_above = arena.alloc(512, 8).unwrap();
        let used_before = arena.used();

        let base = arena.base_ptr() as usize;
        arena.add_free_block(dead as usize - base, 1024);

        assert_eq!(
            arena.retract_cursor_into_free_tail(),
            0,
            "a hole with a live object above it is not a tail",
        );
        assert_eq!(arena.used(), used_before, "cursor must not move");
        assert_eq!(
            arena.free_list_bytes(),
            1024,
            "and the hole stays on the free list",
        );
    }

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

    /// An allocation must not fail while the bytes it needs are present and
    /// merely SPLIT across adjacent free blocks.
    ///
    /// Coalescing used to run only in a collector's post-sweep hook, so between
    /// two sweeps the list accumulated adjacency nothing merged: every `split`
    /// remainder, and on ZGC every TLAB retire (which returns the unused tail of
    /// a chunk whose used part the sweep already free-listed object by object).
    /// The heap then reported `OutOfMemoryError` with a free list holding
    /// hundreds of times the requested bytes — measured live on the 2026-08-11
    /// Hibernate run as `free_list_bytes=1211378512 largest_free_block=65528`
    /// against a 65552-byte request.
    ///
    /// The arm order is the point: the request is proved to FAIL before the
    /// merge and to SUCCEED after it, on the same list. A test that only
    /// asserted the success would pass on an arena that never fragmented.
    #[test]
    fn alloc_merges_adjacent_holes_before_reporting_failure() {
        let mut arena = Arena::new(256 * 1024);
        let cap = arena.capacity();
        arena.alloc(cap, 8).unwrap(); // pin the cursor: the free list is all there is

        // Four abutting 64 KiB-ish holes: 256 KiB of free space, no single
        // block big enough for a 96 KiB request.
        let hole = 64 * 1024;
        for i in 0..4 {
            arena.add_free_block(i * hole, hole);
        }
        assert_eq!(arena.free_list_bytes(), 4 * hole);
        assert_eq!(
            arena.largest_free_block(),
            hole,
            "pre-merge the list must really be capped at one hole",
        );

        // RED: the request cannot be served block-by-block...
        assert!(
            !arena.has_free_block_at_least(96 * 1024),
            "no single block may fit — otherwise the merge is not what serves it",
        );
        // ...but the bytes are all there, contiguously.
        let merged_away = arena.coalesce_free_list();
        assert_eq!(merged_away, 3, "four abutting holes must become one span");
        assert_eq!(arena.largest_free_block(), 4 * hole);
        assert_eq!(
            arena.free_list_bytes(),
            4 * hole,
            "merging must not lose or duplicate a byte",
        );

        // A second merge on an unchanged list is free and a no-op.
        assert_eq!(arena.coalesce_free_list(), 0);

        // And `alloc` reaches the merge by itself, without a collection: rebuild
        // the same fragmented state and ask for 96 KiB directly.
        let mut arena = Arena::new(256 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap();
        for i in 0..4 {
            arena.add_free_block(i * hole, hole);
        }
        assert!(
            arena.alloc(96 * 1024, 8).is_some(),
            "alloc must coalesce and retry rather than return None with 256 KiB free",
        );
        assert_eq!(arena.free_list_bytes(), 4 * hole - 96 * 1024);
    }

    /// The last-resort merge must back off on a heap it cannot help.
    ///
    /// `alloc` reaches that arm once per allocation on a wedged heap, and the
    /// merge is `O(n log n)` over the whole free list — ungated it showed up as
    /// 9.1% of a `type.temporal.ZonedDateTimeTest` profile the day it landed.
    /// A list whose holes are walled by live data has nothing to merge, so the
    /// arm has to switch itself off rather than re-sort forever.
    #[test]
    fn the_last_resort_merge_backs_off_when_merging_never_pays() {
        let mut arena = Arena::new(64 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap();
        // Non-adjacent holes: an 8-byte live wall between each pair.
        for i in 0..8 {
            arena.add_free_block(i * 1024, 1016);
        }
        assert_eq!(arena.coalesce_free_list(), 0, "nothing here is adjacent");
        let after_one = arena.coalesce_threshold;
        assert!(
            after_one > COALESCE_THRESHOLD_MIN,
            "an unproductive merge must raise the bar, got {after_one}",
        );
        // Keep feeding it non-adjacent blocks; the bar must keep rising.
        for round in 0..6 {
            for _ in 0..arena.coalesce_threshold.min(64) {
                arena.free_pushed += 1;
            }
            let before = arena.coalesce_threshold;
            assert_eq!(arena.coalesce_free_list(), 0, "round {round}");
            assert!(
                arena.coalesce_threshold >= before,
                "round {round}: the bar must never fall on an unproductive merge",
            );
        }
        assert!(
            arena.coalesce_threshold >= 4096,
            "six unproductive merges must have effectively switched the arm off, \
             got {}",
            arena.coalesce_threshold,
        );

        // ...and a productive merge re-arms it immediately, so a list that
        // really is merely split is never left unmerged.
        arena.clear_free_list();
        arena.add_free_block(0, 1024);
        arena.add_free_block(1024, 1024);
        assert_eq!(arena.coalesce_free_list(), 1);
        assert_eq!(arena.coalesce_threshold, COALESCE_THRESHOLD_MIN);
    }

    /// The span tier serves BEST fit, and every accounting field stays exact
    /// across takes and splits.
    ///
    /// The tier used to be a `Vec` scanned first-fit, so the block a request got
    /// depended on push order: a 5 KiB request could consume a 1 MiB span and
    /// leave a ~1 MiB remainder, which is how a tier that is supposed to hold "a
    /// handful of spans" ends up holding thousands. Keyed by size, the answer is
    /// the smallest bucket that fits.
    #[test]
    fn span_tier_serves_best_fit_and_keeps_its_accounting_exact() {
        let mut arena = Arena::new(4 * 1024 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap(); // pin the cursor

        // Pushed largest-first on purpose: a first-fit scan would take the
        // 1 MiB span for a 5 KiB request.
        arena.add_free_block(0, 1024 * 1024);
        arena.add_free_block(2 * 1024 * 1024, 64 * 1024);
        arena.add_free_block(3 * 1024 * 1024, 8 * 1024);
        assert_eq!(arena.free_span_shape(), (3, 3));
        assert_eq!(arena.largest_free_block(), 1024 * 1024);
        let total = 1024 * 1024 + 64 * 1024 + 8 * 1024;
        assert_eq!(arena.free_list_bytes(), total);

        // 5 KiB must come out of the 8 KiB span, leaving a 3 KiB remainder that
        // routes to the SMALL tier (below LARGE_BLOCK_MIN).
        let want = 5 * 1024;
        assert!(arena.alloc(want, 8).is_some());
        assert_eq!(
            arena.free_span_shape(),
            (2, 2),
            "best fit must consume the 8 KiB span, not the 1 MiB one",
        );
        assert_eq!(arena.largest_free_block(), 1024 * 1024);
        assert_eq!(
            arena.free_list_bytes(),
            total - want,
            "the split remainder must stay on the list, exactly",
        );

        // Exhaust the 1 MiB span exactly: the bucket must disappear, so the
        // maximum falls to the next span rather than reporting a stale value.
        assert!(arena.alloc(1024 * 1024, 8).is_some());
        assert_eq!(arena.largest_free_block(), 64 * 1024);
        assert_eq!(arena.free_span_shape(), (1, 1));

        // A request past every span is an AUTHORITATIVE miss, and must agree
        // with the probe.
        assert!(!arena.has_free_block_at_least(128 * 1024));
        assert!(arena.alloc(128 * 1024, 8).is_none());
        assert!(arena.has_free_block_at_least(64 * 1024));
    }

    /// Two same-size spans in one bucket are two distinct blocks, and the
    /// alignment window (`align > 8`) still finds a fit only some of them have.
    ///
    /// The bucket head is taken without a per-block test, which is exact only
    /// because every block in a bucket has the same size AND every offset is on
    /// the 8-byte grid. For `align > 8` neither is enough — the fit depends on
    /// the block's own offset — so that window is searched per block. If it were
    /// not, a miss would stop being authoritative and `alloc` would report OOM
    /// with a usable hole on the list.
    #[test]
    fn span_tier_handles_duplicate_sizes_and_the_alignment_window() {
        let mut arena = Arena::new(256 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap();
        arena.add_free_block(0, 8192);
        arena.add_free_block(16384, 8192);
        assert_eq!(
            arena.free_span_shape(),
            (2, 1),
            "two blocks, one size class",
        );
        assert_eq!(arena.free_list_bytes(), 16384);
        assert!(arena.alloc(8192, 8).is_some());
        assert_eq!(arena.free_span_shape(), (1, 1));
        assert!(arena.alloc(8192, 8).is_some());
        assert_eq!(arena.free_span_shape(), (0, 0));
        assert_eq!(arena.largest_free_block(), 0);

        // Alignment window: one span whose base is 4096-aligned and one whose
        // base is not. A 4096-aligned request of exactly the span size can only
        // be served by the first.
        let mut arena = Arena::new(256 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap();
        let base = arena.base_ptr() as usize;
        // Offsets chosen so `base + off` alignment differs by construction.
        let aligned_off = (4096 - (base & 4095)) & 4095;
        arena.add_free_block(aligned_off + 8192, 4096); // 4096-aligned start
        arena.add_free_block(aligned_off + 4096 + 8, 4096); // deliberately not
        let hit = arena.alloc(4096, 4096);
        assert!(
            hit.is_some(),
            "the aligned span must still be found through the alignment window",
        );
        assert_eq!(hit.unwrap() as usize & 4095, 0);
    }

    /// A tier holding many spans must answer a hopeless request without looking
    /// at all of them.
    ///
    /// This is the regression the size-keyed map exists for, written so the old
    /// implementation FAILS IT BY TIMING OUT rather than by an assertion: 40k
    /// spans x 40k failing probes is 1.6e9 block visits under the old unbounded
    /// first-fit scan (minutes), and 40k range probes now (milliseconds). A
    /// plain assertion could not catch it — the old code returned the right
    /// answer, just not this decade. `Arena::alloc` was 76% of all CPU on
    /// `DefaultCatalogAndSchemaTest` because of exactly this shape.
    #[test]
    fn a_hopeless_request_against_a_huge_span_tier_is_not_a_scan() {
        let spans = 40_000usize;
        let span = 8 * 1024usize;
        let mut arena = Arena::new(spans * span * 2);
        arena.alloc(arena.capacity(), 8).unwrap();
        for i in 0..spans {
            arena.add_free_block(i * span * 2, span);
        }
        assert_eq!(arena.free_span_shape(), (spans, 1));
        // Every one of these is a proven miss: no span is anywhere near 1 MiB.
        for _ in 0..spans {
            assert!(arena.alloc(1024 * 1024, 8).is_none());
        }
        // ...and the tier is untouched, so nothing was consumed on the way.
        assert_eq!(arena.free_span_shape(), (spans, 1));
        assert_eq!(arena.free_list_bytes(), spans * span);
    }

    /// The merge must not invent contiguity: a live object between two holes is
    /// a wall, and merging across it would hand the same bytes out twice.
    #[test]
    fn coalesce_does_not_merge_across_a_gap() {
        let mut arena = Arena::new(64 * 1024);
        arena.alloc(arena.capacity(), 8).unwrap();
        arena.add_free_block(0, 4096);
        arena.add_free_block(4096 + 8, 4096); // 8-byte live wall
        assert_eq!(arena.coalesce_free_list(), 0);
        assert_eq!(arena.largest_free_block(), 4096);
        assert_eq!(arena.free_list_bytes(), 8192);
        assert!(arena.alloc(8192, 8).is_none());
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
    fn sort_by_offset_matches_the_reference_sort_on_both_branches() {
        // A deterministic LCG rather than a dependency; the point is a spread
        // of offsets that straddles several radix digits, not randomness.
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // 8 exercises the comparison branch, 5000 the radix branch, and 512 is
        // exactly the boundary.
        for &n in &[0usize, 1, 8, 511, 512, 513, 5000] {
            let mut offsets: Vec<usize> = Vec::with_capacity(n);
            while offsets.len() < n {
                // Unique, 8-aligned, inside a 512 MB arena — the real key shape.
                let off = ((next() as usize) % (64 * 1024 * 1024)) * 8;
                if !offsets.contains(&off) {
                    offsets.push(off);
                }
            }
            let mut v: Vec<(usize, usize)> = offsets.iter().map(|&o| (o, o ^ 0xFF)).collect();
            let mut expected = v.clone();
            expected.sort_by_key(|&(off, _)| off);
            super::sort_by_offset(&mut v);
            assert_eq!(v, expected, "n={n}");
        }
    }

    #[test]
    fn sort_by_offset_is_stable_on_duplicate_keys() {
        // Free blocks never share an offset, so this is belt-and-braces: it
        // pins that the radix branch cannot reorder equal keys, which is what
        // lets both branches claim to match the old `sort_by_key`.
        let mut v: Vec<(usize, usize)> = (0..2000).map(|i| (i % 4, i)).collect();
        let mut expected = v.clone();
        expected.sort_by_key(|&(off, _)| off);
        super::sort_by_offset(&mut v);
        assert_eq!(v, expected);
    }

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
