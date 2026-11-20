// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
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

/// What [`Arena::return_unused_tail`] did with a retiring TLAB's unused tail
/// (gen r4w6/tlab6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailReturn {
    /// The tail ended at the low bump cursor, which now sits at its start.
    Retracted,
    /// The tail is a block on the low free list.
    FreeListed,
    /// Not taken; the caller installs its filler.
    Declined,
}

/// The extents [`Arena::reset_deferring_zero`] left un-zeroed, as arena
/// offsets: `[0, low_end)` below the old low cursor and `[high_start, len)`
/// above the old high one.
#[derive(Debug, Clone, Copy)]
pub struct DeferredWipe {
    low_end: usize,
    high_start: usize,
}

impl DeferredWipe {
    /// Bytes the wipe covers before committed-granule filtering.
    pub fn extent_bytes(&self, capacity: usize) -> usize {
        self.low_end.min(capacity) + capacity.saturating_sub(self.high_start)
    }
}

/// `CRATONVM_GC_OBJECT_STARTS` -- maintain an exact object-start bitmap on the
/// arenas whose owner asks for one, and let `is_object_address` answer from it
/// instead of deducing the answer from header bytes. See [`Arena::starts`].
///
/// **Opt-in.** It was default-ON from 2026-09-05 to 2026-09-06; the flip is
/// reverted and the reason is the MEASUREMENT, not the mechanism.
///
/// # Why the default came back off
///
/// The flip cited "median ~7% faster with it on" from a sequential ABBA of
/// three runs per arm on the shared Azure box. That design does not work here
/// and this tree already knows it does not: ~20 other agents build on that
/// machine, so the disturbance is SPIKY rather than a monotone drift, and a
/// spike parked in an ABBA's two middle slots is arithmetically
/// indistinguishable from the treatment. The same design once read 1.9x for a
/// JIT flag that costs nothing.
///
/// Re-run 2026-09-06 as CONCURRENT PAIRS -- both arms started within two
/// seconds of each other so they see the same host in the same seconds, launch
/// order alternated, only the ratio read, every arm checked for its probe's own
/// success line:
///
/// | workload | pairs | `on/off` ratios | median |
/// |---|---|---|---|
/// | single-threaded churn, 96 MiB live | 6 | 0.914 1.006 0.888 1.053 0.742 1.077 | 0.96 |
/// | 8-thread churn, GROWING young set | 8 | 1.030 1.085 1.001 1.079 1.035 1.059 1.018 0.846 | **1.03** |
///
/// Neither reproduces the 7%. The single-threaded shape is scattered either
/// side of 1.0; the multi-threaded one -- the shape the record that flipped the
/// default explicitly listed as unmeasured, "a workload whose young arenas GROW
/// ... and a multi-threaded allocator" -- is SLOWER in 7 of 8 pairs. The bitmap
/// is rebuilt on every arena growth, which is a mechanism for that and is the
/// path nothing had run.
///
/// So the switch stays, the instrument stays, and the default waits for a
/// number taken under a design this host does not break. `=0` restores the
/// header-shaped deduction exactly and remains the first thing to set if an
/// object is ever suspected of being accepted at an address it does not start
/// at. The footprint is capacity/64 of side table, charged only to the arenas
/// that consult it -- the two young semi-spaces -- rather than to every `Arena`
/// in the process, which is what the first version did (see
/// [`Arena::arm_object_starts`]).
fn object_starts_enabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    matches!(
        Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
        Ok(v) if !v.is_empty() && v != "0" && v != "false" && v != "off" && v != "no"
    )
}



struct ArenaUncommitPolicy {
    release_window: bool,
    floor: usize,
    enabled: bool,
    delay: std::time::Duration,
    idle_since: Vec<Option<std::time::Instant>>,
    /// zr z3/fp: keep a granule's idle stamp when its release was REFUSED (the
    /// `-Xms` floor budget), instead of restarting its clock. Set by the owner
    /// ([`Arena::set_keep_refused_idle_stamps`]); `false` is the old behaviour.
    keep_refused_idle_stamps: bool,
    /// zr z4/un: count a granule the READABLE give-back reset (pages returned,
    /// mapping kept) as no longer committed for the `-Xms` floor and for
    /// `Runtime.totalMemory()`, and do not reset it again until a writer
    /// commits it for use. Set by the owner ([`Arena::set_reset_accounting`]);
    /// `false` is the pre-z4 behaviour (a reset granule stays counted, so the
    /// floor test saw the peak commitment for ever and every give-back pass
    /// re-reset granules that were already empty).
    reset_accounting: bool,
    /// zr z4/un: per granule, "reset by a give-back and not committed for use
    /// since". Empty unless `reset_accounting`.
    reset_idle: Vec<bool>,
    /// zr z4/un: how many `reset_idle` entries are set.
    reset_idle_count: usize,
    /// zt t2/al (`zgc-zt1sp-a-refused-commit-on-a-free-list-fit-drops-the-block-FIXED-20261019.md`):
    /// when the commit of a span a free-list fit has just taken off its list
    /// is REFUSED, put the span back on its list instead of leaving it in no
    /// list until a slide rebuilds the free list. Set by the owner
    /// ([`Arena::set_refused_fit_relists`], ZGC: `CRATONVM_ZGC_REFUSED_FIT_RELISTS`,
    /// default ON, `=0` kills); `false` is the old behaviour. Lives in the
    /// policy box because only an arena with a policy (ZGC) gives free-list
    /// granules back to the OS, which is the only way such a commit can be
    /// refused -- and so every other collector's arena never reads it.
    refused_fit_relists: bool,
    /// zt t2/al: spans [`Self::refused_fit_relists`] put back (engagement).
    refused_fits_relisted: usize,
    /// zt t3/rl (`CRATONVM_ZGC_BITMAP_RELEASE_WITH_GRANULE`): while `Some`,
    /// every span [`Arena::release_unused_span`] gave back (offsets), so the
    /// owner can give back the side-table pages over it under the same lock
    /// ([`Arena::set_release_log`] / [`Arena::take_release_log`]).
    released_log: Option<Vec<(usize, usize)>>,
    /// zt t23/hunt4 (`-XX:+AlwaysPreTouch`): touch every page a commit for use
    /// makes resident again ([`Arena::set_pre_touch`]). Lives in the policy box
    /// for the reason `refused_fit_relists` does: only ZGC installs one.
    pre_touch: bool,
    /// zt t23/hunt4: bytes [`Self::pre_touch`] touched (engagement).
    pre_touched_bytes: usize,
    /// zki (page `zgc-zt23hunt4-*` item 4): commits for use the hugetlbfs
    /// pool refused on a store backed by explicit huge pages
    /// ([`Arena::use_explicit_huge_pages`]): HotSpot logs one `Failed to
    /// commit memory` line for each. Survives a policy change.
    hugetlb_commit_refusals: usize,
    /// zki: the granules of the last such refused request.
    hugetlb_last_refused_granules: usize,
    /// Test hook: refuse every commit for use, as an OS at its commit limit
    /// does (`commit_range_for_use`). Never set outside unit tests.
    #[cfg(test)]
    refuse_commits_for_test: bool,
}

/// Test hook of `Arena::commit_range_for_use`: `ArenaUncommitPolicy::refuse_commits_for_test`.
#[cfg(test)]
fn refuse_commit_for_test(policy: &ArenaUncommitPolicy, len: usize) -> bool {
    policy.refuse_commits_for_test && len != 0
}

/// Outside unit tests no commit is refused by the arena itself.
#[cfg(not(test))]
#[inline(always)]
fn refuse_commit_for_test(_policy: &ArenaUncommitPolicy, _len: usize) -> bool {
    false
}

/// zr z4/un: what one [`Arena::uncommit_idle_pass`] did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArenaIdlePass {
    /// Bytes that became real holes (decommitted).
    pub decommitted: usize,
    /// Bytes reset (physical pages returned, mapping kept).
    pub reset: usize,
    /// Committed, unused granules seen that were not yet due.
    pub pending: usize,
    /// The earliest time a pending granule becomes due, if any.
    pub next_due: Option<std::time::Instant>,
    /// The pass stopped at its granule budget with work left.
    pub budget_hit: bool,
}

pub struct Arena {
    /// EXACT object-start membership for this arena, when
    /// `CRATONVM_GC_OBJECT_STARTS` is on.
    ///
    /// # What it replaces, and why the replacement is only half-done on purpose
    ///
    /// `gen_heap::is_object_address` decides "is this address an allocation
    /// base" by DEDUCING it from the bytes at that address: alignment, a bounds
    /// scan, a commit probe, two enum-tag validations, header plausibility, a
    /// slot-count cap, an extent computation and a second commit probe. It is a
    /// guess, and its own comment names the false positive it cannot eliminate
    /// -- an interior 16-byte `Value` cell of an `Object[]` decodes as a
    /// self-consistent fake header (`class_id=4`, `num_slots=4`), caught only
    /// afterwards by the extent check.
    ///
    /// A bit per 8 bytes, set where the allocation actually happens, answers the
    /// question instead of deducing it: one shift and one bit test, exact.
    ///
    /// It is consulted in the ACCEPT direction only. A hit short-circuits the
    /// deduction; a MISS falls through to it unchanged. That asymmetry is the
    /// safety argument and it is deliberate: a bitmap that is not complete would,
    /// used to REJECT, drop a live root and produce a use-after-free, and this
    /// one is knowably incomplete -- a TLAB chunk is one `hand_out` and the
    /// objects bump-allocated inside it never reach this code. Used to accept,
    /// incompleteness costs only the old path.
    ///
    /// Exactness in the accept direction needs the removal half, and gets it:
    /// `hand_out` is the single chokepoint every allocation funnels through and
    /// `add_free_block` the single one every free does, so a base that is freed
    /// stops being a base here at the same moment. Without that, a larger object
    /// later allocated OVER a freed base would see the stale bit, be accepted at
    /// an interior address, and take a mark-bit write into the middle of a live
    /// object -- which is the corruption the extent check exists to stop.
    ///
    /// `None` when the switch is off, which is the default: a bit per 8 bytes is
    /// capacity/64 bytes of side table (64 MB for a 4 GB heap), and whether that
    /// is worth it is a measurement this ships the instrument for.
    starts: Option<std::sync::Arc<crate::heap_bitmap::HeapBitmap>>,
    /// Backing storage for `capacity` bytes.
    ///
    /// Reserved address space committed in 2 MiB granules as the cursors reach
    /// them, or -- under `CRATONVM_GC_RESERVE=0`, on a platform whose syscalls
    /// this crate does not declare, or when the OS refuses the reservation --
    /// the wholly-committed `alloc_zeroed` block this used to be. See
    /// [`crate::reservation`] for why the difference is visible in a process's
    /// commit charge and its resident set but nowhere else.
    data: crate::reservation::HeapStore,
    /// Only ZGC installs a policy; other arenas retain their original path.
    uncommit_policy: Option<Box<ArenaUncommitPolicy>>,
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
    /// zs s10/sp (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`, ZGC only, default ON since zt t12, `=0` kill):
    /// while non-zero, a LOW free-list block smaller than this serves no
    /// request -- every free-list fit is made from a block of at least
    /// `min_free_span` bytes (the rest of the chosen block goes back on the
    /// list as one block: [`Self::route_fit_remainders`]). The blocks stay
    /// listed (nothing is removed, so nothing can leak or be listed twice);
    /// only the fit predicate changes. zt t1/sp: it binds every exit a SMALL
    /// request can take -- the low free list, a bump tail shorter than it
    /// (both bump arms), the high end's free list as `alloc`'s last resort,
    /// and [`Self::take_smallest_block_at_least`]. Large-object requests
    /// (`alloc_high`) are never below it. `0` (the default, and every
    /// non-ZGC arena) is today's behaviour. Set and cleared by
    /// `ZgcRealHeap::note_final_refusal` / its post-collection re-check.
    min_free_span: usize,
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
    /// one per `1 << anchor_shift`-byte bucket of this arena, stored as
    /// `offset + 1`. `0`
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
    /// Blocks pushed since the last [`Arena::coalesce_free_list`], i.e. how
    /// much adjacency a merge could possibly have to collapse. Bumped by every
    /// push (`push_block_routed`), zeroed by the merge.
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
    /// The cursor value the most recent retractions moved away from, maxed --
    /// see [`Self::low_high_water`], which is the only reader.
    pre_retract_high: usize,
    /// The HIGH cursor's mirror of [`Self::pre_retract_high`]: the lowest that
    /// cursor has been since [`Self::reset_high_low_water`], minned -- see
    /// [`Self::high_low_water`], which is the only reader.
    ///
    /// The high end bumps DOWN, so its water mark is a MINIMUM and a retraction
    /// RAISES the cursor away from it. `usize::MAX` is the "no retraction since
    /// the last reset" identity for `min`.
    pre_retract_low: usize,
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
    /// Give pages back WITHOUT unmapping them (round 9 wave 9, `zgc9`).
    ///
    /// Off by default, and only its owner turns it on: `ZgcRealHeap` does so
    /// when it has published the whole arena to the JIT as safe to load from
    /// raw (`gen_heap::publish_jit_read_bounds`) and
    /// `CRATONVM_ZGC_READABLE_GIVE_BACK` asks for it. When set, every private
    /// give-back -- the three cursor retractions and the two
    /// collection-start passes, all of which go through
    /// `Arena::decommit_span` -- uses [`HeapStore::reset_range`] instead of
    /// [`HeapStore::decommit_range`]: the physical pages still go back to the
    /// OS, but the granule stays committed and readable, so the published
    /// bound stays true and never has to be withdrawn.
    ///
    /// [`HeapStore::reset_range`]: crate::reservation::HeapStore::reset_range
    /// [`HeapStore::decommit_range`]: crate::reservation::HeapStore::decommit_range
    readable_give_back: bool,
    /// Bytes the `readable_give_back` mode has handed back to the OS while
    /// keeping them mapped, summed over the arena's life.
    bytes_reset: usize,
    /// PRISTINE WINDOW (round 9 wave 10, `zgc10`): the offsets
    /// `[pristine_lo, pristine_hi)` that no writer has been given since this
    /// arena was created, and which therefore still read as zero -- the
    /// backing store is zeroed at birth (`HeapStore::new`: a lazily-committed
    /// reservation or `alloc_zeroed`).
    ///
    /// Shrunk, never grown, at every door that makes arena bytes writable:
    /// [`Self::hand_out`] (every allocation), [`Self::commit_for_relocation`]
    /// (the slides' arithmetic destinations) and
    /// [`Self::commit_parallel_evacuation_region`]. Removing a written range
    /// keeps the larger of the two remaining pieces, so a low-bump hand-out
    /// raises `pristine_lo` and a high-end descent lowers `pristine_hi`.
    ///
    /// Growing it back (a give-back returns zeroed pages) is deliberately NOT
    /// done: `ZgcRealHeap::stamp_forwarding_words` writes into span a slide
    /// retracted past without any of the doors above, and under the readable
    /// give-back that span can be a reset granule. A monotone window is immune
    /// to every such writer, because it only ever covers bytes that were never
    /// handed out at all.
    pristine_lo: usize,
    /// Exclusive top of the pristine window; see [`Self::pristine_lo`].
    pristine_hi: usize,
    /// `(offset, lo, hi)` of the most recent [`Self::hand_out`]: its offset and
    /// the part of it that was inside the pristine window when it was handed
    /// out (`lo == hi` when none was). Consumed by
    /// [`Self::take_last_hand_out_pristine`].
    last_hand_out_pristine: (usize, usize, usize),
    /// May a TLAB-chunk carver skip zeroing the pristine part of a chunk?
    /// Off unless the owner opts in ([`Self::set_skip_pristine_zeroing`]);
    /// only `ZgcRealHeap` does. The window itself is tracked either way.
    skip_pristine_zeroing: bool,
    /// zt t31/zc11 (`CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION`, ZGC only):
    /// [`Self::compact_low_to`] leaves the tail above its new cursor as it is
    /// and does not list it as zeroed, so the hand-out that next takes those
    /// bytes zeroes them (`take_last_hand_out_zero_plan`), as HotSpot zeroes at
    /// allocation. Off unless the owner opts in
    /// ([`Self::set_defer_slide_tail_zeroing`]).
    defer_slide_tail_zeroing: bool,
    /// The open TO-SPACE window, or `None` -- which is the state every arena
    /// in this VM is in at every instant except inside a page-evacuating ZGC
    /// pause. See [`ToSpaceWindow`] and [`Self::open_to_space`].
    ///
    /// **Nothing reads this field unless it is `Some`**, and only
    /// [`Self::open_to_space`] can make it `Some`. That is the whole of the
    /// inertness argument for the other two collectors: G1 and the
    /// generational heap never call `open_to_space`, so for them this field is
    /// one `None` initialised in [`Self::new`], tested by three functions that
    /// return early, and never written again.
    to_space: Option<ToSpaceWindow>,
    /// gce e1/y (item 2 of `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`):
    /// LOW spans, `(offset, len)`, ascending and disjoint, that the owner has
    /// PROVED read zero -- the generational non-moving sweep's reclaimed
    /// spans, zeroed by `zero_spans_parallel` in its publication step and
    /// installed through [`Self::set_known_zero_free_spans`] at the end of it.
    /// [`Self::hand_out_is_known_zero`] answers whether a free-list hand-out
    /// lies wholly inside one, so the refill can skip the second memset.
    ///
    /// # Why a span set and not a bit per free block
    ///
    /// A `FreeBlock` flows through the small-bucket tier, the span tier,
    /// splits, merges and every rebuild; a bit would have to be threaded
    /// through all of them. The claim here is about BYTES, not about list
    /// membership, so the list's internal reshaping cannot falsify it.
    ///
    /// # What keeps the claim true
    ///
    /// Bytes of a span stop reading zero only once they are handed out (the
    /// caller writes them). Handed-out bytes can be handed out AGAIN only after
    /// they are listed again, and every door that lists bytes that were once
    /// handed out FORGETS the whole set: [`Self::add_free_block`],
    /// [`Self::return_unused_tail`], [`Self::rebuild_low_free_list`],
    /// [`Self::rebuild_after_in_place_evacuation`] (it also poisons quarantined
    /// spans), [`Self::compact_low_to`], every reset, the cursor retractions
    /// and [`Self::grow`]. The doors that only re-shape what is ALREADY listed
    /// keep it: the split remnants of `alloc`, and [`Self::coalesce_free_list`]
    /// (which saves and restores it around its re-adds). The next sweep
    /// REPLACES the set with its own spans. Empty unless an owner installs a
    /// set, so every other collector's arena never consults it.
    // Boxed: `Arena` is embedded in the hot `ZgcRealHeap`, whose size is
    // ratcheted (`zgc::tests::the_hot_heap_struct_does_not_grow_back`).
    ///
    /// Shares ONE box with the slide's zeroed set (r16 zgcarray, proposal ZA1;
    /// see [`SlideZeroWindow`]), so the second set did not grow the hot heap.
    zero_sets: Box<ZeroSets>,
}

/// The two known-zero span sets, boxed together (see `Arena::zero_sets`).
#[derive(Debug, Default)]
struct ZeroSets {
    /// The generational sweep's set (the `zero_sets` field doc above it).
    known_zero: Vec<(usize, usize)>,
    /// JIT round 23 wave 4, lane L4 (`CRATONVM_GEN_IN_PLACE_LAZY_ZERO`): the
    /// OPPOSITE claim -- LOW offset spans (ascending, disjoint) that may hold
    /// stale bytes although they are free-listed or lie at or above the low
    /// cursor, because a pinned in-place young cycle left them unzeroed
    /// (headers poisoned only). Every hand-out overlapping one is memset in
    /// full ([`Arena::take_lazy_dirty`]); every reset zeroes up to its end
    /// ([`Arena::lazy_dirty_end`]). Removing a byte from it is a promise the
    /// byte is (about to be) zero; adding one only ever costs a memset.
    ///
    /// r23 w4 review (R-L4z): a B-tree interval set (O(log n) per span, no
    /// `Vec::splice` under the young lock) plus the lock-free "not an object"
    /// mirror the conservative validator consults.
    lazy_dirty: LazyDirtySet,
    /// The ZGC slide's set.
    slide: SlideZeroWindow,
}

/// r23 w4 review (R-L4z, `docs/feature-designs/generative-gc-architecture.md`):
/// the lazy-dirty set as a B-tree of disjoint, non-touching LOW offset spans
/// (`start -> end`), and its lock-free mirror: one bit per 8 bytes over the
/// arena, set exactly for bytes inside a span. A span never holds a live
/// object (hand-outs and in-place copies are taken out before anything is
/// built there), so a set bit proves "not an object" -- what
/// `GenerationalHeap::is_object_address` asks it, so a stale conservative word
/// into a dead payload whose bytes decode as a plausible header is declined.
#[derive(Default)]
struct LazyDirtySet {
    spans: Vec<(usize, usize)>,
    mirror: Option<std::sync::Arc<crate::heap_bitmap::HeapBitmap>>,
    /// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`,
    /// default ON, `0` restores one locked RMW per word): latched when the
    /// mirror is created. Every mirror write goes through this arena (under
    /// its lock, or stop the world), so the mirror has one writer at a time
    /// and plain stores leave the words a `fetch_or` / `fetch_and` would.
    plain_stores: bool,
}

impl std::fmt::Debug for LazyDirtySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyDirtySet")
            .field("spans", &self.spans)
            .field("mirror", &self.mirror.is_some())
            .field("plain_stores", &self.plain_stores)
            .finish()
    }
}

impl LazyDirtySet {
    /// The end of the last span, `0` when empty.
    fn end(&self) -> usize {
        self.spans.last().map_or(0, |&(_, e)| e)
    }

    /// Add `[lo, hi)`, joining every span it overlaps or touches.
    pub fn insert(&mut self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        self.insert_batch(&[(lo, hi.saturating_sub(lo))]);
    }

    /// t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
    /// add multiple `(offset, len)` spans in batch, joining any spans that overlap or touch.
    ///
    /// Avoids O(m * log n) individual B-tree searches/removals/rebalances when noting
    /// hundreds of thousands of dead spans reclaimed in a single GC pause.
    fn insert_batch(&mut self, spans: &[(usize, usize)]) {
        if spans.is_empty() {
            return;
        }
        let mut input: Vec<(usize, usize)> = spans
            .iter()
            .filter_map(|&(o, l)| (l > 0).then_some((o, o.saturating_add(l))))
            .collect();
        if input.is_empty() {
            return;
        }

        if !input.windows(2).all(|w| w[0].0 <= w[1].0) {
            input.sort_unstable_by_key(|&(s, _)| s);
        }

        let mut coalesced: Vec<(usize, usize)> = Vec::with_capacity(input.len());
        let mut cur = input[0];
        for &(s, e) in &input[1..] {
            if s <= cur.1 {
                cur.1 = cur.1.max(e);
            } else {
                coalesced.push(cur);
                cur = (s, e);
            }
        }
        coalesced.push(cur);

        if self.spans.is_empty() {
            self.spans = coalesced;
            return;
        }

        let old_spans = std::mem::take(&mut self.spans);
        let mut it_old = old_spans.into_iter().peekable();
        let mut it_new = coalesced.into_iter().peekable();

        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(it_old.len() + it_new.len());
        let mut cur: Option<(usize, usize)> = None;

        while it_old.peek().is_some() || it_new.peek().is_some() {
            let next_item = match (it_old.peek(), it_new.peek()) {
                (Some(&(s1, _)), Some(&(s2, _))) => {
                    if s1 <= s2 {
                        it_old.next().unwrap()
                    } else {
                        it_new.next().unwrap()
                    }
                }
                (Some(_), None) => it_old.next().unwrap(),
                (None, Some(_)) => it_new.next().unwrap(),
                (None, None) => unreachable!(),
            };

            match cur {
                None => cur = Some(next_item),
                Some((ref mut c_s, ref mut c_e)) => {
                    if next_item.0 <= *c_e {
                        *c_e = (*c_e).max(next_item.1);
                    } else {
                        merged.push((*c_s, *c_e));
                        cur = Some(next_item);
                    }
                }
            }
        }
        if let Some(c) = cur {
            merged.push(c);
        }

        self.spans = merged;
    }

    /// Remove `[a, b)`. Returns the overlapped extent clipped to `[a, b)`
    /// (lowest to highest removed byte), `None` when nothing overlapped.
    fn remove(&mut self, a: usize, b: usize) -> Option<(usize, usize)> {
        if b <= a || self.spans.is_empty() {
            return None;
        }
        let i = self.spans.partition_point(|&(_, e)| e <= a);
        if i >= self.spans.len() {
            return None;
        }
        let j = i + self.spans[i..].partition_point(|&(s, _)| s < b);
        if i == j {
            return None;
        }
        let mut lo_hit = usize::MAX;
        let mut hi_hit = 0usize;
        let mut keep = Vec::with_capacity(2);
        for &(s, e) in &self.spans[i..j] {
            lo_hit = lo_hit.min(s.max(a));
            hi_hit = hi_hit.max(e.min(b));
            if s < a {
                keep.push((s, a));
            }
            if e > b {
                keep.push((b, e));
            }
        }
        self.spans.splice(i..j, keep);
        (lo_hit < hi_hit).then_some((lo_hit, hi_hit))
    }

