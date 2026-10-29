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
        cratonvm_types::flags::runtime_var("CRATONVM_GC_OBJECT_STARTS").as_deref(),
        Ok(v) if !v.is_empty() && v != "0" && v != "false" && v != "off" && v != "no"
    )
}

/// JIT round 23 wave 8, lane W8-L3: `CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`
/// (default ON, `0` kills). The lazy-dirty mirror (`LazyDirtySet::mirror`) is
/// written with plain Release stores instead of one locked `fetch_or` /
/// `fetch_and` per 64-bit word: in the non-moving sweep's publication of every
/// reclaimed byte and in every young TLAB refill's clear. The sampler on
/// `R21AllocLean lean-static-long16` (Generational, n23-w8c) put the two at
/// about 7% and 6% of the allocating thread. Read when an arena creates its
/// mirror (per arena, no process latch).
fn gen_lazy_mirror_plain_stores_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES")
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
pub(crate) struct ArenaIdlePass {
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
    spans: BTreeMap<usize, usize>,
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
        self.spans.last_key_value().map_or(0, |(_, &e)| e)
    }

    /// Add `[lo, hi)`, joining every span it overlaps or touches.
    fn insert(&mut self, lo: usize, hi: usize) {
        if hi <= lo {
            return;
        }
        let (mut lo, mut hi) = (lo, hi);
        if let Some((&s, &e)) = self.spans.range(..=lo).next_back() {
            if e >= lo {
                lo = s;
                hi = hi.max(e);
                self.spans.remove(&s);
            }
        }
        while let Some((&s, &e)) = self.spans.range(lo..=hi).next() {
            hi = hi.max(e);
            self.spans.remove(&s);
        }
        self.spans.insert(lo, hi);
    }

    /// Remove `[a, b)`. Returns the overlapped extent clipped to `[a, b)`
    /// (lowest to highest removed byte), `None` when nothing overlapped.
    fn remove(&mut self, a: usize, b: usize) -> Option<(usize, usize)> {
        if b <= a {
            return None;
        }
        let mut lo_hit = usize::MAX;
        let mut hi_hit = 0usize;
        let mut keep_lo: Option<(usize, usize)> = None;
        let mut keep_hi: Option<(usize, usize)> = None;
        if let Some((&s, &e)) = self.spans.range(..a).next_back() {
            if e > a {
                self.spans.remove(&s);
                keep_lo = Some((s, a));
                lo_hit = a;
                hi_hit = e.min(b);
                if e > b {
                    keep_hi = Some((b, e));
                }
            }
        }
        while let Some((&s, &e)) = self.spans.range(a..b).next() {
            self.spans.remove(&s);
            lo_hit = lo_hit.min(s);
            hi_hit = hi_hit.max(e.min(b));
            if e > b {
                keep_hi = Some((b, e));
            }
        }
        if let Some((s, e)) = keep_lo {
            self.spans.insert(s, e);
        }
        if let Some((s, e)) = keep_hi {
            self.spans.insert(s, e);
        }
        (lo_hit < hi_hit).then_some((lo_hit, hi_hit))
    }

    /// The spans overlapping `[a, b)`, unclipped, ascending.
    fn overlapping(&self, a: usize, b: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        if b <= a {
            return out;
        }
        if let Some((&s, &e)) = self.spans.range(..a).next_back() {
            if e > a {
                out.push((s, e));
            }
        }
        out.extend(self.spans.range(a..b).map(|(&s, &e)| (s, e)));
        out
    }

    /// `(offset, len)` pairs, ascending (tests and diagnostics).
    #[cfg(test)]
    fn as_vec(&self) -> Vec<(usize, usize)> {
        self.spans.iter().map(|(&s, &e)| (s, e - s)).collect()
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
}

/// What [`Arena::take_last_hand_out_zero_plan`] says about the most recent
/// hand-out: the absolute ranges inside it that already read zero (ascending,
/// disjoint), the part of those that is the pristine window's, and the
/// absolute addresses of the forwarding records inside them that do NOT read
/// zero (each `cratonvm_types::MIN_OBJECT_SIZE` bytes).
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ZeroPlan {
    pub(crate) known_zero: Vec<(usize, usize)>,
    pub(crate) pristine_bytes: usize,
    pub(crate) slide_bytes: usize,
    pub(crate) dirty_stamps: Vec<usize>,
}

