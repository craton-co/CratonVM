// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The sweep, as a shardable unit.
//!
//! Split out of `zgc.rs` on 2026-09-02. The sweep is the largest single term
//! in this collector's pause -- 170 ms of a 265 ms mean on the
//! 13.0M-dead-object cycle the 2026-08-17 anatomy measured -- and it is the
//! phase most likely to be worked on, so it is worth being able to read it
//! without a 24,000-line file around it.
//!
//! A CHILD module of `zgc`: `impl super::ZgcRealHeap` here reaches the heap's
//! private fields exactly as the parent does, so nothing needed widening.
//!
//! The per-object body ([`super::ZgcRealHeap::sweep_one`]) is written once and
//! called by both drivers, which is what stops the serial and sharded paths
//! drifting on the only question that matters -- which objects are reclaimed.

use std::sync::atomic::Ordering;

use crate::heap::{ObjectHeader, GC_FLAG_MARKED, HEADER_SIZE};

use super::{ZObjectStartsSnapshot, ZgcRealHeap, Z_PARMARK_MAX_WORKERS};

// The sweep, as a shardable unit
// ---------------------------------------------------------------------------
//
// The whole-heap sweep is the largest single term in this collector's pause:
// 170 ms of a 265 ms mean on the 13.0M-dead-object cycle the 2026-08-17 anatomy
// measured, against 6.3 ms of 112 ms for a young cycle's bounded one. It is
// also, unlike the mark, embarrassingly parallel -- a linear scan over a bitmap
// whose only shared output is the arena free list.
//
// The two types below are what make that true rather than merely plausible.
// `ZSweepShard` is everything one worker produces, and nothing in it is shared;
// `ZgcRealHeap::sweep_one` is the per-object body, written once and called by
// both the serial and the parallel driver, so the two cannot drift on the
// question that matters -- which objects are reclaimed.
//
// PARALLEL MARKING IS STILL A LOSS, but not for the reason recorded here
// until 2026-09-02, and the difference is worth carrying because it says
// where to look next.
//
// The 2026-08-14 measurement was +31% at one worker and +153% at four --
// RISING with worker count, which is the signature of contention, and the
// note here read it as "coherence traffic on shared queues". Re-measured on
// BinTreesClassic 16 at -Xmx192m, release, after the per-edge shared counters
// and the unconditional claim RMW came out of the drain loop (`mark_roots`,
// `ZObjectStartBits::claim`), mark_us per cycle:
//
//     workers   0 (serial)      1        2        4
//               4107 5277    8720 13842  8169 9820  6691 7040
//
// Serial still wins by 2x, but the cost now FALLS as workers are added
// instead of rising. That inverts the diagnosis: what is left is not
// contention, it is a fixed per-cycle charge that more workers amortise --
// and `ZMarkCoordinator::new` spawns N OS threads on every cycle, with
// `mark_with_controller_stw` spawning a controller on top. A persistent pool
// parked between cycles is the next thing to try; chasing locks is not.
//
// None of it carries over to the SWEEP either way. A mark is a dependent
// pointer chase, memory-LATENCY bound; a sweep is a linear scan with an
// independent, mostly-store body, bandwidth bound. That is an argument rather
// than a measurement, which is why sharding ships default-off behind
// `CRATONVM_ZGC_PARSWEEP` -- and see that function for the further reason it
// is currently unreachable.

/// `CRATONVM_ZGC_SWEEP_PAGE_CENSUS=1` -- count, once per whole-heap sweep, how
/// many of this heap's 2 MiB logical pages hold **no surviving object at
/// all**. **Default OFF.**
///
/// # The number this exists to take
///
/// `docs/feature-designs/zgc-architecture.md`
/// argues that the sweep can be made `O(pages)` instead of `O(registered
/// objects)` -- it is 170 ms of a 265 ms mean pause, and `words =
/// arena_capacity / 512` whatever the garbage ratio. The argument's load-
/// bearing premise is one sentence:
///
/// > *"On the measured 13.0M-dead-object cycle almost every low-region page is
/// > wholly dead after evacuation, because evacuation is what emptied it."*
///
/// **That has never been measured.** A wholly-dead page is the thing a
/// page-based reclaim frees for free -- return the page, no per-object work --
/// so the fraction of pages that are wholly dead *is* the expected saving. If
/// it is small, stage 4 of that proposal buys a rename; if it is large, it
/// buys the pause. The round's standard for a default flip is a number, and
/// this is the cheapest number in the programme: the answer is already in the
/// two bitmaps the sweep is holding.
///
/// # Why it is a flag and not unconditional
///
/// The census is one extra streaming pass over both bitmaps -- ~16 MB read for
/// a 512 MB arena, sequential, no header reads and no writes. That is the same
/// order as the `want_dead` pre-pass this module already pays for, but that
/// one pays for itself (it sizes a 96 MB allocation exactly); this one is pure
/// instrumentation, and instrumentation inside the pause is charged to every
/// cycle whether or not anybody is reading it.
///
/// Read through [`cratonvm_types::flags::runtime_var_os`] so it layers with
/// `-XX:` like every other switch, and read **per cycle** rather than latched,
/// so a suite can turn it on mid-run. Declared in `types/src/flag_groups.rs`
/// (`zgc-sweep-page-census`, since 2026-09-21), which closed
/// `docs/feature-designs/zgc-architecture.md`.
pub(crate) fn zgc_sweep_page_census_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_SWEEP_PAGE_CENSUS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        None => false,
    }
}

/// `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS` (zr z1/al, **default ON**; `0` /
/// `off` / `false` / `no` restores the retention byte for byte).
///
/// # The defect
///
/// `ZgcRealHeap::alloc_size` is a corruption screen as well as a sizer: it
/// refuses any array whose payload exceeds `MAX_PLAUSIBLE_BODY` (256 MiB:
/// `(1 << 24) * SLOT_SIZE`, and `SLOT_SIZE` is 16) or
/// whose length exceeds `MAX_PLAUSIBLE_ARRAY_LEN` (`1 << 28`). The allocator
/// admits arrays up to `i32::MAX` elements, so `new byte[300_000_000]` is legal
/// and unsizable. Such an array is at least `ZGC_LARGE_OBJECT_MIN`, so it is
/// served from the large-object end, and when it dies the complement sweep
/// hands it to [`ZgcRealHeap::sweep_one_high`] -- which RETAINS every base it
/// cannot size (`ZSweepShard::unsizable_high`), and re-registers it after the
/// bulk prune. A dead 300 MB array was therefore never reclaimed: a loop that
/// allocates and drops one per iteration ran out of heap after a handful of
/// iterations at `-Xmx1g`, where HotSpot ZGC runs it indefinitely.
///
/// # The fix
///
/// For an ARRAY header only, the high-end sweep sizes the array exactly from
/// its header and accepts the size only when every independent check agrees
/// that it is the array's real extent ([`ZgcRealHeap::oversize_array_extent`]):
/// the extent lies inside the large-object end, no other base (in this
/// cycle's snapshot or in the live registry) starts inside it, and no arena
/// free block overlaps it. A clobbered length that passes all three describes
/// bytes no other object and no free block owns, so reclaiming them cannot
/// hand out memory twice; anything that fails a check keeps the old
/// retention. The marker, the slide and every other `alloc_size` consumer are
/// unchanged.
///
/// Read once per heap, in `ZgcRealHeap::with_capacity`.
pub(crate) fn zgc_sweep_sizes_oversize_arrays_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS")
}

/// `CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE` -- **default ON** (zs s2/rf), `=0`
/// restores the old retention.
///
/// The fix above reached only the complement sweep's large-object arm
/// (`sweep_one_high`). The PER-OBJECT sweep (`ZgcRealHeap::sweep_one`: every
/// young cycle, the fallback arm when the complement refuses -- for example
/// `CRATONVM_ZGC_SWEEP_DEAD_RUNS=0` or a hash registry -- and the spill set)
/// still retained every dead array `alloc_size` refuses, in either region, so
/// a loop that drops one 300 MB array per iteration ran out of heap on those
/// arms as it did before zr z1/al. With this switch `sweep_one` frees it when
/// `oversize_array_extent` (checks against the cycle's immutable snapshot, the
/// live registry, the arena free list and the published TLAB tails; for a low
/// extent also the true low bump cursor) accepts its extent.
///
/// Inert under `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS=0` (that screen's
/// own kill). Read once per heap, in `ZgcRealHeap::with_capacity`.
pub(crate) fn zgc_sweep_one_frees_oversize_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE")
}

/// `CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS` -- **default ON** (zs s2/rf), `=0`
/// restores the old predicate. A spilled (off-grid) dead base in the
/// large-object end, swept per object after the complement rebuilt the low
/// free list, used to be judged "already reclaimed by the complement" when its
/// extent ended below `Arena::used` (low cursor plus the large-object end's
/// bytes), so its span was dropped while its registry bit was cleared. The
/// complement never rebuilds the large-object end's free list, so those bytes
/// were lost. Read once per heap.
pub(crate) fn zgc_sweep_spill_high_spans_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS")
}

/// `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS=1` -- on the complement (bitmap)
/// sweep, do NOT zero a dead low-region object's header, and when nothing
/// downstream wants the dead addresses count a word's dead objects with one
/// `popcount` instead of visiting each. **Default OFF** (zr z1/al,
/// 2026-10-12); read per cycle, like the page census, so a suite can A/B it in
/// one process.
///
/// # Why the header write is the dead side's whole cost
///
/// The complement pass reads no dead header (it sizes LIVE objects only), so
/// its per-dead-object work is one 16-byte store at the dead base. For the
/// small-object garbage this collector is measured on (72-byte `bintrees`
/// nodes) those stores touch nearly every cache line of the reclaimed volume,
/// each a read-for-ownership miss -- the header-only zeroing that replaced the
/// body memset (`zgc_sweep_header_zero`) stopped being a saving once objects
/// are about one line long. With this switch the dead side of the pass is
/// `O(bitmap words)`, not `O(dead objects)`.
///
/// # Why skipping it is sound (and why it is still opt-in)
///
/// After the pass, `retain_marked` (`starts &= marks`) has unregistered every
/// dead base, and every reader of arena bytes on this heap goes through the
/// registry before it decodes a header: `is_object_address`,
/// `resolve_interior_for_pin`, `is_heap_addr`'s nearest-base walk,
/// `inflight_cover_start`, the frag report's wall walk, the slide's live set.
/// Every byte the free list hands out is zeroed at hand-out (`alloc_raw`'s
/// `ZeroPlan`, `carve_tlab_chunk`'s chunk memset); a dead object's span is
/// never in the pristine or slide known-zero sets (it was handed out once,
/// which removed it), and its BODY is already left dirty by the default arm,
/// so its header being dirty changes no zeroing decision. The monitor
/// cleanup's dead list carries addresses only, and today's default already
/// hands it zeroed headers, so it cannot depend on their contents.
///
/// What it gives up is a debugging tripwire, not a correctness property: a
/// stale reference (a VM bug) into reclaimed memory decodes `ClassId(0)` with
/// the store and is reported by the reclaimed-receiver verdicts
/// (`vm::memory::reclaim_guard`, `ZgcRealHeap::reclaimed_hole_at`) or the
/// "Stale pointer detected" fallbacks (`dispatch_virtual.rs`, `invoke.rs`);
/// without it, it reads the dead object's old header until the span is handed
/// out again, which is what HotSpot does (it never writes a dead object).
///
/// # Default ON since zr z8/zb (2026-10-13), `=0` kill
///
/// z5a measured the arm at -20 % on `Zr3JaRefArrayAlloc 1 10 64 prim 400`
/// (54-60 -> 45-49 ns/alloc CPU) with every soundness row SAME. The `vm/`
/// audit the opt-in waited for (z8/zb report, finding 4) found no reader that
/// depends on a dead header being zero for an answer: every heap walk, pin
/// probe, conservative root and the terminal verdicts are registry or
/// free-list based, and the two zero-header readers are VM-bug recoveries
/// whose `ClassId(0)` arm only ever ran in the sweep-to-reuse window. A run
/// hunting a stale reference sets `=0` (or `CRATONVM_DBG_ZGC_CORPSE`, which
/// forces the store) to get the tripwire back.
/// The deciding rows: `docs/feature-designs/zgc-architecture.md`.
pub(crate) fn zgc_sweep_skip_dead_headers_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            // An empty value keeps its old meaning (off): it was the opt-in
            // switch's "unset" spelling.
            !(v.is_empty() || matches!(v.as_str(), "0" | "false" | "off" | "no"))
        }
        // The VM's stale-reference hunts read the zeroed header (the
        // all-zero-receiver probes behind them), so an unset switch keeps the
        // store for them; an explicit `=1` still wins.
        None => ZGC_SWEEP_SKIP_DEAD_HEADERS_DEFAULT_ON && !stale_reference_hunt_requested(),
    }
}

/// Is one of the VM's stale-reference diagnostics armed? Each reads an
/// all-zero header as "the sweep reclaimed this": `CRATONVM_DBG_MTROOTS`
/// (`gc_and_alloc::mtroots_selfcheck`), `CRATONVM_DBG_STALE_RECV` and
/// `CRATONVM_DBG_BUG03` (`invoke.rs`, the invokevirtual stale-receiver
/// fallback). Three environment probes, once per cycle.
fn stale_reference_hunt_requested() -> bool {
    STALE_REFERENCE_HUNTS
        .iter()
        .any(|k| cratonvm_types::flags::runtime_var_os(k).is_some())
}

/// The switches [`stale_reference_hunt_requested`] reads.
const STALE_REFERENCE_HUNTS: [&str; 3] = [
    "CRATONVM_DBG_MTROOTS",
    "CRATONVM_DBG_STALE_RECV",
    "CRATONVM_DBG_BUG03",
];

/// What an unset `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` means (zr z8/zb).
pub(crate) const ZGC_SWEEP_SKIP_DEAD_HEADERS_DEFAULT_ON: bool = true;

/// Was `CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS` set in the environment at all?
/// The per-cycle `[GC] zgc-sweep-skip-dead-headers:` line is printed only
/// then, so the default flip adds no stderr line to a `--verbose:gc` run.
pub(crate) fn zgc_sweep_skip_dead_headers_explicit() -> bool {
    cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS").is_some()
}

/// `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL` -- make the whole-heap complement
/// sweep's dead side cost nothing per dead object. **Default ON**, `=0` kill;
/// read once per cycle (zr z5/sw, 2026-10-12).
///
/// # The defect it closes
///
/// `perf` of `Zr3JaRefArrayAlloc 1 5 64 prim 100` (one thread allocating
/// short-lived `int[0..64]`, `-Xmx512m`) put 21.7 % of the process in
/// `sweep_bitmap_range`, against HotSpot ZGC, which frees an all-dead page
/// without visiting it. Three per-dead-object terms were left in the
/// complement pass, which already reads no dead header:
///
/// 1. **the dead-address list.** `MonitorCleanup::wants_dead_addresses` is
///    true whenever ONE monitor is inflated anywhere (every JVM has some:
///    reference-handler and cleaner waits), so the sweep pushed one `usize`
///    per reclaimed object, after a whole extra pass over both bitmaps to size
///    the list exactly -- for a prune that then binary-searches that list for
///    a handful of monitor keys. With this switch the collector asks the
///    cleanup for its KEYS instead (`MonitorCleanup::prune_candidate_keys`)
///    and keeps those that died this cycle (snapshot-registered and no longer
///    registered after the sweep, decided before the slide re-keys anything):
///    the prune sees exactly `keys ∩ dead`, which is all it ever acted on.
/// 2. **the per-bit loop.** With no list wanted, a word wholly below the
///    large-object end counts its dead with one `popcount` -- the existing
///    word step, which the list kept switched off.
/// 3. **the region step.** A 32 KiB block of the MARK bitmap whose summary
///    bit is clear held no marked object this cycle (the bit is set at the
///    claim point, `HeapBitmap::claim` -> `note_word`, and reset right after
///    the mark clear by `HeapBitmap::forget_blocks_within`), so its mark words
///    are not read at all and its start words only popcounted.
///
/// Terms 2 and 3 also need the dead-header store off (it is a per-object
/// write by definition), which is a separate switch:
/// [`zgc_sweep_skip_dead_headers_enabled`] (default ON since zr z8/zb). With
/// the store on (`=0`), this switch removes term 1 only.
///
/// The free-span set is unchanged by construction (the complement is computed
/// from the live objects only); `sweep_region_step_matches_the_word_walk`
/// checks it on randomized heaps, serial and sharded.
pub(crate) fn zgc_sweep_live_proportional_enabled() -> bool {
    match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL") {
        Some(raw) => {
            let v = raw.to_string_lossy().trim().to_ascii_lowercase();
            !matches!(v.as_str(), "0" | "false" | "off" | "no")
        }
        None => true,
    }
}

/// What [`ZgcRealHeap::sweep_page_census`] found: the low region's logical
/// pages, classified by whether anything on them survived.
///
/// The three classes are disjoint and exhaustive over the pages the census
/// looked at, which is what makes the line it prints readable as a fraction
/// rather than as three unrelated counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ZSweepPageCensus {
    /// Logical pages covering `[arena_base, low_cursor)`.
    pub(crate) pages: usize,
    /// Pages holding no registered base at all -- never allocated into, or
    /// emptied by an earlier cycle and not reused. Free already; a page-based
    /// reclaim would have nothing to do with them either.
    pub(crate) empty: usize,
    /// **The number the proposal turns on.** Pages that hold registered bases
    /// and not one live object: a page-based heap frees each of these by
    /// returning the page, with no per-object work at all, where today every
    /// one of their dead bases is visited individually.
    pub(crate) wholly_dead: usize,
    /// Pages with at least one survivor. These keep their per-object cost
    /// under any design, which is why they are the term a generational split
    /// would have to bound.
    pub(crate) occupied: usize,
    /// Live and dead registered bases, summed over the pages looked at. The
    /// denominator for "how much of the sweep is on wholly-dead pages".
    pub(crate) live_objects: usize,
    /// See [`Self::live_objects`].
    pub(crate) dead_objects: usize,
    /// Dead bases that sit on a wholly-dead page -- i.e. the per-object work a
    /// page-granular reclaim would remove outright.
    pub(crate) dead_on_wholly_dead_pages: usize,
}