    /// t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
    /// remove multiple `(offset, len)` spans in batch via a linear two-pointer sweep.
    ///
    /// Avoids O(m * log n) individual B-tree lookups/removals when forgetting
    /// hundreds of thousands of surviving objects.
    /// Returns the removed sub-intervals (for mirror clearing).
    fn remove_batch(&mut self, spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
        if self.spans.is_empty() || spans.is_empty() {
            return Vec::new();
        }
        let old_spans = std::mem::take(&mut self.spans);
        let mut old_iter = old_spans.into_iter();
        let mut cut_iter = spans.iter().filter_map(|&(o, l)| (l > 0).then_some((o, o.saturating_add(l))));

        let mut cur_d = old_iter.next();
        let mut cur_c = cut_iter.next();

        let mut remaining = Vec::with_capacity(32);
        let mut removed = Vec::with_capacity(32);

        while let Some((d_lo, d_hi)) = cur_d {
            let Some((c_lo, c_hi)) = cur_c else {
                remaining.push((d_lo, d_hi));
                remaining.extend(old_iter);
                break;
            };

            if c_hi <= d_lo {
                cur_c = cut_iter.next();
            } else if c_lo >= d_hi {
                remaining.push((d_lo, d_hi));
                cur_d = old_iter.next();
            } else {
                if d_lo < c_lo {
                    remaining.push((d_lo, c_lo));
                }
                let r_lo = d_lo.max(c_lo);
                let r_hi = d_hi.min(c_hi);
                if r_hi > r_lo {
                    match removed.last_mut() {
                        Some((_, ref mut h)) if *h == r_lo => *h = r_hi,
                        _ => removed.push((r_lo, r_hi)),
                    }
                }
                if d_hi > c_hi {
                    cur_d = Some((c_hi, d_hi));
                    cur_c = cut_iter.next();
                } else {
                    cur_d = old_iter.next();
                }
            }
        }

        self.spans = remaining;
        removed
    }

    /// The spans overlapping `[a, b)`, unclipped, ascending.
    fn overlapping(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if b <= a || self.spans.is_empty() {
            return out;
        }
        let i = self.spans.partition_point(|&(_, e)| e <= a);
        for &(s, e) in &self.spans[i..] {
            if s >= b {
                break;
            }
            out.push((s, e));
        }
        out
    }

    /// `(offset, len)` pairs, ascending (tests and diagnostics).
    #[cfg(test)]
    fn as_vec(&self) -> Vec<(usize, usize)> {
        self.spans.iter().map(|&(s, e)| (s, e - s)).collect()
    }
}

/// ZA1 (`docs/feature-designs/zgc-architecture.md`):
/// `spans` (LOW offsets, ascending, disjoint) are the bytes
/// [`Arena::compact_low_to`] zero-filled -- the vacated spans it free-listed
/// and the tail above its new cursor -- that no writer has been given since,
/// minus the words in `stamps`.
///
/// Without it every byte a garbage-heavy ZGC cycle reclaimed was zeroed
/// TWICE: once by the slide (load-bearing: a conservative scan must not meet a
/// corpse header, and `ZgcRealHeap::stamp_forwarding_words` relies on the tail
/// reading zero) and again when an allocation handed the same bytes out
/// (`carve_tlab_chunk` per chunk, `alloc_raw` per object). Measured on
/// `R16ZgcarrayAlloc -Dlens=4096`: the slide's memset alone was half of a
/// no-storm profile.
///
/// # What keeps the claim true
///
/// The set only SHRINKS: every door that makes arena bytes writable
/// ([`Arena::note_writable`]: every hand-out, every relocation and
/// parallel-evacuation destination) removes its range, and the next slide
/// REPLACES the set. So re-listing OTHER bytes (a sweep, a TLAB retire) cannot
/// falsify it, unlike the generational `known_zero` set, which is not shrunk
/// at hand-out and is therefore forgotten at every such door. The writers
/// that reach a span without a door write only into bytes that were handed
/// out (and so already left the set): the sweep's dead-header zeroing and a
/// TLAB retire's filler. The one exception is `stamp_forwarding_words`, which
/// writes into the vacated tail and records each 16-byte record it writes
/// ([`Arena::note_slide_stamp`]) so a consumer zeroes those words itself. A
/// give-back of a granule inside a span returns pages that read zero or their
/// old contents, and the old contents are the zeroes (and the recorded
/// stamps) this set already describes.
///
/// Only an arena whose owner opted into skipping pristine zeroing
/// ([`Arena::set_skip_pristine_zeroing`], ZGC) ever fills one.
#[derive(Debug, Default)]
struct SlideZeroWindow {
    spans: Vec<(usize, usize)>,
    /// Offsets of forwarding records stamped inside the set, each
    /// `cratonvm_types::MIN_OBJECT_SIZE` bytes long. Appended in whatever
    /// order the slide stamps them; sorted (once) on the first read, see
    /// `stamps_sorted`.
    stamps: Vec<usize>,
    stamps_sorted: bool,
    /// The offset of the most recent [`Arena::note_writable`] and the pieces
    /// of its range that were in the set (LOW offsets, ascending). See
    /// [`Arena::take_last_hand_out_zero_plan`].
    last_at: usize,
    last: Vec<(usize, usize)>,
}

impl SlideZeroWindow {
    /// Remove `[a, b)` from the set, recording the removed pieces in `last`.
    fn take_range(&mut self, a: usize, b: usize) {
        self.last.clear();
        self.last_at = a;
        if self.spans.is_empty() || a >= b {
            return;
        }
        // The first span that ends after `a`.
        let first = self.spans.partition_point(|&(_, hi)| hi <= a);
        let mut i = first;
        let mut keep: [(usize, usize); 2] = [(0, 0); 2];
        let mut nkeep = 0;
        while i < self.spans.len() && self.spans[i].0 < b {
            let (lo, hi) = self.spans[i];
            self.last.push((lo.max(a), hi.min(b)));
            if lo < a {
                keep[nkeep] = (lo, a);
                nkeep += 1;
            }
            if hi > b {
                keep[nkeep.min(1)] = (b, hi);
                nkeep = (nkeep + 1).min(2);
            }
            i += 1;
        }
        if i > first {
            self.spans.splice(first..i, keep[..nkeep].iter().copied());
        }
    }

    /// Add `[a, b)` (LOW offsets, proved zero by the caller) to the set,
    /// coalescing with any touching or overlapping spans.
    fn insert_range(&mut self, a: usize, b: usize) {
        if a >= b {
            return;
        }
        let lo = self.spans.partition_point(|&(_, hi)| hi < a);
        let hi = self.spans.partition_point(|&(slo, _)| slo <= b);
        let mut new_lo = a;
        let mut new_hi = b;
        if lo < hi {
            new_lo = new_lo.min(self.spans[lo].0);
            new_hi = new_hi.max(self.spans[hi - 1].1);
        } else if self.spans.len() >= 32 {
            if lo == 0 {
                self.spans[0] = (new_lo, new_hi);
            } else {
                self.spans.remove(0);
                self.spans.insert(lo - 1, (new_lo, new_hi));
            }
            return;
        }
        self.spans.splice(lo..hi, std::iter::once((new_lo, new_hi)));
    }
}

/// What [`Arena::take_last_hand_out_zero_plan`] says about the most recent
/// hand-out: the absolute ranges inside it that already read zero (ascending,
/// disjoint), the part of those that is the pristine window's, and the
/// absolute addresses of the forwarding records inside them that do NOT read
/// zero (each `cratonvm_types::MIN_OBJECT_SIZE` bytes).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ZeroPlan {
    pub known_zero: Vec<(usize, usize)>,
    pub pristine_bytes: usize,
    pub slide_bytes: usize,
    pub dirty_stamps: Vec<usize>,
}

impl ZeroPlan {
    /// Zero `[ptr, ptr + len)` except what `plan` proves already reads zero
    /// (and zero the dirty stamps inside that). `None` zeroes it all.
    ///
    /// # Safety
    /// `[ptr, ptr + len)` must be writable and exclusively the caller's, and
    /// `plan` must be the arena's answer for exactly that hand-out.
    pub unsafe fn zero(plan: Option<&ZeroPlan>, ptr: *mut u8, len: usize) {
        let Some(plan) = plan else {
            // SAFETY: the caller's contract.
            unsafe { std::ptr::write_bytes(ptr, 0, len) };
            return;
        };
        let start = ptr as usize;
        let end = start + len;
        let mut cursor = start;
        for &(lo, hi) in &plan.known_zero {
            // SAFETY: the plan's ranges are clamped to the hand-out, ascending
            // and disjoint, so `cursor <= lo <= end`.
            unsafe { std::ptr::write_bytes(cursor as *mut u8, 0, lo - cursor) };
            cursor = hi;
        }
        // SAFETY: as above; `cursor <= end`.
        unsafe { std::ptr::write_bytes(cursor as *mut u8, 0, end - cursor) };
        for &stamp in &plan.dirty_stamps {
            let lo = stamp.max(start);
            let hi = stamp
                .saturating_add(cratonvm_types::MIN_OBJECT_SIZE)
                .min(end);
            if lo < hi {
                // SAFETY: inside the hand-out.
                unsafe { std::ptr::write_bytes(lo as *mut u8, 0, hi - lo) };
            }
        }
    }

    /// zt t2/al (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`, opt-in): exactly the
    /// bytes [`Self::zero`] zeroes, written in `block`-byte pieces from the
    /// TOP of the hand-out down, so the LOWEST addresses -- the ones a bump
    /// allocator hands out first -- are the most recently written when the
    /// zeroing ends, and the likeliest to still be in L1 / L2 when the first
    /// headers are stored into them. Each piece is an ordinary ascending
    /// `write_bytes` of at most `block` bytes, which also keeps every piece
    /// below the size where a `memset` switches to non-temporal stores (those
    /// leave the zeroed lines in DRAM, and the bump then misses on every one).
    ///
    /// Same set of bytes, same final contents: the known-zero ranges are
    /// skipped and the dirty stamps zeroed exactly as `zero` does, so this is
    /// observably identical and differs only in store order. `block == 0` is
    /// [`Self::zero`].
    ///
    /// # Safety
    /// As [`Self::zero`].
    /// Zero `[lo, hi)` using non-temporal streaming stores on 64B-aligned
    /// blocks >= 64 KiB (avoiding DRAM RFO reads and L1/L2 pollution), followed
    /// by `_mm_sfence()`. Returns `true` if a fence is required/issued.
    #[cfg(target_arch = "x86_64")]
    unsafe fn stream_zero_slice(lo: usize, hi: usize) -> bool {
        if hi.saturating_sub(lo) < 64 * 1024 {
            unsafe { std::ptr::write_bytes(lo as *mut u8, 0, hi.saturating_sub(lo)) };
            return false;
        }
        let al_lo = (lo + 63) & !63;
        let al_hi = hi & !63;
        if al_hi <= al_lo || al_hi - al_lo < 64 * 1024 {
            unsafe { std::ptr::write_bytes(lo as *mut u8, 0, hi - lo) };
            return false;
        }
        if al_hi < hi {
            unsafe { std::ptr::write_bytes(al_hi as *mut u8, 0, hi - al_hi) };
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            unsafe { stream_zero_avx512(al_lo as *mut u8, al_hi - al_lo) };
        } else if std::arch::is_x86_feature_detected!("avx2") {
            unsafe { stream_zero_avx2(al_lo as *mut u8, al_hi - al_lo) };
        } else {
            unsafe { std::ptr::write_bytes(al_lo as *mut u8, 0, al_hi - al_lo) };
            if al_lo > lo {
                unsafe { std::ptr::write_bytes(lo as *mut u8, 0, al_lo - lo) };
            }
            return false;
        }
        if al_lo > lo {
            unsafe { std::ptr::write_bytes(lo as *mut u8, 0, al_lo - lo) };
        }
        true
    }

    /// Zero a newly carved TLAB/Eden region `[ptr, ptr+len)`: stream upper
    /// cold bulk (`>= start + 32 KiB`) non-temporally when large, then zero
    /// the warm bottom `[start, start + 32 KiB)` normally so first allocations
    /// hit warm L1d.
    pub unsafe fn fast_zero_tlab_bytes(ptr: *mut u8, len: usize) {
        if len <= 64 * 1024 {
            unsafe { std::ptr::write_bytes(ptr, 0, len) };
            return;
        }
        #[cfg(target_arch = "x86_64")]
        {
            let start = ptr as usize;
            let end = start + len;
            let warm_hi = start + 32 * 1024;
            let did_stream = unsafe { Self::stream_zero_slice(warm_hi, end) };
            if did_stream {
                unsafe { core::arch::x86_64::_mm_sfence() };
            }
            unsafe { std::ptr::write_bytes(ptr, 0, 32 * 1024) };
            return;
        }
        #[cfg(not(target_arch = "x86_64"))]
        unsafe {
            std::ptr::write_bytes(ptr, 0, len);
        }
    }

    pub unsafe fn zero_descending(
        plan: Option<&ZeroPlan>,
        ptr: *mut u8,
        len: usize,
        block: usize,
    ) {
        if block == 0 {
            // SAFETY: the caller's contract.
            unsafe { Self::zero(plan, ptr, len) };
            return;
        }
        let warm_cutoff = (ptr as usize).saturating_add(32 * 1024);
        let mut streamed = false;
        /// Zero `[lo, hi)`, streaming upper cold portions (`>= warm_cutoff`)
        /// non-temporally when large and keeping the bottom `[start..warm_cutoff)`
        /// zeroed normally so it stays hot in L1d.
        unsafe fn down(lo: usize, hi: usize, block: usize, warm_cutoff: usize, streamed: &mut bool) {
            if lo >= hi {
                return;
            }
            #[cfg(target_arch = "x86_64")]
            {
                let stream_lo = lo.max(warm_cutoff);
                if hi.saturating_sub(stream_lo) >= 64 * 1024 {
                    if unsafe { ZeroPlan::stream_zero_slice(stream_lo, hi) } {
                        *streamed = true;
                    }
                    let mut top = stream_lo;
                    while top > lo {
                        let bottom = top.saturating_sub(block).max(lo);
                        unsafe { std::ptr::write_bytes(bottom as *mut u8, 0, top - bottom) };
                        top = bottom;
                    }
                    return;
                }
            }
            let _ = warm_cutoff;
            let _ = streamed;
            let mut top = hi;
            while top > lo {
                let bottom = top.saturating_sub(block).max(lo);
                // SAFETY: `lo <= bottom < top <= hi`, inside the caller's range.
                unsafe { std::ptr::write_bytes(bottom as *mut u8, 0, top - bottom) };
                top = bottom;
            }
        }
        let start = ptr as usize;
        let end = start + len;
        let known: &[(usize, usize)] = match plan {
            Some(p) => &p.known_zero,
            None => &[],
        };
        // The gaps between the known-zero ranges, from the top down. The
        // ranges are clamped to the hand-out, ascending and disjoint, so each
        // gap `[khi, top)` has `khi <= top`.
        let mut top = end;
        for &(klo, khi) in known.iter().rev() {
            // SAFETY: a gap inside the hand-out.
            unsafe { down(khi, top, block, warm_cutoff, &mut streamed) };
            top = klo;
        }
        // SAFETY: the lowest gap, inside the hand-out.
        unsafe { down(start, top, block, warm_cutoff, &mut streamed) };
        #[cfg(target_arch = "x86_64")]
        if streamed {
            unsafe { core::arch::x86_64::_mm_sfence() };
        }
        if let Some(plan) = plan {
            for &stamp in &plan.dirty_stamps {
                let lo = stamp.max(start);
                let hi = stamp
                    .saturating_add(cratonvm_types::MIN_OBJECT_SIZE)
                    .min(end);
                if lo < hi {
                    // SAFETY: inside the hand-out.
                    unsafe { std::ptr::write_bytes(lo as *mut u8, 0, hi - lo) };
                }
            }
        }
    }
}