impl ZeroPlan {
    /// Zero `[ptr, ptr + len)` except what `plan` proves already reads zero
    /// (and zero the dirty stamps inside that). `None` zeroes it all.
    ///
    /// # Safety
    /// `[ptr, ptr + len)` must be writable and exclusively the caller's, and
    /// `plan` must be the arena's answer for exactly that hand-out.
    pub(crate) unsafe fn zero(plan: Option<&ZeroPlan>, ptr: *mut u8, len: usize) {
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
    pub(crate) unsafe fn zero_descending(
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
        /// Zero `[lo, hi)` in `block`-byte pieces, highest piece first.
        ///
        /// # Safety
        /// `[lo, hi)` is writable and the caller's.
        unsafe fn down(lo: usize, hi: usize, block: usize) {
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
            unsafe { down(khi, top, block) };
            top = klo;
        }
        // SAFETY: the lowest gap, inside the hand-out.
        unsafe { down(start, top, block) };
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
    pub(crate) fn note_lazy_dirty(&mut self, spans: &[(usize, usize)]) {
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
            self.zero_sets.lazy_dirty.plain_stores = gen_lazy_mirror_plain_stores_enabled();
        }
        let base = self.data.as_ptr() as usize;
        let set = &mut self.zero_sets.lazy_dirty;
        let plain = set.plain_stores;
        for &(o, l) in spans {
            if l == 0 {
                continue;
            }
            set.insert(o, o + l);
            if let Some(m) = set.mirror.as_deref() {
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
    pub(crate) fn take_lazy_dirty(&mut self, offset: usize, len: usize) -> Option<(usize, usize)> {
        self.take_lazy_dirty_counted(offset, len).map(|(hit, _)| hit)
    }

    /// t1 lane F (`gcj-j2yg2`, re-measure): [`Self::take_lazy_dirty`], plus
    /// the EXACT number of dirty bytes inside the hand-out (the sum of the
    /// overlapped spans clipped to `[offset, offset + len)`; at most `len`).
    /// `len - dirty` is the part of the hand-out the caller's full memset
    /// writes although the set did not name it -- zero already wherever the
    /// free list's other doors kept their promise. A census only: the caller
    /// still memsets the whole hand-out.
    pub(crate) fn take_lazy_dirty_counted(
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
    /// gcj yg3-residuals: TOUCHING spans are taken out as one run. The pinned
    /// in-place cycle hands every survivor here as its own span (~120 000 a
    /// cycle on `GcfF3yYoungPremiumProbe`), and one B-tree range query and
    /// remove per object was 9 % of that process's samples. Removing `[a, b)`
    /// and `[b, c)` from the set is removing `[a, c)`, and the mirror is
    /// cleared over the whole run (bytes that hold live objects, which the
    /// mirror must never name), so the result is the same.
    pub(crate) fn forget_lazy_dirty_over(&mut self, spans: &[(usize, usize)]) {
        if self.zero_sets.lazy_dirty.spans.is_empty() {
            return;
        }
        let mut run: Option<(usize, usize)> = None;
        for &(o, l) in spans {
            if l == 0 {
                continue;
            }
            run = match run {
                Some((lo, hi)) if o == hi => Some((lo, o + l)),
                Some((lo, hi)) => {
                    self.forget_lazy_dirty_run(lo, hi);
                    Some((o, o + l))
                }
                None => Some((o, o + l)),
            };
        }
        if let Some((lo, hi)) = run {
            self.forget_lazy_dirty_run(lo, hi);
        }
    }

    /// One run of [`Self::forget_lazy_dirty_over`]: `[lo, hi)`, LOW offsets.
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
    pub(crate) fn suspend_lazy_mirror_over(&self, spans: &[(usize, usize)]) {
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
    pub(crate) fn restore_lazy_mirror_over(&self, spans: &[(usize, usize)]) {
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
    pub(crate) fn lazy_dirty_contains(&self, addr: usize) -> bool {
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
    pub(crate) fn lazy_dirty_mirror(&self) -> Option<std::sync::Arc<crate::heap_bitmap::HeapBitmap>> {
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
    pub(crate) fn lazy_dirty_end(&self) -> usize {
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
    pub(crate) fn set_skip_pristine_zeroing(&mut self, on: bool) {
        self.skip_pristine_zeroing = on;
    }

    /// zt t31/zc11: see the `defer_slide_tail_zeroing` field.
    pub(crate) fn set_defer_slide_tail_zeroing(&mut self, on: bool) {
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
    pub(crate) fn note_slide_stamp(&mut self, off: usize) {
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
    pub(crate) fn slide_zero_census(&self) -> (usize, usize, usize) {
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
    pub(crate) fn take_last_hand_out_zero_plan(
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
    pub(crate) fn take_last_hand_out_pristine(&mut self, ptr: *const u8) -> Option<(usize, usize)> {
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
    pub(crate) fn set_uncommit_policy(
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
    pub(crate) fn set_pre_touch(&mut self, on: bool) -> usize {
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
    pub(crate) fn set_store_pre_touch(&mut self, on: bool) -> usize {
        let cap = self.capacity();
        self.data.set_pre_touch(on, cap)
    }

    /// zki (page `zgc-zt23hunt4-*` item 4): back this arena with explicit
    /// hugetlbfs pages ([`crate::reservation::HeapStore::use_explicit_huge_pages`]).
    /// Only ZGC calls it, before anything is committed.
    pub(crate) fn use_explicit_huge_pages(&mut self) -> bool {
        self.data.use_explicit_huge_pages()
    }

    /// zki: is this arena backed by explicit hugetlbfs pages?
    pub(crate) fn is_explicit_huge_pages(&self) -> bool {
        self.data.is_explicit_huge_pages()
    }

    /// zki: commits for use the hugetlbfs pool refused since the policy was
    /// installed (`0` without a policy or explicit pages).
    pub(crate) fn hugetlb_commit_refusals(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.hugetlb_commit_refusals)
    }

    /// zki: the granules of the last refused hugetlbfs commit for use (`0`
    /// before any).
    pub(crate) fn hugetlb_last_refused_granules(&self) -> usize {
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
    pub(crate) fn commit_initial_explicit(&mut self, bytes: usize) -> (usize, usize) {
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
    pub(crate) fn probe_explicit_uncommit(&mut self) -> bool {
        use crate::reservation::GRANULE;
        if self.capacity() < GRANULE || !self.data.commit_range(0, GRANULE) {
            return false;
        }
        let _ = self.data.decommit_range(0, GRANULE, "zgc-uncommit-probe");
        true
    }

    /// zt t23/hunt4: bytes the `-XX:+AlwaysPreTouch` path has touched, `0`
    /// without a policy.
    pub(crate) fn pre_touched_bytes(&self) -> usize {
        self.uncommit_policy
            .as_ref()
            .map_or(0, |p| p.pre_touched_bytes)
    }

    /// zt t3/rl: start (`true`) or stop recording the spans the give-back
    /// releases. A no-op without a policy (every collector but ZGC).
    pub(crate) fn set_release_log(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.released_log = on.then(Vec::new);
        }
    }

    /// zt t3/rl: the spans released since [`Self::set_release_log`] (arena
    /// offsets, in release order), and stop recording.
    pub(crate) fn take_release_log(&mut self) -> Vec<(usize, usize)> {
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
    pub(crate) fn set_reset_accounting(&mut self, on: bool) {
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
    pub(crate) fn set_keep_refused_idle_stamps(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.keep_refused_idle_stamps = on;
        }
    }

    /// zt t2/al: see `ArenaUncommitPolicy::refused_fit_relists`. A no-op on an
    /// arena without a policy (every collector but ZGC). Survives a later
    /// [`Self::set_uncommit_policy`].
    pub(crate) fn set_refused_fit_relists(&mut self, on: bool) {
        if let Some(p) = self.uncommit_policy.as_mut() {
            p.refused_fit_relists = on;
        }
    }

    /// zt t2/al: spans a refused free-list commit put back on their list
    /// (`ArenaUncommitPolicy::refused_fit_relists`). Zero without a policy.
    pub(crate) fn refused_fits_relisted(&self) -> usize {
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

    pub(crate) fn uncommit_floor(&self) -> usize {
        self.uncommit_policy.as_ref().map_or(0, |p| p.floor)
    }

    /// `(enabled, delay)` of the installed uncommit policy, or `None` when the
    /// owner installed none (zr z3/fp: lets the floor change keep both).
    pub(crate) fn uncommit_settings(&self) -> Option<(bool, std::time::Duration)> {
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
    pub(crate) fn take_low_free_list_for_deferred_sweep(&mut self) -> usize {
        let dropped = self.clear_low_free_list();
        self.clear_alloc_anchors();
        dropped
    }

    /// zt t21/zc2: the LOW free list's blocks, ascending, and the list
    /// emptied -- what the window between a deferred sweep's pause and its
    /// install put there (retired TLAB tails above the pause's cursor), for the
    /// install to put back after [`Self::rebuild_low_free_list`].
    pub(crate) fn take_low_free_blocks(&mut self) -> Vec<(usize, usize)> {
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
    pub(crate) fn used_low_for_compaction(&self) -> usize {
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
    pub(crate) fn low_high_water(&self) -> usize {
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
    pub(crate) fn high_low_water(&self) -> usize {
        self.high_cursor.min(self.pre_retract_low)
    }

    /// Forget the retraction history, so the next
    /// [`Self::low_high_water`] starts from the current cursor.
    ///
    /// Called once per collection, AFTER every bitmap over this arena has been
    /// cleared -- calling it earlier would hand a later pass a bound that no
    /// longer covers the bits it has to reach.
    pub(crate) fn reset_low_high_water(&mut self) {
        self.pre_retract_high = 0;
    }

    /// [`Self::reset_low_high_water`] for the large-object end, with the same
    /// "call it AFTER every bitmap is clear" obligation.
    pub(crate) fn reset_high_low_water(&mut self) {
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
    pub(crate) fn compact_nursery_to(
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
            !bucket.is_empty()
        });
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
    pub(crate) fn zero_withheld_low_spans(&mut self, spans: &[(usize, usize)]) {
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
    /// `pub(crate)` because "everything else below the cursor is dead" is a
    /// claim only the relocator that just moved the survivors can make.
    pub(crate) fn rebuild_after_in_place_evacuation(
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
    pub(crate) fn stamp_idle_unused(&mut self) -> usize {
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
    pub(crate) fn uncommit_idle_pass(
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
    pub(crate) fn commit_for_relocation(&mut self, offset: usize, len: usize) -> bool {
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
    pub(crate) fn advise_no_huge_pages(&self) -> bool {
        self.data.advise_no_huge_pages()
    }

    /// zt t24/hunt7: [`crate::reservation::HeapStore::advise_huge_pages`] on
    /// this arena's backing store, for HotSpot's
    /// `-XX:+UseTransparentHugePages` (`ZgcRealHeap::advise_transparent_huge_pages`;
    /// zki: the generational heap's semi-spaces too,
    /// `GenerationalHeap::advise_transparent_huge_pages`).
    pub(crate) fn advise_huge_pages(&self) -> bool {
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
    pub(crate) fn commit_initial_zgc(&mut self, bytes: usize) -> usize {
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

    /// **A high-end retraction must widen the bitmap bound, exactly as a low-end
    /// one does.**
    ///
    /// `live_bitmap_bounds` passes over `[0, low_high_water)` and
    /// `[high_low_water, capacity)`. The low half has used a water mark since
    /// the bounds landed; the high half used the RAW cursor, and
    /// `retract_high_cursor_into_free_head` RAISES that cursor -- so the span
    /// the retraction gave back fell outside the next clear and kept this
    /// cycle's mark bits. Once the cursor bumps back down over them, a fresh
    /// object there reads as already-marked, `mark` never hands it to the
    /// worklist, and everything reachable only through it is swept while live.
    ///
    /// Latent rather than measured: found while root-causing
    /// `ZGC-RELOC-DECOMMIT.1`, which it did not cause -- that workload allocates
    /// nothing at or above `ZGC_LARGE_OBJECT_MIN`, so this cursor never
    /// retracts there. It needs a workload that fragments the large-object end.
    #[test]
    fn a_high_end_retraction_keeps_the_bitmap_bound_over_the_span_it_gave_back() {
        let mut arena = Arena::new(8192);
        let _live = arena.alloc_high(1024, 8).expect("live");
        let dead_a = arena.alloc_high(1024, 8).expect("dead a") as usize;
        let dead_b = arena.alloc_high(1024, 8).expect("dead b") as usize;
        let base = arena.base_ptr() as usize;
        let low_of_dead = (dead_b - base).min(dead_a - base);
        let cursor_before = arena.high_cursor();
        assert_eq!(
            arena.high_low_water(),
            cursor_before,
            "with no retraction yet the water mark IS the cursor"
        );

        arena.add_free_block(dead_a - base, 1024);
        arena.add_free_block(dead_b - base, 1024);
        assert_eq!(arena.coalesce_high(), 1, "the dead spans must merge");
        let handed_back = arena.retract_high_cursor_into_free_head();
        assert_eq!(handed_back, 2048, "the merged span starts at the cursor");
        assert!(
            arena.high_cursor() > cursor_before,
            "a high-end retraction RAISES the cursor"
        );

        // The bound must still reach down over the span just given back --
        // that is where this cycle's mark bits are.
        assert_eq!(
            arena.high_low_water(),
            low_of_dead,
            "the bound must cover the retracted span, or its mark bits survive \
             the clear and a later cycle reads them as a live object's"
        );
        assert!(
            arena.high_low_water() < arena.high_cursor(),
            "the water mark is BELOW the raised cursor, which is the whole point"
        );

        // And a reset (after every bitmap is clear) hands the next cycle the
        // tighter bound back.
        arena.reset_high_low_water();
        assert_eq!(
            arena.high_low_water(),
            arena.high_cursor(),
            "reset must return the bound to the current cursor, or the saving decays"
        );
    }

    /// **A decommitted free block reads as UNMAPPED, and `commit_for_relocation` is
    /// what a direct writer must call before touching it.**
    ///
    /// The unit-level half of the `relocate_stw` destination-commit bug (see
    /// `ZgcRealHeap`'s `the_slide_commits_a_destination_the_give_back_had_decommitted`).
    /// `decommit_free_blocks` documents its contract as "every path that hands
    /// one out again goes through `hand_out`, which commits" -- so any path
    /// that does NOT go through `hand_out`, as the compacting slide does not,
    /// has to commit for itself. This pins both directions of that: the
    /// give-back really does leave the granule unreadable, and
    /// `commit_for_relocation` really does bring it back.
    ///
    /// Skipped on a wholly-committed fallback store, where the whole question
    /// is vacuous -- `is_readable_at` is unconditionally `true` there.
    #[test]
    fn commit_for_relocation_restores_a_granule_the_give_back_released() {
        use crate::reservation::GRANULE;
        // Several granules, so there is a whole one strictly inside the hole
        // after `decommit_free_blocks` rounds inward.
        let mut arena = Arena::new(8 * GRANULE);
        let live = arena.alloc(1024, 8).expect("live head");
        let base = arena.base_ptr() as usize;
        let live_off = live as usize - base;
        let hole_off = live_off + 1024;
        let hole_len = 5 * GRANULE;
        let _dead = arena.alloc(hole_len, 8).expect("dead span");

        arena.add_free_block(hole_off, hole_len);
        arena.coalesce_free_list();
        let released = arena.decommit_free_blocks();
        if released == 0 {
            // Fallback (non-reserving) store: nothing can be decommitted and
            // nothing can fault, so there is nothing to assert.
            assert!(arena.is_readable_at(hole_off + GRANULE));
            return;
        }

        let probe = (hole_off + GRANULE) & !(GRANULE - 1);
        assert!(
            !arena.is_readable_at(probe),
            "a released granule must report as unmapped, or the write-side \
             hazard this guards would be invisible"
        );
        assert!(
            arena.commit_for_relocation(probe, 64),
            "the OS must give a released granule back on request"
        );
        assert!(
            arena.is_readable_at(probe),
            "commit_for_relocation must leave the destination safe to write"
        );
        // And the re-committed bytes read as zero, which is what a caller
        // handed a reused block is entitled to.
        let p = (base + probe) as *const u8;
        assert_eq!(unsafe { *p }, 0, "a re-committed granule reads as zero");
    }

    use super::*;

    /// gen r4w5/pinned5: `rebuild_after_in_place_evacuation` leaves the arena
    /// in the state a sweep would — the cursor on the last survivor, the
    /// zeroed complement on the free list — and the allocator never hands a
    /// survivor's bytes out again, however long it keeps allocating.
    #[test]
    fn rebuild_after_in_place_evacuation_never_hands_out_a_survivor() {
        let mut arena = Arena::new(64 * 1024);
        let base = arena.base_ptr() as usize;
        let offs: Vec<usize> = (0..4)
            .map(|_| {
                arena
                    .alloc(1024, 8)
                    .expect("an empty 64 KiB arena fits 4 KiB") as usize
                    - base
            })
            .collect();
        assert_eq!(offs, vec![0, 1024, 2048, 3072], "precondition: four bumps");
        // Two survivors (one of them "pinned"); the two dead spans were never
        // written, so they already read as zero.
        let live = [(1024usize, 1024usize), (3072, 1024)];
        let free = [(0usize, 1024usize), (2048, 1024)];
        arena.rebuild_after_in_place_evacuation(4096, &live, &free, 4096);
        assert_eq!(arena.used(), 4096, "the cursor sits on the last survivor");
        assert_eq!(arena.free_blocks_sorted(), free.to_vec());
        let mut handed = 0usize;
        while let Some(p) = arena.alloc(64, 8) {
            let off = p as usize - base;
            for &(lo, len) in &live {
                assert!(
                    off + 64 <= lo || off >= lo + len,
                    "handed out {off:#x}, inside the survivor at {lo:#x}+{len:#x}"
                );
            }
            handed += 1;
        }
        assert!(
            handed >= 32,
            "both free blocks and the tail served ({handed})"
        );
    }

    /// **`-Xms` moves committed bytes and moves nothing else.**
    ///
    /// The contract [`Arena::commit_initial_prefix`] states, asserted rather
    /// than trusted: an arena whose prefix has been committed is
    /// indistinguishable from one that has not, except in `committed_bytes()`.
    /// A "fix" that resized the arena, moved the cursor or disturbed the free
    /// list would break the collectors that captured `base`/`capacity` once —
    /// which is the failure mode
    /// `docs/feature-designs/generative-gc-architecture.md`
    /// forbids by name.
    #[test]
    fn commit_initial_prefix_moves_committed_bytes_and_nothing_else() {
        const CAP: usize = 16 * 1024 * 1024;
        let mut arena = Arena::new(CAP);

        let before = (
            arena.capacity(),
            arena.used(),
            arena.free_list_bytes(),
            arena.base_ptr() as usize,
            arena.remaining(),
        );
        let committed_before = arena.committed_bytes();

        let n = arena.commit_initial_prefix(8 * 1024 * 1024);
        assert_eq!(n, 8 * 1024 * 1024, "the request fits and must be honoured");

        assert_eq!(
            (
                arena.capacity(),
                arena.used(),
                arena.free_list_bytes(),
                arena.base_ptr() as usize,
                arena.remaining(),
            ),
            before,
            "-Xms must not resize the arena, move a cursor, touch the free list or relocate the base: every one of those is read without the arena lock by a collector that captured it at construction",
        );
        assert!(
            arena.committed_bytes() >= committed_before,
            "committed bytes cannot fall",
        );
        // On a RESERVING store this is a strict rise. On the wholly-committed
        // fallback (`CRATONVM_GC_RESERVE=0`, and any platform without a
        // reservation) the arena already had everything committed and there is
        // nothing to rise — which is not a failure, it is the store saying the
        // bytes were already there.
        if committed_before < CAP {
            assert!(
                arena.committed_bytes() > committed_before,
                "the store reserves ({committed_before} of {CAP} committed), so an 8 MiB prefix commit must raise the committed bytes; it left them at {}",
                arena.committed_bytes(),
            );
        }

        // The committed prefix is WRITABLE, which is the point of committing
        // it. A bump allocation out of the prefix must succeed and be readable.
        let p = arena
            .alloc(64, 8)
            .expect("the committed prefix must allocate");
        // SAFETY: `p` is a fresh 64-byte allocation from this arena.
        unsafe {
            std::ptr::write_bytes(p, 0xA5, 64);
            assert_eq!(*p, 0xA5);
        }
    }

    /// **An oversized `-Xms` is clamped, and a zero one is a no-op.**
    ///
    /// `-Xms` above `-Xmx` is clamped rather than refused at every level of
    /// this stack (`VmHeap::new_with_heap_sizing` says so for the flag), so the
    /// arena has to behave the same way: the two sizes are specified
    /// separately and an operator who oversizes one should still get a heap.
    #[test]
    fn commit_initial_prefix_clamps_and_ignores_zero() {
        const CAP: usize = 4 * 1024 * 1024;
        let mut arena = Arena::new(CAP);

        assert_eq!(
            arena.commit_initial_prefix(0),
            0,
            "no -Xms is not a commit of nothing, it is no call at all",
        );

        assert_eq!(
            arena.commit_initial_prefix(CAP * 8),
            CAP,
            "an -Xms above the arena's capacity commits the whole arena rather than being refused",
        );
        assert_eq!(
            arena.capacity(),
            CAP,
            "and clamping must not have grown the arena to meet the request",
        );
    }

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
    /// t1 wave 10 lane ZT3GN: the nursery-local install keeps, drops and
    /// truncates exactly the blocks `compact_low_to` does (the vacated spans
    /// are published merged, so the comparison is on the merged byte set),
    /// leaves the free-byte total and the cursor equal, and refuses a slide
    /// that wrote below its floor.
    #[test]
    fn the_nursery_install_lists_what_compact_low_to_lists() {
        const K: usize = 1024;
        fn fixture() -> Arena {
            let mut a = Arena::new(256 * K);
            let _ = a.alloc(200 * K, 8).expect("fresh arena has room");
            a.add_free_block(K, 512); // old gen, below lo: kept in place
            a.add_free_block(30 * K, 30 * K); // large, wholly below lo: in place
            a.add_free_block(63 * K, 2 * K); // straddles lo = 64K, overlaps touched
            a.add_free_block(80 * K, 256); // inside touched: dropped
            a.add_free_block(110 * K, 4 * K); // between touched and the spans: kept
            a.add_free_block(130 * K, 8 * K); // inside a vacated span: dropped
            a.add_free_block(170 * K, 640); // straddles nothing, below cursor: kept
            a.add_free_block(178 * K, 8 * K); // straddles the new cursor: truncated
            a.add_free_block(190 * K, 4 * K); // above the new cursor: dropped
            a
        }
        // Merge touching spans of a sorted block list (the install merges the
        // vacated spans; the old door lists them one per page).
        fn merged(v: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
            let mut out: Vec<(usize, usize)> = Vec::new();
            for (off, sz) in v {
                match out.last_mut() {
                    Some(last) if last.0 + last.1 == off => last.1 += sz,
                    _ => out.push((off, sz)),
                }
            }
            out
        }
        let lo = 64 * K;
        let new_cursor = 180 * K;
        let touched = 64 * K..96 * K;
        // Two touching per-page spans and one apart.
        let vacated = [(128 * K, 144 * K), (144 * K, 160 * K), (164 * K, 168 * K)];

        let mut old = fixture();
        let r_old = old.compact_low_to(new_cursor, touched.clone(), &vacated);
        let mut new = fixture();
        let r_new = new
            .compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, true)
            .expect("the slide fits the premise");
        assert_eq!(r_old, r_new);
        assert_eq!(old.used_low_for_compaction(), new.used_low_for_compaction());
        assert_eq!(old.free_list_bytes(), new.free_list_bytes());
        assert_eq!(
            merged(old.free_blocks_sorted()),
            merged(new.free_blocks_sorted())
        );
        let kept = new.free_blocks_sorted();
        assert!(kept.contains(&(K, 512)), "{kept:?}");
        assert!(kept.contains(&(178 * K, 2 * K)), "truncated: {kept:?}");
        assert!(
            kept.contains(&(128 * K, 32 * K)),
            "touching vacated spans publish as one block: {kept:?}"
        );
        // The deferred-zero arm lists the same bytes and does not write them.
        let mut lazy = fixture();
        let p = lazy.base_ptr_mut();
        // SAFETY: inside the arena, below the cursor.
        unsafe { *p.add(150 * K) = 0xAB };
        let _ = lazy
            .compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, false)
            .expect("the slide fits the premise");
        assert_eq!(merged(lazy.free_blocks_sorted()), merged(kept.clone()));
        // SAFETY: as above.
        assert_eq!(unsafe { *lazy.base_ptr().add(150 * K) }, 0xAB);
        let mut eager = fixture();
        // SAFETY: as above.
        unsafe { *eager.base_ptr_mut().add(150 * K) = 0xAB };
        let _ = eager.compact_nursery_to(new_cursor, lo, touched.clone(), &vacated, true);
        // SAFETY: as above.
        assert_eq!(unsafe { *eager.base_ptr().add(150 * K) }, 0);
        // A slide that wrote below the floor does not fit: nothing changes.
        let mut refused = fixture();
        let before = refused.free_blocks_sorted();
        let cursor_before = refused.used_low_for_compaction();
        assert_eq!(
            refused.compact_nursery_to(new_cursor, lo, 60 * K..96 * K, &vacated, true),
            None
        );
        assert_eq!(refused.free_blocks_sorted(), before);
        assert_eq!(refused.used_low_for_compaction(), cursor_before);
    }

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
            "precondition: the low end is walled by one survivor per chunk — if this succeeds the assertion below proves nothing",
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
        unsafe { std::ptr::copy((base + c) as *const u8, (base + b) as *mut u8, 1024) };
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
        let p = arena
            .alloc(64, 8)
            .expect("the last resort must still serve this rather than raise OutOfMemoryError");
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

    /// gengc-round2-alloc2, 2026-09-20 — the bump path's alignment padding is
    /// free-listed instead of orphaned.
    ///
    /// Verification step 2(b) of
    /// `gengc-alloc-arena-bump-path-orphans-alignment-padding-FIXED-20260923.md`
    /// verbatim: allocate with `align = 16` from a cursor at offset 8 and
    /// assert `free_list_bytes()` grew by the 8 padding bytes. Before this it
    /// did neither — the bytes simply vanished, counted by `used()`, absent
    /// from the free list, and left for a linear walk to read as an
    /// `ObjectHeader`.
    ///
    /// The setup is the whole test. `align = 8` cannot produce padding (the
    /// cursor is 8-aligned by construction), so the first allocation has to
    /// move the cursor OFF the 16-byte grid while staying on the 8-byte one —
    /// which an 8-byte request does exactly.
    #[test]
    fn the_bump_path_free_lists_its_alignment_padding() {
        let mut arena = Arena::new(64 * 1024);
        assert_eq!(arena.free_list_bytes(), 0);
        // The free-list tiers align the ABSOLUTE address, so the reuse half at
        // the end needs an 8-aligned backing base. `alloc_zeroed_heap` asks for
        // `align = 1` and no type in this tree asserts more — but every real
        // allocator delivers page alignment for a 64 KiB block, and
        // `note_object_start`'s own `debug_assert` already depends on the same
        // thing. Stated here so a failure names the cause instead of showing up
        // as a mysterious free-list miss.
        assert_eq!(
            arena.base_ptr() as usize % 8,
            0,
            "the arena's backing base is not 8-aligned",
        );

        // Cursor 0 -> 8. No padding is possible at `align = 8`.
        assert!(arena.alloc(8, 8).is_some());
        assert_eq!(arena.used(), 8);
        assert_eq!(arena.free_list_bytes(), 0, "an 8-aligned bump pads nothing");

        // Cursor 8, asking for 16-byte alignment: 8 bytes of padding.
        let p = arena.alloc(32, 16).expect("the arena is nearly empty");
        // The bump tier aligns the OFFSET, not the absolute address (the
        // free-list tiers align the address; `heap_bitmap`'s header documents
        // the asymmetry and the overflow set that covers it). Assert what this
        // path actually promises.
        assert_eq!(
            (p as usize - arena.base_ptr() as usize) % 16,
            0,
            "the allocation itself must honour the alignment it asked for"
        );
        assert_eq!(
            arena.free_list_bytes(),
            8,
            "the 8 bytes the round-up stepped over must be on the free list, \
             not orphaned inside [0, cursor)"
        );
        // The padding is a legal low-tier block: both ends on the 8-byte grid.
        // (`push_block_routed` warns otherwise, and a 1..7-byte splinter there
        // would be the grid violation this fix must not introduce.)
        let blocks = arena.free_blocks_sorted();
        assert_eq!(blocks, vec![(8, 8)]);

        // ...and it is reusable, which is the accounting half: a bump-path
        // alignment gap used to be unreachable forever.
        let reused = arena
            .alloc(8, 8)
            .expect("the 8-byte hole fits an 8-byte request");
        assert_eq!(reused as usize - arena.base_ptr() as usize, 8);
        assert_eq!(arena.free_list_bytes(), 0);
    }

    /// gengc-round1-alloc, 2026-09-20 — an offset this arena could not have
    /// handed out records nothing, and does not panic.
    ///
    /// The grid screen added beside it must not turn a stale offset (one from
    /// before a `grow`, say) into an assertion failure: that is the behaviour
    /// `get_mut` has always given and a collector handing over a stale offset
    /// must never take the VM down. The screen therefore runs only for offsets
    /// inside the arena.
    #[test]
    fn note_object_start_ignores_offsets_outside_the_arena() {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        arena.note_object_start(cap); // one past the last byte
        arena.note_object_start(cap * 4);
        arena.note_object_start(usize::MAX);
        assert!(
            arena.take_alloc_anchors().is_empty(),
            "an out-of-range offset must record no anchor",
        );
        // ...and the last in-range grid slot still does record.
        arena.note_object_start(cap - 8);
        assert_eq!(arena.take_alloc_anchors(), vec![cap - 8]);
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

    /// gen r4w6/tlab6 — `return_unused_tail`'s three answers. A tail at the
    /// cursor retracts it (and forgets the chunk base's start bit and anchor
    /// when the whole chunk comes back); a tail below the cursor is listed if
    /// it reaches `min_free`; everything else is declined untouched.
    #[test]
    fn an_unused_tlab_tail_retracts_the_cursor_or_joins_the_free_list() {
        let mut a = Arena::new(64 * 1024);
        a.arm_object_starts();
        let base = a.base_ptr() as usize;
        // Two 16 KiB "TLAB" chunks, as `refill_tlab` carves them.
        let c1 = a.alloc(16 * 1024, 8).unwrap() as usize - base;
        a.note_object_start(c1);
        let c2 = a.alloc(16 * 1024, 8).unwrap() as usize - base;
        a.note_object_start(c2);
        assert_eq!(a.used_low_for_compaction(), 32 * 1024);

        // Off-grid, empty, past the cursor, or in no chunk: declined, no change.
        assert_eq!(a.return_unused_tail(c2 + 4, 64, 0), TailReturn::Declined);
        assert_eq!(a.return_unused_tail(c2, 0, 0), TailReturn::Declined);
        assert_eq!(a.return_unused_tail(c2, 32 * 1024, 0), TailReturn::Declined);
        assert_eq!(a.used_low_for_compaction(), 32 * 1024);
        assert_eq!(a.free_list_bytes(), 0);

        // The first chunk's tail is not at the cursor: below `min_free` it is
        // declined, at or above it is listed.
        let t1 = c1 + 4096;
        assert_eq!(
            a.return_unused_tail(t1, 12 * 1024, 16 * 1024),
            TailReturn::Declined
        );
        assert_eq!(
            a.return_unused_tail(t1, 12 * 1024, 8 * 1024),
            TailReturn::FreeListed
        );
        assert_eq!(a.free_list_bytes(), 12 * 1024);
        assert_eq!(
            a.used_low_for_compaction(),
            32 * 1024,
            "listing never moves the cursor"
        );

        // The second chunk comes back WHOLE (retired before its first
        // allocation): the cursor retracts onto its base, and the base stops
        // being an object start and an anchor.
        // `Some(true)` when `CRATONVM_GC_OBJECT_STARTS` armed the bitmap,
        // `None` when it is off (the default); never `Some(false)` here.
        assert_ne!(a.is_object_start(base + c2), Some(false));
        assert!(a.alloc_anchors_snapshot().contains(&c2));
        assert_eq!(
            a.return_unused_tail(c2, 16 * 1024, 1 << 30),
            TailReturn::Retracted,
            "a tail at the cursor retracts whatever its size"
        );
        assert_eq!(a.used_low_for_compaction(), c2);
        assert_ne!(a.is_object_start(base + c2), Some(true));
        assert!(!a.alloc_anchors_snapshot().contains(&c2));
        assert!(
            a.low_high_water() >= 32 * 1024,
            "the high-water mark keeps the given-back span"
        );
        // The next carve starts exactly where the returned tail did (14 KiB, so
        // the listed 12 KiB block cannot serve it first).
        let c3 = a.alloc(14 * 1024, 8).unwrap() as usize - base;
        assert_eq!(c3, c2);
    }

    /// A grown arena must not answer object-start queries from the bitmap it
    /// had before it moved.
    ///
    /// `grow_to` changes both the base and the span, and the bitmap is indexed
    /// off the base and bounded by the span. Left in place it would answer for
    /// an address space that no longer exists — in the ACCEPT direction, which
    /// means handing back `Some(true)` for an address this arena never
    /// allocated, and a mark-bit write into whatever is actually there.
    ///
    /// The arm is explicit here because `Arena::new` deliberately does not do it
    /// (only `GenerationalHeap` consults the bitmap, and every other `Arena` in
    /// the process would otherwise pay capacity/64 for nothing).
    #[test]
    fn a_grown_arena_rebuilds_its_object_start_bitmap() {
        let mut a = Arena::new(4096);
        a.arm_object_starts();
        if !a.tracks_object_starts() {
            return; // switch off in this environment; nothing to assert
        }
        let p = a.alloc(64, 8).expect("fresh arena has room");
        let old_addr = p as usize;
        assert_eq!(
            a.is_object_start(old_addr),
            Some(true),
            "the allocation door must record the base it handed out"
        );

        a.reset();
        assert_eq!(
            a.is_object_start(old_addr),
            Some(false),
            "reset invalidates every base; a stale bit would be accepted next cycle"
        );

        a.grow(1 << 20);
        assert!(
            a.tracks_object_starts(),
            "a grown arena must still track, or the collector silently loses the fast path with no census entry to show for it"
        );
        let q = a.alloc(64, 8).expect("grown arena has room");
        assert_eq!(
            a.is_object_start(q as usize),
            Some(true),
            "the rebuilt bitmap must be indexed off the NEW base"
        );
        // And nothing in the new arena inherits a bit from the old geometry.
        assert_eq!(a.is_object_start(q as usize + 8), Some(false));
    }

    /// A freed base stops being a base at the moment the free list learns of it.
    ///
    /// Without the removal half, a larger object later allocated over the hole
    /// sees the stale bit, is accepted at an INTERIOR address, and takes a
    /// mark-bit write into the middle of a live object — the corruption the
    /// extent check in `is_object_address` exists to stop.
    #[test]
    fn freeing_a_block_clears_its_object_start_bit() {
        let mut a = Arena::new(4096);
        a.arm_object_starts();
        if !a.tracks_object_starts() {
            return;
        }
        let p = a.alloc(64, 8).expect("fresh arena has room");
        let addr = p as usize;
        let off = addr - a.base_ptr() as usize;
        assert_eq!(a.is_object_start(addr), Some(true));
        a.add_free_block(off, 64);
        assert_eq!(
            a.is_object_start(addr),
            Some(false),
            "a freed base must stop being a base"
        );
    }

    /// gengc-round1-alloc, 2026-09-20 — `reset` clears the object-start bitmap
    /// over the BUMPED spans only, and that bound is the high-water mark
    /// rather than the live cursor.
    ///
    /// `clear_all` was O(capacity) on every collection — one store per 512
    /// bytes of `-Xmx`-sized from-space whether the nursery held ten objects
    /// or ten million. Bounding it to what was handed out is the same change
    /// `heap_bitmap::clear_within` was added for. The bound has to be
    /// `low_high_water`, not `cursor`: a retraction moves the cursor away from
    /// bytes whose bits are set, and a pass bounded by the post-retraction
    /// cursor leaves them set — after which the next cycle's allocation over
    /// that span is ACCEPTED at an interior address.
    ///
    /// The two allocations are 1 KiB apart on purpose: one bitmap word covers
    /// 512 arena bytes, so a smaller spacing would have both bits in the word
    /// the narrow bound clears anyway and the test would pass either way.
    #[test]
    fn reset_clears_object_start_bits_the_cursor_retracted_past() {
        let mut a = Arena::new(1 << 20);
        a.arm_object_starts();
        if !a.tracks_object_starts() {
            return; // switch off in this environment; nothing to assert
        }
        let base = a.base_ptr() as usize;
        let p = a.alloc(1024, 8).expect("fresh arena has room");
        let q = a.alloc(1024, 8).expect("fresh arena has room");
        assert_eq!(a.is_object_start(p as usize), Some(true));
        assert_eq!(a.is_object_start(q as usize), Some(true));

        // Give the second allocation's span back to the bump tail. The cursor
        // no longer covers `q`; its start bit is untouched by the retraction.
        a.retract_cursor_to(q as usize - base);
        assert_eq!(
            a.is_object_start(q as usize),
            Some(true),
            "a retraction moves a cursor, not a bitmap",
        );

        a.reset();
        assert_eq!(a.is_object_start(p as usize), Some(false));
        assert_eq!(
            a.is_object_start(q as usize),
            Some(false),
            "a bound taken from the post-retraction cursor would leave this set, \
             and the next cycle would accept an interior address as a base",
        );
    }

    /// An arena nobody armed answers `None`, which is NOT `Some(false)`.
    ///
    /// The distinction is the whole safety contract: `None` means "not tracked,
    /// fall back to the deduction", and a caller that collapsed it to "not a
    /// base" would reject every live root in an unarmed arena.
    #[test]
    fn an_unarmed_arena_answers_none_rather_than_false() {
        let mut a = Arena::new(4096);
        let p = a.alloc(64, 8).expect("fresh arena has room");
        assert!(!a.tracks_object_starts());
        assert_eq!(a.is_object_start(p as usize), None);
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

    /// zgc10: the pristine window hands a carver exactly the never-written
    /// part of a hand-out, only when opted in, and only once.
    #[test]
    fn pristine_window_reports_only_never_written_bytes() {
        let mut arena = Arena::new(1 << 20);
        let base = arena.base_ptr() as usize;

        // Not opted in: never reported, though the window is tracked.
        let a = arena.alloc(4096, 16).expect("a");
        assert_eq!(arena.take_last_hand_out_pristine(a), None);

        arena.set_skip_pristine_zeroing(true);
        let b = arena.alloc(8192, 16).expect("b");
        let (lo, hi) = arena
            .take_last_hand_out_pristine(b)
            .expect("a fresh bump hand-out is pristine");
        assert_eq!((lo, hi), (b as usize, b as usize + 8192));
        // One-shot.
        assert_eq!(arena.take_last_hand_out_pristine(b), None);

        // Give everything back to the bump tail and allocate over it: the part
        // that was handed out before is NOT pristine any more, the rest is.
        let used = arena.cursor;
        assert_eq!(used, (b as usize + 8192) - base);
        arena.retract_cursor_to(0);
        let c = arena.alloc(used + 4096, 16).expect("c");
        assert_eq!(c as usize, base);
        let (lo, hi) = arena
            .take_last_hand_out_pristine(c)
            .expect("the tail above the old high-water mark is pristine");
        assert_eq!(lo, base + used, "bytes below the old cursor were written");
        assert_eq!(hi, base + used + 4096);

        // A mismatched pointer is refused.
        let d = arena.alloc(64, 16).expect("d");
        assert_eq!(arena.take_last_hand_out_pristine(a), None);
        let _ = d;
    }

    /// r16 zgcarray ZA1: after a slide, the allocation that hands its
    /// zero-filled spans out again -- the tail above the new cursor and the
    /// vacated spans it free-listed below it -- is told they already read
    /// zero, except the forwarding records stamped into them; the set only
    /// ever shrinks, and other re-listing does not falsify it.
    /// zt t31/zc11 (`CRATONVM_ZGC_SLIDE_TAIL_ZERO_AT_ALLOCATION`): an owner
    /// that defers the tail leaves `[new_cursor, old_cursor)` as it was and
    /// does not list it as zeroed, so the hand-out that takes it is told to
    /// zero it all.
    #[test]
    fn a_deferred_slide_tail_is_zeroed_by_its_hand_out() {
        const K: usize = 1024;
        let mut arena = Arena::new(1 << 20);
        arena.set_skip_pristine_zeroing(true);
        arena.set_defer_slide_tail_zeroing(true);
        let base = arena.base_ptr() as usize;
        let p = arena.alloc(64 * K, 16).expect("fill") as usize;
        // SAFETY: inside the hand-out above.
        unsafe { std::ptr::write_bytes((base + 4 * K) as *mut u8, 0xAB, 60 * K) };
        let _ = arena.take_last_hand_out_zero_plan(p as *const u8, 64 * K);
        arena.compact_low_to(44 * K, 0..4 * K, &[]);
        // Nothing is listed as zeroed.
        assert_eq!(arena.slide_zero_census(), (0, 0, 0));
        // SAFETY: inside the arena; the tail was not zeroed by the slide.
        let tail = unsafe { std::slice::from_raw_parts((base + 50 * K) as *const u8, 14 * K) };
        assert!(tail.iter().all(|&b| b == 0xAB), "the slide left the tail");
        // The bump hands the tail out with no known-zero part: zero it all.
        let used = arena.used_low_for_compaction();
        assert_eq!(used, 44 * K);
        let c = arena.alloc(20 * K, 16).expect("the tail") as usize;
        assert_eq!(c, base + 44 * K);
        let plan = arena.take_last_hand_out_zero_plan(c as *const u8, 20 * K);
        assert!(
            plan.as_ref().map_or(true, |p| p.known_zero.is_empty()),
            "nothing of the tail is claimed zero: {plan:?}"
        );
        // SAFETY: `c` is this hand-out's `20 K` bytes.
        unsafe { ZeroPlan::zero(plan.as_ref(), c as *mut u8, 20 * K) };
        let handed = unsafe { std::slice::from_raw_parts(c as *const u8, 20 * K) };
        assert!(handed.iter().all(|&b| b == 0), "the hand-out zeroed it");
    }

    #[test]
    fn a_slides_zeroed_spans_are_reported_once_minus_their_stamps() {
        const K: usize = 1024;
        let mut arena = Arena::new(1 << 20);
        arena.set_skip_pristine_zeroing(true);
        let base = arena.base_ptr() as usize;
        let p = arena.alloc(64 * K, 16).expect("fill") as usize;
        assert_eq!(p, base);
        // Dirty everything the slide is about to vacate.
        // SAFETY: inside the hand-out above.
        unsafe { std::ptr::write_bytes((base + 4 * K) as *mut u8, 0xAB, 60 * K) };
        let _ = arena.take_last_hand_out_zero_plan(p as *const u8, 64 * K);

        // Survivors packed into [0, 4K) and one left at [40K, 44K): the slide
        // vacated [8K, 40K) below the new cursor and zeroes [44K, 64K) above.
        arena.compact_low_to(44 * K, 0..4 * K, &[(8 * K, 40 * K)]);
        assert_eq!(arena.slide_zero_census(), (2, 32 * K + 20 * K, 0));
        arena.note_slide_stamp(48 * K);
        arena.note_slide_stamp(46 * K);
        assert_eq!(arena.slide_zero_census().2, 2);

        // A sweep-style re-listing elsewhere does not falsify the set.
        arena.add_free_block(4 * K, 4 * K);
        assert_eq!(arena.slide_zero_census().0, 2);

        // The vacated span, served from the free list.
        let v = arena.alloc(16 * K, 16).expect("from the free list") as usize;
        let plan = arena
            .take_last_hand_out_zero_plan(v as *const u8, 16 * K)
            .expect("a vacated span is known zero");
        let (vlo, vhi) = (base + 8 * K, base + 40 * K);
        if v >= vlo && v + 16 * K <= vhi {
            assert_eq!(plan.known_zero, vec![(v, v + 16 * K)]);
            assert_eq!(plan.slide_bytes, 16 * K);
            assert!(plan.dirty_stamps.is_empty());
        }

        // The tail, served by the bump.
        let used = arena.used_low_for_compaction();
        assert_eq!(used, 44 * K);
        let c = arena.alloc(64 * K, 16).expect("a block or the tail") as usize;
        let plan = arena.take_last_hand_out_zero_plan(c as *const u8, 64 * K);
        if c == base + 44 * K {
            let plan = plan.expect("the slide's tail is known zero");
            assert_eq!(plan.known_zero[0].0, base + 44 * K);
            assert_eq!(plan.slide_bytes, 20 * K);
            assert_eq!(plan.dirty_stamps, vec![base + 46 * K, base + 48 * K]);
        }
        // SAFETY: the slide zero-filled [44K, 64K); read it back.
        let tail = unsafe { std::slice::from_raw_parts((base + 50 * K) as *const u8, 14 * K) };
        assert!(tail.iter().all(|&b| b == 0), "the set's claim is true");
        // One-shot.
        assert_eq!(
            arena.take_last_hand_out_zero_plan(c as *const u8, 64 * K),
            None
        );

        // A relocation destination inside the set shrinks it too.
        arena.compact_low_to(4 * K, 0..4 * K, &[]);
        let (_, before, _) = arena.slide_zero_census();
        assert!(arena.commit_for_relocation(20 * K, 4 * K));
        assert_eq!(arena.slide_zero_census().1, before - 4 * K);

        // Not opted in: a slide records nothing.
        let mut plain = Arena::new(1 << 20);
        let _ = plain.alloc(64 * K, 16);
        plain.compact_low_to(4 * K, 0..4 * K, &[]);
        assert_eq!(plain.slide_zero_census(), (0, 0, 0));
    }

    /// ZA1: a slide over bytes the arena already knows are zero leaves them
    /// alone, but still clears a forwarding stamp written into them.
    #[test]
    fn a_slide_skips_known_zero_bytes_but_clears_their_stamps() {
        const K: usize = 1024;
        let mut arena = Arena::new(1 << 20);
        arena.set_skip_pristine_zeroing(true);
        let base = arena.base_ptr() as usize;
        let _ = arena.alloc(64 * K, 16).expect("fill");
        let _ = arena.take_last_hand_out_zero_plan(base as *const u8, 64 * K);
        arena.compact_low_to(4 * K, 0..4 * K, &[]);
        // A forwarding record lands in the zeroed tail.
        // SAFETY: inside the arena's committed low region.
        unsafe { std::ptr::write_bytes((base + 20 * K) as *mut u8, 0xCD, 16) };
        arena.note_slide_stamp(20 * K);
        // The next slide, from a cursor above the old one, sweeps over it.
        let more = arena.alloc(64 * K, 16).expect("bump") as usize;
        let _ = arena.take_last_hand_out_zero_plan(more as *const u8, 64 * K);
        // SAFETY: the bump hand-out above.
        unsafe { std::ptr::write_bytes(more as *mut u8, 0xEE, 64 * K) };
        let top = arena.used_low_for_compaction();
        arena.compact_low_to(4 * K, 0..4 * K, &[]);
        // SAFETY: inside the arena.
        let all = unsafe { std::slice::from_raw_parts((base + 4 * K) as *const u8, top - 4 * K) };
        assert!(
            all.iter().all(|&b| b == 0),
            "everything above the new cursor reads zero"
        );
    }

    /// zgc10: a relocation destination and a high-end descent both leave the
    /// window, so nothing that was ever writable is reported as pristine.
    #[test]
    fn pristine_window_excludes_relocation_targets_and_the_high_end() {
        let mut arena = Arena::new(1 << 20);
        arena.set_skip_pristine_zeroing(true);
        let base = arena.base_ptr() as usize;
        let cap = arena.capacity();

        // A slide destination far above the cursor.
        assert!(arena.commit_for_relocation(64 * 1024, 4096));
        // A large object at the top.
        let h = arena.alloc_high(16 * 1024, 8).expect("high") as usize;
        assert!(h - base < cap);

        let p = arena.alloc(128 * 1024, 16).expect("low");
        match arena.take_last_hand_out_pristine(p) {
            None => {}
            Some((lo, hi)) => {
                let (lo, hi) = (lo - base, hi - base);
                assert!(lo < hi);
                let reloc = 64 * 1024..64 * 1024 + 4096;
                assert!(
                    hi <= reloc.start || lo >= reloc.end,
                    "a relocation destination must never read as pristine: [{lo:#x},{hi:#x})"
                );
                assert!(hi <= h - base, "the high end must never read as pristine");
            }
        }
        // Whatever was reported, the window itself stays clear of both.
        assert!(arena.pristine_hi <= h - base);
        assert!(
            arena.pristine_lo >= 64 * 1024 + 4096 || arena.pristine_hi <= 64 * 1024,
            "window [{:#x},{:#x}) overlaps the relocation target",
            arena.pristine_lo,
            arena.pristine_hi
        );
    }
}

/// gcd d1/d (2026-09-27): the floor-aware give-back behind
/// `GenerationalHeap::uncommit_evacuated_young`'s `-Xms` floor.
#[cfg(test)]
mod gcd_d1d_floor_tests {
    use super::*;

    const MIB: usize = 1024 * 1024;

    /// With a floor, nothing below it goes back and everything above it that
    /// the unfloored give-back would release still does; the floor 0 is the
    /// two unfloored calls exactly.
    #[test]
    fn a_floored_give_back_keeps_the_prefix_and_releases_the_rest() {
        let g = crate::reservation::GRANULE;
        let mut floored = Arena::new(16 * MIB);
        if floored.commit_bits().is_none() {
            // The wholly committed fallback store releases nothing.
            return;
        }
        let committed = floored.commit_initial_prefix(12 * MIB);
        assert_eq!(committed, 12 * MIB);
        let floor = 2 * g;
        let released = floored.decommit_unused_above(floor);
        assert_eq!(
            floored.committed_bytes(),
            floor,
            "the floor's granules stay committed, the rest of the prefix goes back"
        );
        assert_eq!(released, 12 * MIB - floor, "what went back");

        // A floor that does not sit on a granule boundary keeps the granule it
        // lands in (inward rounding of the released span).
        let mut straddled = Arena::new(16 * MIB);
        let _ = straddled.commit_initial_prefix(12 * MIB);
        let _ = straddled.decommit_unused_above(g + 8);
        assert_eq!(straddled.committed_bytes(), 2 * g);

        let mut unfloored = Arena::new(16 * MIB);
        let _ = unfloored.commit_initial_prefix(12 * MIB);
        let mut split = Arena::new(16 * MIB);
        let _ = split.commit_initial_prefix(12 * MIB);
        let whole = unfloored.decommit_unused_above(0);
        let parts = split.decommit_unbumped_middle() + split.decommit_free_blocks();
        assert_eq!(whole, parts, "floor 0 is the two calls");
        assert_eq!(unfloored.committed_bytes(), split.committed_bytes());
        assert_eq!(unfloored.committed_bytes(), 0);
    }
}

/// gce e1/y: the known-zero set of the LOW free list (item 2 of
/// `gcd-d4m-sweep-trigger-follows-the-moving-pause-goal-20260928.md`).
#[cfg(test)]
mod e1y_known_zero_tests {
    use super::*;

    /// A sweep-shaped arena: 1 MiB bumped, two holes listed and claimed zero.
    fn swept() -> Arena {
        let mut a = Arena::new(4 << 20);
        let _ = a.alloc(1 << 20, 8).expect("bump");
        a.add_free_block(4096, 8192);
        a.add_free_block(65536, 4096);
        a.set_known_zero_free_spans(vec![(4096, 8192), (65536, 4096)]);
        a
    }

    #[test]
    fn e1y_a_hand_out_is_known_zero_only_wholly_inside_one_span() {
        let a = swept();
        assert_eq!(a.known_zero_bytes(), 12288);
        assert!(a.hand_out_is_known_zero(4096, 8192), "the whole span");
        assert!(a.hand_out_is_known_zero(4096 + 64, 128), "inside");
        assert!(a.hand_out_is_known_zero(65536, 4096));
        assert!(
            !a.hand_out_is_known_zero(4096 + 8192 - 8, 16),
            "straddles the end"
        );
        assert!(
            !a.hand_out_is_known_zero(4088, 16),
            "starts before the span"
        );
        assert!(!a.hand_out_is_known_zero(32768, 64), "between spans");
        assert!(
            !a.hand_out_is_known_zero(4096, 0),
            "an empty hand-out claims nothing"
        );
        assert!(!a.hand_out_is_known_zero(usize::MAX - 4, 16), "no overflow");
        assert!(
            !Arena::new(1 << 20).hand_out_is_known_zero(0, 8),
            "no set installed"
        );
    }

    #[test]
    fn e1y_doors_that_relist_handed_out_bytes_forget_the_set() {
        let mut a = swept();
        a.add_free_block(262144, 64);
        assert_eq!(a.known_zero_bytes(), 0, "add_free_block");

        let mut a = swept();
        a.clear_free_list();
        assert_eq!(a.known_zero_bytes(), 0, "clear_free_list");

        let mut a = swept();
        let _ = a.rebuild_low_free_list(&[(4096, 8192)]);
        assert_eq!(a.known_zero_bytes(), 0, "rebuild_low_free_list");

        let mut a = swept();
        a.reset();
        assert_eq!(a.known_zero_bytes(), 0, "reset");

        let mut a = swept();
        let _ = a.reset_deferring_zero();
        assert_eq!(a.known_zero_bytes(), 0, "reset_deferring_zero");

        let mut a = swept();
        let _ = a.return_unused_tail(131072, 4096, 0);
        assert_eq!(a.known_zero_bytes(), 0, "return_unused_tail");

        let mut a = swept();
        let _ = a.retract_cursor_to(512 << 10);
        assert_eq!(a.known_zero_bytes(), 0, "retract_cursor_to");
    }

    #[test]
    fn e1y_a_merge_and_a_split_keep_the_set() {
        let mut a = Arena::new(4 << 20);
        let _ = a.alloc(1 << 20, 8).expect("bump");
        // Two ADJACENT holes, so the coalescer has something to merge.
        a.add_free_block(8192, 4096);
        a.add_free_block(12288, 4096);
        a.set_known_zero_free_spans(vec![(8192, 8192)]);
        let _ = a.coalesce_free_list();
        assert_eq!(
            a.known_zero_bytes(),
            8192,
            "coalesce re-lists only listed bytes"
        );
        // A free-list hand-out splits the block; the remnant stays listed and
        // the claim about the bytes stays as it was.
        let before = a.used_low_for_compaction();
        let p = a.alloc(1024, 8).expect("a free-list hand-out");
        let off = p as usize - a.base_ptr() as usize;
        assert!(off < before, "served from the free list, below the cursor");
        assert!(a.hand_out_is_known_zero(off, 1024));
        assert_eq!(a.known_zero_bytes(), 8192);
    }
}

/// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`): the carry primitives.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j2yg2_`.
#[cfg(test)]
mod gcj_j2yg2_known_zero_carry_tests {
    use super::*;

    const K: usize = 1024;

    /// 1 MiB bumped, holes at 4K+8K and 64K+4K listed and claimed.
    fn swept() -> Arena {
        let mut a = Arena::new(4 << 20);
        let _ = a.alloc(1 << 20, 8).expect("bump");
        a.add_free_block(4 * K, 8 * K);
        a.add_free_block(64 * K, 4 * K);
        a.set_known_zero_free_spans(vec![(4 * K, 8 * K), (64 * K, 4 * K)]);
        a
    }

    #[test]
    fn gcj_j2yg2_union_sorts_merges_touching_spans_and_drops_empty_ones() {
        let u = union_known_zero_spans(
            vec![(64, 16), (0, 32), (200, 0)],
            vec![(32, 8), (100, 50), (120, 10)],
        );
        assert_eq!(u, vec![(0, 40), (64, 16), (100, 50)]);
        assert!(union_known_zero_spans(Vec::new(), Vec::new()).is_empty());
    }

    /// Only the claimed bytes that are still on the free list are carried:
    /// a block partly handed out carries its free part, an unclaimed free
    /// block carries nothing.
    #[test]
    fn gcj_j2yg2_known_zero_within_is_the_set_on_the_free_list() {
        let a = swept();
        assert_eq!(
            a.known_zero_within(&a.free_blocks_sorted()),
            vec![(4 * K, 8 * K), (64 * K, 4 * K)]
        );
        // The first hole's head went out (only its tail is free), and an
        // unclaimed block is free too.
        let free = [(6 * K, 6 * K), (32 * K, 4 * K), (64 * K, 4 * K)];
        assert_eq!(a.known_zero_within(&free), vec![(6 * K, 6 * K), (64 * K, 4 * K)]);
        assert!(a.known_zero_within(&[]).is_empty());
        assert!(Arena::new(1 << 20).known_zero_within(&free).is_empty(), "no set");
    }

    /// The keeping door: a free-listed tail joins the set (merged with a
    /// span it touches), a retraction only clips the set at the new cursor.
    /// The plain door still forgets (`e1y_doors_that_relist_handed_out_bytes_forget_the_set`).
    #[test]
    fn gcj_j2yg2_the_keeping_tail_door_keeps_the_set() {
        let mut a = swept();
        assert_eq!(
            a.return_unused_tail_keeping_known_zero(128 * K, 16 * K, 0),
            TailReturn::FreeListed
        );
        assert_eq!(a.known_zero_bytes(), 12 * K + 16 * K);
        assert!(a.hand_out_is_known_zero(128 * K, 16 * K), "the tail is claimed");
        assert!(a.hand_out_is_known_zero(4 * K, 8 * K), "the old spans stay");
        // A tail touching a claimed span merges with it.
        assert_eq!(
            a.return_unused_tail_keeping_known_zero(68 * K, 4 * K, 0),
            TailReturn::FreeListed
        );
        assert!(a.hand_out_is_known_zero(64 * K, 8 * K), "one span 64K..72K");

        // A retraction below every span leaves the set as it was.
        let cursor = a.used_low_for_compaction();
        let before = a.known_zero_bytes();
        assert_eq!(
            a.return_unused_tail_keeping_known_zero(cursor - 8 * K, 8 * K, 0),
            TailReturn::Retracted
        );
        assert_eq!(a.known_zero_bytes(), before);

        // A span that reaches the cursor is clipped at the new one.
        let mut b = Arena::new(4 << 20);
        let _ = b.alloc(1 << 20, 8).expect("bump");
        b.set_known_zero_free_spans(vec![(4 * K, 8 * K), ((1 << 20) - 16 * K, 16 * K)]);
        assert_eq!(
            b.return_unused_tail_keeping_known_zero((1 << 20) - 4 * K, 4 * K, 0),
            TailReturn::Retracted
        );
        assert_eq!(b.known_zero_bytes(), 8 * K + 12 * K);
        assert!(!b.hand_out_is_known_zero((1 << 20) - 4 * K, 8), "above the new cursor");
    }
}

#[cfg(test)]
mod zgc_uncommit_policy_tests {
    use super::*;
    use crate::reservation::GRANULE;
    use std::time::{Duration, Instant};

    #[test]
    fn zgc_uncommit_policy_delays_and_resets_on_reuse() {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        arena.uncommit_policy.as_mut().unwrap().release_window = true;
        let t = Instant::now();
        assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", t), 0);
        assert_eq!(
            arena.decommit_span_at(0, 8 * GRANULE, "test", t + Duration::from_secs(299)),
            0
        );
        // A relocation destination/hand-out restarts the unused interval.
        assert!(arena.commit_for_relocation(4 * GRANULE, GRANULE));
        let freed = arena.decommit_span_at(0, 8 * GRANULE, "test", t + Duration::from_secs(300));
        if arena.data.commit_bits().is_some() {
            assert_eq!(freed, 7 * GRANULE);
            assert!(arena.data.is_committed_at(4 * GRANULE));
            assert_eq!(
                arena.decommit_span_at(
                    4 * GRANULE,
                    5 * GRANULE,
                    "test",
                    t + Duration::from_secs(599)
                ),
                0
            );
            assert_eq!(
                arena.decommit_span_at(
                    4 * GRANULE,
                    5 * GRANULE,
                    "test",
                    t + Duration::from_secs(600)
                ),
                GRANULE
            );
        }
    }

    /// zr z3/fp: the end-of-sweep stamp starts the clock, so the first
    /// give-back a delay later releases; without it that pass only stamps.
    #[test]
    fn zr3fp_a_sweep_stamp_starts_the_uncommit_clock() {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        if arena.data.commit_bits().is_none() {
            return; // wholly-committed fallback store
        }
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        // Middle = the whole arena (nothing bumped), all committed.
        let stamped = arena.stamp_idle_unused();
        assert_eq!(stamped, 8);
        // Already stamped: a second pass keeps the older times.
        assert_eq!(arena.stamp_idle_unused(), 0);
        arena.uncommit_policy.as_mut().unwrap().release_window = true;
        let later = Instant::now() + Duration::from_secs(300);
        assert_eq!(
            arena.decommit_span_at(0, 8 * GRANULE, "test", later),
            8 * GRANULE
        );

        // Control: no stamp, and the same pass only starts the clock.
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        arena.uncommit_policy.as_mut().unwrap().release_window = true;
        assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", later), 0);
        // Off / zero delay: nothing to stamp.
        arena.set_uncommit_policy(0, true, Duration::ZERO);
        assert_eq!(arena.stamp_idle_unused(), 0);
        arena.set_uncommit_policy(0, false, Duration::from_secs(300));
        assert_eq!(arena.stamp_idle_unused(), 0);
    }

    /// zr z3/fp: a release the `-Xms` floor refused keeps its stamp, so the
    /// granule goes back as soon as the floor allows; the old rule restarted
    /// its clock.
    #[test]
    fn zr3fp_a_floor_refused_release_keeps_its_idle_stamp() {
        for keep in [true, false] {
            let mut arena = Arena::new(8 * GRANULE);
            arena.commit_initial_prefix(8 * GRANULE);
            if arena.data.commit_bits().is_none() {
                return;
            }
            arena.set_uncommit_policy(7 * GRANULE, true, Duration::from_secs(300));
            arena.set_keep_refused_idle_stamps(keep);
            arena.uncommit_policy.as_mut().unwrap().release_window = true;
            let t = Instant::now();
            assert_eq!(arena.decommit_span_at(0, 8 * GRANULE, "test", t), 0);
            // One granule above the floor: granule 0 goes, 1..7 are refused.
            let t300 = t + Duration::from_secs(300);
            assert_eq!(
                arena.decommit_span_at(0, 8 * GRANULE, "test", t300),
                GRANULE
            );
            // Commitment above the floor comes back (a relocation destination).
            assert!(arena.commit_for_relocation(0, GRANULE));
            let t301 = t + Duration::from_secs(301);
            let freed = arena.decommit_span_at(0, 8 * GRANULE, "test", t301);
            if keep {
                assert_eq!(freed, GRANULE, "granule 1 has been idle 301 s");
            } else {
                assert_eq!(freed, 0, "the old rule restarted granule 1's clock");
            }
        }
    }

    /// zt t23/hunt4 (`-XX:+AlwaysPreTouch`): turning the bit on touches what
    /// is committed now (the startup commit), a commit that makes a granule
    /// resident touches that whole granule, an already-resident range touches
    /// nothing more, the bit survives a policy change, and no byte changes.
    /// Off: nothing is touched.
    #[test]
    fn zt23hunt4_pre_touch_touches_the_startup_commit_and_later_commits() {
        let mut arena = Arena::new(8 * GRANULE);
        if arena.data.commit_bits().is_none() {
            // The wholly-committed fallback store: nothing commits lazily.
            return;
        }
        arena.commit_initial_prefix(2 * GRANULE);
        arena.set_uncommit_policy(2 * GRANULE, true, Duration::from_secs(300));
        // SAFETY: byte 0 is inside the committed prefix.
        unsafe { *arena.data.as_mut_ptr() = 0x5A };
        assert_eq!(arena.set_pre_touch(false), 0);
        assert!(arena.commit_range_for_use(2 * GRANULE, 8));
        assert_eq!(arena.pre_touched_bytes(), 0, "off: no touch");
        // Granules 0..3 are committed now: one coalesced span.
        let committed = arena.committed_bytes();
        assert_eq!(committed, 3 * GRANULE);
        assert_eq!(arena.set_pre_touch(true), committed);
        assert_eq!(arena.pre_touched_bytes(), committed);
        // SAFETY: byte 0 is committed; the touch wrote back what it read.
        assert_eq!(unsafe { *arena.data.as_ptr() }, 0x5A);
        assert!(arena.commit_range_for_use(0, GRANULE));
        assert_eq!(arena.pre_touched_bytes(), committed, "resident: no touch");
        // A policy change keeps the bit; a new granule is touched whole.
        arena.set_uncommit_policy(GRANULE, true, Duration::from_secs(300));
        assert!(arena.commit_range_for_use(5 * GRANULE + 16, 8));
        assert_eq!(arena.pre_touched_bytes(), committed + GRANULE);
    }

    #[test]
    fn zgc_cursor_retraction_defers_unmapping_until_explicit_give_back() {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        arena.set_uncommit_policy(0, true, Duration::ZERO);
        arena.alloc(4 * GRANULE, 8).unwrap();
        let before = arena.committed_bytes();
        arena.retract_cursor_to(0);
        assert_eq!(arena.committed_bytes(), before);
        let released = arena.decommit_unused_above(0);
        if arena.commit_bits().is_some() {
            assert_eq!(released, before);
        }
    }

    #[test]
    fn zgc_startup_commit_covers_both_allocation_ends_without_growing() {
        let mut arena = Arena::new(32 * GRANULE);
        let before = (arena.cursor, arena.high_cursor, arena.capacity());
        assert_eq!(arena.commit_initial_zgc(8 * GRANULE), 8 * GRANULE);
        assert_eq!((arena.cursor, arena.high_cursor, arena.capacity()), before);
        assert!(arena.data.is_committed_at(0));
        assert!(arena.data.is_committed_at(31 * GRANULE));
        if arena.commit_bits().is_some() {
            assert_eq!(arena.committed_bytes(), 8 * GRANULE);
        }
    }

    #[test]
    fn zgc_uncommit_policy_covers_retraction_and_disabled_mode() {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        arena.set_uncommit_policy(2 * GRANULE + 1, true, Duration::ZERO);
        arena.decommit_unused_above(0);
        assert!(arena.committed_bytes() >= 3 * GRANULE);
        let _ = arena.commit_initial_prefix(8 * GRANULE);
        arena.set_uncommit_policy(0, false, Duration::ZERO);
        let before = arena.committed_bytes();
        assert_eq!(arena.decommit_unused_above(0), 0);
        assert_eq!(arena.committed_bytes(), before);
    }
}

#[cfg(test)]
mod zgc_readable_floor_tests {
    use super::*;
    use crate::reservation::GRANULE;

    #[test]
    fn readable_give_back_never_admits_reset_of_minimum_commitment() {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(4 * GRANULE);
        arena.set_uncommit_policy(4 * GRANULE, true, std::time::Duration::ZERO);
        arena.set_readable_give_back(true);
        let committed = arena.committed_bytes();
        assert_eq!(arena.decommit_unused_above(0), 0);
        assert_eq!(arena.committed_bytes(), committed);
        // Even Windows's reset/recommit-failure path cannot release the floor:
        // no reset is attempted when the commitment equals the minimum.
        if arena.data.commit_bits().is_some() {
            assert_eq!(arena.bytes_reset, 0);
        }
    }
}

/// gci i10/gy (2026-10-13): `Arena::grow` must size itself from the capacity
/// the store reached, not from the request.
#[cfg(test)]
mod gci_i10gy_grow_tests {
    use super::*;
    use crate::reservation::GRANULE;

    /// A grow the OS refuses (no address space for a bigger reservation)
    /// leaves the arena exactly as it was. Before the fix `grow` set
    /// `high_cursor` to the REQUEST while `HeapStore::grow_to` kept the old
    /// length, so `used()` (`len - high_cursor`) underflowed and `remaining()`
    /// promised bytes the arena did not have. 1 EiB is past every 64-bit
    /// user address space (57-bit LA57 included), so the reservation fails.
    /// Only on the reserving store: the wholly committed fallback grows by
    /// `Vec::resize`, which would abort instead of refusing.
    #[test]
    fn a_refused_grow_leaves_the_arena_unchanged() {
        let mut arena = Arena::new(2 * GRANULE);
        arena.set_uncommit_policy(0, true, std::time::Duration::ZERO);
        if arena.data.commit_bits().is_none() {
            return;
        }
        let cap = arena.capacity();
        let base = arena.base_ptr();
        let _ = arena.grow(1usize << 60);
        assert_eq!(arena.capacity(), cap, "a refused reservation keeps the old capacity");
        assert_eq!(arena.base_ptr(), base, "and the old base");
        assert_eq!(arena.high_cursor(), cap, "the high end stays at the real capacity");
        assert_eq!(arena.used(), 0, "an empty arena still reads empty");
        assert_eq!(arena.remaining(), cap, "and offers only what it has");
        assert!(arena.alloc(64, 8).is_some(), "and still allocates");
    }

    /// The ordinary grow inside and past the reservation still lands on the
    /// requested (8-aligned) capacity with the high end at the top.
    #[test]
    fn an_admitted_grow_reaches_the_request() {
        let mut arena = Arena::new(2 * GRANULE);
        let _ = arena.grow(4 * GRANULE + 12);
        assert_eq!(arena.capacity(), 4 * GRANULE + 8);
        assert_eq!(arena.high_cursor(), arena.capacity());
        assert_eq!(arena.used(), 0);
        assert_eq!(arena.remaining(), arena.capacity());
    }
}

#[cfg(test)]
mod zr4_un_tests {
    use super::*;
    use crate::reservation::GRANULE;
    use std::time::{Duration, Instant};

    fn idle_arena(floor: usize, delay: Duration, accounting: bool) -> Option<Arena> {
        let mut arena = Arena::new(8 * GRANULE);
        arena.commit_initial_prefix(8 * GRANULE);
        arena.commit_bits()?;
        arena.set_uncommit_policy(floor, true, delay);
        arena.set_readable_give_back(true);
        arena.set_reset_accounting(accounting);
        Some(arena)
    }

    /// zr z4/un: the uncommitter's pass stamps first, releases a delay later,
    /// and reports when the next granule is due.
    #[test]
    fn zr4un_idle_pass_releases_after_the_delay_only() {
        let Some(mut arena) = idle_arena(0, Duration::from_secs(2), true) else {
            return;
        };
        let t = Instant::now();
        let first = arena.uncommit_idle_pass(t, 0..0, usize::MAX);
        assert_eq!(first.reset + first.decommitted, 0);
        assert_eq!(first.pending, 8);
        assert_eq!(first.next_due, Some(t + Duration::from_secs(2)));
        let later = arena.uncommit_idle_pass(t + Duration::from_secs(2), 0..0, usize::MAX);
        assert_eq!(later.reset + later.decommitted, 8 * GRANULE);
        assert_eq!(arena.net_committed_bytes(), 8 * GRANULE - later.reset);
        // Nothing left: a reset granule is not reset again.
        let again = arena.uncommit_idle_pass(t + Duration::from_secs(9), 0..0, usize::MAX);
        assert_eq!(again, ArenaIdlePass::default());
    }

    /// zt t10/rl3 (t10/x7 R2): `-XX:ZUncommitDelay=18446744073709551615`
    /// (u64::MAX seconds, which HotSpot 25 accepts) made `since + delay`
    /// overflow and panic the uncommitter. Such a delay is never due: the
    /// pass counts every granule pending and releases nothing.
    #[test]
    fn zt10rl3_an_unrepresentable_uncommit_delay_is_never_due() {
        let Some(mut arena) = idle_arena(0, Duration::from_secs(u64::MAX), true) else {
            return;
        };
        let reset_before = arena.bytes_reset;
        let t = Instant::now();
        let first = arena.uncommit_idle_pass(t, 0..0, usize::MAX);
        assert_eq!(first.reset + first.decommitted, 0);
        assert_eq!(first.pending, 8);
        assert_eq!(first.next_due, None);
        let later = arena.uncommit_idle_pass(t + Duration::from_secs(3600), 0..0, usize::MAX);
        assert_eq!(later.reset + later.decommitted, 0);
        assert_eq!(later.pending, 8);
        assert_eq!(arena.bytes_reset, reset_before);
        assert_eq!(arena.net_committed_bytes(), 8 * GRANULE);
    }

    /// zr z4/un: the excluded span (the last slide's forwarding records) and
    /// a handed-out granule are never touched; the budget bounds a pass.
    #[test]
    fn zr4un_idle_pass_skips_exclusion_hand_outs_and_honours_budget() {
        let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
            return;
        };
        let p = arena.alloc(GRANULE, 8).expect("hand-out") as usize;
        let base = arena.base_ptr() as usize;
        assert_eq!(p - base, 0, "the first low hand-out is granule 0");
        let t = Instant::now();
        let one = arena.uncommit_idle_pass(t, 2 * GRANULE..3 * GRANULE, 1);
        assert!(one.budget_hit);
        assert_eq!(one.reset + one.decommitted, GRANULE);
        let rest = arena.uncommit_idle_pass(t, 2 * GRANULE..3 * GRANULE, usize::MAX);
        assert!(!rest.budget_hit);
        // 8 granules - granule 0 (handed out) - granule 2 (excluded) - 1 done.
        assert_eq!(rest.reset + rest.decommitted, 5 * GRANULE);
        assert!(!arena.is_reset_idle(0));
        assert!(!arena.is_reset_idle(2));
    }

    /// zr z4/un: the pass refuses an arena in decommit mode (a decommitted
    /// granule faults on touch, and mutators run beside the pass).
    #[test]
    fn zr4un_idle_pass_refuses_decommit_mode() {
        let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
            return;
        };
        arena.set_readable_give_back(false);
        let before = arena.committed_bytes();
        assert_eq!(
            arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX),
            ArenaIdlePass::default()
        );
        assert_eq!(arena.committed_bytes(), before);
    }

    /// zr z4/un: under reset accounting the `-Xms` floor holds against the NET
    /// commitment; without it (the `=0` arm) the floor test saw the peak and
    /// every granule was reset.
    #[test]
    fn zr4un_reset_accounting_keeps_the_floor_resident() {
        for accounting in [true, false] {
            let Some(mut arena) = idle_arena(3 * GRANULE, Duration::ZERO, accounting) else {
                return;
            };
            let pass = arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX);
            let reset = pass.reset + pass.decommitted;
            if accounting {
                assert_eq!(reset, 5 * GRANULE);
                assert_eq!(arena.net_committed_bytes(), 3 * GRANULE);
                assert_eq!(arena.reset_idle_bytes(), 5 * GRANULE);
            } else {
                assert_eq!(
                    reset,
                    8 * GRANULE,
                    "the old test never saw the commitment fall"
                );
                assert_eq!(arena.net_committed_bytes(), arena.committed_bytes());
            }
        }
    }

    /// zr z4/un: committing a reset granule for use counts it again.
    #[test]
    fn zr4un_a_reused_reset_granule_counts_again() {
        let Some(mut arena) = idle_arena(0, Duration::ZERO, true) else {
            return;
        };
        let _ = arena.uncommit_idle_pass(Instant::now(), 0..0, usize::MAX);
        assert_eq!(arena.net_committed_bytes(), 0);
        let _ = arena.alloc(GRANULE + 8, 8).expect("bump");
        assert_eq!(arena.net_committed_bytes(), 2 * GRANULE);
        // The policy change at VM start keeps the ledger.
        arena.set_uncommit_policy(0, true, Duration::ZERO);
        assert_eq!(arena.net_committed_bytes(), 2 * GRANULE);
    }
}

/// gci i22/dr (P2/yy): [`Arena::take_smallest_block_at_least`], the best-fit-by-size
/// TLAB refill primitive.
#[cfg(test)]
mod gci_i22dr_smallest_whole_block_tests {
    use super::*;

    const K: usize = 1024;

    /// An arena whose low free list holds exactly `holes` (sizes, in order),
    /// each fenced by a live 64-byte object so no two coalesce. Returns the
    /// arena and each hole's offset.
    fn arena_with_holes(holes: &[usize]) -> (Arena, Vec<usize>) {
        let mut a = Arena::new(8 * K * K);
        let base = a.base_ptr() as usize;
        let mut offs = Vec::new();
        for &size in holes {
            let p = a.alloc(size, 8).expect("hole") as usize;
            let _fence = a.alloc(64, 8).expect("fence");
            offs.push(p - base);
        }
        for (&off, &size) in offs.iter().zip(holes) {
            a.add_free_block(off, size);
        }
        (a, offs)
    }

    #[test]
    fn gci_i22dr_refill_takes_the_smallest_whole_block_first() {
        let (mut a, offs) = arena_with_holes(&[K * K, 16 * K, 64 * K]);
        let base = a.base_ptr() as usize;
        let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("16K");
        assert_eq!((p as usize - base, n), (offs[1], 16 * K), "the smallest hole, whole");
        let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("64K");
        assert_eq!((p as usize - base, n), (offs[2], 64 * K));
        assert_eq!(a.largest_free_block(), K * K, "the large block stayed whole");
        let (p, n) = a.take_smallest_block_at_least(16 * K, K * K).expect("1M");
        assert_eq!((p as usize - base, n), (offs[0], K * K), "a block of exactly max is whole");
        assert_eq!(a.free_list_bytes(), 0);
        assert!(a.take_smallest_block_at_least(16 * K, K * K).is_none(), "nothing left");
    }

    #[test]
    fn gci_i22dr_a_block_larger_than_max_gives_a_max_head() {
        let (mut a, offs) = arena_with_holes(&[K * K]);
        let base = a.base_ptr() as usize;
        let (p, n) = a.take_smallest_block_at_least(16 * K, 256 * K).expect("head");
        assert_eq!((p as usize - base, n), (offs[0], 256 * K));
        assert_eq!(a.free_list_bytes(), 768 * K, "the tail went back");
        assert_eq!(a.largest_free_block(), 768 * K);
    }

    #[test]
    fn gci_i22dr_refusals_leave_the_free_list_untouched() {
        let (mut a, _) = arena_with_holes(&[8 * K, 12 * K]);
        let before = a.free_list_bytes();
        assert!(a.take_smallest_block_at_least(16 * K, K * K).is_none(), "no hole reaches min");
        assert!(a.take_smallest_block_at_least(64 * K, 16 * K).is_none(), "min > max");
        a.set_prefer_bump(true);
        assert!(a.take_smallest_block_at_least(4 * K, K * K).is_none(), "prefer_bump");
        a.set_prefer_bump(false);
        assert_eq!(a.free_list_bytes(), before);
        let (_, n) = a.take_smallest_block_at_least(9 * K, K * K).expect("12K");
        assert_eq!(n, 12 * K, "the 8K hole is below min; the 12K one is whole");
        assert_eq!(a.free_list_bytes(), 8 * K);
    }

    #[test]
    fn gci_i22dr_the_small_tier_answers_by_size_too() {
        let (mut a, offs) = arena_with_holes(&[2048, 1024, 3000 & !7]);
        let base = a.base_ptr() as usize;
        let (p, n) = a.take_smallest_block_at_least(1000, 4096).expect("1024");
        assert_eq!((p as usize - base, n), (offs[1], 1024));
        let (p, n) = a.take_smallest_block_at_least(1000, 4096).expect("2048");
        assert_eq!((p as usize - base, n), (offs[0], 2048));
    }
}

#[cfg(test)]
mod zs10sp_min_free_span_tests {
    //! zs s10/sp: `Arena::set_min_free_span`, the fit predicate behind
    //! `CRATONVM_ZGC_RETIRE_SUB_REQUEST_SPANS`.
    use super::*;

    /// A full arena (bump tail spent) whose free list holds one 16-byte
    /// remainder, one 32-byte hole and one 4 KiB span.
    fn full_with_holes() -> Arena {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(arena.alloc(cap, 8).is_some());
        arena.add_free_block(1024, 16);
        arena.add_free_block(2048, 32);
        arena.add_free_block(8192, 4096);
        arena
    }

    /// The positive control: off (`0`), the 16-byte request is served from the
    /// 16-byte remainder -- the CratonVM shape the L4 probes enter `main`
    /// through.
    #[test]
    fn off_a_sixteen_byte_request_takes_the_remainder() {
        let mut arena = full_with_holes();
        let base = arena.data.as_ptr() as usize;
        let p = arena.alloc(16, 8).expect("served") as usize;
        assert_eq!(p - base, 1024, "the 16-byte remainder");
    }

    /// On (`T = 32`, the refused request's size): neither the 16-byte
    /// remainder may serve it; the 32-byte hole may (it is not smaller than
    /// T), and the excess goes back on the list. With nothing of at least T
    /// left, the request is refused (HotSpot's shape after a failed refill),
    /// and turning the rule off serves it again: nothing was lost.
    #[test]
    fn on_no_block_smaller_than_the_threshold_serves() {
        let mut arena = full_with_holes();
        let base = arena.data.as_ptr() as usize;
        let free_before = arena.free_list_bytes();
        arena.set_min_free_span(32);
        let p = arena.alloc(16, 8).expect("the 32-byte hole") as usize;
        assert_eq!(p - base, 2048);
        assert_eq!(
            arena.free_list_bytes(),
            free_before - 16,
            "the excess went back"
        );
        // T = 8 KiB: only fresh bump space would do; there is none.
        arena.set_min_free_span(8 * 1024);
        assert!(
            arena.alloc(16, 8).is_none(),
            "refused: no block of at least T"
        );
        assert!(arena.alloc(24, 8).is_none());
        arena.set_min_free_span(0);
        assert!(
            arena.alloc(16, 8).is_some(),
            "re-admitted: the blocks never left"
        );
    }

    /// A bump TAIL smaller than T is a hole too: with 16 bytes of bump room
    /// left, T = 32 refuses a 16-byte request the tail would serve off.
    #[test]
    fn a_bump_tail_smaller_than_the_threshold_does_not_serve() {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(
            arena.alloc(cap - 16, 8).is_some(),
            "16 bytes of bump room left"
        );
        arena.set_min_free_span(32);
        assert!(
            arena.alloc(16, 8).is_none(),
            "the 16-byte tail is set aside"
        );
        arena.set_min_free_span(0);
        assert!(
            arena.alloc(16, 8).is_some(),
            "and serves again when re-admitted"
        );
    }

    /// A request at least as large as T is unaffected, and the rule never
    /// touches a bump region of at least T.
    #[test]
    fn requests_at_or_above_the_threshold_and_bump_space_are_unaffected() {
        let mut arena = full_with_holes();
        arena.set_min_free_span(32);
        assert!(
            arena.alloc(4096, 8).is_some(),
            "the span serves a big request"
        );
        let mut fresh = Arena::new(64 * 1024);
        fresh.set_min_free_span(2048);
        assert!(
            fresh.alloc(16, 8).is_some(),
            "bump space serves small requests"
        );
        assert_eq!(fresh.min_free_span(), 2048);
        fresh.set_min_free_span(13);
        assert_eq!(fresh.min_free_span(), 16, "rounded to the object grid");
    }
}

#[cfg(test)]
mod zt1sp_min_free_span_review_tests {
    //! zt t1/sp review of the s10/sp fit predicate (`Arena::set_min_free_span`):
    //! the block a fit chooses keeps serving from its rest, and the two
    //! free-list exits the s10/sp cut left without the predicate (the high
    //! end's last resort, the smallest-whole-block refill) now honour it.
    use super::*;

    /// A full arena (bump tail spent) whose only free block is a 4 KiB span.
    fn full_with_one_span() -> Arena {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(arena.alloc(cap, 8).is_some());
        arena.add_free_block(8192, 4096);
        arena
    }

    /// With the `on` floor (2048), two 16-byte requests are served back to
    /// back from the span and its rest stays ONE block: the s10/sp cut
    /// pushed each fit's excess separately, so the second request was cut
    /// from a new 2048-byte unit and 2032 bytes per object were set aside.
    #[test]
    fn a_fit_keeps_the_rest_of_its_block_in_one_piece() {
        let mut arena = full_with_one_span();
        let base = arena.base_ptr() as usize;
        arena.set_min_free_span(2048);
        let p1 = arena.alloc(16, 8).expect("the span") as usize - base;
        let p2 = arena.alloc(16, 8).expect("the rest of the span") as usize - base;
        assert_eq!((p1, p2), (8192, 8208), "bumped inside one block");
        assert_eq!(arena.free_list_bytes(), 4096 - 32);
        assert_eq!(
            arena.largest_free_block(),
            4096 - 32,
            "one block, not slivers"
        );
    }

    /// The control of the test above: off, the same two requests land at the
    /// same offsets (the predicate changes which block is chosen, never how
    /// a chosen block is cut).
    #[test]
    fn off_the_same_requests_land_at_the_same_offsets() {
        let mut arena = full_with_one_span();
        let base = arena.base_ptr() as usize;
        let p1 = arena.alloc(16, 8).expect("the span") as usize - base;
        let p2 = arena.alloc(16, 8).expect("the rest") as usize - base;
        assert_eq!((p1, p2), (8192, 8208));
        assert_eq!(arena.largest_free_block(), 4096 - 32);
    }

    /// The high end's free list is `alloc`'s last resort for a small object.
    /// A 4 KiB high hole below an 8 KiB threshold serves nothing; turning the
    /// rule off serves the request from it (the kill control).
    #[test]
    fn a_high_end_hole_below_the_threshold_does_not_serve() {
        let mut arena = Arena::new(8192);
        let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
        arena.add_free_block(big, 4096);
        assert_eq!(arena.high_free_shape(), (1, 4096, 4096));
        arena.alloc(2048, 8).unwrap();
        arena.alloc(2048, 8).unwrap();
        assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");
        arena.set_min_free_span(8192);
        assert!(
            arena.alloc(64, 8).is_none(),
            "the high hole is below the threshold"
        );
        assert_eq!(
            arena.high_free_shape(),
            (1, 4096, 4096),
            "and it stays listed"
        );
        arena.set_min_free_span(0);
        let p = arena.alloc(64, 8).expect("re-admitted") as usize - arena.base_ptr() as usize;
        assert!(p >= arena.high_cursor());
    }

    /// `take_smallest_block_at_least` (the gci i22/dr refill primitive) skips
    /// blocks below the threshold even when `min` admits them.
    #[test]
    fn the_smallest_whole_block_refill_honours_the_threshold() {
        let mut arena = Arena::new(1024 * 1024);
        let base = arena.base_ptr() as usize;
        let small = arena.alloc(1024, 8).expect("hole") as usize - base;
        let _fence = arena.alloc(64, 8).expect("fence");
        let large = arena.alloc(2048, 8).expect("hole") as usize - base;
        let _fence = arena.alloc(64, 8).expect("fence");
        arena.add_free_block(small, 1024);
        arena.add_free_block(large, 2048);
        arena.set_min_free_span(2048);
        let (p, n) = arena
            .take_smallest_block_at_least(1000, 4096)
            .expect("the 2048-byte hole");
        assert_eq!((p as usize - base, n), (large, 2048));
        assert!(arena.take_smallest_block_at_least(1000, 4096).is_none());
        arena.set_min_free_span(0);
        let (p, n) = arena
            .take_smallest_block_at_least(1000, 4096)
            .expect("the 1024-byte hole, re-admitted");
        assert_eq!((p as usize - base, n), (small, 1024));
    }
}

#[cfg(test)]
mod zt2al_refused_fit_tests {
    //! zt t2/al (`zgc-zt1sp-a-refused-commit-on-a-free-list-fit-drops-the-block-FIXED-20261019.md`):
    //! a free-list fit whose commit is refused puts its span back on its
    //! list (ZGC's `CRATONVM_ZGC_REFUSED_FIT_RELISTS`, default ON); with the
    //! switch off (the kill control) the span is in no list afterwards.
    use super::*;
    use std::time::Duration;

    /// A spent low end whose only free block is a 4 KiB span, with a ZGC-style
    /// uncommit policy and every commit refused from here on.
    fn full_with_one_span_refusing(relist: bool) -> Arena {
        let mut arena = Arena::new(64 * 1024);
        let cap = arena.capacity();
        assert!(arena.alloc(cap, 8).is_some());
        arena.add_free_block(8192, 4096);
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        arena.set_refused_fit_relists(relist);
        arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = true;
        arena
    }

    #[test]
    fn a_refused_low_fit_puts_its_span_back() {
        let mut arena = full_with_one_span_refusing(true);
        assert!(arena.alloc(64, 8).is_none(), "every commit is refused");
        assert_eq!(arena.free_list_bytes(), 4096, "no byte left the free list");
        assert!(arena.refused_fits_relisted() >= 1, "the relist engaged");
        // The bytes are still allocatable once the OS commits again.
        arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = false;
        let base = arena.base_ptr() as usize;
        let p = arena.alloc(64, 8).expect("served once commits succeed") as usize - base;
        assert!((8192..8192 + 4096).contains(&p), "from the relisted span");
    }

    /// The kill control: off, the refused fit's bytes are lost.
    #[test]
    fn off_a_refused_low_fit_drops_its_span() {
        let mut arena = full_with_one_span_refusing(false);
        assert!(arena.alloc(64, 8).is_none());
        assert!(arena.free_list_bytes() < 4096, "the old drop");
        assert_eq!(arena.refused_fits_relisted(), 0);
    }

    /// The high end's last resort (`alloc`'s small-object arm through
    /// `high_fit`) re-lists on the high list.
    #[test]
    fn a_refused_high_fit_puts_its_span_back_on_the_high_list() {
        for relist in [true, false] {
            let mut arena = Arena::new(8192);
            let big = arena.alloc_high(4096, 8).unwrap() as usize - arena.base_ptr() as usize;
            arena.add_free_block(big, 4096);
            arena.alloc(2048, 8).unwrap();
            arena.alloc(2048, 8).unwrap();
            assert_eq!(arena.low_bump_headroom(), 0, "the two ends have met");
            arena.set_uncommit_policy(0, true, Duration::from_secs(300));
            arena.set_refused_fit_relists(relist);
            arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = true;
            assert!(arena.alloc(64, 8).is_none());
            let bytes = arena.high_free_shape().1;
            if relist {
                assert_eq!(bytes, 4096, "re-listed on the high end");
            } else {
                assert_eq!(bytes, 4096 - 64, "off: the old drop");
            }
        }
    }

    /// No policy (every collector but ZGC): the setter is a no-op and nothing
    /// is counted.
    #[test]
    fn an_arena_without_a_policy_never_relists() {
        let mut arena = Arena::new(64 * 1024);
        arena.set_refused_fit_relists(true);
        assert!(arena.uncommit_policy.is_none());
        assert_eq!(arena.refused_fits_relisted(), 0);
    }

    /// zt t6/rv3 (`zgc-zt3x2-*` item 2): a refused commit leaves the span's
    /// idle stamps alone, so the re-listed span is still stamped for the
    /// uncommitter. The positive control is the same call with commits
    /// allowed, which clears the stamp. The refusal sits where the OS's
    /// answer sits, so a clearing moved back above the commit fails here.
    #[test]
    fn a_refused_commit_keeps_the_idle_stamps() {
        let mut arena = Arena::new(64 * 1024);
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        let stamp = std::time::Instant::now();
        let g = 8192 / crate::reservation::GRANULE;
        {
            let policy = arena.uncommit_policy.as_mut().unwrap();
            assert!(!policy.idle_since.is_empty(), "stamping is armed");
            policy.idle_since[g] = Some(stamp);
            policy.refuse_commits_for_test = true;
        }
        assert!(!arena.commit_range_for_use(8192, 4096), "refused");
        assert_eq!(
            arena.uncommit_policy.as_ref().unwrap().idle_since[g],
            Some(stamp),
            "a refused commit must not clear the stamp"
        );
        arena.uncommit_policy.as_mut().unwrap().refuse_commits_for_test = false;
        assert!(arena.commit_range_for_use(8192, 4096), "committed");
        assert_eq!(
            arena.uncommit_policy.as_ref().unwrap().idle_since[g],
            None,
            "positive control: a granted commit clears it"
        );
    }

    /// The choice survives a later policy change, as the stamp choice does.
    #[test]
    fn the_relist_choice_survives_a_policy_change() {
        let mut arena = Arena::new(64 * 1024);
        arena.set_uncommit_policy(0, true, Duration::from_secs(300));
        arena.set_refused_fit_relists(true);
        arena.set_uncommit_policy(4096, false, Duration::ZERO);
        assert!(arena.uncommit_policy.as_ref().unwrap().refused_fit_relists);
    }
}

#[cfg(test)]
mod zt2al_zero_descending_tests {
    //! zt t2/al (`CRATONVM_ZGC_CHUNK_ZERO_DESCENDING`): `ZeroPlan::zero_descending`
    //! writes exactly the bytes `ZeroPlan::zero` writes, whatever the plan and
    //! the piece size -- only the store order differs.
    use super::*;

    const LEN: usize = 64 * 1024;
    const MARK: u8 = 0x5C;

    /// Run `zero` and `zero_descending(piece)` on two copies of the same dirty
    /// buffer and return both results.
    fn both(plan_of: impl Fn(usize) -> Option<ZeroPlan>, piece: usize) -> (Vec<u8>, Vec<u8>) {
        let mut a = vec![MARK; LEN];
        let mut b = vec![MARK; LEN];
        let pa = plan_of(a.as_ptr() as usize);
        let pb = plan_of(b.as_ptr() as usize);
        // SAFETY: each buffer is `LEN` writable bytes owned here, and each
        // plan's ranges are clamped inside its own buffer.
        unsafe {
            ZeroPlan::zero(pa.as_ref(), a.as_mut_ptr(), LEN);
            ZeroPlan::zero_descending(pb.as_ref(), b.as_mut_ptr(), LEN, piece);
        }
        (a, b)
    }

    #[test]
    fn descending_zeroing_writes_exactly_what_zero_writes() {
        let plans: [&dyn Fn(usize) -> Option<ZeroPlan>; 4] = [
            &|_| None,
            // Known-zero at both ends and in the middle, one dirty stamp in it.
            &|base| {
                Some(ZeroPlan {
                    known_zero: vec![
                        (base, base + 512),
                        (base + 20_000, base + 30_008),
                        (base + LEN - 64, base + LEN),
                    ],
                    pristine_bytes: 0,
                    slide_bytes: 0,
                    dirty_stamps: vec![base + 20_016],
                })
            },
            // The whole hand-out is known zero.
            &|base| {
                Some(ZeroPlan {
                    known_zero: vec![(base, base + LEN)],
                    pristine_bytes: LEN,
                    slide_bytes: 0,
                    dirty_stamps: Vec::new(),
                })
            },
            // Adjacent ranges and an unaligned gap.
            &|base| {
                Some(ZeroPlan {
                    known_zero: vec![(base + 8, base + 4096), (base + 4096, base + 4100)],
                    pristine_bytes: 0,
                    slide_bytes: 4092,
                    dirty_stamps: vec![base + 16, base + LEN - 8],
                })
            },
        ];
        for (i, plan_of) in plans.iter().enumerate() {
            for piece in [0, 8, 100, 4096, 16 * 1024, 1 << 20] {
                let (a, b) = both(plan_of, piece);
                assert!(a == b, "plan {i}, piece {piece}: the two zeroings differ");
            }
        }
        // The positive control: with no plan both zero everything.
        let (a, _) = both(&|_| None, 4096);
        assert!(a.iter().all(|&x| x == 0));
        // ...and a known-zero range is left untouched by both (it read MARK
        // here, which a real plan never allows, so the skip is visible).
        let (a, b) = both(plans[2], 4096);
        assert!(a.iter().all(|&x| x == MARK) && b.iter().all(|&x| x == MARK));
    }
}

/// JIT round 23 wave 4, lane L4: the lazy-dirty set (`ZeroSets::lazy_dirty`).
#[cfg(test)]
mod r23_w4_lazy_dirty_tests {
    use super::*;

    /// Spans merge; a hand-out overlapping one is reported and removed (and
    /// only the overlap is removed); one that misses is not.
    #[test]
    fn r23_w4_take_lazy_dirty_reports_and_removes_only_the_overlap() {
        let mut a = Arena::new(64 * 1024);
        a.note_lazy_dirty(&[(4096, 1024), (5120, 1024), (16 * 1024, 512)]);
        assert_eq!(a.zero_sets.lazy_dirty.as_vec(), vec![(4096, 2048), (16 * 1024, 512)]);
        assert_eq!(a.lazy_dirty_end(), 16 * 1024 + 512);
        assert_eq!(a.take_lazy_dirty(0, 4096), None, "touching is not overlapping");
        assert_eq!(a.take_lazy_dirty(5000, 200), Some((5000, 5200)));
        assert_eq!(
            a.zero_sets.lazy_dirty.as_vec(),
            vec![(4096, 904), (5200, 944), (16 * 1024, 512)]
        );
        assert_eq!(
            a.take_lazy_dirty(16 * 1024 - 64, 1 << 20),
            Some((16 * 1024, 16 * 1024 + 512)),
            "a hand-out past it reports the clipped dirty extent"
        );
        assert_eq!(a.lazy_dirty_end(), 5200 + 944);
        assert_eq!(a.take_lazy_dirty(8192, 64), None);
    }

    /// t1 lane F (`gcj-j2yg2` re-measure): the counted take reports the exact
    /// dirty bytes inside the hand-out -- the gaps between spans and the parts
    /// outside the hand-out are not counted -- and removes what the plain
    /// take removes.
    #[test]
    fn t1_f_take_lazy_dirty_counted_sums_only_the_clipped_overlap() {
        let mut a = Arena::new(64 * 1024);
        a.note_lazy_dirty(&[(1000, 100), (1200, 100), (1400, 100)]);
        // [1050, 1450): 50 of the first, all 100 of the second, 50 of the third.
        assert_eq!(a.take_lazy_dirty_counted(1050, 400), Some(((1050, 1450), 200)));
        assert_eq!(a.zero_sets.lazy_dirty.as_vec(), vec![(1000, 50), (1450, 50)]);
        assert_eq!(a.take_lazy_dirty_counted(1100, 300), None, "the hole is clean");
        assert_eq!(a.take_lazy_dirty_counted(0, 1 << 20), Some(((1000, 1500), 100)));
        assert_eq!(a.lazy_dirty_end(), 0);
    }

    /// R-L4z item 3: the B-tree set merges in any insertion order, joins a
    /// span that bridges several, and a removal spanning several spans keeps
    /// both clipped ends.
    #[test]
    fn r23_w4_review_lazy_set_merges_and_splits_like_the_sorted_vec() {
        let mut s = LazyDirtySet::default();
        s.insert(100, 200);
        s.insert(300, 400);
        s.insert(500, 600);
        s.insert(200, 300); // touches both neighbours
        assert_eq!(s.as_vec(), vec![(100, 300), (500, 100)]);
        s.insert(50, 700); // swallows everything
        assert_eq!(s.as_vec(), vec![(50, 650)]);
        s.insert(800, 900);
        assert_eq!(s.remove(600, 850), Some((600, 850)));
        assert_eq!(s.as_vec(), vec![(50, 550), (850, 50)]);
        assert_eq!(s.remove(0, 40), None);
        assert_eq!(s.remove(700, 800), None, "a gap");
        assert_eq!(s.overlapping(500, 860), vec![(50, 600), (850, 900)]);
        assert_eq!(s.end(), 900);
    }

    /// R-L4z item 2: the mirror marks exactly the noted bytes; a hand-out, a
    /// forget and a reset clear exactly theirs; a suspend/restore round trip
    /// leaves the set's bytes marked and the forgotten copies unmarked.
    #[test]
    fn r23_w4_review_mirror_tracks_the_set_exactly() {
        let mut a = Arena::new(64 * 1024);
        let base = a.base_ptr() as usize;
        assert!(!a.lazy_dirty_contains(base + 4096), "no mirror yet");
        assert!(a.lazy_dirty_mirror().is_none());
        a.note_lazy_dirty(&[(4096, 1024), (8192, 4096)]);
        assert!(a.lazy_dirty_mirror().is_some());
        assert!(a.lazy_dirty_contains(base + 4096));
        assert!(a.lazy_dirty_contains(base + 4096 + 1016));
        assert!(!a.lazy_dirty_contains(base + 4096 + 1024), "end is exclusive");
        assert!(!a.lazy_dirty_contains(base + 4088));
        // A hand-out clears exactly its bytes.
        assert!(a.take_lazy_dirty(4096 + 512, 64).is_some());
        assert!(!a.lazy_dirty_contains(base + 4096 + 512));
        assert!(!a.lazy_dirty_contains(base + 4096 + 568));
        assert!(a.lazy_dirty_contains(base + 4096 + 504));
        assert!(a.lazy_dirty_contains(base + 4096 + 576));
        // An in-place cycle: suspend over its destination, copy, forget the
        // copy, restore.
        a.suspend_lazy_mirror_over(&[(8192, 4096)]);
        assert!(!a.lazy_dirty_contains(base + 9000 - 9000 % 8));
        a.forget_lazy_dirty_over(&[(8192 + 1024, 256)]);
        a.restore_lazy_mirror_over(&[(8192, 4096)]);
        assert!(a.lazy_dirty_contains(base + 8192));
        assert!(!a.lazy_dirty_contains(base + 8192 + 1024), "the live copy");
        assert!(!a.lazy_dirty_contains(base + 8192 + 1272));
        assert!(a.lazy_dirty_contains(base + 8192 + 1280));
        assert!(a.lazy_dirty_contains(base + 4096), "outside the destination, untouched");
        // A reset clears every bit.
        a.reset();
        assert!(!a.lazy_dirty_contains(base + 4096));
        assert!(!a.lazy_dirty_contains(base + 8192));
        assert!(!a.lazy_dirty_contains(base + 12280));
    }

    /// gcj yg3-residuals: forgetting touching survivors as one run leaves the
    /// set and the mirror exactly as forgetting each survivor alone does.
    #[test]
    fn gcj_yg3r_forgetting_touching_spans_as_a_run_matches_one_by_one() {
        let dirty = [(4096, 1024), (8192, 4096), (16384, 512)];
        // Survivors: three touching ones straddling a dirty edge, a gap, two
        // touching ones inside a span, and one over clean bytes.
        let kept = [
            (4096 + 896, 64),
            (4096 + 960, 64),
            (4096 + 1024, 64),
            (8192 + 64, 32),
            (8192 + 96, 32),
            (12288 + 256, 64),
        ];
        let mut runs = Arena::new(64 * 1024);
        let mut each = Arena::new(64 * 1024);
        runs.note_lazy_dirty(&dirty);
        each.note_lazy_dirty(&dirty);
        runs.forget_lazy_dirty_over(&kept);
        for &k in &kept {
            each.forget_lazy_dirty_over(&[k]);
        }
        assert_eq!(
            runs.zero_sets.lazy_dirty.as_vec(),
            each.zero_sets.lazy_dirty.as_vec()
        );
        let (rb, eb) = (runs.base_ptr() as usize, each.base_ptr() as usize);
        for off in (0..32 * 1024).step_by(8) {
            assert_eq!(
                runs.lazy_dirty_contains(rb + off),
                each.lazy_dirty_contains(eb + off),
                "mirror differs at offset {off}"
            );
        }
        assert!(!runs.lazy_dirty_contains(rb + 4096 + 896), "a survivor");
        assert!(runs.lazy_dirty_contains(rb + 4096 + 888), "dead bytes stay");
    }

    /// `reset` zeroes up to the set's end even ABOVE the cursor, and drops the
    /// set: the bump tail reads zero again.
    #[test]
    fn r23_w4_reset_zeroes_the_lazy_dirty_tail_above_the_cursor() {
        let mut a = Arena::new(64 * 1024);
        let p = a.alloc(1024, 8).expect("room") as usize;
        let base = a.base_ptr() as usize;
        assert_eq!(p, base);
        // Stale bytes above the cursor, as a pinned in-place cycle leaves them.
        assert!(a.commit_for_relocation(8192, 4096), "commit the tail page");
        // SAFETY: committed arena memory past the cursor, owned by the test.
        unsafe { std::ptr::write_bytes((base + 8192) as *mut u8, 0xAB, 256) };
        a.note_lazy_dirty(&[(8192, 256)]);
        a.reset();
        assert_eq!(a.lazy_dirty_end(), 0, "the set is dropped");
        // SAFETY: as above.
        let tail = unsafe { std::slice::from_raw_parts((base + 8192) as *const u8, 256) };
        assert!(tail.iter().all(|&b| b == 0), "reset zeroed the dirty tail");
    }

    /// `reset_deferring_zero` hands the dirty tail to the wipe.
    #[test]
    fn r23_w4_the_deferred_wipe_covers_the_lazy_dirty_tail() {
        let mut a = Arena::new(64 * 1024);
        let _ = a.alloc(1024, 8).expect("room");
        a.note_lazy_dirty(&[(8192, 256)]);
        let wipe = a.reset_deferring_zero();
        assert!(wipe.low_end >= 8192 + 256);
        assert_eq!(a.lazy_dirty_end(), 0);
    }
}

/// JIT round 23 wave 4c, lane L4: the allocator-anchor certificate the lazy
/// young object-start grid relies on (`Arena::forget_alloc_anchors_inside`).
#[cfg(test)]
mod r23_w4c_anchor_certificate_tests {
    use super::*;

    /// A hand-out forgets every anchor strictly inside its bytes (an earlier
    /// layout's), keeps one at its base, and leaves anchors outside alone.
    #[test]
    fn r23_w4c_a_hand_out_forgets_the_anchors_inside_it() {
        let mut a = Arena::new(256 * 1024);
        let bucket = 1usize << a.anchor_shift;
        // Stale anchors from an "earlier layout" of bytes not yet bumped over.
        a.note_object_start(8);
        a.note_object_start(bucket + 16);
        a.note_object_start(5 * bucket);
        let p = a.alloc(2 * bucket, 8).expect("bump hand-out") as usize;
        let off = p - a.data.as_ptr() as usize;
        assert_eq!(off, 0, "a fresh arena bumps from its base");
        a.note_object_start(off);
        assert_eq!(
            a.alloc_anchors_snapshot(),
            vec![0, 5 * bucket],
            "8 and bucket+16 lay inside [0, 2 buckets): forgotten; the base and \
             the anchor past the hand-out stay"
        );
    }

    /// The hand-out's base is never forgotten, even when it is the anchor.
    #[test]
    fn r23_w4c_the_base_anchor_survives_its_own_hand_out() {
        let mut a = Arena::new(256 * 1024);
        a.note_object_start(0);
        let _ = a.alloc(4096, 8).expect("bump hand-out");
        assert_eq!(a.alloc_anchors_snapshot(), vec![0]);
    }
}

/// JIT round 23 wave 8, lane W8-L3 (`CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES`):
/// the plain-store mirror ends every operation with the locked arm's words.
#[cfg(test)]
mod r23_w8_lazy_mirror_plain_store_tests {
    use super::*;

    fn run(edits: &[(&str, Option<&str>)]) -> (bool, Vec<u64>, Vec<(usize, usize)>) {
        let mut a = Arena::new(256 * 1024);
        cratonvm_types::flags::with_thread_overrides(edits, || {
            a.note_lazy_dirty(&[(4096, 8192), (20_000 & !7, 3000 & !7), (64 * 1024, 64 * 1024)]);
        });
        let plain = a.zero_sets.lazy_dirty.plain_stores;
        let mut hits = Vec::new();
        for &(o, l) in &[(4096 + 512, 1024), (19_000 & !7, 2048), (64 * 1024 + 8, 100 * 1024)] {
            if let Some(h) = a.take_lazy_dirty(o, l) {
                hits.push(h);
            }
        }
        a.restore_lazy_mirror_over(&[(0, 256 * 1024)]);
        let m = a.lazy_dirty_mirror().expect("a mirror");
        let words = (0..m.word_count()).map(|w| m.word_at(w)).collect();
        (plain, words, hits)
    }

    #[test]
    fn r23_w8_plain_store_mirror_matches_the_locked_mirror() {
        let (plain_on, words_on, hits_on) =
            run(&[("CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES", None)]);
        let (plain_off, words_off, hits_off) =
            run(&[("CRATONVM_GEN_LAZY_MIRROR_PLAIN_STORES", Some("0"))]);
        assert!(plain_on, "default ON, latched at mirror creation");
        assert!(!plain_off, "`0` keeps the locked arm");
        assert_eq!(words_on, words_off);
        assert_eq!(hits_on, hits_off);
        assert!(words_on.iter().any(|&w| w != 0), "the fixture leaves dirty bits");
    }
}

/// t1 wave 8 lane CARDTBL (`CRATONVM_GC_WHOLE_HEAP_YOUNG_HEADROOM`): an arena
/// built with headroom grows in place, inside its reservation.
#[cfg(test)]
mod t1w8_cardtbl_arena_tests {
    use super::*;
    use crate::reservation::GRANULE;

    #[test]
    fn t1w8_cardtbl_headroom_arena_grows_without_moving_its_base() {
        let Some(mut a) = Arena::with_headroom(GRANULE, 4 * GRANULE) else {
            // A host that refuses reservations (or the reserve kill switch)
            // keeps the historical arena; nothing to check.
            return;
        };
        assert_eq!(a.capacity(), GRANULE);
        assert!(a.in_place_capacity() >= 4 * GRANULE);
        let base = a.base_ptr();
        a.grow(4 * GRANULE);
        assert_eq!(a.capacity(), 4 * GRANULE);
        assert_eq!(a.base_ptr(), base, "growth inside the reservation is in place");
        let p = a.alloc(64, 8).expect("an allocation in the grown arena");
        assert!(p as usize >= base as usize && (p as usize) < base as usize + 4 * GRANULE);
    }

    #[test]
    fn t1w8_cardtbl_headroom_arena_refuses_a_reserve_at_or_below_capacity() {
        assert!(Arena::with_headroom(GRANULE, GRANULE).is_none());
        assert!(Arena::with_headroom(2 * GRANULE, GRANULE).is_none());
        let plain = Arena::new(GRANULE);
        assert!(plain.in_place_capacity() >= plain.capacity());
    }
}