/// Everything one sweep worker produces. No shared state, by construction.
///
/// The free spans are the only output with an ordering obligation: the arena's
/// post-sweep coalescer merges spans that are ADJACENT, and it can only see two
/// spans as adjacent if they arrive in ascending order. A shard collects its own
/// spans ascending (the bitmap scan is ascending), and the merge concatenates
/// shards in ascending address order, so the global sequence is ascending too --
/// including across a shard boundary, where the merge additionally JOINS a
/// shard's last run to the next shard's first when they touch.
#[derive(Default)]
pub(crate) struct ZSweepShard {
    /// Survivor bytes and count.
    pub(crate) bytes_copied: usize,
    pub(crate) objects_copied: usize,
    /// Reclaimed bytes and count.
    pub(crate) bytes_freed: usize,
    pub(crate) dead_count: usize,
    /// Registered bases whose header could not be sized this cycle.
    pub(crate) unsizable: usize,
    /// The LARGE-OBJECT-end bases [`ZgcRealHeap::sweep_one_high`] declined to
    /// sweep because it could not size them, so that the complement pass can
    /// put them back after `retain_marked`.
    ///
    /// # Why this list has to exist
    ///
    /// *Added 2026-09-20, closing finding 2 of
    /// `docs/feature-designs/zgc-architecture.md`.*
    ///
    /// [`ZgcRealHeap::sweep_one`] refuses a base whose header it cannot size,
    /// and says at length why: the object *"stays registered, stays rooted
    /// conservatively, and is never handed out twice"*. `sweep_one_high` copies
    /// that refusal -- but it runs on the complement arm, which afterwards does
    /// a bulk `starts &= marks` ([`super::ZObjectStarts::retain_marked`]). That
    /// is a wholesale prune and it cannot know the high arm deliberately
    /// RETAINED these bits, so it clears them anyway.
    ///
    /// The corpse then loses its registry entry -- `is_object_address` denies
    /// it, and conservative rooting drops it -- while its bytes stay off the
    /// high free list, because `sweep_one_high` returned before pushing a span.
    /// The per-object arm keeps both halves. That is an arm divergence in the
    /// one module whose contract is *"the two arms must agree on which objects
    /// are reclaimed"*, and it is in the UNSAFE direction: retention is the
    /// fail-safe answer and the bitmap arm is the one that stops doing it.
    ///
    /// Empty on every healthy run, which is exactly why it is a field and not a
    /// comment -- the divergence is unreachable until a header goes unsizable,
    /// and then it is silent.
    pub(crate) unsizable_high: Vec<usize>,
    /// Survivors this shard promoted out of the young generation.
    pub(crate) gen_promoted: usize,
    /// BYTES those survivors carry — the figure `VmHeap::bytes_promoted_total`
    /// reports for this collector.
    ///
    /// A count of objects cannot stand in for it: the GC-overhead productivity
    /// metric subtracts byte quantities, and `gen_promoted` was the only
    /// promotion figure this collector produced, so the dispatcher answered a
    /// hard `0` instead
    /// (`docs/feature-designs/zgc-architecture.md`).
    ///
    /// Zero on a non-generational run, where nothing is promoted because there
    /// is no old generation to promote into — and that zero is the truth, not a
    /// missing counter.
    pub(crate) gen_promoted_bytes: usize,
    /// zt t5/gn: bytes of survivors in the LARGE-OBJECT region that are still
    /// YOUNG after this sweep aged them. A whole-heap generational cycle
    /// leaves them out of the old live figure (`gen_old_live_bytes`): they sit
    /// above every young floor, so the next young cycle's aged sweep visits
    /// and counts them itself. Zero on a non-generational run.
    pub(crate) gen_young_high_bytes: usize,
    /// zt t6/tc (GN-18): bytes of survivors in the LARGE-OBJECT region that
    /// are OLD after this sweep aged them. A whole-heap generational cycle
    /// leaves them out of the old live figure too when the young cycles after
    /// it take the FLOOR arm (`CRATONVM_ZGC_GEN_OLD_BITMAP=0` or
    /// `CRATONVM_ZGC_GEN_YOUNG_AGED_SWEEP=0`), which sweeps and counts every
    /// base above the floor, old ones included. Zero on a non-generational run.
    pub(crate) gen_old_high_bytes: usize,
    /// zt t21/pl10 (`zgc-zt12gn4-*` item 11): bytes of survivors BELOW the
    /// large-object region that are still YOUNG after this sweep aged them
    /// (`gc_age < promo_age`). A whole-heap generational cycle counts them in
    /// the old live figure (`gen_old_live_bytes`: everything below its new
    /// floor), but HotSpot 25's major tenures by age, so its `ZGC Young
    /// Generation` pool keeps them; `jmx_young_old_split` moves them back
    /// while the last collection was that major
    /// (`CRATONVM_ZGC_GEN_JMX_MAJOR_YOUNG`). Read only after a whole-heap
    /// cycle; the young-aged walk does not add to it. Zero on a
    /// non-generational run, and after a `System.gc()` major (it ages every
    /// survivor to the promotion age).
    pub(crate) gen_young_low_bytes: usize,
    /// Bases this shard actually visited (the nursery's engagement counter).
    pub(crate) swept: usize,
    /// Bytes NOT memset because only the header was zeroed.
    pub(crate) zero_bytes_skipped: usize,
    /// Dead objects that joined a merged run.
    pub(crate) dead_in_runs: usize,
    /// Dead low-region objects whose header the complement pass did NOT
    /// write, under [`zgc_sweep_skip_dead_headers_enabled`]. Its engagement
    /// counter: zero with the switch on means the arm is inert.
    pub(crate) dead_headers_skipped: usize,
    /// Bitmap words the complement pass handled through the region step
    /// ([`zgc_sweep_live_proportional_enabled`]): words of 32 KiB blocks whose
    /// mark summary proved them free of survivors, so no mark word was read and
    /// no dead base visited. Its engagement counter.
    pub(crate) region_words_skipped: usize,
    /// Dead bases, when `MonitorCleanup::wants_dead_addresses` asked for them.
    pub(crate) dead: Vec<usize>,
    /// Identity hashes of the dead, for the native side tables keyed by them.
    pub(crate) dead_hashes: Vec<i32>,
    /// Free spans, ARENA-RELATIVE and ascending, already run-merged within this
    /// shard. Handed to `Arena::add_free_block` by the merge.
    pub(crate) spans: Vec<(usize, usize)>,
    /// The run currently being accumulated; `None` between runs.
    pub(crate) run: Option<(usize, usize)>,
    /// The LOW region's free list has already been computed for this shard as
    /// the complement of the live set, so a per-object span below the low
    /// cursor would describe bytes that are on it twice.
    ///
    /// # Why a shard carries this at all
    ///
    /// *Added 2026-09-20.* Only [`ZgcRealHeap::sweep_bitmap`] sets it, on the
    /// shard it returns, and the only thing that reads it is
    /// [`ZgcRealHeap::sweep_one`] — because `collect_garbage` keeps sweeping
    /// **into that same shard** after the complement pass has finished. The
    /// off-grid spill set belongs to no bitmap word, so it is swept separately
    /// and per-object (`registered.for_each_spilled_base(..)`) whichever arm
    /// ran, and on the complement arm that per-object body would push
    /// `(off, size)` for a dead low-region base whose bytes
    /// `Arena::rebuild_low_free_list` has *already* published as free. The
    /// driver then drains `spans` into `add_free_block`, and the arena holds one
    /// address on its low free list twice: two allocations, one span.
    ///
    /// The spill set is empty on every healthy run, which is exactly why this
    /// is a field and not a comment — the double free is unreachable until the
    /// day something spills, and then it is silent.
    pub(crate) low_reclaimed_by_complement: bool,
    /// zr z7/pc (`CRATONVM_ZGC_SWEEP_SIZE_HINTS`): the LOW-region survivors
    /// this shard sized, ascending, with their [`ZgcRealHeap::size_cell`]
    /// (`live_cells[i]` belongs to `live_addrs[i]`). Filled only by the bitmap
    /// arm and only under [`ZSweepCfg::record_live_cells`]; the slide's page
    /// census reads them instead of the headers. Unsizable survivors are not
    /// recorded (the census then reads their header, as before).
    pub(crate) live_addrs: Vec<usize>,
    pub(crate) live_cells: Vec<u32>,
    /// zt t16/gn6 (GN-10): nanoseconds the young-aged walk spent over grains
    /// holding no non-old base (`[GC] zgc-gen-cost:` `minor_sweep_walk_us=`;
    /// only under [`ZSweepCfg::time_walk`]). Summed by [`Self::absorb`], so a
    /// sharded walk reports the shards' sum, not the wall.
    pub(crate) walk_ns: u64,
}

impl ZSweepShard {
    /// Close the run in flight. Every shard must be flushed before its spans
    /// are read: the last run has no successor to flush it, and missing it
    /// leaks the topmost run of garbage in the shard -- invisibly, because the
    /// cursor retraction cannot reclaim a span that is not on the list.
    pub(crate) fn flush(&mut self) {
        if let Some(run) = self.run.take() {
            self.spans.push(run);
        }
    }

    /// Fold `other` in. Address order is the caller's obligation: `other` must
    /// cover strictly higher addresses than `self`.
    pub(crate) fn absorb(&mut self, other: &mut ZSweepShard) {
        other.flush();
        self.bytes_copied += other.bytes_copied;
        self.objects_copied += other.objects_copied;
        self.bytes_freed += other.bytes_freed;
        self.dead_count += other.dead_count;
        self.unsizable += other.unsizable;
        self.gen_promoted += other.gen_promoted;
        self.gen_promoted_bytes += other.gen_promoted_bytes;
        self.gen_young_high_bytes += other.gen_young_high_bytes;
        self.gen_old_high_bytes += other.gen_old_high_bytes;
        self.gen_young_low_bytes += other.gen_young_low_bytes;
        self.swept += other.swept;
        self.zero_bytes_skipped += other.zero_bytes_skipped;
        self.dead_in_runs += other.dead_in_runs;
        self.dead_headers_skipped += other.dead_headers_skipped;
        self.region_words_skipped += other.region_words_skipped;
        self.walk_ns += other.walk_ns;
        Self::take_or_append(&mut self.dead, &mut other.dead);
        Self::take_or_append(&mut self.dead_hashes, &mut other.dead_hashes);
        // Order is irrelevant here (it is a set of bases to re-insert, not a
        // span list), so this takes the same buffer-stealing path as its
        // neighbours; the empty case every healthy run takes stays
        // allocation-free either way.
        Self::take_or_append(&mut self.unsizable_high, &mut other.unsizable_high);
        // zr z7/pc: ascending, because `other` covers higher addresses (the
        // caller's obligation above) and each shard records in scan order.
        Self::take_or_append(&mut self.live_addrs, &mut other.live_addrs);
        Self::take_or_append(&mut self.live_cells, &mut other.live_cells);
        // CONCATENATED, NOT JOINED, at the seam.
        //
        // A first version merged a shard's last span with the next shard's
        // first when they touched. It was wrong twice over and worth nothing
        // either time:
        //
        //  * `sweep_one` never merges a span that reaches the LARGE-OBJECT end
        //    (see `ZSweepCfg::high_floor`), and a seam join has no way to know
        //    that -- so it could hand `Arena::add_free_block` a span straddling
        //    the line, which routes by offset and would then serve the same
        //    bytes from the low free list and the high cursor both.
        //  * it saved at most `workers - 1` calls per cycle, and
        //    `coalesce_free_list` merges adjacent spans immediately afterwards
        //    regardless.
        //
        // Ascending order -- which is the property the coalescer actually needs
        // -- is preserved by concatenation alone.
        Self::take_or_append(&mut self.spans, &mut other.spans);
        // `low_reclaimed_by_complement` is DELIBERATELY not folded in. It is a
        // statement about who owns the low free list for the shard the DRIVER
        // will keep sweeping into, not a count, and only `sweep_bitmap` is
        // entitled to make it -- which it does on the joined shard, after every
        // `absorb`. A worker shard never has it set, so there is nothing here
        // to propagate and an `||` would only create a way for a future
        // parallel arm to inherit a claim it did not earn.
    }

    /// `dst.append(src)`, except that a `dst` which has never allocated takes
    /// `src`'s buffer instead of copying into a fresh one.
    ///
    /// The first shard of a merge is the whole list on a serial sweep and
    /// 1/workers of it on a parallel one, and copying it was pure loss: the
    /// destination is a fresh [`ZSweepShard::default`] with no buffer to
    /// preserve. Order is unchanged -- an empty `dst` followed by `src` IS
    /// `src`. A `dst` that a caller has already `reserve`d keeps that buffer
    /// (its capacity is non-zero), which is what makes this compose with the
    /// exact reservations below rather than defeat them.
    #[inline]
    fn take_or_append<T>(dst: &mut Vec<T>, src: &mut Vec<T>) {
        if dst.capacity() == 0 {
            std::mem::swap(dst, src);
        } else {
            dst.append(src);
        }
    }
}

/// One word range's share of the complement sweep, plus the two edges the
/// join needs to stitch it to its neighbours.
///
/// Separate from [`ZSweepShard`] because the complement is a CHAIN and a shard
/// cannot close its own ends: the free span below its first survivor reaches
/// back into the previous shard, which is still running. `ZSweepShard` carries
/// what is genuinely per-shard (counters, dead lists, the large-object end's
/// individual holes) and this wraps it with what is not.
#[derive(Default)]
pub(crate) struct ZComplementShard {
    /// Counters, dead lists, and the HIGH-end per-object holes.
    pub(crate) sh: ZSweepShard,
    /// Free spans strictly BETWEEN two survivors this shard saw itself,
    /// arena-relative and ascending.
    pub(crate) spans: Vec<(usize, usize)>,
    /// The base of the first LOW survivor, or `None` if the shard saw none.
    /// The span below it belongs to the join.
    pub(crate) first_live: Option<usize>,
    /// Absolute end of the last live extent. Meaningful only when
    /// [`Self::first_live`] is set.
    pub(crate) last_end: usize,
    /// The last survivor's header could not be sized, so its extent runs to
    /// the next live base -- possibly in the next shard.
    pub(crate) ends_unsizable: bool,
    /// zt t27/pc2: this shard's merge cursor into `ZSweepCfg::size_memo`
    /// (started at the shard's first covered word) ...
    pub(crate) memo_cursor: usize,
    /// ... and the survivors it sized from the memo without a header read.
    pub(crate) memo_hits: usize,
}

/// The per-cycle constants `sweep_one` reads. Gathered once, outside the loop,
/// so no worker re-reads an atomic or an environment-derived flag per object.
pub(crate) struct ZSweepCfg {
    pub(crate) arena_base: usize,
    /// Where the large-object end begins. A merged run must never grow across
    /// it: `Arena::add_free_block` routes a span by its offset and bounds a LOW
    /// span by the low cursor, so a run spanning the line would be pushed onto
    /// the low tier while covering high-region bytes, and the arena would serve
    /// the same memory from the free list and the high cursor both.
    pub(crate) high_floor: usize,
    /// `Arena::used` at the start of the sweep: the low bump cursor PLUS the
    /// large-object end's bytes, so an upper bound on (not equal to) the low
    /// cursor as an arena offset. [`ZgcRealHeap::sweep_bitmap`] and
    /// `sweep_young_complement` bound their complement by it, and
    /// `low_span_already_reclaimed` compares against it; every one of those
    /// reads is fail-safe with the over-estimate (the complement's tail is
    /// clamped and only ever retracts the cursor; the spilled large-object
    /// case is exempted by `CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS`). A reader
    /// that needs the TRUE low cursor takes `Arena::used_low_for_compaction`
    /// under the arena lock, as `oversize_array_extent` does. (Was named
    /// `low_cursor`, which it never held: zs s2/rf residual, item 2.)
    pub(crate) arena_used: usize,
    pub(crate) zero_header_only: bool,
    /// [`zgc_sweep_skip_dead_headers_enabled`], read once per cycle (and
    /// forced off under `CRATONVM_DBG_ZGC_CORPSE`). Honoured by the
    /// complement arm's LOW region only; the per-object sweep and the
    /// large-object end keep zeroing.
    pub(crate) skip_dead_headers: bool,
    pub(crate) merge_dead_runs: bool,
    /// Are the mark bits cleared in bulk after the sweep? When they are, the
    /// survivor arm does not touch the header at all on a non-generational run.
    pub(crate) bulk_clearable: bool,
    pub(crate) want_dead: bool,
    pub(crate) collect_dead_hashes: bool,
    /// [`zgc_sweep_live_proportional_enabled`], read once per cycle: the
    /// complement arm may take the region step over a 32 KiB block whose mark
    /// summary bit is clear. Engages only where the word step would (store
    /// off, no dead list, no dead hashes, wholly below the large-object end).
    pub(crate) region_skip: bool,
    pub(crate) gen_on: bool,
    pub(crate) promo_age: u8,
    /// zr z7/pc (`CRATONVM_ZGC_SWEEP_SIZE_HINTS`): the bitmap arm records each
    /// low-region survivor's size cell into [`ZSweepShard::live_addrs`] /
    /// [`ZSweepShard::live_cells`] for the slide that follows. Set only when a
    /// slide is expected this cycle.
    pub(crate) record_live_cells: bool,
    /// zt t26/pc1 (`CRATONVM_ZGC_SWEEP_RESERVE_HINTS`): how many survivor
    /// hints to reserve up front when [`Self::record_live_cells`] is set --
    /// the previous cycle's count plus 1/16, capped at this cycle's registered
    /// count; `0` reserves nothing (the kill arm, a first cycle). Each shard
    /// of the bitmap arm takes its covered-word share. A sizing hint only:
    /// the lists grow past it as before.
    pub(crate) reserve_live_hints: usize,
    /// zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): the previous sweep's size
    /// cells ([`super::ZSweepSizeMemo`]), already checked valid for this
    /// cycle. The bitmap arm's live loop sizes a survivor whose base it names
    /// from it instead of reading the header; every other arm ignores it.
    /// `None`: every survivor's header is read, as before.
    pub(crate) size_memo: Option<super::ZSweepSizeMemo>,
    /// zt t16/gn6 (GN-10): time the young-aged walk's idle stretches into
    /// [`ZSweepShard::walk_ns`]. Set only on a generational young cycle whose
    /// pause clock runs (the `[GC] zgc-gen-cost:` condition), so a default run
    /// reads no clock in the walk.
    pub(crate) time_walk: bool,
}