#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f")]
unsafe fn stream_zero_avx512(ptr: *mut u8, len: usize) {
    use core::arch::x86_64::{__m512i, _mm512_setzero_si512, _mm512_stream_si512};
    let z = _mm512_setzero_si512();
    let mut p = ptr;
    let end = unsafe { ptr.add(len) };
    while (end as usize) - (p as usize) >= 256 {
        unsafe {
            _mm512_stream_si512(p as *mut __m512i, z);
            _mm512_stream_si512(p.add(64) as *mut __m512i, z);
            _mm512_stream_si512(p.add(128) as *mut __m512i, z);
            _mm512_stream_si512(p.add(192) as *mut __m512i, z);
            p = p.add(256);
        }
    }
    while p < end {
        unsafe {
            _mm512_stream_si512(p as *mut __m512i, z);
            p = p.add(64);
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn stream_zero_avx2(ptr: *mut u8, len: usize) {
    use core::arch::x86_64::{__m256i, _mm256_setzero_si256, _mm256_stream_si256};
    let z = _mm256_setzero_si256();
    let mut p = ptr;
    let end = unsafe { ptr.add(len) };
    while (end as usize) - (p as usize) >= 128 {
        unsafe {
            _mm256_stream_si256(p as *mut __m256i, z);
            _mm256_stream_si256(p.add(32) as *mut __m256i, z);
            _mm256_stream_si256(p.add(64) as *mut __m256i, z);
            _mm256_stream_si256(p.add(96) as *mut __m256i, z);
            p = p.add(128);
        }
    }
    while p < end {
        unsafe {
            _mm256_stream_si256(p as *mut __m256i, z);
            p = p.add(32);
        }
    }
}

/// An open **to-space window**: bump-only allocation strictly above a floor,
/// for a copying collector that must not be handed its own from-space back.
///
/// # The problem this type exists to solve, stated exactly
///
/// [`Arena::alloc`] serves the **free list before it bumps**, and on ZGC the
/// sweep runs immediately before relocation and rebuilds the low free list out
/// of the complement of the live set -- which includes every garbage hole
/// inside the pages relocation is about to evacuate. So the single most likely
/// destination `alloc` can return for an evacuated object is a hole **inside
/// the page that object is being evacuated out of**, and in the limit the
/// destination aliases the source byte for byte. `ZRelocateContext::alloc_in`
/// forbids exactly that ("a destination inside a from-space page would be
/// evacuated again and could alias its source"), and before this type existed
/// there was no allocator on this heap that could promise it, which is why
/// `ZgcRealHeap::alloc_in` answered `None` and `CRATONVM_ZGC_PAGE_EVAC` (retired
/// in zt t12/rt, never wired) switched nothing. See
/// `docs/feature-designs/profile-guided-inlining.md`.
///
/// [`Arena::alloc_high`] is provably outside the low relocation set, but it is
/// the large-object end: putting small survivors there re-creates the
/// fragmentation failure the split was introduced to prevent, from the other
/// side (see [`REGION_LEAK_HITS`]).
///
/// # THE INVARIANT
///
/// > **A to-space allocation never returns memory inside the relocation set.**
///
/// It is discharged by two facts and one refusal, none of which is a property
/// of the collector's bookkeeping:
///
/// 1. **[`Self::floor`] is the low bump cursor at the instant the window was
///    opened.** Every byte an object can occupy at that instant is below the
///    cursor -- that is what the cursor *means* -- so every member of any
///    relocation set chosen from this arena's low region lies wholly in
///    `[0, floor)`. (ZGC's relocation set is chosen from `logical_pages`,
///    which tiles exactly `[arena_base, arena_base + low_cursor)`.)
/// 2. **[`Arena::alloc_to_space`] never consults a free list, at any tier.**
///    It is a bump and nothing else: it starts at [`Arena::cursor`], which is
///    at or above `floor` and only ever ascends while the window is open, so
///    every address it returns is `>= floor`. The free list is where the
///    aliasing destinations live, and this path cannot reach it -- not "does
///    not prefer it", cannot reach it. That is why this is a separate
///    function rather than a flag on `alloc`: `prefer_bump` is documented as
///    "a preference, not a wall" and falls back to the free list, which here
///    would be the bug.
/// 3. **While the window is open the arena refuses to lower the low cursor.**
///    [`Arena::retract_cursor_to`], [`Arena::retract_cursor_into_free_tail`]
///    and [`Arena::reset`] are the three ways `cursor` can descend; a
///    retraction below `floor` would put later to-space bytes over earlier
///    ones. They are refused for the window's lifetime rather than trusted not
///    to be called.
///
/// 1 and 2 together are the proof, and they are joint: neither is sufficient.
/// A bump-only path with no floor could still be handed a cursor that a
/// concurrent retraction had lowered into the set (3 closes that), and a floor
/// with a free-list fallback would step straight over it (2 closes that).
/// [`Arena::alloc_to_space`] asserts `>= floor` at the point of hand-out
/// anyway, because an invariant that is only argued is an invariant that gets
/// re-derived wrongly -- which is how this subsystem acquired two relocators.
///
/// # What it deliberately does NOT do
///
/// * It does not reserve. The window is a floor and a set of counters; the
///   bytes above the cursor stay available to `alloc` for as long as nobody
///   takes them. A collector that wants a reservation should size the
///   relocation set instead, which is `ZRelocationSet::select`'s job.
/// * It does not eat the large-object reserve. An evacuation that cannot fit
///   must fail as [`crate::zgc::relocate::ZRelocateError::OutOfToSpace`] --
///   the driver's answer is to shrink the relocation set -- and taking the
///   reserve would trade a relocation failure for a later large-object
///   `OutOfMemoryError`, i.e. would convert a recoverable state into an
///   unrecoverable one.
/// * It does not free anything on close. The from-space bytes are the
///   *driver's* to reclaim, after the reference rewrite, under ZR-1.
#[derive(Debug, Clone, Copy, Default)]
pub struct ToSpaceWindow {
    /// The low bump cursor when the window was opened, as an arena offset.
    /// **No to-space allocation is ever below this**, and every relocation-set
    /// page is.
    floor: usize,
    /// Bytes handed out through [`Arena::alloc_to_space`] since the open.
    bytes: usize,
    /// Objects handed out through [`Arena::alloc_to_space`] since the open.
    objects: usize,
    /// Allocations refused because the bump tail could not serve them.
    /// Non-zero means the cycle should have relocated a smaller set; it is the
    /// number `OutOfToSpace` is worth reporting against.
    refusals: usize,
}

impl ToSpaceWindow {
    /// Arena offset below which no to-space allocation may land.
    pub fn floor(&self) -> usize {
        self.floor
    }

    /// Bytes handed out since the window opened.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Objects handed out since the window opened.
    pub fn objects(&self) -> usize {
        self.objects
    }

    /// Allocations the bump tail refused. See [`Self::refusals`].
    pub fn refusals(&self) -> usize {
        self.refusals
    }
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
            "free-list ALIGNMENT tripwire [{site}]: unaligned block off={offset} (mod8={}) size={size} (mod8={}) — walk-grid hazard",
            offset & 7,
            size & 7,
        );
    }
}

/// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): the union of two
/// known-zero span lists (each `(offset, len)`, in any order, possibly
/// overlapping), in the form [`Arena::set_known_zero_free_spans`] takes:
/// ascending, disjoint, empty spans dropped, and spans that overlap or TOUCH
/// merged into one -- so a free block the coalescer built from two adjacent
/// claimed spans (a reclaimed span beside a block that was already free and
/// zero) lies wholly inside one span, which is what
/// [`Arena::hand_out_is_known_zero`] asks. Pure.
pub fn union_known_zero_spans(
    mut a: Vec<(usize, usize)>,
    b: Vec<(usize, usize)>,
) -> Vec<(usize, usize)> {
    a.extend(b);
    a.retain(|&(_, l)| l > 0);
    a.sort_unstable_by_key(|&(o, _)| o);
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(a.len());
    for (o, l) in a {
        let end = o.saturating_add(l);
        if let Some(last) = out.last_mut() {
            let last_end = last.0 + last.1;
            if o <= last_end {
                last.1 = last_end.max(end) - last.0;
                continue;
            }
        }
        out.push((o, end - o));
    }
    out
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
        Self::from_store(data, capacity)
    }

    /// t1 wave 8 lane CARDTBL (`CRATONVM_GC_WHOLE_HEAP_YOUNG_HEADROOM`, under
    /// the opt-in `CRATONVM_GC_WHOLE_HEAP_CARDS`): [`Self::new`] whose backing
    /// reserves `reserve` bytes of address space up front, so [`Self::grow`]
    /// up to `reserve` raises the usable length IN PLACE and never moves the
    /// base (`HeapStore::with_headroom`, the store `OldGen::with_growth_headroom`
    /// uses). A young semispace built this way stays inside the whole-heap
    /// card map's hull however far it grows.
    ///
    /// Returns `None` -- the caller builds the historical [`Self::new`] arena
    /// -- when the store could not get the whole reservation
    /// ([`Self::in_place_capacity`] below `reserve`): a pinned store without
    /// headroom would REFUSE a growth that the historical store makes by
    /// moving, so it must not be kept.
    pub fn with_headroom(capacity: usize, reserve: usize) -> Option<Self> {
        let capacity = capacity & !7;
        if reserve <= capacity {
            return None;
        }
        let data = crate::reservation::HeapStore::with_headroom(capacity, reserve, "arena");
        if data.in_place_capacity() < reserve || data.len() != capacity {
            return None;
        }
        Some(Self::from_store(data, capacity))
    }

    /// t1 wave 8 lane CARDTBL: the largest capacity [`Self::grow`] reaches
    /// without moving the base (`HeapStore::in_place_capacity`): the
    /// reservation on a reserving store, the current capacity on the owned
    /// fallback. At least [`Self::capacity`].
    pub fn in_place_capacity(&self) -> usize {
        self.data.in_place_capacity().max(self.data.len())
    }

    /// [`Self::new`]'s body over an already-built store of usable length
    /// `capacity` (a multiple of 8).
    fn from_store(data: crate::reservation::HeapStore, capacity: usize) -> Self {
        let mut a = Self {
            data,
            uncommit_policy: None,
            cursor: 0,
            free_small: (0..SMALL_BUCKETS).map(|_| Vec::new()).collect(),
            small_mask: [0u64; SMALL_MASK_WORDS],
            free_large: BTreeMap::new(),
            free_large_blocks: 0,
            max_free_upper: 0,
            min_free_span: 0,
            free_bytes_total: 0,
            alloc_anchors: Vec::new(),
            anchor_shift: 0,
            prefer_bump: false,
            free_list_after_bump: 0,
            free_pushed: 0,
            coalesce_threshold: COALESCE_THRESHOLD_MIN,
            high_cursor: capacity,
            pre_retract_high: 0,
            pre_retract_low: usize::MAX,
            free_high: Vec::new(),
            high_max: 0,
            high_pushed: 0,
            high_reserve: 0,
            starts: None,
            readable_give_back: false,
            bytes_reset: 0,
            pristine_lo: 0,
            pristine_hi: capacity,
            last_hand_out_pristine: (usize::MAX, 0, 0),
            skip_pristine_zeroing: false,
            defer_slide_tail_zeroing: false,
            // No window: the state every arena is in except inside a
            // page-evacuating ZGC pause. See `Arena::to_space`.
            to_space: None,
            zero_sets: Box::default(),
        };
        a.rearm_alloc_anchors();
        a
    }

    /// gce e1/y: install `spans` (LOW offsets, ascending, disjoint, below the
    /// cursor) as the set of bytes the caller has PROVED read zero right now,
    /// replacing any earlier set. See the `known_zero` field for the doors
    /// that keep the claim true. The generational sweep calls this once per
    /// cycle, after its publication and its coalesce, with the spans it has
    /// just zeroed.
    ///
    /// # Panics (debug only)
    /// Debug-asserts the ordering, disjointness and cursor bound.
    pub fn set_known_zero_free_spans(&mut self, spans: Vec<(usize, usize)>) {
        #[cfg(debug_assertions)]
        {
            let mut prev_end = 0usize;
            for &(off, len) in &spans {
                assert!(
                    off >= prev_end,
                    "known-zero spans must be ordered and disjoint"
                );
                assert!(
                    off + len <= self.cursor,
                    "known-zero span {off}+{len} runs past the low cursor {}",
                    self.cursor,
                );
                prev_end = off + len;
            }
        }
        self.zero_sets.known_zero = spans;
    }

    /// gce e1/y: does the hand-out `[offset, offset + len)` lie wholly inside
    /// one span of the known-zero set (see the `known_zero` field)? Asked
    /// under the arena lock, right after the `alloc` that returned it and
    /// before the caller writes it. `false` for an empty set, an empty
    /// hand-out, or one that straddles a span's end. O(log spans).
    pub fn hand_out_is_known_zero(&self, offset: usize, len: usize) -> bool {
        if len == 0 || self.zero_sets.known_zero.is_empty() {
            return false;
        }
        let Some(end) = offset.checked_add(len) else {
            return false;
        };
        // The last span that starts at or below `offset`.
        let i = self
            .zero_sets
            .known_zero
            .partition_point(|&(o, _)| o <= offset);
        i > 0 && {
            let (o, l) = self.zero_sets.known_zero[i - 1];
            end <= o + l
        }
    }

    /// JIT round 23 wave 4, lane L4: add LOW offset spans to the lazy-dirty set
    /// (`ZeroSets::lazy_dirty`): bytes a pinned in-place young cycle left
    /// unzeroed. Merged with what is there (touching spans join).
    ///
    /// The spans must hold NO live object (the caller's claim: an in-place
    /// cycle's dead spans): they also become "not an object" in the
    /// lock-free mirror ([`Self::lazy_dirty_mirror`], r23 w4 review R-L4z
    /// item 2), created here on first use. O(m log n) for m spans.
    pub fn note_lazy_dirty(&mut self, spans: &[(usize, usize)]) {
        if spans.iter().all(|&(_, l)| l == 0) {
            return;
        }
        if self.zero_sets.lazy_dirty.mirror.is_none() && self.data.len() != 0 {
            self.zero_sets.lazy_dirty.mirror = Some(std::sync::Arc::new(
                crate::heap_bitmap::HeapBitmap::labelled(
                    self.data.as_ptr() as usize,
                    self.data.len(),
                    "lazy-dirty",
                ),
            ));
            // r23 w8 (W8-L3): latched with the mirror it governs.
            self.zero_sets.lazy_dirty.plain_stores = true;
        }
        let base = self.data.as_ptr() as usize;
        let set = &mut self.zero_sets.lazy_dirty;
        let plain = set.plain_stores;
        // t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
        // batch insert coalesces and merges intervals linearly rather than
        // traversing and mutating the B-tree for each of hundreds of thousands of spans.
        set.insert_batch(spans);
        if let Some(m) = set.mirror.as_deref() {
            for &(o, l) in spans {
                if l == 0 {
                    continue;
                }
                if plain {
                    m.set_range_single_writer(base + o, base + o + l);
                } else {
                    m.set_range(base + o, base + o + l);
                }
            }
        }
    }

    /// JIT round 23 wave 4, lane L4: does the hand-out `[offset, offset + len)`
    /// overlap a lazy-dirty span? If so it is removed from the set: the caller
    /// promises to memset the whole hand-out (it must then skip every
    /// "already zero" shortcut). Under the arena lock, right after the `alloc`
    /// that returned the hand-out.
    ///
    /// r23 w4 review (R-L4z): returns the overlapped EXTENT clipped to the
    /// hand-out, `(lo, hi)` offsets (lowest dirty byte to highest dirty byte
    /// + 1), so the refill can still run its occupied-chunk tripwire on the
    /// parts outside it; `None` on a miss. The hand-out's mirror bits are
    /// cleared BEFORE the pointer leaves the lock (Release stores; the
    /// validator reads with Acquire), so the object about to be built there
    /// is never declined. O(log n) per overlapped span (B-tree), no memmove.
    pub fn take_lazy_dirty(&mut self, offset: usize, len: usize) -> Option<(usize, usize)> {
        self.take_lazy_dirty_counted(offset, len).map(|(hit, _)| hit)
    }

    /// t1 lane F (`gcj-j2yg2`, re-measure): [`Self::take_lazy_dirty`], plus
    /// the EXACT number of dirty bytes inside the hand-out (the sum of the
    /// overlapped spans clipped to `[offset, offset + len)`; at most `len`).
    /// `len - dirty` is the part of the hand-out the caller's full memset
    /// writes although the set did not name it -- zero already wherever the
    /// free list's other doors kept their promise. A census only: the caller
    /// still memsets the whole hand-out.
    pub fn take_lazy_dirty_counted(
        &mut self,
        offset: usize,
        len: usize,
    ) -> Option<((usize, usize), usize)> {
        if len == 0 || self.zero_sets.lazy_dirty.spans.is_empty() {
            return None;
        }
        let a = offset;
        let b = offset.saturating_add(len);
        let dirty: usize = self
            .zero_sets
            .lazy_dirty
            .overlapping(a, b)
            .into_iter()
            .map(|(s, e)| e.min(b).saturating_sub(s.max(a)))
            .sum();
        let hit = self.zero_sets.lazy_dirty.remove(a, b)?;
        self.lazy_mirror_clear(a, b);
        Some((hit, dirty))
    }

    /// r23 w4 review (R-L4z): take the LOW offset spans `spans` out of the
    /// lazy-dirty set and its mirror -- bytes that now hold LIVE objects (an
    /// in-place cycle's copies into destination spans the set still named).
    /// Removing is safe for zeroing (a live object is never handed out, and
    /// a reset zeroes everything below the cursor) and required for the
    /// mirror, which must never decline a live object.
    ///
    /// t1 lane F (`t1-F-proposal-young-reset-phase-share-on-windows`):
    /// batch subtraction avoids scanning hundreds of thousands of surviving
    /// objects one by one with individual B-tree lookups.
    pub fn forget_lazy_dirty_over(&mut self, spans: &[(usize, usize)]) {
        if self.zero_sets.lazy_dirty.spans.is_empty() || spans.is_empty() {
            return;
        }
        let removed = self.zero_sets.lazy_dirty.remove_batch(spans);
        if !removed.is_empty() && self.zero_sets.lazy_dirty.mirror.is_some() {
            for (lo, hi) in removed {
                self.lazy_mirror_clear(lo, hi);
            }
        }
    }

    /// One run of [`Self::forget_lazy_dirty_over`]: `[lo, hi)`, LOW offsets.
    #[allow(dead_code)]
    fn forget_lazy_dirty_run(&mut self, lo: usize, hi: usize) {
        if self.zero_sets.lazy_dirty.remove(lo, hi).is_some() {
            self.lazy_mirror_clear(lo, hi);
        }
    }

    /// r23 w4 review (R-L4z): clear the mirror over the LOW offset spans
    /// `spans` WITHOUT touching the set -- an in-place cycle's destination
    /// spans at arm time, so a copy it writes there is never declined while
    /// the pause runs. [`Self::restore_lazy_mirror_over`] puts back what is
    /// still dirty when the cycle finishes. Words already zero are not
    /// written (a load each).
    pub fn suspend_lazy_mirror_over(&self, spans: &[(usize, usize)]) {
        if self.zero_sets.lazy_dirty.mirror.is_none() {
            return;
        }
        for &(o, l) in spans {
            self.lazy_mirror_clear(o, o.saturating_add(l));
        }
    }

    /// r23 w4 review (R-L4z): re-set the mirror for every set span inside the
    /// LOW offset spans `spans` (the destinations [`Self::suspend_lazy_mirror_over`]
    /// cleared), after [`Self::forget_lazy_dirty_over`] removed the copies.
    pub fn restore_lazy_mirror_over(&self, spans: &[(usize, usize)]) {
        let set = &self.zero_sets.lazy_dirty;
        let Some(m) = set.mirror.as_deref() else {
            return;
        };
        let base = self.data.as_ptr() as usize;
        for &(o, l) in spans {
            let hi = o.saturating_add(l);
            for (s, e) in set.overlapping(o, hi) {
                if set.plain_stores {
                    m.set_range_single_writer(base + s.max(o), base + e.min(hi));
                } else {
                    m.set_range(base + s.max(o), base + e.min(hi));
                }
            }
        }
    }

    /// r23 w4 review (R-L4z): is `addr` (absolute) inside a lazy-dirty span,
    /// i.e. certainly NOT an object? Exact on the 8-byte grid; `false` when
    /// no mirror exists. For readers holding the arena; the lock-free
    /// validator reads the published [`Self::lazy_dirty_mirror`] instead.
    #[inline]
    pub fn lazy_dirty_contains(&self, addr: usize) -> bool {
        self.zero_sets
            .lazy_dirty
            .mirror
            .as_deref()
            .is_some_and(|m| m.contains(addr))
    }

    /// r23 w4 review (R-L4z): the lazy-dirty mirror (one bit per 8 bytes, set
    /// = inside a lazy-dirty span, which holds no object), for the heap to
    /// publish beside the object-start bitmap. `None` until the first
    /// in-place lazy cycle notes a span (no cost for an arena that never
    /// has one).
    pub fn lazy_dirty_mirror(&self) -> Option<std::sync::Arc<crate::heap_bitmap::HeapBitmap>> {
        self.zero_sets.lazy_dirty.mirror.clone()
    }

    /// Clear the mirror's bits for the LOW offsets `[lo, hi)`, exactly (the
    /// bits whose 8-byte slot starts in the range), skipping zero words.
    fn lazy_mirror_clear(&self, lo: usize, hi: usize) {
        let Some(m) = self.zero_sets.lazy_dirty.mirror.as_deref() else {
            return;
        };
        let hi = hi.min(m.span);
        if hi <= lo {
            return;
        }
        let first = lo.div_ceil(8);
        let last = hi.div_ceil(8);
        let mut bit = first;
        while bit < last {
            let w = bit >> 6;
            let b = bit & 63;
            let n = (64 - b).min(last - bit);
            let mask = if n == 64 { u64::MAX } else { ((1u64 << n) - 1) << b };
            let v = m.word_at(w);
            if v & mask != 0 {
                if self.zero_sets.lazy_dirty.plain_stores {
                    // r23 w8 (W8-L3): one writer at a time (`plain_stores`).
                    m.store_word_single_writer(w, v & !mask);
                } else {
                    m.clear_word_bits(w, mask);
                }
            }
            bit += n;
        }
    }

    /// JIT round 23 wave 4, lane L4: the end (LOW offset) of the last
    /// lazy-dirty span, `0` when the set is empty. A reset must zero at least
    /// this far, since the bytes above the cursor are no longer guaranteed
    /// zero below it. O(log n).
    pub fn lazy_dirty_end(&self) -> usize {
        self.zero_sets.lazy_dirty.end()
    }

    /// r23 w4 review (R-L4z): drop the whole lazy-dirty set and clear its
    /// mirror (every reset; the caller zeroes up to [`Self::lazy_dirty_end`]
    /// first or hands that extent to the wipe). Stop-the-world.
    fn clear_lazy_dirty(&mut self) {
        let end = self.lazy_dirty_end();
        if end != 0 {
            self.lazy_mirror_clear(0, end);
        }
        self.zero_sets.lazy_dirty.spans.clear();
    }

    /// Bytes the known-zero set currently claims (the census).
    pub fn known_zero_bytes(&self) -> usize {
        self.zero_sets.known_zero.iter().map(|&(_, l)| l).sum()
    }

    /// gce e1/y: drop the known-zero set. Called by every door that lists
    /// bytes which may have been handed out since the set was installed.
    #[inline]
    fn forget_known_zero(&mut self) {
        self.zero_sets.known_zero.clear();
    }

    /// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): the bytes of the
    /// known-zero set that are still on the free list `free` (`(offset, len)`
    /// ascending by offset, as [`Self::free_blocks_sorted`] returns it) and
    /// below the low cursor -- ascending, disjoint, touching spans merged.
    /// Pure: the set is not changed.
    ///
    /// The generational sweep asks this at its start, before its own
    /// publication forgets the set. Bytes the set claims that are still on the
    /// free list were never handed out since the set was installed (a hand-out
    /// takes them off the list, and every door that lists handed-out bytes
    /// again either forgets the whole set or proves them zero), so they still
    /// read zero and the sweep may claim them again beside the spans it zeroes
    /// itself ([`union_known_zero_spans`]). A free list with overlapping
    /// blocks (the A2 overlap the coalescer repairs) can only make this answer
    /// smaller, never claim a byte outside the set.
    pub fn known_zero_within(&self, free: &[(usize, usize)]) -> Vec<(usize, usize)> {
        let set = &self.zero_sets.known_zero;
        if set.is_empty() || free.is_empty() {
            return Vec::new();
        }
        let cursor = self.cursor;
        let mut out = Vec::new();
        let (mut i, mut j) = (0usize, 0usize);
        while i < set.len() && j < free.len() {
            let (a0, a1) = (set[i].0, set[i].0.saturating_add(set[i].1));
            let (b0, b1) = (free[j].0, free[j].0.saturating_add(free[j].1));
            let lo = a0.max(b0);
            let hi = a1.min(b1).min(cursor);
            if lo < hi {
                out.push((lo, hi - lo));
            }
            if a1 <= b1 {
                i += 1;
            } else {
                j += 1;
            }
        }
        union_known_zero_spans(out, Vec::new())
    }

    /// gcj j2/yg2: keep only the known-zero bytes below `end`, the new low
    /// cursor of a tail retraction
    /// ([`Self::return_unused_tail_keeping_known_zero`]). A retraction lists
    /// nothing: the bytes it gives back go above the cursor, where the bump
    /// arm (not this set) answers for them, so the rest of the claim stays
    /// true; only the part at or above the new cursor is dropped, because the
    /// set lives below the cursor.
    fn clip_known_zero_below(&mut self, end: usize) {
        let set = &mut self.zero_sets.known_zero;
        set.retain(|&(o, _)| o < end);
        if let Some(last) = set.last_mut() {
            if last.0.saturating_add(last.1) > end {
                last.1 = end - last.0;
            }
        }
    }

    /// gcj j2/yg2: add `[offset, offset + len)` -- bytes the caller has
    /// PROVED read zero -- to the known-zero set, merged with every span it
    /// overlaps or touches. Two binary searches and one `Vec` splice.
    fn note_known_zero_span(&mut self, offset: usize, len: usize) {
        let Some(end) = offset.checked_add(len) else {
            return;
        };
        if len == 0 {
            return;
        }
        let set = &mut self.zero_sets.known_zero;
        // `set` is ascending and disjoint, so the span ends ascend too: `lo`
        // is the first span ending at or after `offset`, `hi` the first
        // starting after `end`, and every span in `lo..hi` overlaps or touches
        // the new one (`lo <= hi`: a span ending before `offset` starts before
        // `end`).
        let lo = set.partition_point(|&(o, l)| o + l < offset);
        let hi = set.partition_point(|&(o, _)| o <= end);
        let (mut new_lo, mut new_hi) = (offset, end);
        if lo < hi {
            new_lo = new_lo.min(set[lo].0);
            let (o, l) = set[hi - 1];
            new_hi = new_hi.max(o + l);
        }
        set.splice(lo..hi, std::iter::once((new_lo, new_hi - new_lo)));
    }

    /// Turn the `readable_give_back` mode on or off. See that field for who may
    /// and why. Only the arena's owner calls this, under the arena lock.
    pub fn set_readable_give_back(&mut self, on: bool) {
        self.readable_give_back = on;
    }

    /// Is the `readable_give_back` mode on?
    pub fn is_readable_give_back(&self) -> bool {
        self.readable_give_back
    }

    /// Opt this arena's TLAB-chunk carver into skipping the zeroing of bytes
    /// that have never been written (see [`Self::pristine_lo`]). Only the
    /// arena's owner calls this, under the arena lock.
    pub fn set_skip_pristine_zeroing(&mut self, on: bool) {
        self.skip_pristine_zeroing = on;
    }

    /// zt t31/zc11: see the `defer_slide_tail_zeroing` field.
    pub fn set_defer_slide_tail_zeroing(&mut self, on: bool) {
        self.defer_slide_tail_zeroing = on;
    }

    /// Record that `[offset, offset + len)` may now be written, shrinking the
    /// pristine window, and return the part of the range that WAS pristine as
    /// `(lo, hi)` offsets (`lo == hi` when none was).
    #[inline]
    fn note_writable(&mut self, offset: usize, len: usize) -> (usize, usize) {
        let a = offset;
        let b = offset.saturating_add(len);
        // ZA1: the slide's zeroed set loses the range too.
        self.zero_sets.slide.take_range(a, b);
        let lo = a.max(self.pristine_lo);
        let hi = b.min(self.pristine_hi);
        if lo >= hi {
            return (0, 0);
        }
        // `[a, b)` overlaps the window, so `b > pristine_lo` and
        // `a < pristine_hi`. Keep whichever remainder is larger.
        let below = a.saturating_sub(self.pristine_lo);
        let above = self.pristine_hi.saturating_sub(b);
        if above >= below {
            self.pristine_lo = b.min(self.pristine_hi);
        } else {
            self.pristine_hi = a;
        }
        (lo, hi)
    }

    /// Zero `[lo, hi)` (LOW offsets), skipping what the arena proves already
    /// reads zero: the pristine window and the slide's zeroed set (whose
    /// recorded forwarding stamps are zeroed). Without an opted-in owner both
    /// are empty and this is `fill_zero`. See `compact_low_to`.
    fn fill_zero_unless_known(&mut self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        let mut known: Vec<(usize, usize)> = Vec::new();
        if self.skip_pristine_zeroing {
            let (plo, phi) = (lo.max(self.pristine_lo), hi.min(self.pristine_hi));
            if plo < phi {
                known.push((plo, phi));
            }
            let w = &self.zero_sets.slide;
            let first = w.spans.partition_point(|&(_, shi)| shi <= lo);
            for &(slo, shi) in &w.spans[first..] {
                if slo >= hi {
                    break;
                }
                known.push((slo.max(lo), shi.min(hi)));
            }
        }
        if known.is_empty() {
            self.data.fill_zero(lo, hi);
            return;
        }
        known.sort_unstable();
        let mut cursor = lo;
        for &(klo, khi) in &known {
            if klo > cursor {
                self.data.fill_zero(cursor, klo);
            }
            cursor = cursor.max(khi);
        }
        if cursor < hi {
            self.data.fill_zero(cursor, hi);
        }
        // The forwarding records stamped into the slide's set do not read zero.
        let stamp_len = cratonvm_types::MIN_OBJECT_SIZE;
        let w = &mut self.zero_sets.slide;
        if !w.stamps_sorted {
            w.stamps.sort_unstable();
            w.stamps.dedup();
            w.stamps_sorted = true;
        }
        let first = w
            .stamps
            .partition_point(|&s| s.saturating_add(stamp_len) <= lo);
        let dirty: Vec<usize> = w.stamps[first..]
            .iter()
            .copied()
            .take_while(|&s| s < hi)
            .collect();
        for s in dirty {
            self.data.fill_zero(s.max(lo), (s + stamp_len).min(hi));
        }
    }

    /// ZA1: replace the slide's zeroed set with `spans` (LOW offsets, each
    /// already zero-filled by the caller). Called by [`Self::compact_low_to`];
    /// empties the set unless the owner opted into skipping pristine zeroing.
    /// See [`SlideZeroWindow`].
    fn reset_slide_zero_spans(&mut self, mut spans: Vec<(usize, usize)>) {
        let w = &mut self.zero_sets.slide;
        w.stamps.clear();
        w.stamps_sorted = true;
        w.last.clear();
        w.last_at = usize::MAX;
        spans.retain(|&(lo, hi)| lo < hi);
        spans.sort_unstable();
        w.spans.clear();
        if !self.skip_pristine_zeroing {
            return;
        }
        for (lo, hi) in spans {
            match w.spans.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => w.spans.push((lo, hi)),
            }
        }
    }

    /// ZA1: a forwarding record (`cratonvm_types::MIN_OBJECT_SIZE` bytes) was
    /// written at LOW offset `off`. Inside the slide's zeroed set those bytes
    /// no longer read zero; whoever hands them out zeroes them. Called under
    /// the arena lock by `ZgcRealHeap::stamp_forwarding_words`.
    pub fn note_slide_stamp(&mut self, off: usize) {
        let w = &mut self.zero_sets.slide;
        if w.spans.is_empty() {
            return;
        }
        // O(1): a slide stamps one record per survivor it moved out of the
        // tail, in no promised order, and an ordered insert per stamp was
        // quadratic inside the pause.
        if w.stamps.last().is_some_and(|&last| last >= off) {
            w.stamps_sorted = false;
        }
        w.stamps.push(off);
    }

    /// ZA1: `(spans, bytes, stamps)` of the slide's zeroed set. Tests and the
    /// census.
    /// Record that the free-listed LOW offset span `[offset, offset + len)`
    /// already reads zero (such as an untouched VM-TLAB tail returned before
    /// any filler header was written). Active only when the owner opted into
    /// [`Self::set_skip_pristine_zeroing`].
    pub fn note_clean_free_span(&mut self, offset: usize, len: usize) {
        if !self.skip_pristine_zeroing || len == 0 {
            return;
        }
        let Some(end) = offset.checked_add(len) else {
            return;
        };
        self.zero_sets.slide.insert_range(offset, end);
    }

    pub fn slide_zero_census(&self) -> (usize, usize, usize) {
        let w = &self.zero_sets.slide;
        (
            w.spans.len(),
            w.spans.iter().map(|&(lo, hi)| hi - lo).sum(),
            w.stamps.len(),
        )
    }

    /// Everything an allocator needs to skip zeroing the parts of the MOST
    /// RECENT hand-out (`len` bytes at `ptr`) that already read zero: the
    /// pristine window's part (see [`Self::take_last_hand_out_pristine`]) and
    /// the slide's zeroed set's part (ZA1, [`SlideZeroWindow`]), with the
    /// forwarding records inside the latter that the allocator must still
    /// zero. `None` when the owner did not opt in, or neither covers any of
    /// it. One-shot, like the pristine answer.
    pub fn take_last_hand_out_zero_plan(
        &mut self,
        ptr: *const u8,
        len: usize,
    ) -> Option<ZeroPlan> {
        let base = self.data.as_ptr() as usize;
        let at = (ptr as usize).checked_sub(base);
        let pristine = self.take_last_hand_out_pristine(ptr);
        let opted = self.skip_pristine_zeroing;
        let w = &mut self.zero_sets.slide;
        let pieces = std::mem::take(&mut w.last);
        let slide_at = std::mem::replace(&mut w.last_at, usize::MAX);
        let slide_ok = opted && !pieces.is_empty() && at == Some(slide_at);
        if pristine.is_none() && !slide_ok {
            w.last = pieces;
            w.last.clear();
            return None;
        }
        let start = ptr as usize;
        let end = start.saturating_add(len);
        let clamp = |(lo, hi): (usize, usize)| {
            let lo = lo.clamp(start, end);
            (lo, hi.clamp(lo, end))
        };
        let mut plan = ZeroPlan::default();
        let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(pieces.len() + 1);
        if let Some(p) = pristine.map(clamp) {
            plan.pristine_bytes = p.1 - p.0;
            ranges.push(p);
        }
        if slide_ok && !w.stamps_sorted {
            w.stamps.sort_unstable();
            w.stamps.dedup();
            w.stamps_sorted = true;
        }
        if slide_ok {
            for &(lo, hi) in &pieces {
                let sl = clamp((base + lo, base + hi));
                if sl.0 >= sl.1 {
                    continue;
                }
                plan.slide_bytes += sl.1 - sl.0;
                ranges.push(sl);
                // The recorded forwarding records inside it.
                let (olo, ohi) = (sl.0 - base, sl.1 - base);
                let first = w
                    .stamps
                    .partition_point(|&s| s.saturating_add(cratonvm_types::MIN_OBJECT_SIZE) <= olo);
                for &s in &w.stamps[first..] {
                    if s >= ohi {
                        break;
                    }
                    plan.dirty_stamps.push(base + s);
                }
            }
        }
        // Reuse the allocation next time.
        w.last = pieces;
        w.last.clear();
        ranges.retain(|&(lo, hi)| lo < hi);
        ranges.sort_unstable();
        for (lo, hi) in ranges {
            match plan.known_zero.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => plan.known_zero.push((lo, hi)),
            }
        }
        (!plan.known_zero.is_empty()).then_some(plan)
    }

    /// The part of the MOST RECENT hand-out that had never been written, as
    /// ABSOLUTE addresses `(lo, hi)`, if that hand-out started at `ptr` and the
    /// owner opted in with [`Self::set_skip_pristine_zeroing`]; `None`
    /// otherwise. One-shot: the record is cleared, so a second call (or a call
    /// after an allocation that did not go through `hand_out`) answers `None`.
    ///
    /// The caller may skip zeroing `[lo, hi)` -- it reads as zero -- and must
    /// zero the rest of its allocation itself.
    pub fn take_last_hand_out_pristine(&mut self, ptr: *const u8) -> Option<(usize, usize)> {
        let (at, lo, hi) = self.last_hand_out_pristine;
        self.last_hand_out_pristine = (usize::MAX, 0, 0);
        if !self.skip_pristine_zeroing || lo >= hi {
            return None;
        }
        let base = self.data.as_ptr() as usize;
        if (ptr as usize).checked_sub(base) != Some(at) {
            return None;
        }
        Some((base + lo, base + hi))
    }

    /// Bytes handed back to the OS by a READABLE give-back (still mapped and
    /// committed), over this arena's life. Zero unless
    /// [`Self::set_readable_give_back`] turned it on.
    pub fn bytes_reset(&self) -> usize {
        self.bytes_reset
    }

    /// Build the exact object-start bitmap, if `CRATONVM_GC_OBJECT_STARTS` is
    /// on and this arena has any capacity to cover.
    ///
    /// # Why the OWNER arms this and not [`Arena::new`]
    ///
    /// A bit per 8 bytes is capacity/64 of side table, and only a heap that
    /// CONSULTS it gets anything for that. `Arena` is the backing store for the
    /// generational young semi-spaces, for the evacuation buffers, and for the
    /// whole of `ZgcRealHeap` -- and ZGC answers `is_object_address` from its
    /// own registry and would never read a bit of this. Arming in the
    /// constructor charged the DEFAULT collector 64 MB on a 4 GB heap for a
    /// table nothing reads. So the constructor no longer arms, and
    /// `GenerationalHeap` arms the two arenas whose predicate consults it.
    ///
    /// Idempotent, and it REBUILDS: see the call in [`Arena::grow`].
    pub fn arm_object_starts(&mut self) {
        if !object_starts_enabled() || self.data.len() == 0 {
            self.starts = None;
            return;
        }
        self.starts = Some(std::sync::Arc::new(
            crate::heap_bitmap::HeapBitmap::labelled(
                self.data.as_ptr() as usize,
                self.data.len(),
                "arena-object-start",
            ),
        ));
    }

    /// Is `addr` an allocation base this arena handed out and has not freed?
    ///
    /// `None` means "not tracked" -- the switch is off, the arena was never
    /// armed ([`Arena::arm_object_starts`]: only `GenerationalHeap` arms the
    /// arenas whose predicate consults it), or it has no capacity -- and is NOT
    /// the same answer as `Some(false)`. A grown arena that was armed still
    /// answers `Some(_)`: [`Arena::grow`] rebuilds the bitmap over the new
    /// geometry (gci i45/bh R3, corrected gci i46/ts). A caller must fall back
    /// to the deduction on `None`, and may only ever use `Some(true)` to ACCEPT.
    /// See the [`Arena::starts`] field for why the other direction is unsound.
    #[inline]
    pub fn is_object_start(&self, addr: usize) -> Option<bool> {
        self.starts.as_ref().map(|b| b.contains(addr))
    }

    /// This arena's object-start bitmap, for a reader that wants to consult it
    /// WITHOUT taking the arena lock.
    ///
    /// `is_object_address` is the VM's hottest validator and was made lock-free
    /// on purpose -- its own comment records that the triple-mutex containment
    /// check it replaced "contended catastrophically" with the allocator. Asking
    /// this arena directly would put that back, `try_lock` included: a failed
    /// try is still an atomic read-modify-write on a line every allocating
    /// thread wants. So the heap publishes this `Arc` into a lock-free slot
    /// beside `commit_bits` and reads through the raw pointer, exactly as it
    /// already does for the commit screen.
    pub fn object_starts(&self) -> Option<std::sync::Arc<crate::heap_bitmap::HeapBitmap>> {
        self.starts.clone()
    }

    /// Whether this arena is tracking object starts at all.
    #[inline]
    pub fn tracks_object_starts(&self) -> bool {
        self.starts.is_some()
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
        // Stored as `offset + 1`, 0 = empty, so the table is an `alloc_zeroed`
        // block whose pages stay untouched until an anchor lands in them.
        self.alloc_anchors = vec![0usize; (cap >> shift) + 1];
    }

    /// Record `offset` as a VERIFIED object start (the allocator just handed
    /// this exact offset out as the base of an object or of a TLAB, whose
    /// first byte is an object base too — a TLAB is tail-filled at retire, so
    /// it is object-covered end to end).
    ///
    /// One store per bucket per epoch; every later offset in the same bucket
    /// is a load + compare + not-taken branch.
    ///
    /// # Off-grid offsets are DROPPED (gengc-round1-alloc, 2026-09-20)
    ///
    /// Every object in this arena sits on the 8-byte grid: [`Self::alloc`]
    /// opens with `size.checked_add(7)? & !7`, [`Self::new`] rounds the
    /// capacity down to 8, and the sweep only ever publishes 8-aligned spans
    /// (`warn_unaligned_block` fires at any producer that breaks it). So an
    /// address that is not 8-aligned is PROVABLY not an object base.
    ///
    /// Recording one would not be unsound -- every consumer re-proves an
    /// anchor by requiring its chain to land exactly on the next one -- but it
    /// is guaranteed to FAIL that proof, and a single failed chunk abandons
    /// the whole parallel sweep/object-start walk for the cycle and re-runs it
    /// sequentially from scratch (`gen_heap`'s "on any grid anomaly it is
    /// abandoned wholesale"). Dropping it costs at most one longer chunk,
    /// which is the same cost as the anchor never having been offered.
    ///
    /// The test is on the ABSOLUTE address, not on `offset` alone. The grid
    /// the walkers stride is the one in memory, and `Arena`'s backing is a
    /// `HeapStore` whose base no type in this tree asserts to be 8-aligned
    /// (the same gap `heap_bitmap`'s overflow set exists for). On a
    /// hypothetical 4-aligned base every offset would be odd on the 8-byte
    /// grid while every absolute address stayed aligned and every chain still
    /// landed -- so screening `offset` would throw away a grid that works.
    /// An offset outside the arena is IGNORED, not asserted on: a caller
    /// handing over a stale offset (one recorded before a `grow`, say) must
    /// never take the VM down, and that has always been this function's
    /// behaviour through `get_mut`. The grid screen below therefore runs only
    /// for offsets this arena could actually have handed out.
    #[inline]
    pub fn note_object_start(&mut self, offset: usize) {
        if offset >= self.data.len() {
            return;
        }
        let addr = self.data.as_ptr() as usize + offset;
        debug_assert_eq!(
            addr & 7,
            0,
            "Arena::note_object_start: {addr:#x} (offset {offset:#x}) is off the \
             8-byte object grid, so it cannot be an object base -- an anchor there \
             is a chain that cannot land, and one unprovable anchor discards the \
             whole parallel walk for this cycle",
        );
        if addr & 7 != 0 {
            return;
        }
        if let Some(slot) = self.alloc_anchors.get_mut(offset >> self.anchor_shift) {
            if *slot == 0 {
                *slot = offset + 1;
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
            let v = std::mem::replace(slot, 0);
            if v != 0 {
                out.push(v - 1);
            }
        }
        out
    }

    /// Forget every recorded anchor without collecting them.
    pub fn clear_alloc_anchors(&mut self) {
        self.alloc_anchors.fill(0);
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
    /// Same contents, same order.
    pub fn alloc_anchors_snapshot(&self) -> Vec<usize> {
        self.alloc_anchors
            .iter()
            .filter(|&&v| v != 0)
            .map(|&v| v - 1)
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
        // zs s10/sp: the block a free-list fit takes must be at least
        // `fit_size` (`min_free_span`); `alloc_size` when the rule is off.
        let fit_size = alloc_size.max(self.min_free_span);
        if !self.prefer_bump && !self.free_is_empty() && fit_size <= self.max_free_upper {
            let base = self.data.as_ptr() as usize;
            // Tier selection: a request whose worst-case need (size + max
            // alignment padding) reaches LARGE_BLOCK_MIN can never be served
            // by a small block — skip the small tier entirely. Smaller
            // requests try the small tier first: its blocks are object-sized
            // holes, so a same-shaped request lands on its own size class.
            let worst_need = fit_size.saturating_add(align - 1);
            let mut hit = if worst_need < LARGE_BLOCK_MIN {
                self.small_fit(base, fit_size, align)
            } else {
                None
            };
            if hit.is_none() {
                hit = self.large_fit(base, fit_size, align);
            }
            if let Some((alloc_offset, remainders)) = hit {
                self.route_fit_remainders(alloc_offset, alloc_size, fit_size, remainders);
                self.note_region_leak("free-list", alloc_offset, alloc_size);
                return self.hand_out_fit(alloc_offset, alloc_size, false);
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
            let bump_limit = self
                .high_cursor
                .saturating_sub(self.remaining_high_reserve());
            // zs s10/sp: while `min_free_span` is set, a bump TAIL smaller
            // than it is a hole too (HotSpot's TLAB carve needs a whole
            // TLAB): a small request takes the tail only when at least
            // `fit_size` bytes of it remain.
            if end <= bump_limit && aligned.saturating_add(fit_size) <= bump_limit {
                // COMMIT BEFORE HANDING OUT. See `commit_bump`: the two
                // cursors are the only places the arena's writable envelope
                // grows, so committing here is what makes every FREE-LIST
                // block below the cursor writable without a check of its own.
                let from = self.cursor;
                let ptr = self.hand_out(aligned, end - aligned)?;
                self.cursor = end;
                // AFTER the cursor moves, not before. `hand_out` can fail, and
                // publishing `[from, aligned)` as free while `cursor` still
                // pointed at it would let the next bump hand out the same
                // bytes a second time.
                self.adopt_bump_alignment_padding(from, aligned);
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
        let mut hit = self.small_fit(base, fit_size, align);
        if hit.is_none() {
            hit = self.large_fit(base, fit_size, align);
        }
        if let Some((alloc_offset, remainders)) = hit {
            self.route_fit_remainders(alloc_offset, alloc_size, fit_size, remainders);
            if self.prefer_bump {
                // The nursery could not serve this and a hole below the floor
                // did. Counted, because that object is now in the old region and
                // no young cycle will reclaim it — see `prefer_bump`.
                self.free_list_after_bump += 1;
            }
            self.note_region_leak("free-list-retry", alloc_offset, alloc_size);
            return self.hand_out_fit(alloc_offset, alloc_size, false);
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
        // zt t1/sp: `fit_size` (the block the fit must take; `alloc_size`
        // unless the opt-in final-refusal threshold is set): with fewer free
        // bytes than that in all, no merge can produce such a block.
        if self.free_bytes_total >= fit_size
            && self.free_pushed >= self.coalesce_threshold
            && self.coalesce_free_list() != 0
        {
            let base = self.data.as_ptr() as usize;
            let mut hit = self.small_fit(base, fit_size, align);
            if hit.is_none() {
                hit = self.large_fit(base, fit_size, align);
            }
            if let Some((alloc_offset, remainders)) = hit {
                self.route_fit_remainders(alloc_offset, alloc_size, fit_size, remainders);
                self.note_region_leak("free-list-coalesced", alloc_offset, alloc_size);
                return self.hand_out_fit(alloc_offset, alloc_size, false);
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
                return self.hand_out_fit(off, alloc_size, true);
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
                if end <= self.high_cursor && aligned.saturating_add(fit_size) <= self.high_cursor {
                    // Same disposition, same ordering, as the main bump arm —
                    // this arm repeats its three lines and therefore its
                    // hazard.
                    let from = self.cursor;
                    let ptr = self.hand_out(aligned, end - aligned)?;
                    self.cursor = end;
                    self.adopt_bump_alignment_padding(from, aligned);
                    return Some(ptr);
                }
            }
        }
        None
    }

    /// Give `[from, aligned)` — the bytes a bump-path alignment round-up
    /// steps over — to the free list, so nothing is orphaned between two
    /// objects.
    ///
    /// `from` is the cursor as it stood BEFORE the round-up, passed in rather
    /// than read off `self`, because this must run AFTER `self.cursor = end`:
    /// `hand_out` can fail, and publishing the span as free while the cursor
    /// still pointed at it would let the next bump hand out the same bytes a
    /// second time.
    ///
    /// # What this closes
    ///
    /// The two arms of [`Self::alloc`] disagreed about who owns those bytes.
    /// The FREE-LIST arm accounts for them: [`Self::split`] returns a `head`
    /// block for the padding and `alloc` routes it back. The BUMP arm stepped
    /// over them — not handed out, not free-listed, no header written, no
    /// `note_object_start` — so they were inside `[0, cursor)` (and therefore
    /// counted by `used()`), absent from `free_list_bytes()`, and could never
    /// be handed back.
    ///
    /// The accounting leak is the small half. The large half is that a linear
    /// heap walk resumes striding from wherever the previous object ended,
    /// which is exactly `self.cursor`: padding the allocator skipped is bytes
    /// the walk reads as an `ObjectHeader`, and a zeroed sub-`HEADER_SIZE`
    /// region is byte-identical to a live `new Object()` (`class_id = 0`,
    /// `num_slots = 0`). That is the Bug-D walk desync `GAP_FILLER_CLASS_ID`
    /// exists to prevent in the TLAB tail, in the same shape, one function
    /// away. Free-listing the span removes it by a route the walkers already
    /// honour: every one of them resyncs past a listed free block
    /// (`skip_free_blocks`), so the padding is never parsed as a header at all.
    ///
    /// # Why the block is always on the grid
    ///
    /// `push_block_routed` warns on any block whose offset or size is not
    /// 8-aligned, and a sub-8-byte block on the low tier would be exactly
    /// that. It cannot arise here. `Arena::new` rounds capacity down with
    /// `capacity & !7` and starts the cursor at 0; `alloc` rounds every request
    /// up with `size.checked_add(7)? & !7`, so `alloc_size` is a multiple of
    /// the (power-of-two) alignment AND of 8, and `end = aligned + alloc_size`
    /// keeps the cursor 8-aligned forever. With an 8-aligned cursor and a
    /// power-of-two `align`, `aligned - cursor` is 0 for `align <= 8` and a
    /// multiple of 8 otherwise. So the padding is either empty or a legal
    /// low-tier block, and never a 1..7-byte splinter.
    ///
    /// # Latent today, deliberately still armed
    ///
    /// Every caller in the crate passes `align == 8`
    /// (`rg '\.alloc\([^,]*, *[0-9]+\)' gc/src/` is `, 8)` throughout), so
    /// `aligned == from` and this is a compare on a branch that is never
    /// taken. `Arena::alloc` is `pub` and takes a caller-supplied alignment,
    /// which is the whole distance between "unreachable" and "reachable". The
    /// tripwire fires the day a caller appears, which is what this file already
    /// does for every other grid violation.
    /// See `docs/feature-designs/generative-gc-architecture.md`.
    #[inline]
    fn adopt_bump_alignment_padding(&mut self, from: usize, aligned: usize) {
        if aligned <= from {
            return;
        }
        let padding = aligned - from;
        // NAME THE PRODUCER. Not an assertion -- the disposition below is
        // correct for any value and an arena is not a place to abort a VM over
        // an accounting question -- and not `warn_unaligned_block` either,
        // whose message ("unaligned block ... walk-grid hazard", printing
        // `mod8`) would be actively misleading here: this padding IS on the
        // grid (see the alignment argument above), and what is worth reporting
        // is that a caller with `align > 8` exists at all. One-shot, the same
        // shape `Tlab::install_tail_filler`'s tripwire uses, because a warning
        // per allocation would itself be the problem.
        {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    site = "bump-padding",
                    from = from,
                    aligned,
                    padding,
                    "Arena::alloc bump path skipped alignment padding: a caller is \
                     asking for an alignment wider than the arena's 8-byte grid. \
                     The span is free-listed rather than orphaned, so walks and \
                     accounting are correct -- but every caller in this crate \
                     passes align = 8, so this is a NEW producer and the \
                     assumptions in `heap_bitmap`'s exactness argument and in \
                     `note_object_start`'s grid screen are worth re-reading \
                     against it",
                );
            }
        }
        self.push_block_routed(FreeBlock {
            offset: from,
            size: padding,
        });
        // `push_block_routed` deliberately does not touch `max_free_upper` --
        // its usual callers are split remainders, which are strictly smaller
        // than the block they came from. This one is NEW free space, so the
        // cached upper bound has to admit it or the next request of this size
        // would skip the free list on a stale proof of no fit.
        self.max_free_upper = self.max_free_upper.max(padding);
    }

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
        if !self.commit_range_for_use(offset, size) {
            return None;
        }
        // The pristine window loses this range, and the carver may ask which
        // part of it had never been written (see `pristine_lo`).
        let (plo, phi) = self.note_writable(offset, size);
        self.last_hand_out_pristine = (offset, plo, phi);
        // The single door every allocation in this arena goes through, which is
        // what makes the object-start bitmap complete for anything allocated
        // directly (see the `starts` field). A TLAB chunk is ONE hand-out and
        // the objects bump-allocated inside it never reach here -- knowably
        // incomplete, which is precisely why the bitmap is only ever consulted
        // to ACCEPT.
        if let Some(bits) = self.starts.as_ref() {
            bits.insert(self.data.as_ptr() as usize + offset);
        }
        // JIT round 23 wave 4c, lane L4: the allocator-anchor CERTIFICATE the
        // lazy young object-start grid relies on
        // (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`, `young_mark::LazyStartGrid`).
        // These bytes get a new layout now, so an anchor noted inside them for
        // an earlier one (freed since, maybe coalesced, now re-carved) names
        // nothing any more: forget it. The base itself stays a boundary.
        self.forget_alloc_anchors_inside(offset, size);
        // SAFETY: `commit_range` returned `true`, which it only does for a
        // range wholly inside the reservation -- so `offset + size` is in
        // bounds and the bytes are mapped read-write.
        Some(unsafe { self.data.as_mut_ptr().add(offset) })
    }

    /// JIT round 23 wave 4c, lane L4: drop every allocation anchor strictly
    /// inside `(offset, offset + size)`. With it on [`Self::hand_out`], the
    /// one door every allocation takes, an anchor is at all times one of: the
    /// base of a live hand-out (a TLAB's or an object's first byte, or its tail
    /// filler -- an object boundary), a byte not handed out since its hand-out
    /// was freed (inside or at the start of a free block, or above the
    /// cursor), and never an interior byte of a live layout. The lazy grid
    /// drops anchors strictly inside skip blocks and at or above `used`, so
    /// every bound it keeps is a boundary. One load per 4 KiB bucket of the
    /// hand-out (a 64 KiB TLAB: 16).
    fn forget_alloc_anchors_inside(&mut self, offset: usize, size: usize) {
        let Some(end) = offset.checked_add(size) else {
            return;
        };
        if size <= 8 || self.alloc_anchors.is_empty() {
            return;
        }
        let first = offset >> self.anchor_shift;
        let last = (end - 1) >> self.anchor_shift;
        for slot in self
            .alloc_anchors
            .iter_mut()
            .take(last.saturating_add(1))
            .skip(first)
        {
            if *slot > offset + 1 && *slot <= end {
                *slot = 0;
            }
        }
    }

    /// Install the owning heap's minimum committed-byte count and unused interval.
    /// This applies to free lists, unbumped space and cursor retraction.
    pub fn set_uncommit_policy(
        &mut self,
        floor: usize,
        enabled: bool,
        delay: std::time::Duration,
    ) {
        let keep_refused_idle_stamps = self
            .uncommit_policy
            .as_ref()
            .is_some_and(|p| p.keep_refused_idle_stamps);
        // zt t2/al: like the stamp choice above, survives a policy change.
        let (refused_fit_relists, refused_fits_relisted) = self
            .uncommit_policy
            .as_ref()
            .map_or((false, 0), |p| (p.refused_fit_relists, p.refused_fits_relisted));
        // zt t23/hunt4: so does `-XX:+AlwaysPreTouch` (the floor and the
        // options are installed after it may have been).
        let (pre_touch, pre_touched_bytes) = self
            .uncommit_policy
            .as_ref()
            .map_or((false, 0), |p| (p.pre_touch, p.pre_touched_bytes));
        // zki: and the hugetlbfs refusal count.
        let (hugetlb_commit_refusals, hugetlb_last_refused_granules) =
            self.uncommit_policy.as_ref().map_or((0, 0), |p| {
                (p.hugetlb_commit_refusals, p.hugetlb_last_refused_granules)
            });
        // zr z4/un: the reset ledger survives a policy change (the floor is
        // installed after the options at VM start).
        let granules = self.capacity().div_ceil(crate::reservation::GRANULE);
        let (reset_accounting, reset_idle, reset_idle_count) = match self.uncommit_policy.take() {
            Some(p) if p.reset_accounting && p.reset_idle.len() == granules => {
                (true, p.reset_idle, p.reset_idle_count)
            }
            Some(p) if p.reset_accounting => (true, vec![false; granules], 0),
            _ => (false, Vec::new(), 0),
        };
        self.uncommit_policy = Some(Box::new(ArenaUncommitPolicy {
            release_window: false,
            floor: floor.min(self.capacity()),
            enabled,
            delay,
            idle_since: if delay.is_zero() || !enabled {
                Vec::new()
            } else {
                vec![None; self.capacity().div_ceil(crate::reservation::GRANULE)]
            },
            keep_refused_idle_stamps,
            reset_accounting,
            reset_idle,
            reset_idle_count,
            refused_fit_relists,
            refused_fits_relisted,
            released_log: None,
            pre_touch,
            pre_touched_bytes,
            hugetlb_commit_refusals,
            hugetlb_last_refused_granules,
            #[cfg(test)]
            refuse_commits_for_test: false,
        }));
    }

    /// zt t23/hunt4 (`-XX:+AlwaysPreTouch`): from now on every commit for use
    /// that makes a granule resident touches its pages
    /// ([`crate::reservation::pre_touch_range`]), and, when `on`, the bytes
    /// committed NOW (the `-Xms` startup commit) are touched before this
    /// returns -- HotSpot ZGC pre-touches the initial capacity when it
    /// commits it at heap initialisation, and later commits as they are made.
    /// Returns the bytes touched now. A no-op answering `0` without a policy
    /// (every collector but ZGC). On the wholly-committed fallback store
    /// (`CRATONVM_GC_RESERVE=0`) only the first `floor` bytes (the startup
    /// commit's size) are touched: the rest of that store is "committed" only
    /// in name.
    pub fn set_pre_touch(&mut self, on: bool) -> usize {
        let Some(policy) = self.uncommit_policy.as_mut() else {
            return 0;
        };
        policy.pre_touch = on;
        if !on {
            return 0;
        }
        let floor = policy.floor;
        let spans = match &self.data {
            crate::reservation::HeapStore::Owned(_) => self.data.committed_spans(0, floor),
            crate::reservation::HeapStore::Reserved(_) => {
                self.data.committed_spans(0, self.capacity())
            }
        };
        let mut touched = 0usize;
        for (addr, len) in spans {
            // SAFETY: `committed_spans` names committed bytes of this store,
            // which stays mapped while `&mut self` is held.
            touched += unsafe { crate::reservation::pre_touch_range(addr as *mut u8, len) };
        }
        if let Some(policy) = self.uncommit_policy.as_mut() {
            policy.pre_touched_bytes += touched;
        }
        touched
    }

    /// g1gen/c10: `-XX:+AlwaysPreTouch` for an arena WITHOUT a policy (the
    /// generational heap's semi-spaces): the store touches its committed
    /// bytes now and every later commit ([`crate::reservation::HeapStore::
    /// set_pre_touch`]). Returns the bytes touched now.
    pub fn set_store_pre_touch(&mut self, on: bool) -> usize {
        let cap = self.capacity();
        self.data.set_pre_touch(on, cap)
    }

    /// zki (page `zgc-zt23hunt4-*` item 4): back this arena with explicit
    /// hugetlbfs pages ([`crate::reservation::HeapStore::use_explicit_huge_pages`]).
    /// Only ZGC calls it, before anything is committed.
    pub fn use_explicit_huge_pages(&mut self) -> bool {
        self.data.use_explicit_huge_pages()
    }

    /// zki: is this arena backed by explicit hugetlbfs pages?
    pub fn is_explicit_huge_pages(&self) -> bool {
        self.data.is_explicit_huge_pages()
    }

    /// zki: commits for use the hugetlbfs pool refused since the policy was
    /// installed (`0` without a policy or explicit pages).
    pub fn hugetlb_commit_refusals(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.hugetlb_commit_refusals)
    }

    /// zki: the granules of the last refused hugetlbfs commit for use (`0`
    /// before any).
    pub fn hugetlb_last_refused_granules(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.hugetlb_last_refused_granules)
    }

    /// zki (page `zgc-zt23hunt4-*` item 4): HotSpot 25 ZGC's startup commit
    /// on explicit hugetlbfs pages (`ZPhysicalMemoryBacking::commit`): try the
    /// whole `bytes` (rounded up to granules) as the prefix; when the pool
    /// refuses, commit as much as possible by halving -- each step tries half
    /// of what is left, advancing on success and shrinking the end on a
    /// refusal, until less than a granule is left. Returns `(committed,
    /// refusals)`: HotSpot logs one `Failed to commit memory` line per
    /// refusal and fails VM creation when `committed` falls short.
    pub fn commit_initial_explicit(&mut self, bytes: usize) -> (usize, usize) {
        use crate::reservation::GRANULE;
        let n = bytes.min(self.capacity());
        if n == 0 {
            return (0, 0);
        }
        let total = n.div_ceil(GRANULE).min(self.capacity() / GRANULE);
        if total == 0 {
            return (0, 0);
        }
        if self.data.commit_range(0, total * GRANULE) {
            return (n, 0);
        }
        let mut refusals = 1;
        let (mut start, mut end) = (0usize, total);
        loop {
            let len = (end - start) / 2;
            if len == 0 {
                break;
            }
            if self.data.commit_range(start * GRANULE, len * GRANULE) {
                start += len;
            } else {
                refusals += 1;
                end -= len;
            }
        }
        ((start * GRANULE).min(n), refusals)
    }

    /// zki: HotSpot 25 ZGC's uncommit-support probe on explicit hugetlbfs
    /// pages (`try_enable_uncommit`: commit one granule, uncommit it). `false`
    /// when the pool refuses the granule. Before anything else is committed.
    pub fn probe_explicit_uncommit(&mut self) -> bool {
        use crate::reservation::GRANULE;
        if self.capacity() < GRANULE || !self.data.commit_range(0, GRANULE) {
            return false;
        }
        let _ = self.data.decommit_range(0, GRANULE, "zgc-uncommit-probe");
        true
    }

    /// zt t23/hunt4: bytes the `-XX:+AlwaysPreTouch` path has touched, `0`
    /// without a policy.
    pub fn pre_touched_bytes(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.pre_touched_bytes)
    }

    /// zt t3/rl: start (`true`) or stop recording the spans the give-back
    /// releases. A no-op without a policy (every collector but ZGC).
    pub fn set_release_log(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.released_log = on.then(Vec::new);
        }
    }

    /// zt t3/rl: the spans released since [`Self::set_release_log`] (arena
    /// offsets, in release order), and stop recording.
    pub fn take_release_log(&mut self) -> Vec<(usize, usize)> {
        self.uncommit_policy
            .as_mut()
            .and_then(|p| p.released_log.take())
            .unwrap_or_default()
    }

    /// zt t3/rl: record a released span when a log is open.
    fn log_release(&mut self, lo: usize, hi: usize) {
        if let Some(log) = self
            .uncommit_policy
            .as_mut()
            .and_then(|p| p.released_log.as_mut())
        {
            log.push((lo, hi));
        }
    }

    /// zr z4/un: see `ArenaUncommitPolicy::reset_accounting`. A no-op on an
    /// arena without a policy (every collector but ZGC). Survives a later
    /// [`Self::set_uncommit_policy`].
    pub fn set_reset_accounting(&mut self, on: bool) {
        let granules = self.capacity().div_ceil(crate::reservation::GRANULE);
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.reset_accounting = on;
            if on {
                if p.reset_idle.len() != granules {
                    p.reset_idle = vec![false; granules];
                    p.reset_idle_count = 0;
                }
            } else {
                p.reset_idle = Vec::new();
                p.reset_idle_count = 0;
            }
        }
    }

    /// zr z4/un: was granule `g` reset by a give-back and not committed for
    /// use since? Always `false` without reset accounting.
    #[inline]
    fn is_reset_idle(&self, g: usize) -> bool {
        self.uncommit_policy
            .as_ref()
            .is_some_and(|p| p.reset_idle.get(g).copied().unwrap_or(false))
    }

    /// zr z4/un: record that granule `g` was reset (it is still committed).
    fn note_reset_idle(&mut self, g: usize) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            if let Some(slot) = p.reset_idle.get_mut(g) {
                if !*slot {
                    *slot = true;
                    p.reset_idle_count += 1;
                }
            }
        }
    }

    /// zr z4/un: committed bytes, less the granules a readable give-back has
    /// reset and nothing has committed for use since -- what the process
    /// really holds of this arena's commitment, and what HotSpot's
    /// `totalMemory()` reads after its uncommitter ran. Equal to
    /// `committed_bytes()` without reset accounting (and on every collector
    /// but ZGC, which installs no policy).
    pub fn net_committed_bytes(&self) -> usize {
        let reset = self
            .uncommit_policy
            .as_ref()
            .map_or(0, |p| p.reset_idle_count)
            .saturating_mul(crate::reservation::GRANULE);
        self.data.committed_bytes().saturating_sub(reset)
    }

    /// Bytes of this arena a readable give-back has reset and nothing has
    /// committed for use since (zr z4/un). Zero without reset accounting.
    pub fn reset_idle_bytes(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.reset_idle_count)
            .saturating_mul(crate::reservation::GRANULE)
    }

    /// zr z3/fp: see `ArenaUncommitPolicy::keep_refused_idle_stamps`. A no-op
    /// on an arena without a policy (every collector but ZGC). Survives a
    /// later [`Self::set_uncommit_policy`].
    pub fn set_keep_refused_idle_stamps(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.keep_refused_idle_stamps = on;
        }
    }

    /// zt t2/al: see `ArenaUncommitPolicy::refused_fit_relists`. A no-op on an
    /// arena without a policy (every collector but ZGC). Survives a later
    /// [`Self::set_uncommit_policy`].
    pub fn set_refused_fit_relists(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.refused_fit_relists = on;
        }
    }

    /// zt t2/al: spans a refused free-list commit put back on their list
    /// (`ArenaUncommitPolicy::refused_fit_relists`). Zero without a policy.
    pub fn refused_fits_relisted(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.refused_fits_relisted)
    }

    /// [`Self::hand_out`] for a span a free-list fit has just taken off its
    /// list (`[offset, offset + size)`, its remainders already re-listed). On
    /// a refused commit the span goes back on its list -- the high end's when
    /// `high` -- when the owner opted in (`refused_fit_relists`), exactly as
    /// [`Self::take_smallest_block_at_least`] already does; otherwise the old
    /// behaviour: the bytes are in no list until a slide rebuilds it. A
    /// refused `hand_out` changed nothing else (it fails before the pristine
    /// window, the slide set and the start bitmap are touched), so the span
    /// is exactly as free as it was. The span is no larger than the block the
    /// fit took, so `max_free_upper` / `high_max` stay upper bounds.
    #[must_use = "a refused commit must fail the allocation"]
    fn hand_out_fit(&mut self, offset: usize, size: usize, high: bool) -> Option<*mut u8> {
        let got = self.hand_out(offset, size);
        if got.is_some() {
            return got;
        }
        let relist = match self.uncommit_policy.as_mut() {
            Some(p) if p.refused_fit_relists => {
                p.refused_fits_relisted += 1;
                true
            }
            _ => false,
        };
        if relist {
            let block = FreeBlock { offset, size };
            if high {
                self.push_high(block);
            } else {
                self.push_block_routed(block);
            }
        }
        None
    }

    pub fn uncommit_floor(&self) -> usize {
        self.uncommit_policy.as_ref().map_or(0, |p| p.floor)
    }

    /// `(enabled, delay)` of the installed uncommit policy, or `None` when the
    /// owner installed none (zr z3/fp: lets the floor change keep both).
    pub fn uncommit_settings(&self) -> Option<(bool, std::time::Duration)> {
        self.uncommit_policy.as_ref().map(|p| (p.enabled, p.delay))
    }

    fn commit_range_for_use(&mut self, offset: usize, len: usize) -> bool {
        // zt t6/rv3 (`zgc-zt3x2-*` item 2): commit FIRST and clear the idle
        // stamps and reset bits only once the OS said yes. A refused commit
        // used to clear them before it failed; with the refused fit
        // re-listed (`CRATONVM_ZGC_REFUSED_FIT_RELISTS`, default ON) the span
        // then sat on a free list with no stamp and an under-counted
        // `reset_idle_count`. The test hook stands in the commit's own place.
        let refused = self
            .uncommit_policy
            .as_ref()
            .is_some_and(|policy| refuse_commit_for_test(policy, len));
        // zt t23/hunt4 (`-XX:+AlwaysPreTouch`): the commit count before, so a
        // commit that made a granule resident is seen below. One field load
        // when the flag is off.
        let pre_touch_from = self
            .uncommit_policy
            .as_ref()
            .is_some_and(|policy| policy.pre_touch)
            .then(|| self.data.committed_bytes());
        if refused || !self.data.commit_range(offset, len) {
            // zki: a refusal of the hugetlbfs pool is HotSpot's logged one.
            if !refused && self.data.is_explicit_huge_pages() {
                if let Some(policy) = self.uncommit_policy.as_mut() {
                    policy.hugetlb_commit_refusals += 1;
                    let g = crate::reservation::GRANULE;
                    let first = offset / g;
                    let last = offset.saturating_add(len.max(1)).div_ceil(g);
                    policy.hugetlb_last_refused_granules = last.saturating_sub(first).max(1);
                }
            }
            return false;
        }
        // zt t23/hunt4: a granule the reset give-back had emptied and this
        // commit re-counts is resident again too (cleared just below).
        let mut recommitted =
            pre_touch_from.is_some_and(|before| self.data.committed_bytes() > before);
        if let Some(policy) = self.uncommit_policy.as_mut() {
            if !policy.idle_since.is_empty() && len != 0 {
                let first = offset / crate::reservation::GRANULE;
                let end = offset
                    .saturating_add(len)
                    .div_ceil(crate::reservation::GRANULE)
                    .min(policy.idle_since.len());
                if first < end {
                    policy.idle_since[first..end].fill(None);
                }
            }
            // zr z4/un: a granule committed for use is about to be written,
            // so it is resident (and counted) again.
            if policy.reset_idle_count != 0 && len != 0 {
                let first = offset / crate::reservation::GRANULE;
                let end = offset
                    .saturating_add(len)
                    .div_ceil(crate::reservation::GRANULE)
                    .min(policy.reset_idle.len());
                if first < end {
                    for slot in &mut policy.reset_idle[first..end] {
                        if *slot {
                            *slot = false;
                            policy.reset_idle_count -= 1;
                            recommitted = true;
                        }
                    }
                }
            }
        }
        // zt t23/hunt4: touch the granules of the range, as HotSpot ZGC
        // pre-touches what it commits under `-XX:+AlwaysPreTouch`. Granules of
        // the range that were already resident are touched again, harmlessly
        // (the touch never changes a byte), so no per-granule list is kept.
        if pre_touch_from.is_some() && recommitted && len != 0 {
            use crate::reservation::GRANULE;
            let lo = offset / GRANULE * GRANULE;
            let hi = offset
                .saturating_add(len)
                .div_ceil(GRANULE)
                .saturating_mul(GRANULE)
                .min(self.capacity());
            if hi > lo {
                // SAFETY: `commit_range` committed every granule `[offset,
                // offset + len)` touches, which is `[lo, hi)` clipped to the
                // store, and the store stays mapped while `&mut self` is held.
                let touched = unsafe {
                    crate::reservation::pre_touch_range(self.data.as_mut_ptr().add(lo), hi - lo)
                };
                if let Some(policy) = self.uncommit_policy.as_mut() {
                    policy.pre_touched_bytes += touched;
                }
            }
        }
        true
    }

    /// Hand whole granules of `[lo, hi)` back to the OS.
    ///
    /// **The caller must have proved the span holds nothing live and is on no
    /// free list.** Both callers satisfy that by construction: they pass space
    /// a cursor has just retracted past, which is un-bumped by definition.
    ///
    /// `site` names WHICH of those proofs was made. It is remembered with the
    /// span and printed by the crash handler when a fault lands inside it
    /// (`crate::reservation::recent_decommit_covering`), because the two
    /// families fail differently: a span released from the FREE LIST that was
    /// still reachable is a missing root, while one released behind a RETRACTED
    /// CURSOR that was still reachable is a sweep that mis-sized the live set.
    ///
    /// Under the `readable_give_back` mode the pages are RESET instead (still
    /// mapped, committed, reading as zero), and the return value counts only
    /// what really became a hole -- normally zero -- because that is the number
    /// every caller uses to decide whether a raw-load bound must be withdrawn.
    /// The reset bytes are summed in [`Self::bytes_reset`].
    fn decommit_span(&mut self, lo: usize, hi: usize, site: &'static str) -> usize {
        // Other collectors incur neither timestamp storage nor a clock read.
        if self.uncommit_policy.is_none() {
            return if hi > lo {
                self.release_unused_span(lo, hi, site)
            } else {
                0
            };
        }
        self.decommit_span_at(lo, hi, site, std::time::Instant::now())
    }

    fn decommit_span_at(
        &mut self,
        lo: usize,
        hi: usize,
        site: &'static str,
        now: std::time::Instant,
    ) -> usize {
        let (lo, delay) = match self.uncommit_policy.as_ref() {
            Some(p) if !p.enabled => return 0,
            Some(p) => (lo, p.delay),
            None => (lo, std::time::Duration::ZERO),
        };
        if hi <= lo {
            return 0;
        }
        let release_window = self
            .uncommit_policy
            .as_ref()
            .is_none_or(|p| p.release_window);
        if delay.is_zero() {
            return if release_window {
                self.release_unused_span(lo, hi, site)
            } else {
                0
            };
        }
        let granule = crate::reservation::GRANULE;
        let first = lo.div_ceil(granule);
        let end = hi / granule;
        let mut released = 0;
        for g in first..end {
            // zr z4/un: a granule already reset (and untouched since) has
            // nothing left to give back; resetting it again every delay was
            // a syscall per granule per pass for nothing.
            if !self.data.is_committed_at(g * granule) || self.is_reset_idle(g) {
                self.uncommit_policy.as_mut().unwrap().idle_since[g] = None;
                continue;
            }
            let since = *self.uncommit_policy.as_mut().unwrap().idle_since[g].get_or_insert(now);
            if release_window && now.saturating_duration_since(since) >= delay {
                let reset_before = self.bytes_reset;
                let n = self.release_unused_span(g * granule, (g + 1) * granule, site);
                released += n;
                // A readable give-back RESETS (counted in `bytes_reset`) and
                // reports 0 decommitted; that is a release too.
                let acted = n != 0 || self.bytes_reset != reset_before;
                let keep = self
                    .uncommit_policy
                    .as_ref()
                    .is_some_and(|p| p.keep_refused_idle_stamps);
                // zr z3/fp: restart the clock only when the granule really
                // went back. A release the `-Xms` floor budget refused used to
                // reset the stamp too, so once commitment above the floor came
                // back the granule had to sit idle for a whole new delay
                // although it had already been unused that long.
                // `CRATONVM_ZGC_FOOTPRINT_TRIM=0` keeps the old reset.
                if acted || !keep {
                    self.uncommit_policy.as_mut().unwrap().idle_since[g] = None;
                }
            }
        }
        released
    }

    fn release_unused_span(&mut self, lo: usize, hi: usize, site: &'static str) -> usize {
        let floor = self.uncommit_floor();
        let granule = crate::reservation::GRANULE;
        if self.readable_give_back {
            let accounting = self
                .uncommit_policy
                .as_ref()
                .is_some_and(|p| p.reset_accounting);
            if floor == 0 && !accounting {
                let (reset, decommitted) = self.data.reset_range(lo, hi - lo, site);
                self.bytes_reset = self.bytes_reset.saturating_add(reset);
                if reset != 0 || decommitted != 0 {
                    self.log_release(lo, hi);
                }
                return decommitted;
            }
            // Windows reset decommits then recommits. If that commit fails,
            // reset returns a real hole. Admit at most one granule at a time
            // from commitment above Xms, retaining the floor on that failure.
            //
            // zr z4/un: under reset accounting the floor is tested against the
            // NET commitment (a reset granule no longer counts: before, the
            // test saw the peak commitment for ever, so once committed rose
            // above `-Xms` every idle granule was reset, down to nothing), a
            // granule already reset is skipped, and each reset is recorded.
            let mut released = 0;
            for g in lo.div_ceil(granule)..hi / granule {
                let held = if accounting {
                    self.net_committed_bytes()
                } else {
                    self.data.committed_bytes()
                };
                if floor != 0 && held.saturating_sub(floor) < granule {
                    break;
                }
                if self.data.is_committed_at(g * granule) && !self.is_reset_idle(g) {
                    let (reset, decommitted) = self.data.reset_range(g * granule, granule, site);
                    self.bytes_reset = self.bytes_reset.saturating_add(reset);
                    released += decommitted;
                    if reset != 0 || decommitted != 0 {
                        self.log_release(g * granule, (g + 1) * granule);
                    }
                    if accounting && reset != 0 && self.data.is_committed_at(g * granule) {
                        self.note_reset_idle(g);
                    }
                }
            }
            return released;
        }
        if floor == 0 {
            let n = self.data.decommit_range(lo, hi - lo, site);
            if n != 0 {
                self.log_release(lo, hi);
            }
            return n;
        }
        // Xms is a minimum committed-byte count, not an immutable address
        // prefix: ZGC allocates from both ends of this reservation. An idle
        // low granule may replace an extra live high granule in that budget.
        let mut budget = self.net_committed_bytes().saturating_sub(floor) / granule;
        let mut released = 0;
        for g in lo.div_ceil(granule)..hi / granule {
            if budget == 0 {
                break;
            }
            if self.data.is_committed_at(g * granule) {
                let n = self.data.decommit_range(g * granule, granule, site);
                released += n;
                budget -= n / granule;
                if n != 0 {
                    self.log_release(g * granule, (g + 1) * granule);
                }
            }
        }
        released
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
            return self.hand_out_fit(off, alloc_size, true);
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
                return self.hand_out_fit(off, alloc_size, true);
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
        // zt t1/sp: while the final-refusal rule
        // (`CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`) has set a threshold, a
        // high-end hole smaller than it serves no request either: this is
        // `alloc`'s last-resort arm for a SMALL object, and the s10/sp cut
        // left it the one free-list exit without the predicate. Large-object
        // requests are never below the threshold (it is capped under
        // `ZGC_LARGE_OBJECT_MIN`), so `alloc_high` is unaffected. `0`: any.
        let min_block = self.min_free_span;
        // (index, head padding, block size)
        let mut best: Option<(usize, usize, usize)> = None;
        for (i, block) in self.free_high.iter().enumerate() {
            let block_addr = base + block.offset;
            let padding = ((block_addr + align - 1) & !(align - 1)) - block_addr;
            let Some(need) = padding.checked_add(alloc_size) else {
                continue;
            };
            if need <= block.size
                && block.size >= min_block
                && best.is_none_or(|(_, _, s)| block.size < s)
            {
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
        // Remember where the cursor was, for the same reason the LOW retraction
        // does (see `low_high_water`): a bitmap pass bounded by the cursor after
        // this retraction would skip `[old_high, high_cursor)`, and the MARK
        // bitmap has bits there -- set during the cycle, while the cursor was
        // still low. Leaving them uncleared means a later cycle, once the cursor
        // has bumped back DOWN over that span, reads a fresh object as
        // already-marked and never traces it, so everything reachable only
        // through it is swept while live.
        self.pre_retract_low = self.pre_retract_low.min(old_high);
        // As the low retraction: `[old_high, high_cursor)` is un-bumped again
        // and off the free list, so its whole granules go back to the OS.
        self.decommit_span(old_high, self.high_cursor, "high-cursor-retract");
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
        // gce e1/y: a merge re-lists only bytes that were already listed, so
        // the known-zero claim survives it (both calls below forget it).
        let known_zero = std::mem::take(&mut self.zero_sets.known_zero);
        self.clear_low_free_list();
        for (off, sz) in merged {
            self.add_free_block(off, sz);
        }
        self.zero_sets.known_zero = known_zero;
        // The rebuild above counted every re-added block through
        // `add_free_block`; the list it produced IS maximal, so zero it back.
        self.free_pushed = 0;
        // Merging paid: be willing to look again immediately.
        self.coalesce_threshold = COALESCE_THRESHOLD_MIN;
        removed
    }

    /// ZT3GN: the young complement's install without the rebuild. Drops from
    /// the LOW list every block that touches an ascending, disjoint `window`
    /// or starts at or above `limit`, clips the one that crosses `floor`, and
    /// lists `new_spans` (ascending, disjoint from what stays). Blocks outside
    /// the windows are left where they are: no collect, no sort, no re-push.
    /// Returns `(bytes_removed, bytes_inside_windows)`; the list then holds
    /// its old bytes minus the first plus `new_spans`.
    pub fn splice_low_free_list(
        &mut self,
        windows: &[(usize, usize)],
        limit: usize,
        floor: usize,
        new_spans: &[(usize, usize)],
    ) -> (usize, usize) {
        self.forget_known_zero();
        let (mut removed, mut in_windows) = (0usize, 0usize);
        let mut readd: Vec<FreeBlock> = Vec::new();
        // `Some(size)` keeps the block at that size, `None` drops it.
        let mut verdict = |off: usize, size: usize| -> Option<usize> {
            if off >= limit {
                removed += size;
                return None;
            }
            let end = (off + size).min(floor);
            if end <= off {
                removed += size;
                return None;
            }
            let i = windows.partition_point(|&(wo, wl)| wo + wl <= off);
            if windows.get(i).is_some_and(|&(wo, _)| wo < end) {
                let (wo, wl) = windows[i];
                in_windows += end.min(wo + wl) - off.max(wo);
                removed += size;
                return None;
            }
            removed += size - (end - off);
            Some(end - off)
        };
        for k in 0..self.free_small.len() {
            let bucket = &mut self.free_small[k];
            if bucket.is_empty() {
                continue;
            }
            bucket.retain(|b| match verdict(b.offset, b.size) {
                Some(sz) if sz == b.size => true,
                Some(sz) => {
                    readd.push(FreeBlock { offset: b.offset, size: sz });
                    false
                }
                None => false,
            });
            if bucket.is_empty() {
                mask_clear(&mut self.small_mask, k);
            }
        }
        let mut large_dropped = 0usize;
        self.free_large.retain(|_, bucket| {
            bucket.retain(|b| match verdict(b.offset, b.size) {
                Some(sz) if sz == b.size => true,
                Some(sz) => {
                    large_dropped += 1;
                    readd.push(FreeBlock { offset: b.offset, size: sz });
                    false
                }
                None => {
                    large_dropped += 1;
                    false
                }
            });
            !bucket.is_empty()
        });
        self.free_large_blocks -= large_dropped;
        // `removed` counted clipped blocks' loss only; the clipped blocks
        // themselves were taken off the list whole and come back below.
        let readd_bytes: usize = readd.iter().map(|b| b.size).sum();
        self.free_bytes_total -= removed + readd_bytes;
        for b in readd {
            self.push_block_routed(b);
        }
        for &(off, len) in new_spans {
            if len == 0 {
                continue;
            }
            if (off | len) & 7 != 0 {
                warn_unaligned_block("splice", off, len);
            }
            self.max_free_upper = self.max_free_upper.max(len);
            self.push_block_routed(FreeBlock { offset: off, size: len });
        }
        self.free_pushed = 0;
        self.coalesce_threshold = COALESCE_THRESHOLD_MIN;
        self.clear_alloc_anchors();
        (removed, in_windows)
    }

    /// Replace the whole LOW free list with `spans`, which must be
    /// address-ordered, non-overlapping, non-adjacent, and below the low
    /// cursor.
    ///
    /// The sweep used to hand this list over one dead object at a time, and
    /// every consumer downstream then had to undo that: `coalesce_free_list`
    /// sorted the whole list and merged neighbours, `retract_cursor_into_free_tail`
    /// sorted it again to find the top block, `decommit_free_blocks` sorted it
    /// again looking for granules, and `compact_low_to` sorted it once more.
    /// Four or five full sorts and rebuilds per collection, all of them
    /// recovering an ordering the sweep destroyed by walking objects instead
    /// of address space.
    ///
    /// A sweep that computes free space as *the complement of the live set*
    /// produces the merged, ordered list directly, so this installs it as it
    /// stands: no sort, no coalesce pass, and `free_pushed` left at zero
    /// because nothing is pending to merge.
    ///
    /// Returns the bytes that were on the list this call replaced, which is
    /// what a caller needs to tell "free now" from "freed by this cycle".
    ///
    /// # Panics (debug only)
    /// Debug-asserts the ordering and disjointness it relies on, and that
    /// every span lies below the cursor.
    pub fn rebuild_low_free_list(&mut self, spans: &[(usize, usize)]) -> usize {
        #[cfg(debug_assertions)]
        {
            let mut prev_end = 0usize;
            for &(off, len) in spans {
                assert!(len > 0, "an empty span is not a free block");
                assert!(
                    off >= prev_end,
                    "spans must be ordered and disjoint: {off} starts below {prev_end}",
                );
                assert!(
                    off + len <= self.cursor,
                    "span {off}+{len} runs past the low cursor {}",
                    self.cursor,
                );
                prev_end = off + len;
            }
        }
        let was_free = self.clear_low_free_list();
        for &(off, len) in spans {
            if len == 0 {
                continue;
            }
            if (off | len) & 7 != 0 {
                warn_unaligned_block("rebuild", off, len);
            }
            self.max_free_upper = self.max_free_upper.max(len);
            self.push_block_routed(FreeBlock {
                offset: off,
                size: len,
            });
        }
        // Merged by construction, so nothing is pending and the backoff has
        // nothing to back off from.
        self.free_pushed = 0;
        self.coalesce_threshold = COALESCE_THRESHOLD_MIN;
        // The anchors index INTO the list this call just replaced.
        self.clear_alloc_anchors();
        was_free
    }

    /// zs s10/sp: route the remainders of a free-list fit that carved
    /// `fit_size` bytes at `offset` for a request of `alloc_size`
    /// (`fit_size > alloc_size` only while [`Self::min_free_span`] is set).
    ///
    /// The excess `[offset + alloc_size, offset + fit_size)` is given back
    /// JOINED to the split's tail remainder (which starts at
    /// `offset + fit_size`), as one block: the block the fit chose keeps
    /// serving later requests from its rest, as a TLAB carved from a hole
    /// serves the objects bumped after the first one. zt t1/sp review: the
    /// s10/sp cut pushed the excess as a block of its own, so with the
    /// `on` floor (2048) every small object cut a 2048-byte unit from its
    /// block and left `2048 - size` bytes the threshold then set aside --
    /// a 64 KiB span served 32 objects of 16 bytes instead of 4096.
    #[inline]
    fn route_fit_remainders(
        &mut self,
        offset: usize,
        alloc_size: usize,
        fit_size: usize,
        remainders: [Option<FreeBlock>; 2],
    ) {
        let [head, tail] = remainders;
        if let Some(head) = head {
            self.push_block_routed(head);
        }
        let excess = fit_size.saturating_sub(alloc_size);
        if excess == 0 {
            if let Some(tail) = tail {
                self.push_block_routed(tail);
            }
            return;
        }
        let rest = offset + alloc_size;
        match tail {
            Some(tail) if tail.offset == offset + fit_size => {
                self.push_block_routed(FreeBlock {
                    offset: rest,
                    size: excess + tail.size,
                });
            }
            other => {
                if let Some(tail) = other {
                    self.push_block_routed(tail);
                }
                self.push_block_routed(FreeBlock {
                    offset: rest,
                    size: excess,
                });
            }
        }
    }

    /// zs s10/sp: set the smallest LOW free-list block a fit may take (`0`:
    /// any, today's behaviour). Rounded up to the 8-byte object grid.
    pub fn set_min_free_span(&mut self, bytes: usize) {
        self.min_free_span = bytes.saturating_add(7) & !7;
    }

    /// zs s10/sp: the current [`Self::set_min_free_span`] value.
    pub fn min_free_span(&self) -> usize {
        self.min_free_span
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
        // gce e1/y: these bytes may have been handed out since the known-zero
        // set was installed (see the `known_zero` field).
        self.forget_known_zero();
        if size == 0 {
            return;
        }
        if (offset | size) & 7 != 0 {
            warn_unaligned_block("add_free_block", offset, size);
        }
        // A freed base stops being a base HERE, at the same moment the free
        // list learns about it. Without this a larger object later allocated
        // over the hole would see the stale bit, be accepted at an interior
        // address, and take a mark-bit write into the middle of a live object.
        if let Some(bits) = self.starts.as_ref() {
            bits.remove(self.data.as_ptr() as usize + offset);
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
        // gce e1/y: every reset and every caller-driven rebuild comes through
        // here (or `clear_low_free_list`); see the `known_zero` field.
        self.forget_known_zero();
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
    /// CRATONVM-SPRING-GENUINE-BUGLIST.
    ///
    /// The epoch-gated cache that first closed it is GONE, and this doc used
    /// to describe it in the present tense (gengc-round1-alloc, 2026-09-20).
    /// The running total now lives in [`Self::free_bytes_total`], maintained
    /// incrementally at every mutation site, so this is unconditionally O(1)
    /// — including under the steady churn that degraded the cache back to a
    /// per-call recompute. See that field for the whole argument.
    pub fn free_list_bytes(&self) -> usize {
        self.free_bytes_total
    }

    /// gcf f3/o: a sound UPPER BOUND on the largest free block of either tier
    /// (`high_max` is exact for the high tier, [`Self::max_free_upper`] bounds
    /// the low tiers), O(1). `has_free_block_at_least(size)` is `false`
    /// whenever `size` exceeds it. gcg g4/yg: no production reader since lane
    /// jn deleted the lock-free refill gate (`CRATONVM_GC_REFILL_GATE_LOCKFREE`)
    /// that published it; only its soundness test
    /// (`gen_heap::gcf_f3o_refill_gate_tests`) reads it now.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn free_block_upper_bound(&self) -> usize {
        self.max_free_upper.max(self.high_max)
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

    /// Remove and hand out, WHOLE, the smallest LOW free block of at least
    /// `min` bytes -- or, when the smallest such block is larger than `max`,
    /// a `max`-byte head of it (the tail goes back on the free list). `None`
    /// when no low free block reaches `min`, when `min > max`, and under
    /// [`Self::prefer_bump`]; the bump tail and the high end are never
    /// consulted (the caller falls back to [`Self::alloc`]).
    ///
    /// gci i22/dr (P2/yy,
    /// `gch-h1yy-proposal-refill-takes-the-smallest-whole-free-block-20261008`):
    /// best fit BY SIZE for a TLAB refill. [`Self::alloc`] best-fits the
    /// REQUEST, so a refill asking for up to 1 MiB cuts the smallest block of
    /// at least the request -- the large spans a sweep has just coalesced --
    /// while the medium holes below the request stay unused until every
    /// larger block is gone. This takes the medium holes first and leaves the
    /// large blocks whole for the large arrays. One bitmap probe (small tier)
    /// or one range query (span tier); both tiers are ordered by size.
    ///
    /// `min` is rounded up and `max` down to the 8-byte grid. A refused
    /// commit puts the block back and answers `None`.
    pub fn take_smallest_block_at_least(
        &mut self,
        min: usize,
        max: usize,
    ) -> Option<(*mut u8, usize)> {
        // zt t1/sp: the final-refusal threshold (`min_free_span`, 0 when the
        // opt-in rule is off) binds this refill primitive as it binds `alloc`.
        let min = min.max(SMALL_GRAIN).max(self.min_free_span).checked_add(7)? & !7;
        let max = max & !7;
        if min > max || self.prefer_bump || self.free_is_empty() || min > self.max_free_upper {
            return None;
        }
        // Small tier: bucket `k` holds blocks of at least `k * SMALL_GRAIN`
        // bytes, so the first occupied bucket from `min / SMALL_GRAIN` (min is
        // on the grid) holds only blocks of at least `min`, and every bucket
        // below it only smaller ones.
        let small = if min < LARGE_BLOCK_MIN {
            mask_first_from(&self.small_mask, min >> SMALL_GRAIN_SHIFT).map(|k| {
                let list = &mut self.free_small[k];
                let block = list
                    .pop()
                    .expect("an occupied size class holds a block");
                if list.is_empty() {
                    mask_clear(&mut self.small_mask, k);
                }
                self.free_bytes_total -= block.size;
                block
            })
        } else {
            None
        };
        // Span tier: every span is at least LARGE_BLOCK_MIN, above every
        // small block, so it is asked only when the small tier had nothing.
        let block = match small {
            Some(block) => block,
            None => {
                let (&sz, list) = self.free_large.range(min..).next()?;
                let idx = list.len() - 1;
                self.take_from_bucket(sz, idx)
            }
        };
        let take = block.size.min(max) & !7;
        if take < min {
            // Only an off-grid block can get here (its size rounds below
            // `min`): give it back untouched.
            self.push_block_routed(block);
            return None;
        }
        if take < block.size {
            self.push_block_routed(FreeBlock {
                offset: block.offset + take,
                size: block.size - take,
            });
        }
        self.note_region_leak("free-list-smallest-whole", block.offset, take);
        match self.hand_out(block.offset, take) {
            Some(ptr) => Some((ptr, take)),
            None => {
                self.push_block_routed(FreeBlock {
                    offset: block.offset,
                    size: take,
                });
                None
            }
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
    fn clear_low_free_list(&mut self) -> usize {
        // gce e1/y: `rebuild_low_free_list`, `rebuild_after_in_place_evacuation`
        // and `compact_low_to` list bytes that may have been handed out;
        // `coalesce_free_list` restores the set around its call.
        self.forget_known_zero();
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
        dropped
    }

    /// zt t21/zc2 (`CRATONVM_ZGC_CONCURRENT_SWEEP`): drop the whole LOW free
    /// list at the pause of a cycle whose sweep runs after the release, and
    /// return the bytes it held. The deferred complement lists every one of
    /// those bytes again (they are below the low cursor and under no
    /// survivor); until it installs, an allocation bumps or takes a high hole,
    /// so nothing hands out a byte the sweep has not proven free. The
    /// allocation anchors index into the list, so they go with it.
    pub fn take_low_free_list_for_deferred_sweep(&mut self) -> usize {
        let dropped = self.clear_low_free_list();
        self.clear_alloc_anchors();
        dropped
    }

    /// zt t21/zc2: the LOW free list's blocks, ascending, and the list
    /// emptied -- what the window between a deferred sweep's pause and its
    /// install put there (retired TLAB tails above the pause's cursor), for the
    /// install to put back after [`Self::rebuild_low_free_list`].
    pub fn take_low_free_blocks(&mut self) -> Vec<(usize, usize)> {
        let blocks = self.low_blocks_sorted();
        let _ = self.clear_low_free_list();
        blocks
    }

    /// Snapshot of the current free list as `(offset, size)` pairs,
    /// sorted by ascending offset. Used by the non-moving sweep's object
    /// walker to skip holes the same way `OldGen::walk_objects` does.
    ///
    /// Allocates and sorts on every call: a caller that asks about many
    /// addresses must take one snapshot and reuse it (gc-common w16-x,
    /// `GenerationalHeap::reclaimed_hole_probe`).
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

    /// Bytes of the low region a compaction may consider -- the bump cursor.
    ///
    /// Not [`Self::used`], which folds in the large-object region at the other
    /// end and would send a compactor walking addresses above `high_cursor`.
    pub fn used_low_for_compaction(&self) -> usize {
        self.cursor
    }

    /// The highest the low bump cursor has been since
    /// [`Self::reset_low_high_water`], which is the span any bitmap over this
    /// arena may have bits set in.
    ///
    /// # Why the cursor alone is not that bound
    ///
    /// [`Self::retract_cursor_to`] LOWERS the cursor onto the last survivor
    /// after every sweep, and it is the only thing that does. A bitmap pass
    /// bounded by the cursor after a retraction therefore skips the span the
    /// retraction just gave back -- and the MARK bitmap has bits there, set
    /// during the cycle when the cursor was still high. Clearing bounded by
    /// the new cursor leaves them set, and the next cycle then reads a mark
    /// set carrying a previous cycle's bits: it retains whatever they name.
    ///
    /// `pre_retract_high` is what makes the two agree. It is maxed with the
    /// cursor a retraction moved away FROM, so `cursor.max(pre_retract_high)`
    /// is exactly the high-water mark, and it costs a compare on the
    /// retraction path rather than anything on the bump path.
    pub fn low_high_water(&self) -> usize {
        self.cursor.max(self.pre_retract_high)
    }

    /// The lowest the HIGH bump cursor has been since
    /// [`Self::reset_high_low_water`] -- the bottom of the span any bitmap over
    /// the large-object end may have bits set in.
    ///
    /// The mirror of [`Self::low_high_water`], and it exists for exactly the
    /// same reason, arriving from the opposite direction. The high end bumps
    /// DOWN, so its live span is `[high_cursor, capacity)` and
    /// [`Self::retract_high_cursor_into_free_head`] RAISES the cursor, which
    /// SHRINKS that span. A bitmap pass bounded by the raw cursor after a
    /// retraction therefore skips the bytes the retraction just gave back, and
    /// the mark bitmap has bits there.
    ///
    /// Left un-mirrored when `low_high_water` landed. It bites a workload whose
    /// large-object end fragments: once the cursor bumps back down over an
    /// uncleared span, a fresh object there reads as already-marked, the marker
    /// never traces it, and whatever is reachable only through it is swept
    /// while live. Latent rather than measured -- no workload here reproduces
    /// it, because it needs allocations at or above `ZGC_LARGE_OBJECT_MIN` to
    /// fragment that end.
    pub fn high_low_water(&self) -> usize {
        self.high_cursor.min(self.pre_retract_low)
    }

    /// Forget the retraction history, so the next
    /// [`Self::low_high_water`] starts from the current cursor.
    ///
    /// Called once per collection, AFTER every bitmap over this arena has been
    /// cleared -- calling it earlier would hand a later pass a bound that no
    /// longer covers the bits it has to reach.
    pub fn reset_low_high_water(&mut self) {
        self.pre_retract_high = 0;
    }

    /// [`Self::reset_low_high_water`] for the large-object end, with the same
    /// "call it AFTER every bitmap is clear" obligation.
    pub fn reset_high_low_water(&mut self) {
        self.pre_retract_low = usize::MAX;
    }

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
    /// make. It is `pub` so that claim stays inside this crate.
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
    ///
    /// `vacated` is the third argument and the reason it exists is the whole
    /// of `Follow-up 2026-08-29` on
    /// `bug-h2-testkillprocess-zgc-oom-at-97-percent-free-20260821-FIXED-20260829.md`:
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
    pub fn compact_low_to(
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
        // zt t31/zc11 (`CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION`, the
        // concurrent-collector program page's carried r21 item): an opted-in
        // owner (ZGC) does NOT zero the tail above the new cursor here -- ~41 ms
        // of a ~50 ms slide over ~390 MB on the r21 lean row, inside the pause.
        // The bytes are not listed as zeroed either, so every hand-out of them
        // zeroes them first (`take_last_hand_out_zero_plan`: the TLAB carve,
        // `alloc_raw`), HotSpot's place for it. What the zeroing used to
        // guarantee still holds without it:
        //
        // * a corpse header in the tail is never taken for an object: every
        //   conservative or stale probe is answered by the object-start
        //   registry (`ZgcRealHeap::is_object_address`), which the slide
        //   rebuilt without the vacated bases, and no linear walk reaches
        //   above the cursor;
        // * a forwarding record is read only inside the LAST slide's stamped
        //   range, i.e. this tail, and no stale record from an earlier slide
        //   can sit in it: the tail lies below the cursor this slide found,
        //   every byte below that cursor was handed out (and so zeroed) after
        //   any earlier slide that could have stamped it, and the bump cursor
        //   only rises by hand-outs.
        let tail_deferred = self.defer_slide_tail_zeroing;
        // Zero the vacated span. A slid-down survivor leaves its old bytes
        // behind verbatim, including a valid-looking `ObjectHeader`, and a
        // conservative scanner that met one would resurrect a corpse.
        //
        // ...except what the arena already proves reads zero (r16 zgcarray
        // ZA1): never-written bytes, and the last slide's zeroed set. Writing
        // zeroes there is not just wasted work: those pages may never have been
        // touched at all (an array whose body nobody stored to, over bytes the
        // allocator did not re-zero), and the store faults every one of them in
        // INSIDE THE PAUSE. Measured, 1 GiB heap, `R16ZgcarrayAlloc
        // -Dlens=4096`: `relocate_us` 113 ms against 5 ms for the same 105 MB.
        if !tail_deferred {
            self.fill_zero_unless_known(new_cursor, self.cursor);
        }
        let old_cursor = self.cursor;
        self.cursor = new_cursor;
        // ZA1: the zero-filled spans (this tail, and the vacated spans below
        // the cursor zeroed and free-listed further down), so the allocation
        // that hands them out next does not zero them a second time. See
        // `SlideZeroWindow`. A deferred tail is not one of them.
        let mut zeroed: Vec<(usize, usize)> = Vec::with_capacity(vacated.len() + 1);
        if !tail_deferred {
            zeroed.push((new_cursor, old_cursor));
        }
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
                let inside_vacated = vacated.iter().any(|&(s, e)| off < e && s < off + size);
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
                self.fill_zero_unless_known(s, e);
                self.add_free_block(s, e - s);
                zeroed.push((s, e));
            }
        }
        self.reset_slide_zero_spans(zeroed);
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

    /// t1 wave 10 lane ZT3GN (`CRATONVM_ZGC_GEN_YOUNG_EVACUATE_LOCAL_INSTALL`,
    /// inside the opt-in nursery-scoped young slide): [`Self::compact_low_to`]
    /// for a slide that wrote and vacated nothing below the LOW offset `lo`
    /// (the scope floor). The same blocks are kept, dropped and truncated:
    ///
    /// * a block wholly below `lo` cannot overlap `touched`, a vacated span or
    ///   the bytes at or above `new_cursor` (all at or above `lo`), so it is
    ///   left where it is -- no sort, no clear, no re-add of the old
    ///   generation's holes;
    /// * a block reaching `lo` or above is dropped when it starts at or above
    ///   `new_cursor`, overlaps `touched` or a vacated span (one binary search;
    ///   the spans are ascending), truncated when it straddles `new_cursor`,
    ///   and otherwise left in place.
    ///
    /// The vacated spans (clamped at `new_cursor`) are published MERGED where
    /// they touch -- the same bytes as one block, so a pinned newest page
    /// leaves `[dest, pinned page)` as one span, not one per page. With
    /// `zero_vacated` false they are not memset here: the hand-out that takes
    /// them zeroes them (see `zgc_gen_young_evacuate_defer_vacated_zero`), and
    /// they are not listed in the slide's zeroed set. The tail above
    /// `new_cursor` is handled exactly as `compact_low_to` handles it.
    ///
    /// Returns `None`, changing nothing, when the slide does not fit the
    /// premise (`new_cursor` above the cursor or below `lo`, `touched` or a
    /// span below `lo`, spans not ascending and disjoint): the caller then
    /// runs `compact_low_to`. Otherwise the bytes reclaimed (the cursor drop).
    pub fn compact_nursery_to(
        &mut self,
        new_cursor: usize,
        lo: usize,
        touched: std::ops::Range<usize>,
        vacated: &[(usize, usize)],
        zero_vacated: bool,
    ) -> Option<usize> {
        let premise = new_cursor <= self.cursor
            && new_cursor >= lo
            && (touched.start >= touched.end || touched.start >= lo)
            && vacated.iter().all(|&(s, e)| s >= lo && s <= e)
            && vacated.windows(2).all(|w| w[0].1 <= w[1].0);
        if !premise {
            return None;
        }
        let reclaimed = self.cursor - new_cursor;
        let tail_deferred = self.defer_slide_tail_zeroing;
        if !tail_deferred {
            self.fill_zero_unless_known(new_cursor, self.cursor);
        }
        let old_cursor = self.cursor;
        self.cursor = new_cursor;
        // Bytes that were handed out are about to be listed again (the
        // vacated spans), so the generational known-zero claim goes, as at
        // every such door (`compact_low_to` drops it through
        // `clear_low_free_list`).
        self.forget_known_zero();
        let mut zeroed: Vec<(usize, usize)> = Vec::with_capacity(2);
        if !tail_deferred {
            zeroed.push((new_cursor, old_cursor));
        }
        // The verdict for one block reaching `lo` or above: `None` = drop,
        // `Some(size)` = keep with that size (equal = in place).
        let verdict = |off: usize, size: usize| -> Option<usize> {
            if off >= new_cursor {
                return None;
            }
            let size = size.min(new_cursor - off);
            if size == 0 {
                return None;
            }
            if off < touched.end && touched.start < off + size {
                return None;
            }
            // `compact_low_to`'s test, `off < e && s < off + size` for some
            // span, by binary search: the spans are ascending and disjoint, so
            // their ends ascend too, every span from the first one ending
            // after `off` has `off < e`, and the lowest of them starts first.
            let i = vacated.partition_point(|&(_, e)| e <= off);
            if vacated.get(i).is_some_and(|&(s, _)| s < off + size) {
                return None;
            }
            Some(size)
        };
        let mut readd: Vec<FreeBlock> = Vec::new();
        let mut removed = 0usize;
        for k in 0..self.free_small.len() {
            let bucket = &mut self.free_small[k];
            if bucket.is_empty() {
                continue;
            }
            bucket.retain(|b| {
                if b.offset + b.size <= lo {
                    return true;
                }
                match verdict(b.offset, b.size) {
                    Some(sz) if sz == b.size => true,
                    Some(sz) => {
                        removed += b.size;
                        readd.push(FreeBlock {
                            offset: b.offset,
                            size: sz,
                        });
                        false
                    }
                    None => {
                        removed += b.size;
                        false
                    }
                }
            });
            if bucket.is_empty() {
                mask_clear(&mut self.small_mask, k);
            }
        }
        let mut large_dropped = 0usize;
        self.free_large.retain(|_, bucket| {
            bucket.retain(|b| {
                if b.offset + b.size <= lo {
                    return true;
                }
                match verdict(b.offset, b.size) {
                    Some(sz) if sz == b.size => true,
                    Some(sz) => {
                        removed += b.size;
                        large_dropped += 1;
                        readd.push(FreeBlock {
                            offset: b.offset,
                            size: sz,
                        });
                        false
                    }
                    None => {
                        removed += b.size;
                        large_dropped += 1;
                        false
                    }
                }
            });
            !bucket.is_empty()        });
        self.free_large_blocks -= large_dropped;
        self.free_bytes_total -= removed;
        // Truncated blocks are strictly smaller, so `max_free_upper` (an
        // upper bound) stays valid; the offsets were free blocks already, so
        // the starts bitmap has no bit there.
        for b in readd {
            self.push_block_routed(b);
        }
        // The vacated spans, clamped at the new cursor and merged where they
        // touch.
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(vacated.len().min(8));
        for &(s, e) in vacated {
            let e = e.min(new_cursor);
            if e <= s {
                continue;
            }
            match merged.last_mut() {
                Some(last) if last.1 == s => last.1 = e,
                _ => merged.push((s, e)),
            }
        }
        for (s, e) in merged {
            if zero_vacated {
                self.fill_zero_unless_known(s, e);
                zeroed.push((s, e));
            }
            self.add_free_block(s, e - s);
        }
        self.reset_slide_zero_spans(zeroed);
        self.clear_alloc_anchors();
        Some(reclaimed)
    }

    /// Zero LOW `spans` (offsets) that a relocation vacated but is WITHHOLDING
    /// from the free list (zs s3/cr, `CRATONVM_ZGC_EVAC_QUARANTINE`): the same
    /// zeroing [`Self::compact_low_to`] gives a span it publishes, without the
    /// publication. Each span is clipped at the low cursor. Only the ZGC
    /// evacuating slide calls it; inert for every other owner.
    pub fn zero_withheld_low_spans(&mut self, spans: &[(usize, usize)]) {
        for &(s, e) in spans {
            let e = e.min(self.cursor);
            if e > s {
                self.fill_zero_unless_known(s, e);
            }
        }
    }

    /// Publish the outcome of an IN-PLACE young evacuation (gen r4w5/pinned5,
    /// the pinned young copy in `GenerationalHeap::collect_garbage_inner`):
    /// the low region now holds exactly `live` and nothing else.
    ///
    /// The collector has already
    ///
    /// * left the objects on pinned pages where they were and copied every
    ///   other young survivor into spans that were FREE when the cycle began
    ///   (this arena's free blocks and its un-bumped tail) or into old gen;
    /// * zeroed every byte below the old cursor that is neither live nor was
    ///   already free, so a conservative scanner cannot resurrect a corpse and
    ///   the bump tail reads as zero (the zero-once invariant);
    /// * stamped the sub-`HEADER_SIZE` slivers it could not free-list with the
    ///   8-byte GAP sentinel, so every linear walk strides them.
    ///
    /// `live` is `(offset, size)` of every object that stays, ascending and
    /// disjoint; `free` is the zeroed complement below `new_cursor` that is
    /// big enough to be a block. `high_water` is the highest offset this cycle
    /// wrote (its to-space tail), which a bitmap clear must cover.
    ///
    /// The arena state it sets is the one a non-moving sweep leaves — a cursor,
    /// a low free list, anchors — so every consumer (allocation, the next
    /// object-start walk, the major collector's young walks) already knows it.
    /// `pub` because "everything else below the cursor is dead" is a
    /// claim only the relocator that just moved the survivors can make.
    pub fn rebuild_after_in_place_evacuation(
        &mut self,
        new_cursor: usize,
        live: &[(usize, usize)],
        free: &[(usize, usize)],
        high_water: usize,
    ) {
        debug_assert!(new_cursor <= self.data.len());
        debug_assert!(
            live.windows(2).all(|w| w[0].0 + w[0].1 <= w[1].0),
            "live spans must be ascending and disjoint"
        );
        let old_cursor = self.cursor;
        // Object-start registry: every base this epoch handed out, and every
        // destination this cycle wrote, is cleared, and the survivors' bases
        // are re-added. Cleared over the HIGH-WATER bound for the reason
        // `clear_object_starts_over_bumped` gives.
        if let Some(bits) = self.starts.as_ref() {
            let base = self.data.as_ptr() as usize;
            let hw = self
                .low_high_water()
                .max(high_water)
                .max(new_cursor)
                .min(self.data.len());
            bits.clear_within(&[(base, base + hw)]);
            for &(off, _) in live {
                bits.insert(base + off);
            }
        }
        // The cursor may move either way: down onto the last survivor, or up
        // past copies made into the un-bumped tail. Record the bound a later
        // bitmap pass must reach, exactly as a retraction does.
        self.pre_retract_high = self.pre_retract_high.max(old_cursor).max(high_water);
        self.cursor = new_cursor.min(self.data.len());
        let _ = self.clear_low_free_list();
        for &(off, len) in free {
            if len == 0 || off + len > self.cursor {
                continue;
            }
            self.add_free_block(off, len);
        }
        // Rebuilt by construction, so nothing is pending for the coalescer.
        self.free_pushed = 0;
        // Every recorded object start moved; re-anchor on the survivors.
        self.clear_alloc_anchors();
        for &(off, _) in live {
            self.note_object_start(off);
        }
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
    /// Both are `pub`-only claims, which is why this is not `pub`.
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
    pub fn compact_high_to(&mut self, floor: usize, vacated: &[(usize, usize)]) -> usize {
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

    /// Clear the object-start bitmap over the two BUMPED regions only.
    ///
    /// gengc-round1-alloc, 2026-09-20: the three reset paths each called
    /// `HeapBitmap::clear_all`, which is one store per 512 bytes of CAPACITY
    /// and is paid on every collection whether the arena held ten objects or
    /// ten million -- the same O(capacity) shape `snapshot_words_within` (now `snapshot_word_runs`) and
    /// `clear_within` were added to `heap_bitmap` to remove, applied there and
    /// not here. On a 2 GiB from-space that is 4 MiB of stores per cycle for a
    /// nursery that may have used 64 MiB of it.
    ///
    /// Only the two BUMPED spans can hold a set bit: `hand_out` is the single
    /// door every insert goes through, the low end only hands out below its
    /// cursor and the high end only at or above its own, and the un-bumped
    /// middle has never been handed out. `clear_within` rounds each range out
    /// to whole bitmap words, so the cleared set is a superset of the set
    /// bits, and its `debug_assert_clear_outside` verifies exactly this claim
    /// on every debug run.
    ///
    /// The bounds are [`Self::low_high_water`] and [`Self::high_low_water`],
    /// NOT the raw cursors, and that is the whole of the correctness argument.
    /// A retraction (`retract_cursor_to`, `retract_cursor_into_free_tail`,
    /// `retract_high_cursor_into_free_head`) moves a cursor AWAY from bytes
    /// that were handed out while it was further along, and a pass bounded by
    /// the post-retraction cursor would leave their bits set -- which for an
    /// object-start registry means a later allocation over that span is
    /// accepted at an interior address. Those two accessors exist for exactly
    /// this hazard and document it at length.
    ///
    /// **Call before moving the cursors** -- the bound is the pre-reset one.
    fn clear_object_starts_over_bumped(&self) {
        let Some(bits) = self.starts.as_ref() else {
            return;
        };
        let base = self.data.as_ptr() as usize;
        let cap = self.data.len();
        let low_end = self.low_high_water().min(cap);
        let high_start = self.high_low_water().min(cap);
        bits.clear_within(&[(base, base + low_end), (base + high_start, base + cap)]);
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
        // Door 3 of 3 (see `retract_cursor_to`), and the one door that is
        // NOT refused. A reset is a collector's core operation and refusing it
        // would change behaviour for callers that have nothing to do with
        // relocation; instead the window is dropped, loudly, because a reset
        // invalidates every address in it -- including every to-space copy the
        // open window handed out. An arena that never opened one never reaches
        // this branch.
        self.drop_to_space_window_on_reset("reset");
        // Every base this arena handed out is now invalid: the cursors are back
        // at the ends and the whole span is free. A stale bit here would be
        // accepted at an address the next cycle allocates something else over.
        self.clear_object_starts_over_bumped();
        // stw-residual-close forensics: record the wipe range before zeroing
        // (site 2 = from-space reset). Gated; no-op unless the env is set.
        crate::zero_forensics::record(2, 0, self.data.as_ptr() as usize, self.cursor);
        // Zero out used region for safety (prevents stale data reads).
        // JIT round 23 wave 4, lane L4: and the lazy-dirty set, which may lie
        // ABOVE the cursor (a pinned in-place cycle left it unzeroed), so the
        // bump tail reads zero again.
        let low_end = self
            .cursor
            .max(self.lazy_dirty_end())
            .min(self.data.len());
        self.clear_lazy_dirty();
        self.data.fill_zero(0, low_end);
        // The high region too, or a reset arena hands out bytes that still
        // hold a previous object's header — the exact hazard `reset`'s own
        // contract exists to close.
        {
            let n = self.data.len();
            self.data.fill_zero(self.high_cursor, n);
        }
        self.cursor = 0;
        self.high_cursor = self.data.len();
        self.clear_free_list();
        // Every recorded object start just became zeroed bytes.
        self.clear_alloc_anchors();
    }

    /// [`Self::reset`] with the zeroing handed back to the caller.
    ///
    /// The metadata reset is identical (cursors, free list, anchors); what
    /// this does NOT do is the `memset` of everything below the low cursor
    /// and above the high one. The returned [`DeferredWipe`] names those
    /// extents as OFFSETS, and [`Self::deferred_wipe_spans`] turns them into
    /// absolute spans against the arena's backing at the time it is called —
    /// so a `grow` between the two, which may move the backing (and carries
    /// the committed bytes, stale contents included, with it), is handled by
    /// asking late rather than early.
    ///
    /// The caller owns the contract `reset` used to discharge: until every
    /// returned span is zeroed, nothing may read this arena's bytes as
    /// objects. `GenerationalHeap` keeps the arena INACTIVE (the evacuated
    /// semi-space is the next cycle's to-space, allocated into by no mutator)
    /// and joins the wipe before the next collection touches it.
    pub fn reset_deferring_zero(&mut self) -> DeferredWipe {
        // See `reset`: the window is dropped rather than the reset refused.
        self.drop_to_space_window_on_reset("reset_deferring_zero");
        // Every base this arena handed out is now invalid: the cursors are back
        // at the ends and the whole span is free. A stale bit here would be
        // accepted at an address the next cycle allocates something else over.
        self.clear_object_starts_over_bumped();
        crate::zero_forensics::record(2, 0, self.data.as_ptr() as usize, self.cursor);
        // JIT round 23 wave 4, lane L4: the wipe covers the lazy-dirty set too
        // (see `reset`).
        let wipe = DeferredWipe {
            low_end: self.cursor.max(self.lazy_dirty_end()).min(self.data.len()),
            high_start: self.high_cursor,
        };
        self.clear_lazy_dirty();
        self.cursor = 0;
        self.high_cursor = self.data.len();
        self.clear_free_list();
        self.clear_alloc_anchors();
        wipe
    }

    /// The absolute `(addr, len)` spans a [`DeferredWipe`] must zero, over
    /// the arena's CURRENT backing and only its committed granules (an
    /// uncommitted granule already reads as zero and must not be written).
    pub fn deferred_wipe_spans(&self, wipe: &DeferredWipe) -> Vec<(usize, usize)> {
        let len = self.data.len();
        let mut spans = self.data.committed_spans(0, wipe.low_end.min(len));
        spans.extend(self.data.committed_spans(wipe.high_start.min(len), len));
        spans
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
        // See `reset`: the window is dropped rather than the reset refused.
        self.drop_to_space_window_on_reset("reset_no_zero");
        // Every base this arena handed out is now invalid: the cursors are back
        // at the ends and the whole span is free. A stale bit here would be
        // accepted at an address the next cycle allocates something else over.
        self.clear_object_starts_over_bumped();
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
        let _ = self.note_writable(self.cursor, bytes);
        self.commit_range_for_use(self.cursor, bytes)
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

    // -- the to-space window ------------------------------------------------
    //
    // Added 2026-09-21 (round 20260920, wave 4, lane Q) to unblock
    // `ZgcRealHeap::alloc_in`, which answered `None` because this arena had no
    // allocation path that could promise a destination outside the relocation
    // set. See `ToSpaceWindow` for the invariant and the proof, and
    // `docs/feature-designs/zgc-architecture.md`
    // for the three-line `zgc.rs` binding this exists to serve.
    //
    // ADDITIVE AND INERT. Every function below returns early unless
    // `open_to_space` has been called on this arena; `alloc`, `alloc_high`,
    // `hand_out`, the free lists and both cursors are untouched. G1 and the
    // generational heap never open a window, so for them the whole of this
    // section is one `None` field and three not-taken branches.

    /// Open a **to-space window** at the current low bump cursor, returning the
    /// floor as an arena offset.
    ///
    /// Call at a safepoint, once, before the first
    /// [`Self::alloc_to_space`], and pair it with [`Self::close_to_space`].
    /// Everything relocation may evacuate must already be below the low cursor
    /// when this is called -- which it is by construction, because an object
    /// that exists occupies bytes the cursor has passed.
    ///
    /// Re-opening an already-open window is refused (`None`) rather than
    /// silently re-floored: a second open would RAISE the floor over
    /// destinations the first one had already handed out, which is exactly the
    /// bookkeeping mistake the floor exists to make impossible.
    #[must_use = "a refused open must not be followed by to-space allocation"]
    pub fn open_to_space(&mut self) -> Option<usize> {
        if self.to_space.is_some() {
            // Refused, not asserted. The refusal is part of this function's
            // contract and its `Option` return expresses it, so it is a
            // behaviour a test must be able to exercise -- and a
            // `debug_assert!(false)` on a path the API says may be taken is
            // how this round acquired a panic in every debug build (see the
            // namespace-latch finding in `ROUND-SUMMARY.md`, §1 item 12).
            tracing::error!(
                "Arena::open_to_space: a to-space window is already open at floor {}; \
                 refusing to re-floor it over destinations already handed out",
                self.to_space.map(|w| w.floor).unwrap_or(0),
            );
            return None;
        }
        let floor = self.cursor;
        self.to_space = Some(ToSpaceWindow {
            floor,
            bytes: 0,
            objects: 0,
            refusals: 0,
        });
        tracing::debug!(
            floor = floor,
            headroom = self.low_bump_headroom(),
            "Arena: to-space window open -- bump-only allocation above the floor"
        );
        Some(floor)
    }

    /// Close the window and return what it did. `None` if none was open.
    ///
    /// Closing frees nothing and retracts nothing: the from-space bytes belong
    /// to the relocation driver, which may only declare them dead once every
    /// live object has a published forwarding entry (INVARIANT ZR-1(a),
    /// `crate::zgc::relocate`).
    pub fn close_to_space(&mut self) -> Option<ToSpaceWindow> {
        let w = self.to_space.take();
        if let Some(w) = w {
            tracing::debug!(
                floor = w.floor,
                bytes = w.bytes,
                objects = w.objects,
                refusals = w.refusals,
                "Arena: to-space window closed"
            );
        }
        w
    }

    /// The open window, or `None`.
    pub fn to_space_window(&self) -> Option<ToSpaceWindow> {
        self.to_space
    }

    /// Is a to-space window open on this arena?
    #[inline]
    pub fn to_space_is_open(&self) -> bool {
        self.to_space.is_some()
    }

    /// **Bump-only** allocation of `bytes` at `align`, strictly above the open
    /// window's floor. `None` when no window is open, or when the bump tail
    /// cannot serve the request.
    ///
    /// # This is the function the invariant is about
    ///
    /// It consults **no free list at any tier** -- not the small size classes,
    /// not the span map, not the high end -- and it does not fall back to
    /// [`Self::alloc`]. A to-space allocation that could reach a free list
    /// could be handed a garbage hole inside the very page it is evacuating,
    /// because the sweep that runs immediately before relocation is what put
    /// that hole on the list. See [`ToSpaceWindow`] for the full argument;
    /// the one-sentence version is: **every byte this returns is at or above a
    /// floor that the whole relocation set is strictly below.**
    ///
    /// # Why `None` is safe, and what the caller must do with it
    ///
    /// `None` is the honest report of to-space exhaustion and it fails in the
    /// safe direction by construction:
    /// [`crate::zgc::relocate::ZRelocate::relocate_object`] turns it into
    /// `OutOfToSpace`, `drain_page` fails the page, and ZR-1(a) then forbids
    /// marking that page `Relocated` -- so no byte is declared dead and
    /// nothing is freed under a live pointer. The driver's correct response is
    /// to relocate a smaller set next cycle, never to invent a self-forwarding
    /// rung (`relocate.rs`, "Why there is no self-forwarding fallback").
    ///
    /// The refusal does **not** eat the large-object reserve the way
    /// [`Self::alloc`]'s last-resort arm does. That arm exists because the
    /// alternative is an `OutOfMemoryError`; here the alternative is one page
    /// staying un-evacuated for a cycle, which is recoverable, while spending
    /// the reserve to avoid it is not.
    #[must_use = "a refused to-space allocation must fail the page, not be ignored"]
    pub fn alloc_to_space(&mut self, size: usize, align: usize) -> Option<*mut u8> {
        debug_assert!(align.is_power_of_two(), "alignment must be a power of two");
        // GATE 1: no window, no to-space. This is what makes the whole feature
        // inert for every collector that does not ask for it, and it is also
        // what stops a stray caller turning `alloc_to_space` into a second,
        // floor-less bump path.
        let floor = self.to_space?.floor;

        if size & 7 != 0 {
            warn_unaligned_block("alloc-to-space-size", self.cursor, size);
        }
        // Identical grid rounding to `alloc`: whole 8-byte units, so the bump
        // cursor this path leaves behind is on the object grid the sweep's
        // walk and the mark oracle both assume.
        let size = size.checked_add(7)? & !7;
        let alloc_size = size.checked_add(align - 1).map(|v| v & !(align - 1))?;

        let aligned = self
            .cursor
            .checked_add(align - 1)
            .map(|v| v & !(align - 1))?;
        let end = aligned.checked_add(alloc_size)?;
        // `high_cursor` minus the unclaimed reserve, exactly as `alloc`'s
        // ordinary bump path bounds itself -- and unlike `alloc`, with no
        // last-resort arm below that spends the reserve anyway.
        let ceiling = self
            .high_cursor
            .saturating_sub(self.remaining_high_reserve());
        if end > ceiling {
            if let Some(w) = self.to_space.as_mut() {
                w.refusals += 1;
            }
            return None;
        }
        // GATE 2: THE INVARIANT, checked rather than argued.
        //
        // It cannot fail while `open_to_space` snapshots the cursor, this
        // function only bumps, and the three retraction doors refuse while a
        // window is open. It is checked anyway, in release as well as debug,
        // because the cost is one comparison against a word already in a
        // register and the failure it would catch is a destination inside a
        // from-space page -- i.e. an object evacuated onto itself, discovered
        // later as heap corruption with no surviving evidence of its cause.
        debug_assert!(
            aligned >= floor,
            "to-space allocation at {aligned} is below the window floor {floor}",
        );
        if aligned < floor {
            tracing::error!(
                offset = aligned,
                floor = floor,
                cursor = self.cursor,
                "Arena::alloc_to_space: REFUSING a destination below the to-space floor -- \
                 the low cursor has moved backwards under an open window, which would place \
                 an evacuated object inside the relocation set it was evacuated out of"
            );
            if let Some(w) = self.to_space.as_mut() {
                w.refusals += 1;
            }
            return None;
        }

        // The same door every other allocation goes through: commits the
        // granules, shrinks the pristine window, and records the base in the
        // object-start bitmap when the arena keeps one.
        let ptr = self.hand_out(aligned, end - aligned)?;
        self.cursor = end;
        if let Some(w) = self.to_space.as_mut() {
            w.bytes += end - aligned;
            w.objects += 1;
        }
        Some(ptr)
    }

    /// Refuse a cursor-lowering operation while a to-space window is open,
    /// naming the door. Returns `true` when the caller must do nothing.
    ///
    /// Inert by construction: `to_space` is `None` on every arena that never
    /// called [`Self::open_to_space`], so for G1 and the generational heap
    /// this is a load of a `None` and a not-taken branch on a path that was
    /// already doing a free-list walk.
    #[cold]
    fn report_open_to_space(&self, door: &str) {
        tracing::error!(
            door = door,
            floor = self.to_space.map(|w| w.floor).unwrap_or(0),
            cursor = self.cursor,
            "Arena: refusing to lower the low cursor while a TO-SPACE WINDOW is open -- \
             a retraction under an open window places the next evacuated object over one \
             already copied, and in the limit back inside the relocation set. Close the \
             window (Arena::close_to_space) after the reference rewrite, then reclaim."
        );
        // No `debug_assert!(false)` here, for the same reason as
        // `open_to_space`: refusing is the designed behaviour and the tests
        // drive it, so asserting would make the test suite panic on the path
        // it is meant to verify.
    }

    #[inline]
    fn refuse_under_open_to_space(&self, door: &str) -> bool {
        if self.to_space.is_none() {
            return false;
        }
        self.report_open_to_space(door);
        true
    }

    /// A reset under an open window: drop the window, loudly. See [`Self::reset`].
    #[inline]
    fn drop_to_space_window_on_reset(&mut self, door: &str) {
        if let Some(w) = self.to_space.take() {
            tracing::error!(
                door = door,
                floor = w.floor,
                objects = w.objects,
                bytes = w.bytes,
                "Arena: a TO-SPACE WINDOW was open across a reset -- every copy it handed \
                 out has just been invalidated. The window is dropped; the relocation that \
                 opened it must be treated as failed."
            );
            debug_assert!(false, "Arena::{door} with a to-space window open");
        }
    }

    /// Is `offset` a byte a to-space allocation could ever have returned under
    /// the open window? `false` when no window is open.
    ///
    /// The **test the aliasing property is written against**: a relocation-set
    /// member must answer `false` for every byte it covers, because the set was
    /// chosen below the floor. Exposed so that a test, an assertion or a
    /// diagnostic can ask the question directly instead of re-deriving the
    /// floor comparison at each site -- the round's own record is that a rule
    /// re-derived at the call site is a rule that eventually disagrees with
    /// itself.
    #[inline]
    pub fn is_to_space_offset(&self, offset: usize) -> bool {
        match self.to_space {
            Some(w) => offset >= w.floor && offset < self.cursor,
            None => false,
        }
    }

    /// Lower the low bump cursor to `new_cursor`, decommitting what that
    /// gives back. Returns the bytes reclaimed.
    ///
    /// [`Self::retract_cursor_into_free_tail`] does the same thing by SEARCHING
    /// for a free block that ends at the cursor, which means sorting the whole
    /// low free list. A sweep that walked the address space already knows
    /// where the last live object ends, so it can say so directly and the list
    /// never has to carry -- or be sorted to find -- the tail span at all.
    ///
    /// Refuses to RAISE the cursor: this is a reclaim, and handing out bytes
    /// above the cursor is the bump path.s job.
    pub fn retract_cursor_to(&mut self, new_cursor: usize) -> usize {
        if new_cursor >= self.cursor {
            return 0;
        }
        // gce e1/y: conservatively, a retraction ends the known-zero claim.
        self.forget_known_zero();
        // Door 1 of 3 into `ToSpaceWindow`'s fact (3). Refused rather than
        // trusted not to happen: lowering the cursor under an open window
        // makes the next to-space allocation land over one already handed out,
        // and in the worst case back inside the relocation set.
        if self.refuse_under_open_to_space("retract_cursor_to") {
            return 0;
        }
        let old_cursor = self.cursor;
        let reclaimed = old_cursor - new_cursor;
        self.cursor = new_cursor;
        self.pre_retract_high = self.pre_retract_high.max(old_cursor);
        self.decommit_span(new_cursor, old_cursor, "low-cursor-retract");
        reclaimed
    }

    /// Take back the UNUSED tail `[offset, offset + size)` of a TLAB chunk this
    /// arena handed out, instead of letting the retire bury it under a filler
    /// (gen r4w6/tlab6, `CRATONVM_GEN_TLAB_TAIL_SINK`; see
    /// `GenerationalHeap::return_tlab_tail` for the caller and its gate).
    ///
    /// * A tail that ends exactly at the low bump cursor RETRACTS the cursor
    ///   onto the tail's start ([`TailReturn::Retracted`]): the next carve is
    ///   contiguous with what the thread used, and nothing is listed. Unlike
    ///   [`Self::retract_cursor_to`] nothing is decommitted — this runs on a
    ///   mutator's blocking transition, and the bytes are about to be carved
    ///   again.
    /// * Any other tail of at least `min_free` bytes goes on the low free list
    ///   ([`TailReturn::FreeListed`]), where every linear walker already skips
    ///   it (they merge `free_blocks_sorted`) and the trigger's
    ///   `used - free_list_bytes` stops counting it.
    /// * Everything else is [`TailReturn::Declined`] and the caller installs
    ///   the filler it always did: an off-grid or empty span, one reaching the
    ///   high (large-object) region or past the cursor, a sub-`min_free` tail
    ///   that is not at the cursor, or an open to-space window.
    ///
    /// # What the caller must guarantee
    ///
    /// The span is the reserved tail of a TLAB this arena handed out and that
    /// is retiring now: nothing was ever allocated in it, so it reads zero (it
    /// was zero at the carve — `GenerationalHeap::zero_young_hand_out` — and a
    /// TLAB owner writes only below its cursor). That is what keeps the young
    /// one-zeroing-point invariant ("every byte at or above the cursor reads
    /// zero") true across a retraction, and what makes a listed block as clean
    /// as a swept one.
    ///
    /// # Book-keeping that would otherwise go stale
    ///
    /// The span's first byte may be the chunk's base (a TLAB retired before
    /// its first allocation): that address carries an object-start bit
    /// (`hand_out`) and possibly the bucket's allocation anchor
    /// (`note_object_start`). Both are dropped here; a start bit left above a
    /// retracted cursor would outlive `clear_object_starts_over_bumped`, which
    /// clears only up to the cursor.
    pub fn return_unused_tail(
        &mut self,
        offset: usize,
        size: usize,
        min_free: usize,
    ) -> TailReturn {
        self.return_unused_tail_impl(offset, size, min_free, false)
    }

    /// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`, default ON, `=0`
    /// kills; the generational TLAB tail sink, `GenerationalHeap::return_tlab_tail`):
    /// [`Self::return_unused_tail`] without forgetting the known-zero set. The
    /// caller's guarantee above is that the tail reads zero, so neither arm can
    /// falsify the set: a RETRACTION only drops the claim at and above the new
    /// cursor ([`Self::clip_known_zero_below`]), and a FREE-LISTED tail joins
    /// it ([`Self::note_known_zero_span`]) -- its bytes are as clean as a swept
    /// span. The plain door forgets the whole set at every such return,
    /// pause-time retires included, so the next sweep cannot carry any of it
    /// ([`Self::known_zero_within`]) and every free-list refill after a
    /// mid-cycle return memsets its chunk again.
    pub fn return_unused_tail_keeping_known_zero(
        &mut self,
        offset: usize,
        size: usize,
        min_free: usize,
    ) -> TailReturn {
        self.return_unused_tail_impl(offset, size, min_free, true)
    }

    fn return_unused_tail_impl(
        &mut self,
        offset: usize,
        size: usize,
        min_free: usize,
        keep_known_zero: bool,
    ) -> TailReturn {
        if size == 0 || (offset | size) & 7 != 0 {
            return TailReturn::Declined;
        }
        let Some(end) = offset.checked_add(size) else {
            return TailReturn::Declined;
        };
        if end > self.cursor || self.is_high(offset) || self.to_space.is_some() {
            return TailReturn::Declined;
        }
        let retract = end == self.cursor;
        if !retract && size < min_free {
            return TailReturn::Declined;
        }
        if let Some(bits) = self.starts.as_ref() {
            bits.remove(self.data.as_ptr() as usize + offset);
        }
        self.forget_alloc_anchor(offset);
        // gce e1/y: handed-out bytes come back (either arm). gcj j2/yg2: the
        // keeping door proves them zero instead (see its doc).
        if !keep_known_zero {
            self.forget_known_zero();
        }
        if retract {
            self.cursor = offset;
            // As every retraction does: a bitmap pass bounded by
            // `low_high_water` must still cover the bytes given back.
            self.pre_retract_high = self.pre_retract_high.max(end);
            if keep_known_zero {
                self.clip_known_zero_below(offset);
            }
            TailReturn::Retracted
        } else {
            if keep_known_zero {
                // `add_free_block` forgets the set (its blocks are not proved
                // zero in general); this one is, so the set is kept around it
                // and the tail joins it, as `coalesce_free_list` keeps the set
                // around its re-adds.
                let known_zero = std::mem::take(&mut self.zero_sets.known_zero);
                self.add_free_block(offset, size);
                self.zero_sets.known_zero = known_zero;
                self.note_known_zero_span(offset, size);
            } else {
                self.add_free_block(offset, size);
            }
            TailReturn::FreeListed
        }
    }

    /// Drop the allocation anchor for `offset` if its bucket holds exactly that
    /// offset. A bucket keeps the FIRST base noted in it per epoch, so any other
    /// value there is an older, still-valid base and stays.
    fn forget_alloc_anchor(&mut self, offset: usize) {
        if let Some(slot) = self.alloc_anchors.get_mut(offset >> self.anchor_shift) {
            if *slot == offset + 1 {
                *slot = 0;
            }
        }
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
        // Door 2 of 3 -- see `retract_cursor_to`. ZGC's own sweep calls this
        // twice per cycle, so it is the door most likely to be reached with a
        // window open if a driver ever ordered the two wrongly.
        if self.refuse_under_open_to_space("retract_cursor_into_free_tail") {
            return 0;
        }
        // gce e1/y: conservatively, a retraction ends the known-zero claim.
        self.forget_known_zero();
        // LOW blocks only. Reading the whole list here would take the highest
        // block in the ARENA — which, once the large-object end has been used,
        // is a high-region block that by construction never ends at `cursor`.
        // The retraction would then silently answer 0 for the rest of the run,
        // re-arming the one-way-ratchet failure it exists to prevent.
        // ZT3GN: find the highest block by one scan and unlist only it: the
        // collect-and-sort of the whole list cost 5 ms per young cycle.
        let mut top: Option<(usize, usize, Option<usize>)> = None; // (off, size, small bucket)
        for (k, bucket) in self.free_small.iter().enumerate() {
            for b in bucket {
                if top.is_none_or(|t| b.offset > t.0) {
                    top = Some((b.offset, b.size, Some(k)));
                }
            }
        }
        for bucket in self.free_large.values() {
            for b in bucket {
                if top.is_none_or(|t| b.offset > t.0) {
                    top = Some((b.offset, b.size, None));
                }
            }
        }
        let Some((off, size, small)) = top else {
            return 0;
        };
        if size == 0 || off.saturating_add(size) != self.cursor {
            return 0;
        }
        // Those bytes are becoming un-bumped space; leaving them listed would
        // hand them out twice.
        match small {
            Some(k) => {
                let bucket = &mut self.free_small[k];
                let i = bucket.iter().position(|b| b.offset == off).expect("found above");
                bucket.swap_remove(i);
                if bucket.is_empty() {
                    mask_clear(&mut self.small_mask, k);
                }
            }
            None => {
                let bucket = self.free_large.get_mut(&size).expect("found above");
                let i = bucket.iter().position(|b| b.offset == off).expect("found above");
                bucket.swap_remove(i);
                if bucket.is_empty() {
                    self.free_large.remove(&size);
                }
                self.free_large_blocks -= 1;
            }
        }
        self.free_bytes_total -= size;
        let old_cursor = self.cursor;
        self.cursor = off;
        self.pre_retract_high = self.pre_retract_high.max(old_cursor);
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
        self.decommit_span(off, old_cursor, "free-tail-retract");
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
        self.decommit_span(lo, hi, "unbumped-middle")
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
    /// back. The bytes come with an obligation, and it is worth stating
    /// exactly: every path that ALLOCATES one out again goes through
    /// [`Self::hand_out`], which commits before it returns a pointer, and a
    /// re-committed granule reads as zero -- which is what a caller of a reused
    /// block is entitled to and what `Arena::alloc`'s consumers already zero
    /// for themselves.
    ///
    /// **Allocation is not the only path that writes here.** A relocating
    /// collector's slide picks a destination inside free space arithmetically
    /// and `memmove`s into it without asking the allocator for anything, so it
    /// never reaches `hand_out`. That is what [`Self::commit_for_relocation`]
    /// exists for, and both of `ZgcRealHeap`'s slides call it. This doc used to
    /// claim `hand_out` was the only door, and the two slides were writing into
    /// granules this method had already returned to the OS.
    ///
    /// WHOLE granules only, rounded INWARD: a block's ends usually share a
    /// granule with a live object, and rounding outward would take it too --
    /// and taking it is not survivable, because a released granule FAULTS on
    /// touch. `reservation::platform_decommit` is `madvise(MADV_DONTNEED)`
    /// followed by `mprotect(PROT_NONE)` on Linux, a `MAP_FIXED` `PROT_NONE`
    /// remap on macOS, and `VirtualFree(MEM_DECOMMIT)` on Windows; none of them
    /// leaves a page that reads back as zero until it is committed again.
    ///
    /// (This paragraph used to say the opposite -- "a decommitted page reads as
    /// zero rather than faulting" -- and that belief is how the slides came to
    /// write into released granules at all. See
    /// `Self::commit_for_relocation`.)
    ///
    /// **Call at a safepoint**, after the sweep has coalesced: the lists are
    /// then a handful of maximal spans rather than one entry per dead object,
    /// so this is a walk of tens rather than millions.
    ///
    /// Returns the bytes released.
    pub fn decommit_free_blocks(&mut self) -> usize {
        self.decommit_free_blocks_above(0)
    }

    /// [`Self::decommit_unbumped_middle`] then [`Self::decommit_free_blocks`],
    /// neither of them releasing anything below the arena offset `floor`
    /// (gcd d1/d, 2026-09-27).
    ///
    /// The floor-aware give-back
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// asked for: the generational heap keeps an evacuated semi-space's `-Xms`
    /// prefix committed (`GenerationalHeap::uncommit_evacuated_young`), and
    /// without a floor here it released that prefix every young pause and
    /// committed it again straight after — a decommit and a commit syscall
    /// per granule per pause, and a page fault per page on the next pass
    /// over it. Each span is clipped at `floor` before
    /// [`Self::decommit_span`]'s inward rounding, so a granule that straddles
    /// the floor stays committed; nothing at or above the floor is kept that
    /// the unfloored give-back would have released, except that straddling
    /// granule. `floor == 0` is the two calls exactly (same spans, same order,
    /// same sites).
    pub fn decommit_unused_above(&mut self, floor: usize) -> usize {
        // A ZGC policy defers cursor retraction give-back until this explicit
        // safepoint window, after forwarding records have been retired. This
        // keeps every actual unmap adjacent to JIT raw-load-bound withdrawal.
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.release_window = true;
        }
        let (lo, hi) = (self.cursor.max(floor), self.high_cursor);
        let middle = self.decommit_span(lo, hi, "unbumped-middle");
        let released = middle + self.decommit_free_blocks_above(floor);
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.release_window = false;
        }
        released
    }

    /// [`Self::decommit_free_blocks`], clipped at `floor` (see
    /// [`Self::decommit_unused_above`]).
    fn decommit_free_blocks_above(&mut self, floor: usize) -> usize {
        let mut released = 0usize;
        // zr z3/fp: only a block of at least one granule can hold a whole one,
        // and every pass below rounds inward to whole granules, so the
        // small-bucket tier and sub-granule spans release nothing whatever is
        // done with them. Walking only the span tier's big keys gives the same
        // spans in the same (ascending) order as sorting the whole list did --
        // which on a mosaic heap was hundreds of thousands of holes sorted in
        // the pause for nothing.
        let granule = crate::reservation::GRANULE;
        let mut low: Vec<(usize, usize)> = self
            .free_large
            .range(granule..)
            .flat_map(|(_, v)| v.iter().map(|b| (b.offset, b.size)))
            .collect();
        sort_by_offset(&mut low);
        for (off, size) in low {
            released += self.decommit_span(off.max(floor), off + size, "free-list-low");
        }
        let high: Vec<(usize, usize)> = self.free_high.iter().map(|b| (b.offset, b.size)).collect();
        for (off, size) in high {
            released += self.decommit_span(off.max(floor), off + size, "free-list-high");
        }
        released
    }

    /// Start the `ZUncommitDelay` clock on every committed granule that is
    /// unused NOW -- the un-bumped middle and the whole granules inside free
    /// blocks -- without releasing anything (zr z3/fp). Returns how many
    /// granules it stamped (granules already stamped keep their older time).
    ///
    /// # Why the clock has to start here
    ///
    /// The only other stamping site is the give-back pass at the START of a
    /// collection ([`Self::decommit_unused_above`]), which both stamps and
    /// releases. A granule the sweep freed at the end of cycle N was therefore
    /// first seen idle at the start of cycle N+1, and released at the first
    /// collection that started a whole delay AFTER that -- up to two collection
    /// intervals later than HotSpot's rule ("uncommit memory that has been
    /// unused for `ZUncommitDelay` seconds"). Stamping at the end of the sweep
    /// makes the delay run from the moment the memory became unused; the
    /// release itself stays at the safepoint window, where it is sound.
    ///
    /// Only blocks of at least one granule can hold a whole one, so the small
    /// free-list tier (sub-granule by construction) is not walked: the cost is
    /// the big low blocks, the large-object end's blocks, and the middle.
    /// A no-op without a ZGC policy, with uncommit off, or with a zero delay
    /// (then every pass releases at once and there is no clock to start).
    pub fn stamp_idle_unused(&mut self) -> usize {
        let granule = crate::reservation::GRANULE;
        match self.uncommit_policy.as_ref() {
            Some(p) if p.enabled && !p.delay.is_zero() && !p.idle_since.is_empty() => {}
            _ => return 0,
        }
        let spans = self.idle_candidate_spans();
        if spans.is_empty() {
            return 0;
        }
        let now = std::time::Instant::now();
        let data = &self.data;
        let Some(policy) = self.uncommit_policy.as_mut() else {
            return 0;
        };
        let mut stamped = 0usize;
        for (lo, hi) in spans {
            let end = (hi / granule).min(policy.idle_since.len());
            for g in lo.div_ceil(granule)..end {
                // zr z4/un: a reset granule has nothing left to give back.
                if !data.is_committed_at(g * granule)
                    || policy.reset_idle.get(g).copied().unwrap_or(false)
                {
                    continue;
                }
                let slot = &mut policy.idle_since[g];
                if slot.is_none() {
                    *slot = Some(now);
                    stamped += 1;
                }
            }
        }
        stamped
    }

    /// The spans that can hold a whole UNUSED granule: the un-bumped middle,
    /// then every free block of at least one granule at the low end (the span
    /// tier, ascending by size bucket) and at the high end. Shared by
    /// [`Self::stamp_idle_unused`] and [`Self::uncommit_idle_pass`].
    fn idle_candidate_spans(&self) -> Vec<(usize, usize)> {
        let granule = crate::reservation::GRANULE;
        let mut spans: Vec<(usize, usize)> = Vec::new();
        if self.high_cursor > self.cursor {
            spans.push((self.cursor, self.high_cursor));
        }
        for blocks in self.free_large.range(granule..).map(|(_, v)| v) {
            spans.extend(blocks.iter().map(|b| (b.offset, b.offset + b.size)));
        }
        spans.extend(
            self.free_high
                .iter()
                .filter(|b| b.size >= granule)
                .map(|b| (b.offset, b.offset + b.size)),
        );
        spans
    }

    /// zr z4/un: the UNCOMMITTER's pass -- give back the committed granules
    /// that have been unused for the policy's delay, OUTSIDE a collection
    /// (HotSpot's `ZUncommitter`; before this an idle heap gave nothing back,
    /// because the only give-back ran at the start of a collection).
    ///
    /// Same candidate spans as [`Self::stamp_idle_unused`], same `-Xms` floor
    /// (`release_unused_span`), same clock (a granule first seen unused is
    /// stamped `now`; one stamped by the sweep keeps its older time). What
    /// makes it safe to run while mutators run:
    ///
    /// * **This lock.** Every path that takes a free block or moves a cursor
    ///   holds it, and every path that writes into free space commits first
    ///   (`commit_range_for_use`), which clears the granule's clock. A TLAB
    ///   chunk, a pinned or a huge object is a hand-out, never a free block,
    ///   so it is never a candidate.
    /// * **Readable mode only.** Refused (no work) unless the arena resets
    ///   rather than decommits: a reset granule stays mapped and reads zero,
    ///   so a stale reader never faults. (On Windows the reset is a decommit
    ///   plus a re-commit; a re-commit the OS refuses is a real hole, counted
    ///   in `decommitted`, and the caller must then withdraw any raw-load
    ///   bound.)
    /// * **`exclude`** (arena offsets) is never touched: the owner passes the
    ///   span holding the last slide's forwarding records, which the load
    ///   barrier reads without this lock.
    /// * **The owner excludes collections** (a slide writes into free space
    ///   arithmetically, under its own pause).
    ///
    /// At most `max_granules` are released per call, so the lock is held for
    /// a bounded time; `budget_hit` says there is more.
    pub fn uncommit_idle_pass(
        &mut self,
        now: std::time::Instant,
        exclude: std::ops::Range<usize>,
        max_granules: usize,
    ) -> ArenaIdlePass {
        let mut out = ArenaIdlePass::default();
        if !self.readable_give_back {
            return out;
        }
        let granule = crate::reservation::GRANULE;
        let (delay, clocked) = match self.uncommit_policy.as_ref() {
            Some(p) if p.enabled => (p.delay, !p.idle_since.is_empty()),
            _ => return out,
        };
        let total = self.capacity().div_ceil(granule);
        let mut acted = 0usize;
        'spans: for (lo, hi) in self.idle_candidate_spans() {
            for g in lo.div_ceil(granule)..(hi / granule).min(total) {
                let (g_lo, g_hi) = (g * granule, (g + 1) * granule);
                if g_lo < exclude.end && g_hi > exclude.start {
                    continue;
                }
                if !self.data.is_committed_at(g_lo) || self.is_reset_idle(g) {
                    continue;
                }
                if clocked {
                    let policy = self.uncommit_policy.as_mut().unwrap();
                    let since = *policy.idle_since[g].get_or_insert(now);
                    // zt t10/rl3 (t10/x7 R2): a delay past the clock's range
                    // (`-XX:ZUncommitDelay=18446744073709551615`, which
                    // HotSpot 25 boots with) is never due; `since + delay`
                    // panicked the uncommitter thread.
                    let Some(due) = since.checked_add(delay) else {
                        out.pending += 1;
                        continue;
                    };
                    if now < due {
                        out.pending += 1;
                        out.next_due = Some(out.next_due.map_or(due, |d| d.min(due)));
                        continue;
                    }
                }
                if acted >= max_granules {
                    out.budget_hit = true;
                    break 'spans;
                }
                let reset_before = self.bytes_reset;
                let n = self.release_unused_span(g_lo, g_hi, "uncommitter");
                let reset = self.bytes_reset - reset_before;
                if n == 0 && reset == 0 {
                    // The `-Xms` floor refused it, and it refuses every later
                    // granule too. Keep the stamp: the granule has been idle
                    // that long whatever the floor says.
                    break 'spans;
                }
                out.decommitted += n;
                out.reset += reset;
                acted += 1;
                if clocked {
                    self.uncommit_policy.as_mut().unwrap().idle_since[g] = None;
                }
            }
        }
        out
    }

    /// Bytes of this arena that no writer has ever been handed (the pristine
    /// window, see [`Self::pristine_lo`]). `capacity() - pristine_bytes()` is
    /// an upper bound on the arena bytes that have ever been resident (zr
    /// z3/fp, the `zgc-footprint` summary line).
    pub fn pristine_bytes(&self) -> usize {
        self.pristine_hi.saturating_sub(self.pristine_lo)
    }

    /// Bytes of BOOKKEEPING this arena holds beside the heap bytes themselves,
    /// by capacity rather than length (what the process holds): the free-list
    /// tiers, the allocation anchors, the uncommit clock and the known-zero
    /// span sets (zr z3/fp). Walks the 512 small buckets and the span tier's
    /// keys; diagnostics only.
    pub fn bookkeeping_bytes(&self) -> usize {
        let block = std::mem::size_of::<FreeBlock>();
        let small: usize = self
            .free_small
            .iter()
            .map(|v| v.capacity() * block)
            .sum::<usize>()
            + self.free_small.capacity() * std::mem::size_of::<Vec<FreeBlock>>();
        let large: usize = self
            .free_large
            .values()
            .map(|v| v.capacity() * block + std::mem::size_of::<(usize, Vec<FreeBlock>)>())
            .sum();
        let high = self.free_high.capacity() * block;
        let anchors = self.alloc_anchors.capacity() * std::mem::size_of::<usize>();
        let clock = self.uncommit_policy.as_ref().map_or(0, |p| {
            p.idle_since.capacity() * std::mem::size_of::<Option<std::time::Instant>>()
        });
        let zero = (self.zero_sets.known_zero.capacity() + self.zero_sets.slide.spans.capacity())
            * std::mem::size_of::<(usize, usize)>()
            + self.zero_sets.slide.stamps.capacity() * std::mem::size_of::<usize>();
        small + large + high + anchors + clock + zero
    }

    /// Make `[offset, offset + len)` writable for a caller that is about to
    /// write arena bytes WITHOUT going through [`Self::hand_out`].
    ///
    /// # The one such caller, and why it needs its own door
    ///
    /// [`Self::decommit_free_blocks`] gives free-list granules back to the OS
    /// on a stated promise: "every path that hands one out again goes through
    /// `hand_out`, which commits before it returns a pointer". A relocating
    /// collector's SLIDE is a path that hands one out and does not — it picks
    /// a destination inside free space arithmetically and `memmove`s into it,
    /// because the whole point of a slide is that it never asks the allocator
    /// for anything. `ZgcRealHeap`'s two slides (`compact_high_region` and the
    /// low slide in `relocate_stw`) are exactly that shape.
    ///
    /// Measured 2026-09-04: with the give-back active, the first high slide
    /// after one packs a survivor against `capacity` and faults inside
    /// `__memcpy_avx512_unaligned_erms` on a `PROT_NONE` granule — reproduced
    /// 3/3 in 7 s on `org.h2.test.store.TestRandomMapOps`, and 0/3 with
    /// `CRATONVM_GC_RESERVE=0`, which is the whole of the reserve/commit
    /// store's involvement.
    ///
    /// Returns `false` when the OS refuses, and the caller must then leave the
    /// object where it is: a slide that cannot have its destination is a
    /// missed compaction, never a reason to write anyway.
    #[must_use = "a refused commit means the object must not be moved"]
    pub fn commit_for_relocation(&mut self, offset: usize, len: usize) -> bool {
        let _ = self.note_writable(offset, len);
        self.commit_range_for_use(offset, len)
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

    /// This arena's per-granule commit bitmap, readable without the arena
    /// lock. `None` on the wholly-committed fallback store.
    ///
    /// See [`crate::reservation::HeapStore::commit_bits`] for what a reader is
    /// buying: the right to ask "is this address backed?" before dereferencing
    /// it, which `[base, base + capacity)` alone cannot answer.
    pub fn commit_bits(&self) -> Option<std::sync::Arc<[std::sync::atomic::AtomicU64]>> {
        self.data.commit_bits()
    }

    /// zr z7/rs: [`crate::reservation::HeapStore::advise_no_huge_pages`] on
    /// this arena's backing store. Only ZGC calls it
    /// (`CRATONVM_ZGC_ARENA_NO_THP`); every other arena keeps its mapping as
    /// it was.
    pub fn advise_no_huge_pages(&self) -> bool {
        self.data.advise_no_huge_pages()
    }

    /// zt t24/hunt7: [`crate::reservation::HeapStore::advise_huge_pages`] on
    /// this arena's backing store, for HotSpot's
    /// `-XX:+UseTransparentHugePages` (`ZgcRealHeap::advise_transparent_huge_pages`;
    /// zki: the generational heap's semi-spaces too,
    /// `GenerationalHeap::advise_transparent_huge_pages`).
    pub fn advise_huge_pages(&self) -> bool {
        self.data.advise_huge_pages()
    }

    /// Clip absolute `(addr, len)` spans to the parts of them that are still
    /// COMMITTED, dropping anything that is not.
    ///
    /// # Why a caller needs this
    ///
    /// The deferred young wipe and the give-back are two ways of saying the
    /// same thing to the same bytes, and running both is a race rather than
    /// belt-and-braces: the wipe thread `memset`s spans of this arena, and a
    /// granule the give-back has released FAULTS on write.
    ///
    /// They are not interchangeable either. A released granule comes back from
    /// the OS zeroed, so it needs no wipe — but `hand_out` does not zero, and
    /// the give-back only releases WHOLE granules rounded inward, so whatever
    /// sits in the partial granules at a span's edges is still the previous
    /// cycle's object bytes and still has to be written over.
    ///
    /// So: give back first, then wipe what is left. This is "what is left".
    /// On the wholly-committed fallback store (no reservation, nothing ever
    /// released) every span is returned unchanged, which is exactly the
    /// pre-give-back behaviour.
    pub fn retain_committed_spans(&self, spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
        let Some(bits) = self.data.commit_bits() else {
            // No per-granule tracking: the store is wholly committed and
            // nothing can have been released from it.
            return spans;
        };
        let base = self.data.as_ptr() as usize;
        let g = crate::reservation::GRANULE;
        let mut out = Vec::with_capacity(spans.len());
        for (addr, len) in spans {
            if len == 0 || addr < base {
                continue;
            }
            let start = addr - base;
            let end = start + len;
            // Walk granule by granule and coalesce the committed runs, so a
            // span straddling a released granule yields its two live halves
            // rather than being dropped whole.
            let mut cur: Option<(usize, usize)> = None;
            let mut off = start;
            while off < end {
                let gran = off / g;
                let gran_end = ((gran + 1) * g).min(end);
                if crate::reservation::granule_committed(&bits, gran) {
                    match &mut cur {
                        Some((_, e)) => *e = gran_end,
                        None => cur = Some((off, gran_end)),
                    }
                } else if let Some((s, e)) = cur.take() {
                    out.push((base + s, e - s));
                }
                off = gran_end;
            }
            if let Some((s, e)) = cur.take() {
                out.push((base + s, e - s));
            }
        }
        out
    }

    /// Commit the initial ZGC budget across both allocation ends. Reserve
    /// one eighth for high allocations (at least one granule), matching the
    /// arena's two-ended geometry without moving either cursor or resizing.
    pub fn commit_initial_zgc(&mut self, bytes: usize) -> usize {
        use crate::reservation::GRANULE;
        let n = bytes.min(self.capacity());
        if n == 0 {
            return 0;
        }
        if n <= GRANULE || self.capacity() % GRANULE != 0 {
            return self.commit_initial_prefix(n);
        }
        let rounded = n.div_ceil(GRANULE) * GRANULE;
        let high = (rounded / 8).max(GRANULE).min(rounded - GRANULE) / GRANULE * GRANULE;
        let low = rounded - high;
        let low_committed = self.commit_initial_prefix(low);
        if !self.commit_range_for_use(self.capacity() - high, high) {
            tracing::warn!("ZGC: the OS refused the high-end startup commit");
            return low_committed;
        }
        (low_committed + high).min(n)
    }

    /// `-Xms`: commit the first `bytes` of this arena NOW, and report how many
    /// were committed.
    ///
    /// This is the prefix part of startup commitment for an arena. It is a
    /// `HeapStore::commit_range(0, n)` and **nothing else**: no resize, no
    /// cursor move, no free-list change, no zeroing. An arena that has had its
    /// prefix committed is indistinguishable from one that has not, except in
    /// `committed_bytes()` and in the process's commit charge — which is the
    /// point.
    ///
    /// # Why commit and not reserve
    ///
    /// `-Xmx` sizes the RESERVATION and `-Xms` is what the VM commits at
    /// startup; they are separate numbers with separate meanings, and making
    /// `-Xms` size the arena would make `-Xms512m -Xmx8g` a heap that can never
    /// reach `-Xmx`
    /// (`docs/feature-designs/generative-gc-architecture.md`
    /// forbids exactly that "alias for `-Xmx`" spelling). Committing a prefix
    /// changes neither the envelope nor the capacity, so every consumer that
    /// captured `base`/`capacity` once — ZGC's `conservative_addr_span`, its
    /// object-start bitmap grid, the JIT read-bounds publication — sees the
    /// same two words it did before.
    ///
    /// # Return value
    ///
    /// The bytes committed: `bytes` clamped to [`Self::capacity`], or `0` if
    /// the OS refused. A refusal is reported rather than panicked on, because
    /// an `-Xms` the kernel will not honour is a reason to start with a smaller
    /// committed prefix, never a reason not to start.
    ///
    /// On the wholly-committed fallback store (`CRATONVM_GC_RESERVE=0`, and
    /// every platform without a reservation) the whole arena is already
    /// committed, so this reports the clamped request and does nothing — which
    /// is accurate: those bytes ARE committed.
    pub fn commit_initial_prefix(&mut self, bytes: usize) -> usize {
        let n = bytes.min(self.capacity());
        if n == 0 {
            return 0;
        }
        if self.commit_range_for_use(0, n) {
            n
        } else {
            tracing::warn!(
                target: "cratonvm::gc",
                requested = n,
                capacity = self.capacity(),
                "-Xms: the OS refused the startup commit; the heap will commit on demand instead",
            );
            0
        }
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
        // gce e1/y: the backing may move; no claim about it survives.
        self.forget_known_zero();
        // r23 w4 review (R-L4z): the lazy-dirty mirror is indexed off the old
        // base. Clear it (a reader that loaded its published pointer before
        // the republish then accepts, the pre-mirror answer) and drop it; the
        // set itself is offsets and stays. The next note recreates it.
        if let Some(m) = self.zero_sets.lazy_dirty.mirror.take() {
            m.clear_all();
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
        // gci i10/gy: the capacity the store ACTUALLY has now. `grow_to`
        // keeps the old reservation when the OS refuses a bigger one (its
        // `None` arm), so `data.len()` can still be `old_capacity`. Every
        // field below is sized from this, never from the request: setting
        // `high_cursor` past `data.len()` made `used()` (`len - high_cursor`)
        // underflow and `remaining()` promise bytes the arena does not have.
        let new_capacity = self.data.len();
        if new_capacity <= old_capacity {
            return old_base;
        }
        // The high end anchors at capacity, so growing moves it. Safe for the
        // same reason the assert above allows the grow at all: `cursor == 0`
        // means the arena is empty, so there is nothing at the old top to
        // strand. (`assert_eq!(self.cursor, 0)` fires above otherwise.)
        debug_assert_eq!(
            self.high_cursor, old_capacity,
            "an empty arena must have an un-bumped high region",
        );
        self.high_cursor = new_capacity;
        // The uncommit policy's per-granule idle clocks are indexed by
        // capacity too (`decommit_span_at` indexes them directly).
        if let Some(policy) = self.uncommit_policy.as_mut() {
            if !policy.idle_since.is_empty() {
                policy
                    .idle_since
                    .resize(new_capacity.div_ceil(crate::reservation::GRANULE), None);
            }
            if policy.reset_accounting {
                policy
                    .reset_idle
                    .resize(new_capacity.div_ceil(crate::reservation::GRANULE), false);
            }
        }
        // The bucket table is indexed by capacity; `grow` is the only other
        // place `data.len()` moves. `cursor == 0` was just asserted, so there
        // is nothing recorded to preserve.
        self.rearm_alloc_anchors();
        // The object-start bitmap is indexed off `base` and bounded by `span`,
        // and `grow_to` has just changed both. Leaving the old one in place
        // would have it answering for an address space that no longer exists --
        // in the ACCEPT direction, which means handing back a `Some(true)` for
        // an address this arena never allocated. Rebuild it over the new
        // geometry; `cursor == 0` is asserted above, so there is no live base to
        // preserve and a zeroed bitmap is exactly right.
        //
        // Only if it was armed: an unarmed arena stays unarmed, so `grow` never
        // charges an owner for a table it did not ask for.
        if self.starts.is_some() {
            self.arm_object_starts();
        }
        if gc_flags().dbg_youngstate {
            // t1 wave 8 lane CARDTBL: `in_place` says whether the base held
            // (a headroom store, `Arena::with_headroom`), the whole-heap
            // hull's engagement witness for young growth.
            eprintln!(
                "[youngstate] arena-grow {old_capacity} -> {new_capacity} bytes in_place={}",
                std::ptr::eq(self.data.as_ptr(), old_base)
            );
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
mod prefer_bump_tests;

#[cfg(test)]
mod tests;

/// gcd d1/d (2026-09-27): the floor-aware give-back behind
/// `GenerationalHeap::uncommit_evacuated_young`'s `-Xms` floor.
#[cfg(test)]
mod gcd_d1d_floor_tests;

/// gce e1/y: the known-zero set of the LOW free list (item 2 of
/// `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`).
#[cfg(test)]
mod e1y_known_zero_tests;

/// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): the carry primitives.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j2yg2_`.
#[cfg(test)]
mod gcj_j2yg2_known_zero_carry_tests;

#[cfg(test)]
mod zgc_uncommit_policy_tests;

#[cfg(test)]
mod zgc_readable_floor_tests;

/// gci i10/gy (2026-10-13): `Arena::grow` must size itself from the capacity
/// the store reached, not from the request.
#[cfg(test)]
mod gci_i10gy_grow_tests;

#[cfg(test)]
mod zr4_un_tests;

/// gci i22/dr (P2/yy): [`Arena::take_smallest_block_at_least`], the best-fit-by-size
/// TLAB refill primitive.
#[cfg(test)]
mod gci_i22dr_smallest_whole_block_tests;

#[cfg(test)]
mod zs10sp_min_free_span_tests;

#[cfg(test)]
mod zt1sp_min_free_span_review_tests;

#[cfg(test)]
mod zt2al_refused_fit_tests;

#[cfg(test)]
mod zt2al_zero_descending_tests;

/// JIT round 23 wave 4, lane L4: the lazy-dirty set (`ZeroSets::lazy_dirty`).
#[cfg(test)]
mod r23_w4_lazy_dirty_tests;

/// JIT round 23 wave 4c, lane L4: the allocator-anchor certificate the lazy
/// young object-start grid relies on (`Arena::forget_alloc_anchors_inside`).
#[cfg(test)]
mod r23_w4c_anchor_certificate_tests;

/// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`):
/// the plain-store mirror ends every operation with the locked arm's words.
#[cfg(test)]
mod r23_w8_lazy_mirror_plain_store_tests;

/// t1 wave 8 lane CARDTBL (`CRATONVM_GC_WHOLE_HEAP_YOUNG_HEADROOM`): an arena
/// built with headroom grows in place, inside its reservation.
#[cfg(test)]
mod t1w8_cardtbl_arena_tests;