impl ZgcRealHeap {
    /// Would pushing a free span for `[base, base + size)` duplicate one the
    /// complement pass has already published?
    ///
    /// `true` only for a shard [`Self::sweep_bitmap`] filled — i.e. only for the
    /// per-object stragglers (the off-grid spill set) that `collect_garbage`
    /// sweeps into that same shard afterwards — and only for bytes wholly below
    /// the low bump cursor, which is exactly the region
    /// `Arena::rebuild_low_free_list` replaced and `retract_cursor_to` gave
    /// back. The LARGE-OBJECT end is never suppressed: the complement does not
    /// rebuild its free list, so its per-object holes still have to be handed
    /// over one at a time.
    ///
    /// See [`ZSweepShard::low_reclaimed_by_complement`] for what the duplicate
    /// costs.
    ///
    /// *Since 2026-09-20 this gates `ZSweepShard::bytes_freed` as well as the
    /// span push* — the driver credits the arena's own `now_free - was_free`
    /// for the same bytes, so crediting them here too reported the reclaim
    /// twice. Same predicate, same cause, one place.
    #[inline]
    fn low_span_already_reclaimed(
        base: usize,
        size: usize,
        cfg: &ZSweepCfg,
        sh: &ZSweepShard,
    ) -> bool {
        let end = base.saturating_sub(cfg.arena_base).saturating_add(size);
        sh.low_reclaimed_by_complement && end <= cfg.arena_used
    }

    /// Sweep one registered base into `sh`.
    ///
    /// THE WHOLE PER-OBJECT BODY, written once. The serial and parallel
    /// drivers both call it and neither adds anything, so the two cannot drift
    /// on the only question that matters -- which objects are reclaimed. The
    /// arm-equivalence test asserts that as an outcome rather than trusting it.
    ///
    /// Takes `&self` and is called concurrently from
    /// [`Self::sweep_parallel`]'s workers. That is sound because every shard
    /// owns a disjoint set of bases (the split is on bitmap WORD boundaries and
    /// an object start is one bit), so the `header_mut` reborrows below never
    /// alias, the `write_bytes` spans never overlap, and the only shared writes
    /// are `registry.remove` (one `fetch_and`) and, on a generational cycle,
    /// the remembered set -- which is why [`Self::sweep_workers`] refuses to
    /// shard one.
    ///
    /// `registered` is the cycle's immutable start snapshot. zs s2/rf: the
    /// oversize screen for a DEAD array `alloc_size` refuses
    /// ([`Self::oversize_array_extent`], `low_region_ok`) needs it, because
    /// under [`Self::sweep_parallel`] another shard's `registry.remove` can
    /// hide a base inside the claimed extent from the live registry; the
    /// snapshot cannot change under the walk. `None` (no snapshot) keeps the
    /// pre-fix retention for such an array.
    #[inline]
    pub(crate) fn sweep_one(
        &self,
        base: usize,
        cfg: &ZSweepCfg,
        sh: &mut ZSweepShard,
        registered: Option<&ZObjectStartsSnapshot>,
    ) {
        sh.swept += 1;
        let header = self.header_mut(base as *mut u8);
        // A header this collector cannot size must not be swept. The dead arm
        // below `write_bytes`es `size` bytes and hands the same span to
        // `Arena::add_free_block`, whose only bound is a `debug_assert!` — so
        // passing `object_body_size`'s 1 TiB corrupt-header sentinel through
        // here memsets a terabyte from `base` and free-lists memory that is not
        // in the arena.
        //
        // Retaining it instead leaks one object until its layout resolves again
        // (or forever, if its class really is gone), which is the fail-safe
        // direction: it stays registered, stays rooted conservatively, and is
        // never handed out twice. The one-shot `tracing::warn!` at the call
        // site makes the leak visible rather than silent.
        // A dead array the oversize screen sized below; its counters are
        // credited only once `already_reclaimed` is known (zs s3/rv2 §7: they
        // used to be credited first, so the complement-reclaimed path counted
        // the array twice).
        let mut oversize_freed = false;
        let size = match Self::alloc_size(header) {
            Some(size) => size,
            None => {
                let marked = self.mark_is_set(base);
                // zs s2/rf (dg residual page, row 1): a DEAD array past the
                // corruption screen's caps, in either region, is freed when
                // every extent check of `oversize_array_extent` passes -- the
                // high-end sweep's screen, plus the low region bounded by the
                // true bump cursor -- instead of being retained forever.
                // Kill: `CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE=0` (and the
                // screen's own `CRATONVM_ZGC_SWEEP_SIZES_OVERSIZE_ARRAYS=0`).
                let dead_extent = if !marked
                    && registered.is_some()
                    && self
                        .counters
                        .sweep_one_frees_oversize
                        .load(Ordering::Relaxed)
                {
                    self.oversize_array_extent(base, header, cfg, registered, true)
                } else {
                    None
                };
                match dead_extent {
                    Some(sz) => {
                        oversize_freed = true;
                        sz
                    }
                    None => {
                        // zs s2/rv: a LIVE array past `MAX_PLAUSIBLE_ARRAY_LEN`
                        // is retained either way; the oversize screen only lets
                        // its bytes count as a survivor's instead of an
                        // "unsizable" retention (dg residual page, row 2).
                        // Kill: `CRATONVM_ZGC_REWRITE_OVERSIZE_ARRAYS=0`.
                        match marked.then(|| self.oversize_live_array_extent(base, header)) {
                            Some(Some(sz)) => {
                                sh.bytes_copied += sz;
                                sh.objects_copied += 1;
                            }
                            _ => sh.unsizable += 1,
                        }
                        if !cfg.bulk_clearable {
                            header.clear_gc_flags(GC_FLAG_MARKED);
                        }
                        return;
                    }
                }
            }
        };
        if self.mark_is_set(base) {
            // Survivor: keep it, and clear the mark bit for the next cycle.
            //
            // ON THE BITMAP ARM THERE IS NOTHING TO CLEAR HERE. One `clear_all`
            // after the walk does the whole heap, which is the difference
            // between a linear store over `capacity / 512` bytes and a
            // read-modify-write of every survivor's 64-byte header line -- i.e.
            // between touching 8 MB and dirtying the entire live set, on a
            // phase that is otherwise read-only on a non-generational run.
            if !cfg.bulk_clearable {
                header.clear_gc_flags(GC_FLAG_MARKED);
            }
            sh.bytes_copied += size;
            sh.objects_copied += 1;
            // PHASE G: one more collection survived. `age_survivor` returns
            // true on the collection that promotes it, and cards it -- see
            // there for why the card at promotion is what makes a young cycle
            // sound. Free on a non-generational run: one relaxed load and a
            // branch.
            if cfg.gen_on && self.age_survivor(base, header, cfg.promo_age) {
                sh.gen_promoted += 1;
                sh.gen_promoted_bytes += size;
            }
            // zt t5/gn: see `ZSweepShard::gen_young_high_bytes`; zt t6/tc:
            // and `gen_old_high_bytes`.
            if cfg.gen_on && base.saturating_sub(cfg.arena_base) >= cfg.high_floor {
                if header.gc_age() < cfg.promo_age {
                    sh.gen_young_high_bytes += size;
                } else {
                    sh.gen_old_high_bytes += size;
                }
            } else if cfg.gen_on && header.gc_age() < cfg.promo_age {
                // zt t21/pl10: see `ZSweepShard::gen_young_low_bytes`.
                sh.gen_young_low_bytes += size;
            }
            return;
        }
        // Dead: zero it (so a later scan can't see a stale header) and return
        // the span to the arena free list.
        //
        // THE HEADER IS THE WHOLE REASON -- see `zgc_sweep_header_zero`.
        // `HEADER_SIZE` is the entire `ObjectHeader`, so this leaves
        // `class_id=0, num_slots=0`, which is the identical corpse a reader of
        // a vacated span met before; and the body is only reachable through
        // that header, which is why the bytes below it need not be touched.
        // `.min(size)` is belt-and-braces: no allocation is shorter than its
        // header, and a `write_bytes` past the end of one would be exactly the
        // bug `alloc_size` refuses above.
        let zero = if cfg.zero_header_only {
            let n = HEADER_SIZE.min(size);
            sh.zero_bytes_skipped += size - n;
            n
        } else {
            size
        };
        // BEFORE the zeroing: the identity hash lives in the mark word, so
        // `write_bytes` below is what destroys it. A `0` means "never hashed"
        // (or hashed then monitor-inflated, which displaces it) and such an
        // object cannot be a key in an identity-hash-keyed table, so skipping
        // it is exact rather than approximate. See `identity_side_tables`.
        if cfg.collect_dead_hashes {
            let h = header.installed_identity_hash();
            if h != 0 {
                sh.dead_hashes.push(h);
            }
        }
        // SAFETY: `base` is a registered allocation of `size` bytes inside the
        // arena, and both arms write at most `size` of them.
        unsafe { std::ptr::write_bytes(base as *mut u8, 0, zero) };
        // ONE PREDICATE FOR THE SPAN *AND* THE COUNTER.
        //
        // *Added 2026-09-20, closing finding 3 of
        // `docs/feature-designs/zgc-architecture.md`.*
        // These bytes have already been reclaimed once, as part of the
        // complement pass's wholesale rebuild of the low free list, and wave 1
        // stopped the free-list half of the duplicate (the unsound half -- two
        // allocations over one span). The `bytes_freed` half was left, and it
        // is a double count from the same cause: the driver ADDS the arena's
        // `now_free - was_free` to `swept_total.bytes_freed`, so a spilled
        // base credited here is credited twice. A reported number that is
        // silently 2x on a path nobody runs is exactly the shape of the
        // counters wave 1 had to go back and fix, so it is fixed at the source
        // rather than left for the next measurement to trip over.
        //
        // zs s2/rf: the predicate's doc says "the LARGE-OBJECT end is never
        // suppressed", but `cfg.arena_used` is `Arena::used` (low cursor PLUS
        // the large-object end's bytes), so a spilled dead base in the
        // large-object end whose extent ends below that sum was skipped: its
        // span was never pushed while its registry bit was removed below --
        // the bytes were lost to every allocator for good. Kill:
        // `CRATONVM_ZGC_SWEEP_SPILL_HIGH_SPANS=0`.
        let already_reclaimed = base >= cfg.arena_base
            && Self::low_span_already_reclaimed(base, size, cfg, sh)
            && !(base - cfg.arena_base >= cfg.high_floor
                && self.counters.sweep_spill_high_spans.load(Ordering::Relaxed));
        if base >= cfg.arena_base && !already_reclaimed {
            let off = base - cfg.arena_base;
            // A large object never joins a run -- see `ZSweepCfg::high_floor`.
            let mergeable =
                cfg.merge_dead_runs && off < cfg.high_floor && off + size <= cfg.high_floor;
            if mergeable {
                // ONE SPAN PER RUN. The walk is ascending, so a dead object
                // adjacent to the previous one extends it; anything else
                // flushes and starts a new run. See `zgc_sweep_dead_runs` for
                // why the post-coalesce result is the same either way.
                sh.dead_in_runs += 1;
                match sh.run {
                    Some((ro, rl)) if ro + rl == off => sh.run = Some((ro, rl + size)),
                    Some(run) => {
                        sh.spans.push(run);
                        sh.run = Some((off, size));
                    }
                    None => sh.run = Some((off, size)),
                }
            } else {
                // FLUSH FIRST. The coalescer only sees adjacent spans as
                // adjacent because they arrive in ascending order; emitting
                // this one ahead of the run below it would break that ordering
                // for the rest of the cycle.
                sh.flush();
                sh.spans.push((off, size));
            }
        }
        if !already_reclaimed {
            sh.bytes_freed += size;
            if oversize_freed {
                self.counters
                    .oversize_arrays_swept
                    .fetch_add(1, Ordering::Relaxed);
                self.counters
                    .oversize_array_bytes_swept
                    .fetch_add(size, Ordering::Relaxed);
                self.counters
                    .sweep_one_oversize_freed
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        sh.dead_count += 1;
        // THE REGISTRY PRUNE, IN PLACE.
        //
        // It used to run after the loop, over a collected `dead` slice -- which
        // is what made that slice unconditional. The base is already in hand
        // here, so the second pass over 104 MB of cold `usize`s bought nothing
        // but the ordering note below, which still holds:
        //
        // the prune is a REMOVAL, never a wholesale replacement of the
        // structure by the mark snapshot's survivors. An allocation registered
        // by another path between the mark snapshot and here would be erased by
        // a replacement -- leaking its memory forever (unsweepable) and, worse,
        // making `is_object_address` deny it so conservative rooting drops it
        // while reachable.
        //
        // No mutator is running (`retire_all_tlabs` ran before the snapshot and
        // this is inside the stop-the-world pause), so nothing can allocate into
        // the span this call is freeing.
        self.registry.remove(base);
        if cfg.want_dead {
            sh.dead.push(base);
        }
    }

    /// The sweep as a **streaming pass over two bitmaps**, computing free
    /// space as the complement of the live set instead of as the union of the
    /// dead objects.
    ///
    /// # What it stops doing
    ///
    /// [`Self::sweep_one`] reads a header for every registered object -- live
    /// or dead -- to size it. On a heap where a few percent survive that is
    /// one cache miss per DEAD object to learn something the two bitmaps
    /// already imply: a start bit with no mark bit is garbage, and the bytes
    /// between one live object's end and the next live object's start are free
    /// whatever used to be in them. So this reads headers for LIVE objects
    /// only.
    ///
    /// It also stops removing dead bases one at a time: `starts &= marks`,
    /// word by word, is the same prune at one `fetch_and` per 512 arena bytes
    /// ([`super::ZObjectStarts::retain_marked`]).
    ///
    /// # Why the complement is exact rather than optimistic
    ///
    /// Every byte below the low cursor is covered by a live object, covered by
    /// a dead one, or already on the free list, and the last two are both
    /// reclaimable. "Not covered by a live object" is therefore precisely the
    /// reclaimable set -- and it is *more* precise than the per-object sweep,
    /// which can only free what it can size and so leaks the bytes of every
    /// object whose header it refuses (`unsizable`).
    ///
    /// That is also why the free list is REPLACED rather than added to: this
    /// recomputes what was already free along with what has just become free,
    /// in address order, already merged
    /// ([`crate::arena::Arena::rebuild_low_free_list`]).
    ///
    /// # The four things it must not free, and how each is kept
    ///
    /// * **The large-object end.** Objects above the high cursor are packed
    ///   downward from the top of the arena and have their own free list; they
    ///   take the per-object path here, as before.
    /// * **A live object whose header cannot be sized.** Its extent is
    ///   unknown, so the pass claims everything up to the next live base and
    ///   frees nothing it cannot account for.
    /// * **An un-retired TLAB tail** published by the stop-the-world protocol
    ///   for a peer frozen in compiled code: it holds no live object, so the
    ///   complement would hand it out from under its owner.
    /// * **The bytes above the last live object.** Those are not free-listed
    ///   at all -- the cursor is lowered onto them instead, which is what
    ///   `retract_cursor_into_free_tail` did with a full sort.
    ///
    /// Returns `(shard, free spans as arena offsets, new low cursor)`, or
    /// `None` when the shape is unavailable -- no mark bitmap (the header
    /// arm), a hash registry with no words to walk, or an operator who asked
    /// for a per-object behaviour this pass structurally cannot provide (see
    /// below) -- and the caller takes the per-object sweep.
    ///
    /// # Why the two per-object sweep switches force this arm off
    ///
    /// *Added 2026-09-20.* `CRATONVM_ZGC_SWEEP_HEADER_ZERO=0` asks for a
    /// full-body memset of every dead object and
    /// `CRATONVM_ZGC_SWEEP_DEAD_RUNS=0` asks for one free-list span per dead
    /// object. This pass can honour **neither**: the body length is exactly the
    /// header read it declines to make, and the free list it produces is the
    /// complement of the live set, which is merged by construction and has no
    /// per-object spans to un-merge.
    ///
    /// It used to ignore both and win the arm selection anyway, because it is
    /// default-on and `collect_garbage` tries it first. So on a default
    /// whole-heap cycle -- the only cycle that reaches here -- **both switches
    /// parsed, were read into [`ZSweepCfg`], and did nothing**. That is the
    /// `CRATONVM_ZGC_PARSWEEP` defect this module already records one function
    /// down: a switch that answers is worse than one that is absent, because it
    /// makes "I measured the full-body memset" a sentence someone can say about
    /// a run that never memset a body.
    ///
    /// Refusing here is what makes them honest, and it is the behaviour their
    /// own documentation promises -- each is described as restoring the
    /// per-object sweep "byte for byte", which is what the caller's fallback
    /// arm now actually runs. Both default to `true`, so a default run selects
    /// this pass exactly as before.
    pub(crate) fn sweep_bitmap(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> Option<(ZSweepShard, Vec<(usize, usize)>, usize)> {
        let (mut sh, mut spans, mut new_cursor) =
            self.sweep_bitmap_complement(registered, cfg, workers)?;
        // The complement answered, so neither of these can fail: it took the
        // same two `?`s before it walked.
        let marks = self.mark_bits.as_ref()?;
        let base = cfg.arena_base;
        let low_end = base.checked_add(cfg.arena_used)?;

        let skips = self.jit_tlab_skip_regions();
        if !skips.is_empty() {
            Self::withhold_skip_regions(&mut spans, &skips, base);
            // ...AND THE CURSOR, not just the free list.
            //
            // Clipping the spans keeps a published tail off the free list, and
            // that is only half of it: this pass also LOWERS the bump cursor
            // onto the bytes above the last survivor, and a tail is above the
            // last survivor by definition -- it holds no live object. Retracting
            // through one hands its owner's next bump straight to the bump
            // path as fresh space. The floor is the same one the slide applies
            // (`jit_tlab_skip_floor`), for the same reason.
            new_cursor = new_cursor
                .max(self.jit_tlab_skip_floor(base, low_end))
                .min(low_end);
        }
        // THE PRUNE, in one `fetch_and` per 512 arena bytes rather than one
        // `remove` per dead object. It has to run against the LIVE registry
        // rather than the snapshot the walk above read, because an object
        // allocated black after that snapshot was taken is registered, marked,
        // and must stay registered -- `starts &= marks` keeps exactly those.
        //
        // Serial, and left that way deliberately: it is one `fetch_and` per 512
        // arena bytes over a span the bounded passes have already narrowed to
        // the bumped ends, which is nothing beside the per-object work above.
        //
        // NO `?` HERE, AND THAT IS A FIX, NOT A SIMPLIFICATION.
        //
        // *2026-09-21 (wave 4, lane Q).* This line was
        // `self.registry.retain_marked(marks)?;` — a bail-out to the caller's
        // per-object sweep from a point where this pass has **already zeroed
        // every dead object's header** and computed the cycle's free spans.
        // The per-object sweep cannot size a header that is no longer there,
        // so the fallback would reclaim a different (smaller) set of bytes
        // than the spans this pass had already decided on. It could not fire
        // only because the hash arm is screened out at the top of this
        // function by `registered.words.is_empty()` — i.e. the safety of a
        // mid-mutation `?` rested on a screen in a different place that says
        // nothing about this line. `retain_marked` is now total (see
        // `ZObjectStarts::retain_marked`), so there is no `None` to propagate.
        //
        // BOUNDED (2026-10-07) like the snapshot: the whole-bitmap walk visited
        // every word of a capacity-sized bitmap, 33 million at a 16 GiB `-Xmx`,
        // however little the arena had bumped. `CRATONVM_ZGC_BITMAP_BOUNDS=0`
        // restores it with the rest of the bounded passes.
        if super::zgc_bitmap_bounds_enabled() {
            self.registry
                .retain_marked_within(marks, &self.live_bitmap_bounds());
        } else {
            self.registry.retain_marked(marks);
        }
        // ...AND PUT BACK WHAT THE HIGH ARM REFUSED TO SWEEP.
        //
        // `retain_marked` is a bulk `starts &= marks` and cannot know that
        // `sweep_one_high` deliberately RETAINED these unmarked bases: it could
        // not size their headers, so it freed nothing, and a base whose bytes
        // are still held must stay registered or `is_object_address` denies an
        // extent that is neither on a free list nor rooted. Re-inserting here
        // makes the two sweep arms agree again. Empty on every healthy run.
        if !sh.unsizable_high.is_empty() {
            tracing::warn!(
                target: "zgc",
                count = sh.unsizable_high.len(),
                "zgc sweep: re-registering large-object-end bases the complement arm could \
                 not size -- their bytes are NOT on the high free list, so dropping their \
                 start bits would leak an extent that conservative rooting can no longer see"
            );
            self.registry.insert_all(&sh.unsizable_high);
            sh.unsizable_high.clear();
        }
        sh.dead_in_runs = sh.dead_count;
        // The LOW free list is now this pass's, wholesale: `spans` replaces it
        // rather than adding to it. Anything the driver sweeps into this shard
        // afterwards -- the off-grid spill set, which belongs to no bitmap word
        // and takes the per-object body -- must therefore not push a low-region
        // span of its own. See `ZSweepShard::low_reclaimed_by_complement`.
        sh.low_reclaimed_by_complement = true;
        Some((sh, spans, new_cursor - base))
    }

    /// The complement pass of [`Self::sweep_bitmap`] alone: the page census,
    /// the walk (serial or sharded) and the seam join. Returns `(shard, low
    /// free spans as arena offsets, new low cursor as an ABSOLUTE address)`, or
    /// `None` on the same shapes `sweep_bitmap` declines.
    ///
    /// Split out (zt t21/zc2) so the deferred sweep
    /// (`CRATONVM_ZGC_CONCURRENT_SWEEP`, `concurrent_sweep.rs`) can run the walk
    /// after the pause without the two steps that must stay in it: the
    /// skip-region withhold (the published tails are retired at the release)
    /// and the bulk registry prune (a `starts &= marks` with mutators
    /// allocating would clear a window object's start bit). Nothing in the
    /// walk writes the registry or the arena but the large-object end's dead
    /// headers and `registry.remove`s, which are of bases no allocator can
    /// reach until the free spans are installed.
    pub(crate) fn sweep_bitmap_complement(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> Option<(ZSweepShard, Vec<(usize, usize)>, usize)> {
        let marks = self.mark_bits.as_ref()?;
        if registered.words.is_empty() {
            return None; // the hash arm has no word structure to stream
        }
        if !cfg.zero_header_only || !cfg.merge_dead_runs {
            // The operator asked for a per-object behaviour; hand the cycle to
            // the per-object sweep rather than silently ignoring the request.
            return None;
        }
        let base = cfg.arena_base;
        let low_end = base.checked_add(cfg.arena_used)?;
        let high_floor = base.saturating_add(cfg.high_floor);
        let words = registered.words.len().min(marks.word_count());

        // THE PAGE CENSUS, before anything is reclaimed.
        //
        // Default OFF (`CRATONVM_ZGC_SWEEP_PAGE_CENSUS`); see
        // `zgc_sweep_page_census_enabled` for the number it takes and why it is
        // worth taking. It runs HERE, above the walk, because it describes the
        // heap the mark just produced -- after this point the free list is
        // being rebuilt and the dead headers are being zeroed, and a census of
        // that is a census of the sweep rather than of the workload.
        if zgc_sweep_page_census_enabled() {
            let c = Self::sweep_page_census(registered, marks, base, low_end, words);
            let pct = |n: usize| -> f64 {
                if c.pages == 0 {
                    0.0
                } else {
                    n as f64 * 100.0 / c.pages as f64
                }
            };
            eprintln!(
                "[GC] zgc-sweep-pages: pages={} wholly_dead={} ({:.1}%) occupied={} ({:.1}%) \
                 empty={} ({:.1}%) live_objects={} dead_objects={} \
                 dead_on_wholly_dead_pages={} ({:.1}% of dead) page_bytes={}",
                c.pages,
                c.wholly_dead,
                pct(c.wholly_dead),
                c.occupied,
                pct(c.occupied),
                c.empty,
                pct(c.empty),
                c.live_objects,
                c.dead_objects,
                c.dead_on_wholly_dead_pages,
                if c.dead_objects == 0 {
                    0.0
                } else {
                    c.dead_on_wholly_dead_pages as f64 * 100.0 / c.dead_objects as f64
                },
                Self::Z_LOGICAL_PAGE_BYTES,
            );
            tracing::debug!(
                target: "zgc",
                pages = c.pages,
                wholly_dead = c.wholly_dead,
                occupied = c.occupied,
                empty = c.empty,
                live_objects = c.live_objects,
                dead_objects = c.dead_objects,
                dead_on_wholly_dead_pages = c.dead_on_wholly_dead_pages,
                "zgc sweep page census"
            );
        }

        // ONE BODY, TWO ARMS -- and the serial arm goes through the same join,
        // so it is not a shortcut past it. `sweep_one` established the rule for
        // the header-walk sweep (see this module's header) and the complement
        // pass needs it more, not less: the seam logic is the part a divergence
        // would hide, and a serial arm that skipped it would test nothing.
        //
        // Split on the COVERED words (`ZObjectStartsSnapshot::split_covered`),
        // not on `0..words`: the bounded snapshot's never-bumped middle is all
        // zero, and an even split of the raw range gave most workers nothing
        // but that middle to stream.
        let mut shards: Vec<ZComplementShard> = if workers > 1 {
            // zs s3/ac: each spawned worker's thread CPU, one clock pair per
            // worker per cycle (`ZgcRealHeap::collection_cpu_totals`).
            let cpu_on = self.collection_cpu_accounting_on();
            let mut pool_cpu_ns = 0u64;
            let shards = std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(workers);
                for (w0, w1) in registered.split_covered(workers) {
                    let w1 = w1.min(words);
                    if w0 >= w1 {
                        continue;
                    }
                    handles.push(scope.spawn(move || {
                        let cpu = super::mark::ZThreadCpuSpan::start(cpu_on);
                        let shard = self
                            .sweep_bitmap_range(registered, cfg, marks, w0, w1, base, high_floor);
                        (shard, cpu.elapsed_ns())
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        let (shard, ns) = h.join().expect("zgc complement sweep worker panicked");
                        pool_cpu_ns = pool_cpu_ns.saturating_add(ns);
                        shard
                    })
                    .collect()
            });
            self.book_sweep_pool_cpu(pool_cpu_ns);
            shards
        } else {
            vec![self.sweep_bitmap_range(registered, cfg, marks, 0, words, base, high_floor)]
        };
        // zt t27/pc2: the survivors sized from the memo (`[GC] zgc-cpu-skips:`).
        if cfg.size_memo.is_some() {
            let hits: usize = shards.iter().map(|s| s.memo_hits).sum();
            self.counters.size_memo_census[0].fetch_add(hits, Ordering::Relaxed);
            self.counters.size_memo_census[1].fetch_add(1, Ordering::Relaxed);
        }

        Some(Self::join_complement(&mut shards, base, low_end))
    }

    /// Classify the low region's logical pages by whether anything on them
    /// survived, **from the two bitmaps alone**.
    ///
    /// No header is read, no byte is written, and the per-object loops of the
    /// sweep are not touched: this is a second, independent streaming pass
    /// over `registered.words` and `marks`, ~16 MB for a 512 MB arena, in
    /// sequential order. Keeping it independent is deliberate -- folding the
    /// tally into the sweep's hot loops would put permanent instrumentation
    /// cost on the largest term in the pause in exchange for a number nobody
    /// reads on a default run.
    ///
    /// See [`zgc_sweep_page_census_enabled`] for the measurement this is for
    /// and why it is default-off. The classes are exhaustive over
    /// [`ZSweepPageCensus::pages`]: `empty` is derived last, as the pages that
    /// were neither occupied nor wholly dead.
    ///
    /// A page is **wholly dead** iff it holds at least one registered base and
    /// `starts & marks` is zero across every one of its words. That is exactly
    /// the condition under which a page-based heap frees it by returning the
    /// page, so the count is the expected saving rather than a proxy for it.
    pub(crate) fn sweep_page_census(
        registered: &ZObjectStartsSnapshot,
        marks: &super::starts::ZObjectStartBits,
        base: usize,
        low_end: usize,
        words: usize,
    ) -> ZSweepPageCensus {
        let mut out = ZSweepPageCensus::default();
        let page_bytes = Self::Z_LOGICAL_PAGE_BYTES;
        if low_end <= base || words == 0 || page_bytes < 512 {
            return out;
        }
        // The word grid is relative to the SNAPSHOT's base, and every span this
        // module publishes is relative to the arena's. They are the same value
        // on every real heap -- `sweep_bitmap_range` already relies on it, one
        // line at a time -- but this is instrumentation, so it declines to
        // guess rather than mis-attributing bases to the wrong page.
        if registered.base != base {
            tracing::debug!(
                target: "zgc",
                snapshot_base = registered.base,
                arena_base = base,
                "zgc sweep page census: snapshot base is not the arena base; skipping"
            );
            return out;
        }
        out.pages = (low_end - base).div_ceil(page_bytes);
        // 512 arena bytes per bitmap word (64 bits x the 8-byte object grid).
        let words_per_page = page_bytes >> 9;
        for p in 0..out.pages {
            let w0 = p * words_per_page;
            if w0 >= words {
                break;
            }
            let w1 = (w0 + words_per_page).min(words);
            let mut live = 0usize;
            let mut dead = 0usize;
            for w in w0..w1 {
                let start_w = registered.words[w];
                if start_w == 0 {
                    continue; // 512 arena bytes holding no allocation base
                }
                let mark_w = marks.word_at(w);
                live += (start_w & mark_w).count_ones() as usize;
                dead += (start_w & !mark_w).count_ones() as usize;
            }
            out.live_objects += live;
            out.dead_objects += dead;
            if live != 0 {
                out.occupied += 1;
            } else if dead != 0 {
                out.wholly_dead += 1;
                out.dead_on_wholly_dead_pages += dead;
            }
        }
        out.empty = out
            .pages
            .saturating_sub(out.occupied)
            .saturating_sub(out.wholly_dead);
        out
    }

    /// The complement pass over ONE word range. The shared body of the serial
    /// and sharded arms.
    ///
    /// Emits only the free spans strictly BETWEEN two live objects it saw
    /// itself. The gap that reaches back past `w0` belongs to no shard --
    /// resolving it needs the previous shard's last live extent, which is not
    /// known until that shard finishes -- so it is described rather than
    /// emitted, in [`ZComplementShard::first_live`], and
    /// [`ZgcRealHeap::join_complement`] closes it.
    #[allow(clippy::too_many_arguments)]
    fn sweep_bitmap_range(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        marks: &super::starts::ZObjectStartBits,
        w0: usize,
        w1: usize,
        base: usize,
        high_floor: usize,
    ) -> ZComplementShard {
        let mut out = ZComplementShard::default();
        let sh = &mut out.sh;
        // RESERVE THE DEAD LIST EXACTLY, BEFORE THE WALK.
        //
        // `sh.dead` takes one `usize` per reclaimed object -- 12.0 M of them
        // (96 MB) on a whole-heap `CratonBench bintrees -Xmx512m` cycle -- and
        // growing it by doubling copies about as many bytes again. That growth
        // was 1.94 % of all busy samples on that run, inside the pause, and it
        // is the largest of the three copies this list used to pay for (see
        // `absorb` and `collect_garbage`'s swap into `dead_scratch`).
        //
        // The count is exact and needs no state carried between cycles: a dead
        // base is a start bit with no mark bit, so one streaming pass over the
        // two bitmaps this walk is about to read anyway counts them. At 512
        // arena bytes per word that pass reads ~16 MB for a 512 MB arena, in
        // sequential order, against the ~192 MB of copying it removes.
        //
        // Both passes stream only the snapshot's COVERED words
        // (`ZObjectStartsSnapshot::segments`): every other word is zero by the
        // bounded snapshot's invariant, and walking them was 59% of a
        // ~1 MiB-live 4 GiB cycle (r16 zgcarray).
        let segments = registered.segments(w0, w1);
        if cfg.want_dead {
            let mut dead_bits = 0usize;
            for &(a, b) in &segments {
                for w in a..b {
                    let start_w = registered.words[w];
                    if start_w != 0 {
                        dead_bits += (start_w & !marks.word_at(w)).count_ones() as usize;
                    }
                }
            }
            sh.dead.reserve_exact(dead_bits);
        }
        // zt t26/pc1 (`CRATONVM_ZGC_SWEEP_RESERVE_HINTS`): the survivor hints
        // are one entry per low survivor -- a million on a near-full 256 MiB
        // `grow` cycle -- and grew by doubling from empty, inside the pause.
        // Reserve this shard's covered-word share of last cycle's count.
        if cfg.record_live_cells && cfg.reserve_live_hints != 0 {
            let here: usize = segments.iter().map(|&(a, b)| b - a).sum();
            let total = registered.covered_word_count().max(1);
            let n = if here >= total {
                cfg.reserve_live_hints
            } else {
                cfg.reserve_live_hints.saturating_mul(here) / total
            };
            sh.live_addrs.reserve_exact(n);
            sh.live_cells.reserve_exact(n);
        }
        // zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): the memo's merge cursor
        // starts at this shard's first covered byte; the live loop advances it.
        if let (Some(memo), Some(&(seg_a, _))) = (cfg.size_memo.as_ref(), segments.first()) {
            out.memo_cursor = memo.cursor_for(registered.base + (seg_a << 9));
        }
        // Valid only once `first_live` is set; the join supplies everything
        // below it.
        let mut prev_end = 0usize;
        let mut unsizable_live = false;

        // THE REGION STEP (zr z5/sw, `CRATONVM_ZGC_SWEEP_LIVE_PROPORTIONAL`).
        // Allowed exactly where the per-word popcount step below is: nothing
        // per dead object is owed (no header store, no dead list, no hashes).
        // A block whose mark summary bit is clear has every mark word zero, so
        // the word step would treat each of its start bits as dead -- which is
        // what the popcount below credits -- and its live loop would find
        // nothing, so it neither emits a span nor moves `prev_end` /
        // `first_live`. The chain therefore crosses the block untouched and
        // the next survivor's gap covers it: one span, no per-word walk.
        let region_skip = cfg.region_skip
            && cfg.skip_dead_headers
            && !cfg.want_dead
            && !cfg.collect_dead_hashes
            && registered.base == marks.word_base(0);
        const BW: usize = crate::heap_bitmap::HEAP_BITMAP_BLOCK_WORDS;

        for &(seg_a, seg_b) in &segments {
            let mut w = seg_a;
            while w < seg_b {
                if region_skip && (w == seg_a || w % BW == 0) {
                    let blk = w / BW;
                    let end = ((blk + 1) * BW).min(seg_b);
                    if registered.base.saturating_add(end << 9) <= high_floor
                        && !marks.block_may_hold_bits(blk)
                    {
                        let mut n = 0usize;
                        for x in w..end {
                            n += registered.words[x].count_ones() as usize;
                        }
                        out.sh.swept += n;
                        out.sh.dead_count += n;
                        out.sh.dead_headers_skipped += n;
                        out.sh.region_words_skipped += end - w;
                        w = end;
                        continue;
                    }
                }
                self.sweep_bitmap_word(
                    registered,
                    cfg,
                    marks,
                    w,
                    base,
                    high_floor,
                    &mut out,
                    &mut prev_end,
                    &mut unsizable_live,
                );
                w += 1;
            }
        }
        out.last_end = prev_end;
        out.ends_unsizable = unsizable_live;
        out
    }

    /// One bitmap word of [`Self::sweep_bitmap_range`]: the dead bases, then
    /// the survivors and the gaps between them. Split out (zr z5/sw) so the
    /// region step can skip whole blocks around it; the body is unchanged.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn sweep_bitmap_word(
        &self,
        registered: &ZObjectStartsSnapshot,
        cfg: &ZSweepCfg,
        marks: &super::starts::ZObjectStartBits,
        w: usize,
        base: usize,
        high_floor: usize,
        out: &mut ZComplementShard,
        prev_end: &mut usize,
        unsizable_live: &mut bool,
    ) {
        let sh = &mut out.sh;
        {
            let start_w = registered.words[w];
            if start_w == 0 {
                return; // 512 arena bytes holding no allocation base
            }
            let word_base = registered.base + (w << 9);
            let mark_w = marks.word_at(w);

            // --- the dead: addresses only, no header read -----------------
            let mut dead_w = start_w & !mark_w;
            // THE WORD AT ONCE (`CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS`, zr
            // z1/al). With the header store skipped, a dead low-region object
            // that nobody wants listed has nothing left to do but be counted,
            // so a word wholly below the large-object end costs one popcount.
            if dead_w != 0
                && cfg.skip_dead_headers
                && !cfg.want_dead
                && !cfg.collect_dead_hashes
                && word_base.saturating_add(512) <= high_floor
            {
                let n = dead_w.count_ones() as usize;
                sh.swept += n;
                sh.dead_count += n;
                sh.dead_headers_skipped += n;
                dead_w = 0;
            }
            while dead_w != 0 {
                let bit = dead_w.trailing_zeros() as usize;
                dead_w &= dead_w - 1;
                let addr = word_base + (bit << 3);
                sh.swept += 1;
                if addr >= high_floor {
                    // The large-object end keeps the per-object protocol: its
                    // free list is not the one this pass rebuilds.
                    self.sweep_one_high(addr, cfg, sh, Some(registered));
                    continue;
                }
                sh.dead_count += 1;
                if cfg.collect_dead_hashes {
                    let h = self.header_ref(addr as *mut u8).installed_identity_hash();
                    if h != 0 {
                        sh.dead_hashes.push(h);
                    }
                }
                if cfg.want_dead {
                    sh.dead.push(addr);
                }
                // A pure write, and only of the header -- the body is left for
                // the next allocation to zero. Same contract as `sweep_one`'s
                // `zero_header_only`, which is default-on; the full-body arm
                // cannot be served here because the size is exactly what this
                // pass declines to read, which is why `sweep_bitmap` REFUSES
                // the cycle outright when `CRATONVM_ZGC_SWEEP_HEADER_ZERO=0`
                // rather than quietly zeroing 16 bytes and calling it a memset.
                if cfg.skip_dead_headers {
                    sh.dead_headers_skipped += 1;
                } else {
                    // SAFETY: `addr` is a registered base below the low cursor;
                    // every allocation is at least `HEADER_SIZE` bytes.
                    unsafe { std::ptr::write_bytes(addr as *mut u8, 0, HEADER_SIZE) };
                }
            }

            // --- the live: one header read, extent, and the complement ----
            let mut live_w = start_w & mark_w;
            while live_w != 0 {
                let bit = live_w.trailing_zeros() as usize;
                live_w &= live_w - 1;
                let addr = word_base + (bit << 3);
                sh.swept += 1;
                // zt t27/pc2 (`CRATONVM_ZGC_SWEEP_SIZE_MEMO`): the previous
                // sweep's cell for this base answers the size with no header
                // read (`ZSweepSizeMemo` says why it is this object's). Low
                // region only (the memo holds no other base), and never on a
                // generational cycle (`collect_garbage` passes no memo there:
                // the arm below reads and writes the age).
                let memo_cell = match cfg.size_memo.as_ref() {
                    Some(memo) if addr < high_floor => memo.cell_at(&mut out.memo_cursor, addr),
                    _ => 0,
                };
                let size = if memo_cell != 0 {
                    debug_assert!(!cfg.gen_on, "no size memo on a generational cycle");
                    // The `Some` arm below, from the cell: the same size, the
                    // same accounting, the same cell recorded for the slide.
                    let size = (memo_cell & !super::ZSIZE_CELL_NOREF) as usize;
                    out.memo_hits += 1;
                    sh.bytes_copied += size;
                    sh.objects_copied += 1;
                    if cfg.record_live_cells {
                        sh.live_addrs.push(addr);
                        sh.live_cells.push(memo_cell);
                    }
                    Some(size)
                } else {
                    let header = self.header_mut(addr as *mut u8);
                    let size = match Self::alloc_size(header) {
                        Some(size) => {
                            sh.bytes_copied += size;
                            sh.objects_copied += 1;
                            Some(size)
                        }
                        None => {
                            // zs s2/rv: a live over-long array is a survivor, not
                            // an unsizable retention -- accounting only, `size`
                            // stays `None` so the cell and the complement are the
                            // pre-fix ones (dg residual page, row 2).
                            match self.oversize_live_array_extent(addr, header) {
                                Some(sz) => {
                                    sh.bytes_copied += sz;
                                    sh.objects_copied += 1;
                                }
                                None => sh.unsizable += 1,
                            }
                            None
                        }
                    };
                    if cfg.gen_on && self.age_survivor(addr, header, cfg.promo_age) {
                        sh.gen_promoted += 1;
                        // `None` is the unsizable-header arm above, which already
                        // declined to add the object to `bytes_copied`; a promotion
                        // whose size could not be read contributes nothing here
                        // rather than a guess.
                        sh.gen_promoted_bytes += size.unwrap_or(0);
                    }
                    if addr >= high_floor {
                        // zt t5/gn: see `ZSweepShard::gen_young_high_bytes`;
                        // zt t6/tc: and `gen_old_high_bytes`.
                        if cfg.gen_on {
                            if header.gc_age() < cfg.promo_age {
                                sh.gen_young_high_bytes += size.unwrap_or(0);
                            } else {
                                sh.gen_old_high_bytes += size.unwrap_or(0);
                            }
                        }
                        continue; // accounted, but not part of the low complement
                    }
                    // zt t21/pl10: see `ZSweepShard::gen_young_low_bytes`.
                    if cfg.gen_on && header.gc_age() < cfg.promo_age {
                        sh.gen_young_low_bytes += size.unwrap_or(0);
                    }
                    if cfg.record_live_cells {
                        let cell = Self::size_cell_sized(header, size);
                        if cell != 0 {
                            sh.live_addrs.push(addr);
                            sh.live_cells.push(cell);
                        }
                    }
                    size
                };
                if out.first_live.is_none() {
                    // The gap below this one crosses `w0`. The join owns it.
                    out.first_live = Some(addr);
                } else {
                    if *unsizable_live {
                        // The previous survivor ends at or before this base.
                        *unsizable_live = false;
                        *prev_end = (*prev_end).max(addr);
                    }
                    if addr > *prev_end {
                        out.spans.push((*prev_end - base, addr - *prev_end));
                    }
                }
                match size {
                    Some(size) => *prev_end = (*prev_end).max(addr.saturating_add(size)),
                    None => {
                        *unsizable_live = true;
                        *prev_end = (*prev_end).max(addr);
                    }
                }
            }
        }
    }

    /// Stitch the shards' complements into the one ascending, merged sequence a
    /// serial walk would have produced, and return the bump cursor with it.
    ///
    /// # What a shard cannot know
    ///
    /// The complement is a CHAIN: a free span runs from the end of one live
    /// extent to the base of the next, and at `w0` the previous live extent is
    /// in another shard, still running. So a shard emits its interior spans and
    /// describes its two edges -- the base of its first survivor and the end of
    /// its last -- and this pass walks the shards in address order closing each
    /// seam with the span `[chain, first_live)`.
    ///
    /// Two details carry across a seam and neither is optional:
    ///
    ///  * **An unsizable survivor.** A live object whose header could not be
    ///    sized has no known extent, so it runs to the next live base -- which
    ///    may be in the next shard. `ends_unsizable` carries the pending
    ///    resolution over, and `chain` is raised to the next `first_live`
    ///    exactly as the serial walk raises `prev_end`.
    ///  * **A shard with no survivor at all** contributes nothing and must not
    ///    advance `chain`. Its whole range is free, and the next shard's seam
    ///    span covers it, because that span runs from `chain` to wherever the
    ///    next survivor actually is.
    ///
    /// A shard's HIGH-end spans travel in `ZSweepShard::spans` instead and are
    /// concatenated by `absorb`, which is correct for them: they are individual
    /// per-object holes routed by offset, not a chain.
    fn join_complement(
        shards: &mut [ZComplementShard],
        base: usize,
        low_end: usize,
    ) -> (ZSweepShard, Vec<(usize, usize)>, usize) {
        let mut out = ZSweepShard::default();
        // The exact total, so the concatenation below allocates once and
        // never doubles. Leaving `out.dead` unreserved would also make
        // `take_or_append` swap the first shard's buffer in and then grow it
        // through every later shard.
        let dead_total: usize = shards.iter().map(|s| s.sh.dead.len()).sum();
        if dead_total != 0 {
            out.dead.reserve_exact(dead_total);
        }
        let mut spans: Vec<(usize, usize)> = Vec::new();
        let mut chain = base;
        let mut pending_unsizable = false;
        for shard in shards.iter_mut() {
            if let Some(first) = shard.first_live {
                if pending_unsizable {
                    pending_unsizable = false;
                    chain = chain.max(first);
                }
                if first > chain {
                    spans.push((chain - base, first - chain));
                }
                spans.append(&mut shard.spans);
                chain = shard.last_end;
                pending_unsizable = shard.ends_unsizable;
            }
            // ASCENDING, because the coalescer requires it: `shards` is in
            // range order and the high-end spans inside each are too.
            out.absorb(&mut shard.sh);
        }
        if pending_unsizable {
            // Nothing past the last survivor can be justified, so nothing past
            // it is reclaimed this cycle.
            chain = low_end;
        }
        (out, spans, chain.min(low_end))
    }

    // `jit_tlab_skip_regions` USED TO BE STUBBED HERE, returning empty, with a
    // note saying the VM-TLAB branch would have to fill it in when the two
    // met. They met on 2026-09-02 and it did: the real one lives in
    // `zgc/vm_tlab.rs` and returns the reserved tails the stop-the-world
    // protocol published for peers frozen in compiled code. The complement
    // sweep calls that one now, which is what `withhold_skip_regions` below
    // exists to consume -- a tail its owner will resume bumping into holds no
    // live object, so the complement would otherwise hand those bytes out
    // from under it.

    /// Clip published TLAB tails out of the free spans. A frozen peer resumes
    /// bumping into its tail, so the complement must not offer those bytes to
    /// anyone else.
    ///
    /// # The span list is not small any more, so the shape had to change
    ///
    /// *Rewritten 2026-09-20.* The skip list is empty unless a peer was frozen
    /// mid-allocation, and that emptiness used to be the whole justification
    /// for an `O(spans x skips)` pass that allocated `1 + skips.len()` `Vec`s
    /// **per span**. It was written when the spans were the per-object holes a
    /// header-walk sweep produced for one cycle's dead objects; since the
    /// complement sweep the list is the whole low free list -- one entry per
    /// gap between survivors, so `O(live objects)`, millions on the heaps this
    /// collector is measured on. Two million spans times two allocations each,
    /// inside the pause, on the one path that runs when a peer is frozen in
    /// compiled code: the cost lands exactly where the collector is already in
    /// trouble.
    ///
    /// The answer is unchanged -- it is still the set difference
    /// `spans \ skips` -- but the skips are normalised **once** (arena-relative,
    /// empties dropped, sorted, overlaps merged) and each span then binary-
    /// searches for the first cut that can touch it. A span with no cut near it
    /// costs one `partition_point` and no allocation at all, which is every
    /// span but the handful straddling a published tail. Sorting and merging
    /// the cuts is not a behaviour change: carving a span against overlapping
    /// or unordered cuts one at a time, as the old loop did, produces the same
    /// difference.
    fn withhold_skip_regions(
        spans: &mut Vec<(usize, usize)>,
        skips: &[(usize, usize)],
        base: usize,
    ) {
        // NORMALISE ONCE, outside the span loop. An empty range carves nothing
        // and a `saturating_sub` past the arena base collapses to zero, so both
        // are dropped here rather than re-tested per span.
        let mut cuts: Vec<(usize, usize)> = skips
            .iter()
            .map(|&(s, e)| (s.saturating_sub(base), e.saturating_sub(base)))
            .filter(|&(s, e)| e > s)
            .collect();
        if cuts.is_empty() {
            return;
        }
        cuts.sort_unstable();
        // Merge touching and overlapping cuts so the carve below can assume the
        // next cut starts strictly above the previous one's end -- which is
        // what lets it walk forward instead of restarting per cut.
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(cuts.len());
        for (s, e) in cuts {
            match merged.last_mut() {
                Some((_, pe)) if s <= *pe => {
                    if e > *pe {
                        *pe = e;
                    }
                }
                _ => merged.push((s, e)),
            }
        }
        let cuts = merged;

        let mut out: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
        for &(off, len) in spans.iter() {
            let (lo, hi) = (off, off + len);
            // The first cut that can touch this span, by binary search. This is
            // the fast path and it is almost every span: no cut reaches `lo`,
            // or the first one that does starts past `hi`.
            let mut i = cuts.partition_point(|&(_, e)| e <= lo);
            if i >= cuts.len() || cuts[i].0 >= hi {
                out.push((off, len));
                continue;
            }
            let mut cur = lo;
            while i < cuts.len() && cuts[i].0 < hi {
                let (s, e) = cuts[i];
                if s > cur {
                    out.push((cur, s - cur));
                }
                cur = cur.max(e);
                if cur >= hi {
                    break;
                }
                i += 1;
            }
            if cur < hi {
                out.push((cur, hi - cur));
            }
        }
        *spans = out;
    }

    /// The exact extent of a DEAD array at `base` in the large-object end
    /// that [`Self::alloc_size`]'s corruption screen refused, or `None` when
    /// any check below fails (the caller then keeps the old retention).
    ///
    /// See [`zgc_sweep_sizes_oversize_arrays_enabled`] for the defect. The
    /// checks, each independent of the header being trusted:
    ///
    /// 1. it is an array whose length the allocator could have produced
    ///    (`<= ZGC_REAL_MAX_ARRAY_LENGTH`) and whose size does not overflow;
    /// 2. `[base, base + size)` lies inside the large-object end
    ///    (`[high_floor, capacity)` as arena offsets) -- where every object
    ///    `alloc_high` placed lives, packed with no other allocator's bytes;
    /// 3. no other base starts inside it, in the cycle's snapshot (immutable,
    ///    so a parallel shard's removals cannot hide one) or in the live
    ///    registry (an object registered after the snapshot);
    /// 4. no arena free block overlaps it (one guard acquire and one free-list
    ///    snapshot -- paid only for an oversize dead array, at most a few per
    ///    cycle on any real heap);
    /// 5. no published VM-TLAB tail overlaps it. Cooperative buffers were
    ///    retired before the snapshot, so their tails are free blocks (check
    ///    4); a frozen peer's tail is not, and `Arena::alloc`'s last-resort arm
    ///    can carve a chunk out of a large-object hole.
    ///
    /// A header whose length passes all five names bytes that no object, no
    /// free block and no live buffer owns, so freeing them cannot hand memory
    /// out twice.
    ///
    /// zs s2/rf: with `low_region_ok` (the per-object sweep, `sweep_one`)
    /// check 2 also admits an extent wholly inside the LOW region at or below
    /// the low bump cursor -- where `alloc_raw`'s fallback places a large
    /// object when the large-object end refuses it. Checks 3-5 are the same.
    pub(crate) fn oversize_array_extent(
        &self,
        base: usize,
        header: &ObjectHeader,
        cfg: &ZSweepCfg,
        registered: Option<&ZObjectStartsSnapshot>,
        low_region_ok: bool,
    ) -> Option<usize> {
        if !self
            .counters
            .sweep_sizes_oversize_arrays
            .load(Ordering::Relaxed)
            || header.kind() != super::ObjectKind::Array
        {
            return None;
        }
        let refuse = || {
            self.counters
                .oversize_arrays_refused
                .fetch_add(1, Ordering::Relaxed);
            None
        };
        // zt t21/zc2: during a deferred sweep's walk the mutators run, so
        // checks 4 and 5 below would describe the window, not the pause (a
        // chunk carved from a large-object hole since then is on no list and
        // in no published tail). Retained instead: the fail-safe answer.
        if self.counters.csweep.walking() {
            return refuse();
        }
        let len = header.array_length() as usize;
        if len > super::ZGC_REAL_MAX_ARRAY_LENGTH {
            return refuse();
        }
        let Some(size) = super::array_data_size(len, header.element_type())
            .ok()
            .and_then(|data| super::ARRAY_DATA_OFFSET.checked_add(data))
        else {
            return refuse();
        };
        let capacity = self.arena_end.saturating_sub(cfg.arena_base);
        let Some(off) = base.checked_sub(cfg.arena_base) else {
            return refuse();
        };
        let Some(end_off) = off.checked_add(size) else {
            return refuse();
        };
        if end_off > capacity || size <= HEADER_SIZE {
            return refuse();
        }
        // zs s2/rf: the LOW region is admitted only for the per-object sweep's
        // caller (`sweep_one`, `CRATONVM_ZGC_SWEEP_ONE_FREES_OVERSIZE`); the
        // high-end caller never passes a low base. Its region bound is checked
        // under the arena guard below, against the TRUE low bump cursor
        // (`cfg.arena_used` is `Arena::used`, which folds in the large-object
        // end's bytes and so overstates it).
        let low = off < cfg.high_floor;
        if low && (!low_region_ok || end_off > cfg.high_floor) {
            return refuse();
        }
        let end = base + size;
        if registered.is_some_and(|snap| snap.any_start_in(base + 8, end))
            || self
                .registry
                .nearest_base_within_total(end - 1, base + 8)
                .is_some()
        {
            return refuse();
        }
        let overlaps_free = {
            let arena = self.arena.lock();
            // A low extent must end at or below the bump cursor: the bytes
            // above it are no allocator's and on no free list, so a clobbered
            // length reaching into them passes checks 3-4 and would free-list
            // bytes the bump path also serves.
            if low && end_off > arena.used_low_for_compaction() {
                drop(arena);
                return refuse();
            }
            let blocks = arena.free_blocks_sorted();
            // The last block starting below `end_off` is the only one that can
            // reach into `[off, end_off)` from below or inside; sorted and
            // disjoint, so every earlier one ends at or below its start.
            let i = blocks.partition_point(|&(o, _)| o < end_off);
            i > 0 && {
                let (o, l) = blocks[i - 1];
                o + l > off
            }
        };
        if overlaps_free
            || self
                .jit_tlab_skip_regions()
                .iter()
                .any(|&(s, e)| s < end && e > base)
        {
            return refuse();
        }
        Some(size)
    }

    /// The dead half of [`Self::sweep_one`] for an object in the large-object
    /// end, whose free list this pass does not rebuild.
    ///
    /// `registered` is the cycle's snapshot, for the oversize-array screen
    /// ([`Self::oversize_array_extent`]); `None` skips that half of check 3.
    fn sweep_one_high(
        &self,
        base: usize,
        cfg: &ZSweepCfg,
        sh: &mut ZSweepShard,
        registered: Option<&ZObjectStartsSnapshot>,
    ) {
        let header = self.header_mut(base as *mut u8);
        let size = match Self::alloc_size(header) {
            Some(size) => Some(size),
            None => {
                let size = self.oversize_array_extent(base, header, cfg, registered, false);
                if let Some(size) = size {
                    self.counters
                        .oversize_arrays_swept
                        .fetch_add(1, Ordering::Relaxed);
                    self.counters
                        .oversize_array_bytes_swept
                        .fetch_add(size, Ordering::Relaxed);
                }
                size
            }
        };
        let Some(size) = size else {
            sh.unsizable += 1;
            // RETAINED, and the retention has to be re-asserted after the bulk
            // prune -- `retain_marked` is a `starts &= marks` and this base is
            // unmarked, so it would clear exactly the bit this refusal is
            // keeping. See `ZSweepShard::unsizable_high`.
            sh.unsizable_high.push(base);
            return;
        };
        sh.dead_count += 1;
        if cfg.collect_dead_hashes {
            let h = header.installed_identity_hash();
            if h != 0 {
                sh.dead_hashes.push(h);
            }
        }
        let zero = if cfg.zero_header_only {
            HEADER_SIZE.min(size)
        } else {
            size
        };
        unsafe { std::ptr::write_bytes(base as *mut u8, 0, zero) };
        if base >= cfg.arena_base {
            sh.flush();
            sh.spans.push((base - cfg.arena_base, size));
        }
        sh.bytes_freed += size;
        if cfg.want_dead {
            sh.dead.push(base);
        }
        self.registry.remove(base);
    }

    /// The sweep, on this thread.
    pub(crate) fn sweep_serial(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
    ) -> ZSweepShard {
        let mut sh = ZSweepShard::default();
        if floor == 0 {
            registered.for_each_base_in_words(0, registered.word_count(), 0, |base| {
                self.sweep_one(base, cfg, &mut sh, Some(registered))
            });
        } else {
            // A YOUNG CYCLE SWEEPS THE UNION, not the range. See
            // `note_young_page`: the free list serves from below the floor, so
            // an object born into a hole a previous sweep left is invisible to
            // a floor-bounded cycle and is retained until the next major.
            registered.for_each_young_base(
                floor,
                |off| self.young_page_at_offset(off),
                |base| self.sweep_one(base, cfg, &mut sh, Some(registered)),
            );
        }
        sh.flush();
        sh
    }

    /// The sweep, split over `workers` threads at bitmap word boundaries.
    ///
    /// # Why the split is sound
    ///
    /// A bitmap word covers 64 grid slots = 512 arena bytes, and an object
    /// START is a single bit. Splitting on a word boundary therefore gives each
    /// worker a disjoint set of BASES -- no object is visited twice and none is
    /// missed, whatever its size. A large object may span many words, but it is
    /// swept by whichever worker owns the word its BASE sits in, and no other
    /// worker touches those bytes because no other base lies inside a live
    /// allocation.
    ///
    /// Each worker writes only into its own [`ZSweepShard`]. The two writes
    /// that leave a shard are `registry.remove` (one `fetch_and` on a word this
    /// worker's range owns) and the per-object `write_bytes` (disjoint spans).
    /// The arena free list is NOT touched: spans are handed over afterwards in
    /// one ordered batch, which is also why the arena guard is not held for the
    /// walk.
    ///
    /// # Why the shards are merged in address order
    ///
    /// `Arena::coalesce_free_list` merges spans that are adjacent, and it can
    /// only see two spans as adjacent if they arrive ascending.
    /// [`ZSweepShard::absorb`] concatenates in range order and joins the seam
    /// where one shard's last run touches the next shard's first, so the merged
    /// sequence is exactly what a serial walk would have produced.
    pub(crate) fn sweep_parallel(
        &self,
        registered: &ZObjectStartsSnapshot,
        floor: usize,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> ZSweepShard {
        // Split on the COVERED words, as the complement arm does (zr z1/al):
        // an even split of `0..word_count()` handed every base to the first
        // worker or two on a large `-Xmx` with a small bumped low end. The
        // ranges ascend and are disjoint, and `for_each_base_in_words` streams
        // only the covered segments inside each, so the absorb order below and
        // the set of bases visited are unchanged.
        let total_words = registered.word_count();
        // zs s3/ac: each spawned worker's thread CPU (`sweep_pool_ms=`).
        let cpu_on = self.collection_cpu_accounting_on();
        let mut pool_cpu_ns = 0u64;
        let mut shards: Vec<ZSweepShard> = std::thread::scope(|scope| {
            let mut handles = Vec::with_capacity(workers);
            for (w0, w1) in registered.split_covered(workers) {
                let w1 = w1.min(total_words);
                if w0 >= w1 {
                    continue;
                }
                handles.push(scope.spawn(move || {
                    let cpu = super::mark::ZThreadCpuSpan::start(cpu_on);
                    let mut sh = ZSweepShard::default();
                    registered.for_each_base_in_words(w0, w1, floor, |base| {
                        self.sweep_one(base, cfg, &mut sh, Some(registered))
                    });
                    sh.flush();
                    (sh, cpu.elapsed_ns())
                }));
            }
            handles
                .into_iter()
                .map(|h| {
                    let (sh, ns) = h.join().expect("zgc sweep worker panicked");
                    pool_cpu_ns = pool_cpu_ns.saturating_add(ns);
                    sh
                })
                .collect()
        });
        self.book_sweep_pool_cpu(pool_cpu_ns);
        // ASCENDING, because `absorb` requires it and the coalescer requires
        // `absorb`. `shards` is already in range order; draining forwards keeps
        // it.
        let mut out = ZSweepShard::default();
        // As in `join_complement`: one allocation for the merged dead list.
        let dead_total: usize = shards.iter().map(|s| s.dead.len()).sum();
        if dead_total != 0 {
            out.dead.reserve_exact(dead_total);
        }
        for sh in shards.iter_mut() {
            out.absorb(sh);
        }
        out
    }

    // =====================================================================
    // zt t6/tc (GN-17) -- THE SHARDED YOUNG-AGED SWEEP
    // =====================================================================

    /// The young-aged sweep's per-object walk over snapshot words `[w0, w1)`:
    /// every registered NON-OLD base (`registered & !old`, word by word) below
    /// `stop_at`, ascending, through [`Self::sweep_one`]. Returns the shard
    /// (flushed) and the below-`floor` slice of its accounting, `(visited,
    /// bytes_freed, bytes_copied, promoted_bytes)` -- the values the shard
    /// held when it reached the first base at or above `floor` (all of it when
    /// it never did, nothing when its first base was already above).
    ///
    /// The serial young-aged sweep is this over `[0, word_count)` with
    /// `stop_at = usize::MAX`; the young complement's pass (1) on the aged arm
    /// is this over `[0, floor_w + 1)` with `stop_at = floor`. Both used to
    /// carry their own copy of the loop.
    ///
    /// The old word is read once per word, BEFORE any base in it is swept:
    /// `age_survivor` can promote a base of this very word (setting its old
    /// bit), and the walk must keep visiting the set it started with -- the
    /// serial loop's behaviour, unchanged.
    ///
    /// # zt t16/gn6 (GN-10): grains and the walk-time split
    ///
    /// The words are taken one young GRAIN at a time (`Z_YOUNG_GRAIN_BYTES`,
    /// 128 words), which changes no visit order. With `cfg.time_walk` the
    /// clock runs from the end of the last grain that held a non-old base to
    /// the first non-old word of the next one, into [`ZSweepShard::walk_ns`]
    /// -- the word walk over grains with nothing to visit (two clock reads per
    /// busy grain, none per idle one). zt t20/gn7: the grain summary that
    /// skipped such grains (GN-10's opt-in young summary) is retired --
    /// on R1/R2 the walk plus the pre-mark and the stream were 12 % of the
    /// minor pause, under the 25 % bar it had to clear (t16 row verdicts).
    pub(crate) fn sweep_young_aged_words(
        &self,
        registered: &ZObjectStartsSnapshot,
        w0: usize,
        w1: usize,
        floor: usize,
        stop_at: usize,
        cfg: &ZSweepCfg,
    ) -> (ZSweepShard, (usize, usize, usize, usize)) {
        const GRAIN_WORDS: usize = ZgcRealHeap::Z_YOUNG_GRAIN_BYTES >> 9;
        let mut sh = ZSweepShard::default();
        let old = self.counters.gen_old_bits.get();
        let old_word = |w: usize| old.map_or(0, |o| o.word_at(w));
        let mut below: Option<(usize, usize, usize, usize)> = None;
        let mut idle_from = cfg.time_walk.then(std::time::Instant::now);
        'words: for (a, b) in registered.segments(w0, w1) {
            let mut c0 = a;
            while c0 < b {
                let g = c0 / GRAIN_WORDS;
                let c1 = ((g + 1) * GRAIN_WORDS).min(b);
                let mut busy = false;
                for w in c0..c1 {
                    let mut word = registered.words[w] & !old_word(w);
                    if word != 0 && !busy {
                        busy = true;
                        if let Some(t) = idle_from.take() {
                            sh.walk_ns += t.elapsed().as_nanos() as u64;
                        }
                    }
                    while word != 0 {
                        let bit = word.trailing_zeros() as usize;
                        word &= word - 1;
                        let addr = registered.base + ((w * 64 + bit) << 3);
                        if addr >= stop_at {
                            break 'words; // ascending: nothing later is below it
                        }
                        if below.is_none() && addr >= floor {
                            below = Some((
                                sh.swept,
                                sh.bytes_freed,
                                sh.bytes_copied,
                                sh.gen_promoted_bytes,
                            ));
                        }
                        self.sweep_one(addr, cfg, &mut sh, Some(registered));
                    }
                }
                if busy && cfg.time_walk {
                    idle_from = Some(std::time::Instant::now());
                }
                c0 = c1;
            }
        }
        if let Some(t) = idle_from {
            sh.walk_ns += t.elapsed().as_nanos() as u64;
        }
        sh.flush();
        let below = below.unwrap_or((
            sh.swept,
            sh.bytes_freed,
            sh.bytes_copied,
            sh.gen_promoted_bytes,
        ));
        (sh, below)
    }

    /// [`Self::sweep_young_aged_words`] over `[0, w_end)`, split over
    /// `workers` threads at bitmap-word boundaries of the COVERED words below
    /// `w_end` (`CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS`, opt-in; `<= 1` is the
    /// serial walk on this thread, byte for byte the old code path).
    ///
    /// # Why sharding a GENERATIONAL sweep is sound (the audit
    /// [`Self::sweep_workers`] asked for)
    ///
    /// [`Self::sweep_parallel`]'s argument covers the non-generational body:
    /// disjoint bases per shard (a start is one bit, the split is on words),
    /// disjoint `write_bytes` spans, `registry.remove` a `fetch_and` on a word
    /// the shard owns, the oversize screens reading the immutable snapshot.
    /// A generational survivor adds three writes, through `age_survivor`:
    ///
    /// * `header.set_gc_age` -- the survivor's own header, owned by the shard
    ///   that owns its base;
    /// * `note_old_start` -> `ZObjectStartBits::insert` -- a
    ///   `fetch_or` on the old-start bitmap, whose word for a base is the
    ///   snapshot word the shard owns (same geometry: `premark_from_old_bits`
    ///   pairs `registered.words[w]` with `old.word_at(w)`);
    /// * `card_object` -> `ZRememberedSetTable::register_old_page_with_base_in`
    ///   + `ZRememberedSet::remember` -- the mutator's card barrier
    ///   (`note_ref_store_slow`), which every Java thread already runs
    ///   concurrently: an `RwLock` page registry, a relaxed base store of the
    ///   same value, and `fetch_or`s on the card and summary words.
    ///
    /// The shard's own accounting (`gen_promoted*`, `gen_young_high_bytes`)
    /// is per shard and summed by [`ZSweepShard::absorb`]. The below-floor
    /// slice is additive: a shard wholly below the floor contributes all of
    /// its figures, one wholly above contributes none, and the one shard whose
    /// range holds the floor contributes its prefix -- so the sum is what the
    /// serial walk reports.
    ///
    /// The spans come back in ascending order (shards are absorbed in range
    /// order), which is all the free-list hand-over needs.
    pub(crate) fn sweep_young_aged_sharded(
        &self,
        registered: &ZObjectStartsSnapshot,
        w_end: usize,
        floor: usize,
        stop_at: usize,
        cfg: &ZSweepCfg,
        workers: usize,
    ) -> (ZSweepShard, (usize, usize, usize, usize)) {
        let ranges = if workers > 1 {
            Self::split_word_segments(&registered.segments(0, w_end), workers)
        } else {
            Vec::new()
        };
        if ranges.len() <= 1 {
            return self.sweep_young_aged_words(registered, 0, w_end, floor, stop_at, cfg);
        }
        self.counters
            .gen_young_sweep_sharded
            .fetch_add(1, Ordering::Relaxed);
        self.counters
            .gen_young_sweep_shards
            .fetch_add(ranges.len(), Ordering::Relaxed);
        let cpu_on = self.collection_cpu_accounting_on();
        let mut pool_cpu_ns = 0u64;
        let mut parts: Vec<(ZSweepShard, (usize, usize, usize, usize))> =
            std::thread::scope(|scope| {
                let mut handles = Vec::with_capacity(ranges.len());
                for &(w0, w1) in &ranges {
                    handles.push(scope.spawn(move || {
                        let cpu = super::mark::ZThreadCpuSpan::start(cpu_on);
                        let out =
                            self.sweep_young_aged_words(registered, w0, w1, floor, stop_at, cfg);
                        (out, cpu.elapsed_ns())
                    }));
                }
                handles
                    .into_iter()
                    .map(|h| {
                        let (out, ns) = h.join().expect("zgc young sweep worker panicked");
                        pool_cpu_ns = pool_cpu_ns.saturating_add(ns);
                        out
                    })
                    .collect()
            });
        self.book_sweep_pool_cpu(pool_cpu_ns);
        let mut out = ZSweepShard::default();
        let dead_total: usize = parts.iter().map(|(s, _)| s.dead.len()).sum();
        if dead_total != 0 {
            out.dead.reserve_exact(dead_total);
        }
        let mut below = (0usize, 0usize, 0usize, 0usize);
        for (sh, b) in parts.iter_mut() {
            out.absorb(sh);
            below.0 += b.0;
            below.1 += b.1;
            below.2 += b.2;
            below.3 += b.3;
        }
        (out, below)
    }

    /// Split ascending word segments into at most `parts` contiguous word
    /// ranges of about equal covered size, ascending and disjoint (a range may
    /// span the uncovered gap between two segments, which
    /// `ZObjectStartsSnapshot::segments` skips again inside the range). The
    /// same construction as `ZObjectStartsSnapshot::split_covered`, over a
    /// caller-bounded segment list.
    pub(crate) fn split_word_segments(
        segs: &[(usize, usize)],
        parts: usize,
    ) -> Vec<(usize, usize)> {
        let total: usize = segs.iter().map(|&(a, b)| b.saturating_sub(a)).sum();
        if total == 0 || parts == 0 {
            return Vec::new();
        }
        let per = total.div_ceil(parts);
        let mut out = Vec::with_capacity(parts);
        let mut start: Option<usize> = None;
        let mut taken = 0usize;
        for &(a, b) in segs {
            let mut w = a;
            while w < b {
                let s0 = *start.get_or_insert(w);
                let take = (per - taken).min(b - w);
                w += take;
                taken += take;
                if taken == per {
                    out.push((s0, w));
                    start = None;
                    taken = 0;
                }
            }
        }
        if let Some(s0) = start {
            out.push((s0, segs.last().map_or(s0, |&(_, b)| b)));
        }
        out
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` for THIS heap: the young-aged
    /// sweep's shard count (zt t6/tc, GN-17). `0` / `1` is serial (the
    /// default). Clamped to [`Z_PARMARK_MAX_WORKERS`] but NOT to the core
    /// count, so a unit test shards on a one-core runner too (oversubscribing
    /// a stop-the-world phase costs time, never an answer).
    pub fn set_gen_young_sweep_workers(&self, n: usize) {
        self.counters
            .gen_young_sweep_workers
            .store(n.clamp(1, Z_PARMARK_MAX_WORKERS), Ordering::Relaxed);
    }

    /// The young-aged sweep's shard count for this cycle (see
    /// [`Self::set_gen_young_sweep_workers`]).
    pub(crate) fn gen_young_sweep_workers(&self) -> usize {
        self.counters
            .gen_young_sweep_workers
            .load(Ordering::Relaxed)
            .max(1)
    }

    /// `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS` -- the seed for
    /// [`Self::gen_young_sweep_workers`]; clamped to the cores like
    /// `CRATONVM_ZGC_PARSWEEP`. Unset, empty or unparsable is 1 (serial).
    pub(crate) fn gen_young_sweep_workers_requested() -> usize {
        match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS") {
            Ok(v) => Self::clamp_sweep_workers(v.trim().parse::<usize>().unwrap_or(1)),
            Err(_) => 1,
        }
    }

    /// `[GC] zgc-gen-cost:`'s `young_sweep_sharded=` / `young_sweep_shards=`:
    /// young-aged sweeps that ran on more than one shard, and the shards they
    /// used (cumulative). `0` with the switch above 1 and minor cycles > 0 is
    /// a vacuous row (the covered words were too few to split, or no young
    /// cycle took the aged arm).
    pub fn gen_young_sweep_shard_report(&self) -> (usize, usize) {
        (
            self.counters
                .gen_young_sweep_sharded
                .load(Ordering::Relaxed),
            self.counters.gen_young_sweep_shards.load(Ordering::Relaxed),
        )
    }

    /// How many threads the sweep should use. `1` is serial.
    ///
    /// # It reaches the complement sweep since 2026-09-04
    ///
    /// It did not before. [`Self::sweep_bitmap`] is default-on and wins the arm
    /// selection in `collect_garbage`, and the worker count was consulted only
    /// by the arms that lost -- so `CRATONVM_ZGC_PARSWEEP=<n>` parsed, clamped,
    /// reported, and did nothing. A switch that answers is worse than one that
    /// is absent: it made "I measured the sharded sweep" a sentence someone
    /// could say about a run that never sharded, and it made
    /// `a_sharded_sweep_leaves_the_arena_exactly_as_the_serial_one_does` green
    /// and vacuous, because both of its arms ran the same serial code.
    ///
    /// The complement was not trivially shardable, which is why it stayed that
    /// way: it chains `prev_end` across the whole address space to produce the
    /// free list as one merged, ordered sequence, and a shard boundary falls in
    /// the middle of that chain. Each shard now emits only the spans strictly
    /// between survivors it saw itself and describes its two edges, and
    /// `ZgcRealHeap::join_complement` walks the shards in address order closing
    /// each seam. `the_worker_count_reaches_the_complement_sweep` is what stops
    /// the switch going quietly inert again.
    ///
    /// # Why this is default-off, on a phase that should parallelise
    ///
    /// The sweep is the largest single term in a whole-heap pause -- 170 ms of
    /// 265 ms on the 13.0M-dead-object cycle the 2026-08-17 anatomy measured --
    /// and unlike the mark it is a linear scan with an independent body, so it
    /// is bandwidth bound rather than latency bound and should scale.
    ///
    /// That is an argument, not a measurement, and this tree has shipped a ZGC
    /// marking feature default-ON on an argument before: parallel STW marking,
    /// 2026-08-14, which cost +31% at one worker and +153% at four and was
    /// reverted. `Z_CONC_START_PERCENT_DEFAULT` records the lesson in as many
    /// words. `CRATONVM_ZGC_PARSWEEP=<n>` is the opt-in, and
    /// `gen_sweep_cost_stats`' `swept` counter beside `--verbose:gc`'s
    /// `sweep_us` is how the arm is compared.
    ///
    /// # The one configuration it refuses
    ///
    /// A GENERATIONAL cycle, whatever the setting says. Its survivor arm calls
    /// `age_survivor`, which on the promoting collection calls `card_object`
    /// and writes the remembered set -- shared state whose concurrency this
    /// change has not audited. A young cycle also has nothing to gain: its
    /// sweep is 6.3 ms of a 112 ms pause, because the floor already bounds it.
    /// (zt t6/tc: that audit is done for the young-aged sweep, which is NOT
    /// floor-bounded and was ~210 ms young cycles on `Zr1GnGenShape` -- see
    /// [`Self::sweep_young_aged_sharded`], its own opt-in
    /// `CRATONVM_ZGC_GEN_YOUNG_SWEEP_WORKERS`. This count stays 1 for the
    /// other generational arms.)
    /// `CRATONVM_ZGC_BITMAP_SWEEP`: compute free space as the complement of
    /// the live set in one streaming pass over the two bitmaps
    /// ([`Self::sweep_bitmap`]) rather than as the union of the dead objects,
    /// one header read at a time. Default ON; `0`/`off`/`false`/`no` restores
    /// the per-object sweep and the per-object free-list pushes exactly.
    /// # The measurement
    ///
    /// `BinTreesClassic 16` at `-Xmx192m`, release, interleaved on a quiet
    /// host. Both arms performed 5 collections over the same live set and
    /// reported the same `registered=3,125,050 dead=2,940,700`, so they
    /// reclaimed identically and only the cost differs:
    ///
    /// ```text
    ///   sweep_us (mean per cycle)      wall clock
    ///   on    31,253  31,796  30,297  29,883      2839  3106  3157  2888 ms
    ///   off   84,645  61,867  67,357  61,591      4122  3472  3614  3386 ms
    /// ```
    ///
    /// A 2.1-2.7x faster sweep and ~11% off the whole run, 4/4 rounds. Read
    /// `sweep_us` on the `[GC] zgc-pause:` line rather than the wall clock
    /// when re-measuring: at a heap size where the run performs one or two
    /// collections the sweep is a rounding error in the total and the arms
    /// are indistinguishable, which is exactly what an earlier attempt at
    /// `-Xmx512m` showed.
    ///
    /// The regression suite is 87/0 on BOTH arms, release binary, quiet host.
    /// Runs against the DEBUG binary while the box was building something else
    /// showed 5-7 failures per arm whose sets differed in both directions --
    /// three vectors failed only with this feature OFF, which it cannot cause --
    /// and every one of them passed in isolation afterwards. That is the
    /// host, not the sweep, and it is worth knowing before reading a red one.
    ///
    /// Read per COLLECTION rather than latched in a `OnceLock`: this is
    /// consulted once a cycle, so caching it buys nothing and costs the
    /// ability to A/B the two sweeps against one another in one process --
    /// which is the only way to assert they agree.
    pub(crate) fn bitmap_sweep_enabled(&self) -> bool {
        match cratonvm_types::flags::runtime_var_os("CRATONVM_ZGC_BITMAP_SWEEP") {
            Some(raw) => {
                let v = raw.to_string_lossy().trim().to_ascii_lowercase();
                !matches!(v.as_str(), "0" | "off" | "false" | "no")
            }
            None => true,
        }
    }

    pub(crate) fn sweep_workers(&self, gen_on: bool) -> usize {
        if gen_on {
            return 1;
        }
        self.sweep_worker_count.load(Ordering::Relaxed).max(1)
    }

    /// Set the sweep's worker count for THIS heap. `0` or `1` is serial.
    ///
    /// Per heap rather than a process-wide latch, for the reason
    /// `generational_enabled` and `tlab_enabled` are: the flag readers cache in
    /// a `OnceLock`, so the first test to touch `CRATONVM_ZGC_PARSWEEP` would
    /// decide the arm for every other test in the binary -- and the arm
    /// equivalence this feature rests on can only be asserted by a test that
    /// runs both in one process.
    pub fn set_sweep_workers(&self, n: usize) {
        self.sweep_worker_count
            .store(Self::clamp_sweep_workers(n), Ordering::Relaxed);
    }

    /// Never more workers than cores: this is a stop-the-world phase, so
    /// oversubscription buys nothing and costs context switches inside the
    /// pause it is meant to shorten.
    pub(crate) fn clamp_sweep_workers(n: usize) -> usize {
        if n <= 1 {
            return 1;
        }
        let cores = std::thread::available_parallelism()
            .map(|c| c.get())
            .unwrap_or(1);
        n.min(cores).min(Z_PARMARK_MAX_WORKERS)
    }

    /// `CRATONVM_ZGC_PARSWEEP` -- the seed for [`Self::sweep_worker_count`].
    pub(crate) fn sweep_workers_requested() -> usize {
        match cratonvm_types::flags::runtime_var("CRATONVM_ZGC_PARSWEEP") {
            Ok(v) => Self::clamp_sweep_workers(v.trim().parse::<usize>().unwrap_or(1)),
            Err(_) => 1,
        }
    }

    /// zs s3/ac: add the spawned sweep workers' thread CPU of one cycle to
    /// `sweep_pool_ms=` ([`ZgcRealHeap::collection_cpu_totals`]). Zero when
    /// accounting is off (the spans read no clock).
    fn book_sweep_pool_cpu(&self, ns: u64) {
        if ns != 0 {
            self.counters
                .sweep_pool_cpu_ns
                .fetch_add(ns, Ordering::Relaxed);
        }
    }
}

// ===========================================================================
// Tests
// ===========================================================================

/// `withhold_skip_regions`, against a byte-granular oracle.
///
/// # Why this module exists
///
/// *Added 2026-09-20 (wave 2), closing the gap wave 1 flagged on its own work.*
///
/// [`ZgcRealHeap::withhold_skip_regions`] was rewritten on 2026-09-20 from an
/// `O(spans x skips)` loop that allocated `1 + skips.len()` `Vec`s **per span**
/// into a normalise-once / binary-search-per-span pass. The rewrite is pure
/// performance and it landed with **no direct test at all** -- the only thing
/// exercising it was a whole-heap integration path that needs a peer frozen in
/// compiled code to publish a TLAB tail, which is not a shape a unit test can
/// reach and not a shape CI reaches either.
///
/// A logic error here is silent in the worst way this collector has. The
/// function's job is to keep a published TLAB tail **off the free list**: its
/// owner is parked mid-allocation in compiled code and will resume bumping into
/// those bytes. Hand them to `Arena::add_free_block` and the arena serves the
/// same address to another thread, so two Java objects are constructed over one
/// another -- and the cut that was dropped leaves no trace, because the free
/// list it should have appeared on is the one being rebuilt wholesale.
///
/// So every test below compares against an **independent oracle** that decides
/// membership one byte at a time, rather than spot-checking a few boundaries.
/// The rewrite's three assumptions -- cuts sorted, cuts merged, and each span
/// entered at the first cut that can touch it -- are each given a case that is
/// wrong without them: unsorted cuts, overlapping cuts, touching cuts, a cut
/// spanning several spans, and cuts at both ends of a span.
#[cfg(test)]
mod tests {
    use super::*;

    /// zr z8/zb: the dead-header skip is default ON, `=0` (and an empty value)
    /// turns the store back on, an armed stale-reference hunt keeps the store
    /// when the switch is unset, and an explicit `=1` wins over the hunt.
    #[test]
    fn the_dead_header_skip_is_default_on_and_yields_to_a_stale_reference_hunt() {
        const K: &str = "CRATONVM_ZGC_SWEEP_SKIP_DEAD_HEADERS";
        let read = |edits: &[(&str, Option<&str>)]| {
            cratonvm_types::flags::with_thread_overrides(edits, zgc_sweep_skip_dead_headers_enabled)
        };
        let with = |k: Option<&str>, hunt: Option<&'static str>| {
            let mut e: Vec<(&str, Option<&str>)> = STALE_REFERENCE_HUNTS
                .iter()
                .map(|&h| (h, (Some(h) == hunt).then_some("1")))
                .collect();
            e.push((K, k));
            read(&e)
        };
        assert!(ZGC_SWEEP_SKIP_DEAD_HEADERS_DEFAULT_ON);
        assert!(with(None, None), "unset = default ON");
        assert!(!with(Some("0"), None), "`=0` is the kill");
        assert!(!with(Some(""), None), "an empty value is off");
        assert!(with(Some("1"), None));
        for hunt in STALE_REFERENCE_HUNTS {
            assert!(!with(None, Some(hunt)), "{hunt} keeps the store");
            assert!(with(Some("1"), Some(hunt)), "`=1` wins over {hunt}");
        }
    }

    /// Set difference decided per byte: the slowest possible implementation,
    /// which is the point -- it shares no structure with the one under test.
    ///
    /// Byte granularity keeps the oracle obviously correct, so every extent
    /// here is small. Zero-length spans are deliberately never constructed:
    /// the producers (`sweep_bitmap_range`, `join_complement`) both push only
    /// under a strict `>`, so a zero-length span is not in the domain.
    fn oracle(
        spans: &[(usize, usize)],
        skips: &[(usize, usize)],
        base: usize,
    ) -> Vec<(usize, usize)> {
        let cuts: Vec<(usize, usize)> = skips
            .iter()
            .map(|&(s, e)| (s.saturating_sub(base), e.saturating_sub(base)))
            .filter(|&(s, e)| e > s)
            .collect();
        let mut out: Vec<(usize, usize)> = Vec::new();
        for &(off, len) in spans.iter() {
            let mut run: Option<(usize, usize)> = None;
            for b in off..off + len {
                if cuts.iter().any(|&(s, e)| b >= s && b < e) {
                    if let Some(r) = run.take() {
                        out.push(r);
                    }
                } else {
                    run = match run {
                        Some((ro, rl)) => Some((ro, rl + 1)),
                        None => Some((b, 1)),
                    };
                }
            }
            if let Some(r) = run.take() {
                out.push(r);
            }
        }
        out
    }

    /// Run the real function and the oracle over the same input and demand they
    /// agree exactly, element for element and in order.
    #[track_caller]
    fn agree(spans: &[(usize, usize)], skips: &[(usize, usize)], base: usize) {
        let expected = oracle(spans, skips, base);
        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, skips, base);
        assert_eq!(
            got, expected,
            "withhold_skip_regions disagreed with the per-byte oracle\n  spans: \
             {spans:?}\n  skips: {skips:?}\n  base:  {base}"
        );
        // Ascending and non-overlapping, because the arena's coalescer only
        // sees two spans as adjacent if they arrive in order.
        for w in got.windows(2) {
            let (a, b) = (w[0], w[1]);
            assert!(
                a.0 + a.1 <= b.0,
                "output must stay ascending and disjoint: {a:?} then {b:?}"
            );
        }
        // No empty spans: an empty free block is a free-list entry nobody can
        // allocate out of and the producers never make one.
        assert!(
            got.iter().all(|&(_, l)| l > 0),
            "withhold_skip_regions must not emit a zero-length span: {got:?}"
        );
    }

    const BASE: usize = 0x1_0000;

    /// The shape the function exists for: one big low-region free span with a
    /// frozen peer's published TLAB tail sitting inside it.
    ///
    /// The tail must come out and **both flanks must survive**. An
    /// implementation that dropped the whole span would be "safe" against the
    /// double-allocation bug and would leak the rest of the region every cycle
    /// a peer is frozen, so the flanks are asserted explicitly and not only
    /// through the oracle.
    #[test]
    fn a_frozen_peers_published_tail_is_carved_out_and_the_flanks_survive() {
        let spans = [(0usize, 400usize)];
        let tail = (BASE + 120, BASE + 180);
        agree(&spans, &[tail], BASE);

        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &[tail], BASE);
        assert_eq!(
            got,
            vec![(0, 120), (180, 220)],
            "the tail is withheld and the bytes either side are still free"
        );
    }

    /// A tail that covers a whole span removes it, and a tail that covers every
    /// span empties the list. This is the case where "the free list is now
    /// shorter" must not be mistaken for a bug by a future reader.
    #[test]
    fn a_tail_covering_a_span_removes_it_entirely() {
        let spans = [(0usize, 64usize), (64, 64), (200, 40)];
        let skips = [(BASE + 64, BASE + 128)];
        agree(&spans, &skips, BASE);

        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &skips, BASE);
        assert_eq!(got, vec![(0, 64), (200, 40)]);

        let whole = [(BASE, BASE + 1024)];
        agree(&spans, &whole, BASE);
        let mut all = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut all, &whole, BASE);
        assert!(all.is_empty(), "every byte was withheld: {all:?}");
    }

    /// A tail flush against the start or the end of a span leaves exactly one
    /// piece. These are the `cur == lo` and `cur >= hi` arms of the carve, and
    /// an off-by-one in either produces a one-byte span or a one-byte leak --
    /// neither of which a coarse assertion would notice.
    #[test]
    fn a_tail_flush_with_either_edge_leaves_one_piece() {
        let spans = [(100usize, 100usize)];
        agree(&spans, &[(BASE + 100, BASE + 140)], BASE);
        agree(&spans, &[(BASE + 160, BASE + 200)], BASE);
        // ...and one that abuts without overlapping touches nothing.
        agree(&spans, &[(BASE + 60, BASE + 100)], BASE);
        agree(&spans, &[(BASE + 200, BASE + 260)], BASE);

        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 200, BASE + 260)], BASE);
        assert_eq!(got, vec![(100, 100)], "an abutting cut removes nothing");
    }

    /// **Unsorted** cuts. The rewrite sorts once up front; the old loop did not
    /// need to because it restarted per cut. If the sort were dropped, the
    /// forward walk would stop at the first cut whose start is past `hi` and
    /// silently keep the bytes a later, lower cut covers -- which is the
    /// silent-leak direction.
    #[test]
    fn cuts_are_normalised_regardless_of_the_order_they_arrive_in() {
        let spans = [(0usize, 500usize)];
        let ascending = [
            (BASE + 40, BASE + 60),
            (BASE + 100, BASE + 130),
            (BASE + 300, BASE + 310),
        ];
        let shuffled = [
            (BASE + 300, BASE + 310),
            (BASE + 40, BASE + 60),
            (BASE + 100, BASE + 130),
        ];
        agree(&spans, &ascending, BASE);
        agree(&spans, &shuffled, BASE);

        let mut a = spans.to_vec();
        let mut b = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut a, &ascending, BASE);
        ZgcRealHeap::withhold_skip_regions(&mut b, &shuffled, BASE);
        assert_eq!(a, b, "the answer cannot depend on the skip list's order");
    }

    /// **Overlapping and touching** cuts. The merge step is what lets the carve
    /// assume the next cut starts strictly above the previous one's end; without
    /// it `cur` can move backwards and emit an overlapping -- i.e. double-freed
    /// -- output span.
    #[test]
    fn overlapping_and_touching_cuts_merge_before_the_carve() {
        let spans = [(0usize, 400usize)];
        agree(
            &spans,
            &[(BASE + 50, BASE + 150), (BASE + 100, BASE + 200)],
            BASE,
        );
        agree(
            &spans,
            &[(BASE + 50, BASE + 150), (BASE + 150, BASE + 200)],
            BASE,
        );
        // One cut wholly inside another.
        agree(
            &spans,
            &[(BASE + 40, BASE + 300), (BASE + 100, BASE + 120)],
            BASE,
        );
        // Three identical cuts.
        agree(
            &spans,
            &[
                (BASE + 10, BASE + 20),
                (BASE + 10, BASE + 20),
                (BASE + 10, BASE + 20),
            ],
            BASE,
        );
    }

    /// One cut straddling several spans, which is the shape a published tail
    /// takes when the complement pass found survivors inside it. Each span is
    /// entered independently through `partition_point`, so this is the case
    /// where a stale `i` carried between spans would show.
    #[test]
    fn one_cut_across_several_spans() {
        let spans = [(0usize, 50usize), (60, 50), (120, 50), (200, 50)];
        agree(&spans, &[(BASE + 30, BASE + 210)], BASE);

        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 30, BASE + 210)], BASE);
        assert_eq!(got, vec![(0, 30), (210, 40)]);
    }

    /// Several cuts inside one span, including two with a survivor between
    /// them: the `while i < cuts.len() && cuts[i].0 < hi` loop must advance
    /// through both.
    #[test]
    fn several_cuts_inside_one_span() {
        let spans = [(0usize, 300usize)];
        let skips = [
            (BASE + 20, BASE + 40),
            (BASE + 60, BASE + 80),
            (BASE + 250, BASE + 400),
        ];
        agree(&spans, &skips, BASE);

        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &skips, BASE);
        assert_eq!(got, vec![(0, 20), (40, 20), (80, 170)]);
    }

    /// Degenerate skips: empty ranges, inverted ranges, and ranges below the
    /// arena base. `saturating_sub` collapses the last of these onto offset 0,
    /// which is the behaviour the clip has always had -- a cut reaching below
    /// the arena clips to its start rather than wrapping.
    #[test]
    fn degenerate_skips_are_dropped_or_clipped_but_never_wrap() {
        let spans = [(0usize, 200usize)];
        // Empty and inverted: no effect at all.
        agree(&spans, &[(BASE + 50, BASE + 50)], BASE);
        agree(&spans, &[(BASE + 90, BASE + 50)], BASE);
        let mut got = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut got, &[(BASE + 90, BASE + 50)], BASE);
        assert_eq!(got, vec![(0, 200)], "an inverted range carves nothing");

        // Wholly below the base: both ends saturate to 0, so it is empty and
        // dropped rather than becoming a huge wrapped cut.
        agree(&spans, &[(BASE - 400, BASE - 200)], BASE);
        let mut low = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut low, &[(BASE - 400, BASE - 200)], BASE);
        assert_eq!(low, vec![(0, 200)]);

        // Straddling the base: clipped to [0, e - base).
        agree(&spans, &[(BASE - 100, BASE + 30)], BASE);
        let mut straddle = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut straddle, &[(BASE - 100, BASE + 30)], BASE);
        assert_eq!(straddle, vec![(30, 170)]);
    }

    /// An empty skip list is the overwhelmingly common case and must be exactly
    /// a no-op.
    #[test]
    fn an_empty_skip_list_changes_nothing() {
        let spans = vec![(0usize, 64usize), (128, 32), (4096, 8)];
        let mut got = spans.clone();
        ZgcRealHeap::withhold_skip_regions(&mut got, &[], BASE);
        assert_eq!(got, spans);
        // ...and so does a list of nothing but degenerate ranges.
        let mut got2 = spans.clone();
        ZgcRealHeap::withhold_skip_regions(&mut got2, &[(BASE + 8, BASE + 8)], BASE);
        assert_eq!(got2, spans);
    }

    /// Applying the same cuts twice must be a no-op the second time. The pass
    /// runs once per cycle today, but idempotence is what says the output is a
    /// genuine set difference rather than an operation with internal state --
    /// and it is the property a future "clip the cursor too" caller will assume.
    #[test]
    fn carving_is_idempotent() {
        let spans = [(0usize, 300usize), (400, 100)];
        let skips = [(BASE + 20, BASE + 60), (BASE + 280, BASE + 420)];
        let mut once = spans.to_vec();
        ZgcRealHeap::withhold_skip_regions(&mut once, &skips, BASE);
        let mut twice = once.clone();
        ZgcRealHeap::withhold_skip_regions(&mut twice, &skips, BASE);
        assert_eq!(once, twice);
    }

    /// A deterministic pseudo-random sweep over many span/skip layouts.
    ///
    /// No `rand` dependency and no seed from the clock: an xorshift with a
    /// fixed seed, so a failure here reproduces exactly. This is what covers
    /// the interleavings the named cases above do not enumerate -- in
    /// particular the `partition_point` boundary, which is only interesting
    /// when a cut ends exactly at a span's start or begins exactly at its end.
    #[test]
    fn randomised_layouts_agree_with_the_oracle() {
        let mut state: u64 = 0x5A47_4352_4C43_0001;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..600 {
            // Ascending, disjoint spans -- the shape the complement pass emits.
            let mut spans: Vec<(usize, usize)> = Vec::new();
            let mut cursor = 0usize;
            let n = (next() % 6) as usize + 1;
            for _ in 0..n {
                cursor += (next() % 12) as usize;
                let len = (next() % 24) as usize + 1;
                spans.push((cursor, len));
                cursor += len;
            }
            // Skips in ABSOLUTE addresses, unsorted and free to overlap, which
            // is what `set_jit_tlab_skip_regions` allows.
            let mut skips: Vec<(usize, usize)> = Vec::new();
            let m = (next() % 5) as usize;
            for _ in 0..m {
                let s = (next() % 160) as usize;
                let e = s + (next() % 30) as usize;
                skips.push((BASE + s, BASE + e));
            }
            agree(&spans, &skips, BASE);
        }
    }

    // -- the page census ---------------------------------------------------

    /// **A page with one survivor is not wholly dead, and a page with a
    /// thousand corpses and no survivor is.**
    ///
    /// The census is the measurement
    /// `proposal-b-page-based-evacuation.md`'s O(pages) sweep argument rests
    /// on, so the classification has to be exact at the boundary that matters:
    /// ONE live object is the difference between a page a page-based heap
    /// returns for free and a page it must still walk. The test therefore
    /// builds a page whose only difference from a wholly-dead one is a single
    /// mark bit.
    #[test]
    fn the_page_census_classifies_each_logical_page_by_its_survivors() {
        use super::super::starts::ZObjectStartBits;

        const BASE: usize = 0x1000_0000;
        let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        let pages = 3usize;
        let span = page * pages;
        // 512 arena bytes per bitmap word.
        let words_per_page = page >> 9;

        let marks = ZObjectStartBits::new(BASE, span);
        let mut words = vec![0u64; words_per_page * pages];
        let mut set_start = |addr: usize, words: &mut Vec<u64>| {
            let idx = (addr - BASE) >> 3;
            words[idx >> 6] |= 1u64 << (idx & 63);
        };

        // Page 0: two survivors and three corpses -> OCCUPIED.
        for k in 0..5usize {
            let addr = BASE + k * 64;
            set_start(addr, &mut words);
            if k < 2 {
                marks.insert(addr);
            }
        }
        // Page 1: five corpses, no survivor -> WHOLLY DEAD. Spread across the
        // page so the classification cannot come from one word being zero.
        for k in 0..5usize {
            set_start(BASE + page + k * 4096, &mut words);
        }
        // Page 2: nothing at all -> EMPTY.

        let snapshot = ZObjectStartsSnapshot {
            words: words.into(),
            base: BASE,
            extra: rustc_hash::FxHashSet::default(),
            covered: None,
        };
        let n = snapshot.words.len().min(marks.word_count());
        let c = ZgcRealHeap::sweep_page_census(&snapshot, &marks, BASE, BASE + span, n);

        assert_eq!(c.pages, pages);
        assert_eq!(c.occupied, 1, "the page with survivors");
        assert_eq!(c.wholly_dead, 1, "the page with corpses and no survivor");
        assert_eq!(c.empty, 1, "the page nothing was ever allocated in");
        assert_eq!(c.occupied + c.wholly_dead + c.empty, c.pages, "exhaustive");
        assert_eq!(c.live_objects, 2);
        assert_eq!(c.dead_objects, 3 + 5);
        assert_eq!(
            c.dead_on_wholly_dead_pages, 5,
            "only page 1's corpses are on a page a page-based reclaim could return"
        );

        // THE BOUNDARY: mark ONE object on the wholly-dead page and it stops
        // being wholly dead. Nothing else about the heap changed.
        marks.insert(BASE + page);
        let c2 = ZgcRealHeap::sweep_page_census(&snapshot, &marks, BASE, BASE + span, n);
        assert_eq!(c2.wholly_dead, 0, "one survivor is enough to keep the page");
        assert_eq!(c2.occupied, 2);
        assert_eq!(c2.dead_on_wholly_dead_pages, 0);
        assert_eq!(c2.live_objects, 3);
        assert_eq!(c2.dead_objects, 3 + 4);
    }

    /// The census refuses a snapshot whose base is not the arena base rather
    /// than attributing bases to the wrong page. Instrumentation that guesses
    /// is worse than instrumentation that is absent -- the round's own record
    /// is a census that was 42% high and carried the authority of having been
    /// machine-checked.
    #[test]
    fn the_page_census_declines_a_snapshot_it_cannot_place() {
        use super::super::starts::ZObjectStartBits;

        const BASE: usize = 0x1000_0000;
        let page = ZgcRealHeap::Z_LOGICAL_PAGE_BYTES;
        let marks = ZObjectStartBits::new(BASE, page);
        let snapshot = ZObjectStartsSnapshot {
            words: vec![0u64; page >> 9].into(),
            base: BASE + 4096,
            extra: rustc_hash::FxHashSet::default(),
            covered: None,
        };
        let c = ZgcRealHeap::sweep_page_census(
            &snapshot,
            &marks,
            BASE,
            BASE + page,
            snapshot.words.len(),
        );
        assert_eq!(c, ZSweepPageCensus::default());
    }

    /// Everything the complement pass reports that the region step could
    /// change: the low free spans, the new cursor, the high-end holes, and
    /// every counter.
    #[derive(Debug, PartialEq, Eq)]
    struct ComplementOutcome {
        spans: Vec<(usize, usize)>,
        cursor: usize,
        high_spans: Vec<(usize, usize)>,
        swept: usize,
        dead_count: usize,
        dead_headers_skipped: usize,
        bytes_copied: usize,
        objects_copied: usize,
        bytes_freed: usize,
        unsizable: usize,
        unsizable_high: usize,
    }

    /// Build a heap from `seed` (same seed, same layout: no TLABs, so every
    /// allocation is a deterministic bump), mark a live set chosen in phases --
    /// long all-dead stretches (whole 32 KiB blocks with no survivor), sparse
    /// stretches (mostly-dead words), dense stretches -- plus huge arrays at
    /// the large-object end, some live, and run the complement sweep with the
    /// region step on or off. Returns the outcome (arena-relative) and the
    /// region step's engagement.
    fn complement_outcome(seed: u64, region: bool, workers: usize) -> (ComplementOutcome, usize) {
        use crate::collector::GarbageCollector;
        use cratonvm_types::{ArrayElementType, ClassId};
        let heap = ZgcRealHeap::with_capacity(16 * 1024 * 1024);
        heap.set_tlab_enabled(false);
        let marks = heap
            .mark_bits
            .as_ref()
            .expect("side mark bits are the default");
        let mut s = seed | 1;
        let mut next = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };
        let big = super::super::ZGC_LARGE_OBJECT_MIN;
        let mut phase_left = 0u64;
        let mut live_one_in = 0u64; // 0 = none live
        for i in 0..24_000u64 {
            if phase_left == 0 {
                phase_left = 50 + next() % 600;
                live_one_in = match next() % 4 {
                    0 | 1 => 0, // an all-dead stretch
                    2 => 40 + next() % 60,
                    _ => 1 + next() % 3,
                };
            }
            phase_left -= 1;
            let o = if i % 4_001 == 4_000 {
                heap.alloc_array(
                    ClassId::new(1),
                    ArrayElementType::Byte,
                    big + (next() % 4096) as usize,
                )
            } else {
                heap.alloc_object(ClassId::new(11), (next() % 40) as usize)
            };
            let live = live_one_in != 0 && next() % live_one_in == 0;
            if live {
                assert!(marks.claim(o.as_ptr() as usize));
            }
        }
        let cfg = {
            let arena = heap.arena.lock();
            ZSweepCfg {
                arena_base: arena.base_ptr() as usize,
                high_floor: arena.high_cursor(),
                arena_used: arena.used(),
                zero_header_only: true,
                skip_dead_headers: true,
                merge_dead_runs: true,
                bulk_clearable: true,
                want_dead: false,
                collect_dead_hashes: false,
                region_skip: region,
                gen_on: false,
                promo_age: 3,
                record_live_cells: false,
                reserve_live_hints: 0,
                size_memo: None,
                time_walk: false,
            }
        };
        let registered = heap.registry.snapshot();
        let (mut sh, spans, cursor) = heap
            .sweep_bitmap(&registered, &cfg, workers)
            .expect("the complement arm accepts this cycle");
        sh.flush();
        let skipped = sh.region_words_skipped;
        (
            ComplementOutcome {
                spans,
                cursor,
                high_spans: std::mem::take(&mut sh.spans),
                swept: sh.swept,
                dead_count: sh.dead_count,
                dead_headers_skipped: sh.dead_headers_skipped,
                bytes_copied: sh.bytes_copied,
                objects_copied: sh.objects_copied,
                bytes_freed: sh.bytes_freed,
                unsizable: sh.unsizable,
                unsizable_high: sh.unsizable_high.len(),
            },
            skipped,
        )
    }

    /// zr z5/sw: **the region step frees exactly what the word walk frees.**
    /// On randomized heaps, serial and sharded (shard seams land inside
    /// blocks and inside dead stretches), the free-span set, the cursor, the
    /// large-object end's holes and every counter are identical with the step
    /// on and off -- and the step really engaged (positive control), so the
    /// equality is not two runs of the same code.
    #[test]
    fn sweep_region_step_matches_the_word_walk() {
        let mut engaged = 0usize;
        for seed in [0x9E37_79B9_7F4A_7C15u64, 7, 0xDEAD_BEEF, 12_345_678_901] {
            for workers in [1usize, 3] {
                let (off, off_skipped) = complement_outcome(seed, false, workers);
                let (on, on_skipped) = complement_outcome(seed, true, workers);
                assert_eq!(off_skipped, 0, "the step must not run when it is off");
                assert_eq!(
                    on, off,
                    "seed {seed:#x}, {workers} worker(s): the region step changed the sweep"
                );
                assert!(
                    !on.spans.is_empty() && on.dead_count > 0,
                    "a heap with garbage"
                );
                engaged += on_skipped;
            }
        }
        assert!(
            engaged > 0,
            "positive control: some block must have been skipped"
        );
    }

    /// The step must stand down whenever a per-dead-object obligation is
    /// owed: with the header store on, or a dead list wanted, the outcome is
    /// the word walk's and the counter stays 0.
    #[test]
    fn the_region_step_stands_down_when_dead_objects_are_owed_work() {
        use crate::collector::GarbageCollector;
        use cratonvm_types::ClassId;
        let heap = ZgcRealHeap::with_capacity(4 * 1024 * 1024);
        heap.set_tlab_enabled(false);
        for _ in 0..2_000 {
            heap.alloc_object(ClassId::new(11), 30); // all dead: whole empty blocks
        }
        for (skip_headers, want_dead) in [(false, false), (true, true)] {
            let cfg = {
                let arena = heap.arena.lock();
                ZSweepCfg {
                    arena_base: arena.base_ptr() as usize,
                    high_floor: arena.high_cursor(),
                    arena_used: arena.used(),
                    zero_header_only: true,
                    skip_dead_headers: skip_headers,
                    merge_dead_runs: true,
                    bulk_clearable: true,
                    want_dead,
                    collect_dead_hashes: false,
                    region_skip: true,
                    gen_on: false,
                    promo_age: 3,
                    record_live_cells: false,
                    reserve_live_hints: 0,
                    size_memo: None,
                    time_walk: false,
                }
            };
            let registered = heap.registry.snapshot();
            let marks = heap.mark_bits.as_ref().expect("side mark bits");
            let words = registered.words.len().min(marks.word_count());
            let base = cfg.arena_base;
            let shard = heap.sweep_bitmap_range(
                &registered,
                &cfg,
                marks,
                0,
                words,
                base,
                base.saturating_add(cfg.high_floor),
            );
            assert_eq!(
                shard.sh.region_words_skipped, 0,
                "skip_headers={skip_headers} want_dead={want_dead}: dead objects are owed \
                 per-object work, so no block may be skipped"
            );
            assert_eq!(shard.sh.dead_count, 2_000);
            if want_dead {
                assert_eq!(shard.sh.dead.len(), 2_000, "the list is complete");
            }
        }
    }
}
