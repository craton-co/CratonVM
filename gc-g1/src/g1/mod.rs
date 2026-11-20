// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! G1 (Garbage-First) garbage collector.
//!
//! A region-based, generational, incremental, parallel, mostly concurrent
//! garbage collector. Key features:
//!
//! - **Region-based heap:** Fixed-size regions classified as Eden, Survivor,
//!   Old, Humongous, or Free.
//! - **Young collection (STW):** Evacuate all Eden + Survivor regions.
//! - **Mixed collection:** Evacuate young + selected old regions (worst-first).
//! - **Concurrent marking:** Tri-color marking with SATB barriers.
//! - **IHOP:** Initiating Heap Occupancy Percent triggers concurrent marking.
//! - **Region pinning (JEP 423):** Pin regions during JNI critical sections.
//! - **String deduplication:** Deduplicate identical String backing arrays.
//! - **Humongous allocation:** Objects > region_size/2 span contiguous regions.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use parking_lot::{Mutex, RwLock};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup};
use cratonvm_gc_core::concurrent_mark::{ConcurrentGcPhase, ConcurrentGcState};
use cratonvm_gc_core::gc::{GcResult, GcStats};
use cratonvm_gc_core::gc_flags;
use cratonvm_gc_core::heap::{
    array_data_size, array_element_type_from_tag, object_kind_from_tag, ArrayElementType,
    ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET, GC_FLAG_OLD_GEN, HEADER_SIZE, SLOT_SIZE,
};
use cratonvm_gc_core::mark_bitmap::MarkBitmap;
use cratonvm_gc_core::region::{RegionType, RememberedSet};
use cratonvm_gc_core::satb::SatbQueue;
use cratonvm_types::{ClassId, ObjectRef, Value};

mod overlay;
pub use overlay::g1_overlay_census;

// The G1 near-full proxy-define page: the in-place full compaction
// (HotSpot's `G1FullCollector` shape), see its module doc.
mod compact_in_place;
pub use compact_in_place::{
    g1_in_place_compaction_census, g1_in_place_compaction_declines, g1_in_place_phase_us,
    g1_in_place_compaction_exclusions, g1_starved_full_census,
};

// gci i27/gr -- the per-object referent verdict for young regions a pause kept
// in place (`jr20-g1-young-pause-keeps-every-referent-in-a-jit-pinned-region-FIXED-20261016`).
mod pinned_referents;

// g1ep7 -- root regions: a concurrent cycle never marks or traces a young
// object (HotSpot's `G1CMRootMemRegions`), see its module doc.
mod root_regions;
pub use root_regions::g1_root_region_census;
pub use pinned_referents::{
    G1ClosureHolder, G1ClosureHolderKind, G1ClosurePinSource, G1KeptFinalizableClosure,
    G1PinnedReferentDecline, G1PinnedReferentVerdict,
};

#[cfg(test)]
mod needs_gc_scaling;

// gci i19/yc -- the parallel holder body screen and the oversized-copy guard.
#[cfg(test)]
mod i19yc_tests;

// gci i19/fz -- cleanup's dead-object scrub versus an unrooted queued finalizable.
#[cfg(test)]
mod i19fz_tests;

// gci i22/g1rk -- the remark liveness view and the trusted post-cleanup verdict.
#[cfg(test)]
mod i22rk_tests;

// gci i24/ym -- the young model's mtc50 shrink replayed through its predictor functions.
#[cfg(test)]
mod i24ym_tests;

// gci i25/pl -- the Survivor-PLAB copy path's per-copy copy-end publish.
#[cfg(test)]
mod i25pl_tests;

// gci i26/pc -- the parallel copy's per-worker age / promoted counters, flushed at retire_all.
#[cfg(test)]
mod i26pc_tests;

// gci nd48 -- the TLAB reserve's reclaim credit for small fallible requests.
#[cfg(test)]
mod i48_nd48_tests;

// jit r21 parkcredit -- a movable claim unpins only the calling thread's own words.
#[cfg(test)]
mod jr21_movclaim_tests;

// gci i10/rh -- HotSpot's mixed-choice arithmetic (`g1_mixed_choice.rs`). `pub` so its
// functions are reachable and never `dead_code` while only the switch arm calls them.
pub mod mixed_choice;
#[cfg(test)]
mod g1sz_sorted_map_tests;
#[cfg(test)]
mod gci_i9fa_tests;
#[cfg(test)]
mod g1sz_copy_object_bytes_tests;
/// A per-thread positive memo over `G1Collector::is_addr_in_live_region`.
///
/// # The cost it removes
///
/// `is_addr_in_live_region` rejects an out-of-arena address lock-free, and that
/// gate is what made the conservative stack scan affordable. Every address that
/// passes it — i.e. every REAL object pointer — then takes `regions.lock()`, a
/// process-global mutex, to read one region's type, base and cursor. The
/// callers are not rare: `is_object_address` runs it per candidate word of every
/// JIT frame scan and once per object-shaped argument of every native call
/// (`pin_value_for_native_call`). On netty's `PcapWriteHandler` loop — whose
/// stacks are full of genuine `ByteBuf` pointers, so the arena gate rejects
/// almost nothing — `is_addr_in_live_region` was the largest single symbol in
/// the profile at **9.1%**, of which the `MutexGuard` drop alone was 2.9%.
///
/// This is the same shape as the two G1 lock defects already closed
/// (`G1::needs_gc`'s per-allocation region scan, and every G1 accessor taking
/// this mutex to ask `humongous_span`): a global lock on a per-operation path.
///
/// # Why it is sound
///
/// The memo answers **only "yes"**, and only for an address it has already seen
/// this exact answer for, from a region incarnation it can prove is unchanged:
///
/// * **Span.** An entry records `[base, limit)` where `limit` was `base +
///   cursor` at fill time. A region's cursor only ever GROWS within one
///   incarnation, so a memoized limit is an UNDER-approximation of the live
///   span: an address past it misses and takes the authoritative path, which
///   then refreshes the entry. There is no way for the memo to accept an
///   address the locked path would have rejected on cursor grounds.
///
/// * **Incarnation.** Recycling or retyping a region is exactly what
///   `rset_cache_epoch` counts — it is bumped under the `regions` lock, with
///   `Release` ordering, at the start of `young_collection`, `mixed_collection`
///   and `cleanup`. An entry carries the epoch READ BEFORE its fill took the
///   lock, so a recycle racing that fill leaves the entry stamped with an
///   already-superseded epoch and it can never be believed. This is the same
///   invariant `post_write_barrier_rset`'s TLS cache already relies on, for the
///   same hazard — a cached view of a region that a collection may have
///   recycled underneath it — so the memo adds no new maintenance obligation.
///
/// * **Collector identity.** `instance_id` is minted per collector and never
///   reused, so a new collector constructed at a dropped one's address cannot
///   inherit its entries (the failure `post_write_barrier_rset` hit with
///   `self as *const Self`, and the reason that field exists).
///
/// * **Thread locality.** Entries live in thread-local storage, so there is no
///   sharing, no atomics beyond the one epoch load, and no way for one thread's
///   view to be published to another.
///
/// The evacuation-failure arm (`kept_unresolved_*`) is deliberately excluded:
/// liveness there is a per-address set membership, not a span, and the memo can
/// only express a span. Those addresses keep taking the lock.
///
/// # What a miss costs
///
/// One extra atomic load and up to `WAYS` integer compares before the lock that
/// was going to be taken anyway. `CRATONVM_G1_NO_LIVE_REGION_MEMO=1` withdraws
/// the memo entirely so one binary can be A/B'd against its own pre-change
/// behaviour.
pub mod live_region_memo {
    /// Entries per thread. Small and fixed: a stack scan walks one or two
    /// regions (the current Eden, plus whatever survivors it points into), and
    /// a linear walk of four is cheaper than any hashing.
    const WAYS: usize = 4;

    #[derive(Clone, Copy)]
    struct Entry {
        instance_id: u64,
        epoch: u64,
        region_idx: usize,
        base: usize,
        limit: usize,
        /// The span is a `HumongousStart` region, whose only object start is
        /// `base` (see `super::LiveSpan`). A region's type is fixed for one
        /// incarnation, so the epoch that guards the span guards this too.
        humongous_start: bool,
    }

    impl Entry {
        const EMPTY: Entry = Entry {
            instance_id: 0,
            epoch: 0,
            region_idx: usize::MAX,
            base: 0,
            // An empty entry can never accept: `addr >= base && addr < limit`
            // is false for every address when `limit == 0`.
            limit: 0,
            humongous_start: false,
        };
    }

    thread_local! {
        static ENTRIES: std::cell::Cell<[Entry; WAYS]> =
            const { std::cell::Cell::new([Entry::EMPTY; WAYS]) };
        /// Round-robin victim, so a fifth region does not permanently evict the
        /// same slot while three others idle.
        static VICTIM: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    /// `CRATONVM_G1_NO_LIVE_REGION_MEMO=1` — withdraw the memo. Read once.
    fn enabled() -> bool {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *ON.get_or_init(|| {
            !false
        })
    }

    /// `CRATONVM_DBG_G1_LIVE_MEMO=1` — hit/miss, printed at VM shutdown.
    ///
    /// A `hit` here is one `regions.lock()` acquire/release that did not
    /// happen. It exists because the alternative instrument does not work on
    /// this hardware and barely works on this host: the shared Azure box has no
    /// PMU (`perf stat -e instructions` answers `<not supported>`), and its load
    /// average swings between 6 and 33 within an hour, which is far more than
    /// the effect. A count is neither. Read this before quoting any timing
    /// number for the memo — an inert gate shows up as `hit=0`, which is a fact
    /// about the build rather than about the host.
    pub mod stats {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::OnceLock;

        pub(super) static HITS: AtomicU64 = AtomicU64::new(0);
        pub(super) static MISSES: AtomicU64 = AtomicU64::new(0);

        pub(super) fn on() -> bool {
            static ON: OnceLock<bool> = OnceLock::new();
            *ON.get_or_init(|| {
                false            })        }

        /// Final tally. Called from the VM's shutdown reporting alongside the
        /// site-cache dump.
        pub fn dump() {
            if !on() {
                return;
            }
            let h = HITS.load(Ordering::Relaxed);
            let m = MISSES.load(Ordering::Relaxed);
            let total = h + m;
            let pct = if total == 0 {
                0.0
            } else {
                // Widening: u64 counts to f64 for a percentage.
                (h as f64) * 100.0 / (total as f64)
            };
            eprintln!(
                "[g1-live-memo] FINAL hit={h} miss={m} hit_rate={pct:.1}% (a hit is one regions.lock() that did not happen)"
            );
        }
    }

    /// Is `addr` inside a span this thread has already proved live, in a region
    /// incarnation `epoch` says is still current? `Some` carries the span's
    /// kind, exactly as the authoritative path reported it at fill time.
    #[inline]
    pub(super) fn hit(
        instance_id: u64,
        epoch: u64,
        region_idx: usize,
        addr: usize,
    ) -> Option<super::LiveSpan> {
        if !enabled() {
            return None;
        }
        let answered = ENTRIES.with(|c| {
            let entries = c.get();
            for e in entries.iter() {
                if e.region_idx == region_idx
                    && e.instance_id == instance_id
                    && e.epoch == epoch
                    && addr >= e.base
                    && addr < e.limit
                {
                    return Some(if e.humongous_start {
                        super::LiveSpan::HumongousStart { base: e.base }
                    } else {
                        super::LiveSpan::Allocated
                    });
                }
            }
            None
        });
        if stats::on() {
            let counter = if answered.is_some() {
                &stats::HITS
            } else {
                &stats::MISSES
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        answered
    }

    /// `limit`: the memoized end of the span holding `addr` (the region's
    /// base plus its cursor at fill time), under the same match as [`hit`];
    /// a cursor seen once, so it may be stale in either direction, and only
    /// fit to ACCEPT with (see `G1Collector::root_scan_end_is_below_cursor`).
    ///
    /// Not counted in `stats`, which measure `hit`'s lock avoidance.
    #[inline]
    pub(super) fn limit(
        instance_id: u64,
        epoch: u64,
        region_idx: usize,
        addr: usize,
    ) -> Option<usize> {
        if !enabled() {
            return None;
        }
        ENTRIES.with(|c| {
            let entries = c.get();
            for e in entries.iter() {
                if e.region_idx == region_idx
                    && e.instance_id == instance_id
                    && e.epoch == epoch
                    && addr >= e.base
                    && addr < e.limit
                {
                    return Some(e.limit);
                }
            }
            None
        })
    }

    /// Record the span the authoritative path just accepted from.
    ///
    /// `epoch` must be the value read BEFORE the `regions` lock was taken — see
    /// the module doc's incarnation argument.
    #[inline]
    pub(super) fn fill(
        instance_id: u64,
        epoch: u64,
        region_idx: usize,
        base: usize,
        limit: usize,
        humongous_start: bool,
    ) {
        if !enabled() {
            return;
        }
        ENTRIES.with(|c| {
            let mut entries = c.get();
            let slot = entries
                .iter()
                .position(|e| e.region_idx == region_idx && e.instance_id == instance_id)
                .unwrap_or_else(|| {
                    VICTIM.with(|v| {
                        let s = v.get();
                        v.set((s + 1) % WAYS);
                        s
                    })
                });
            entries[slot] = Entry {
                instance_id,
                epoch,
                region_idx,
                base,
                limit,
                humongous_start,
            };
            c.set(entries);
        });
    }
}

#[cfg(test)]
mod tests;

/// gch h4/g1: the remark callback's keep closure, the TLAB floor and the
/// address epoch.


#[cfg(test)]
mod gci_i5g_g1o_tests;

#[cfg(test)]
mod gci_i7_g1y_tests;



#[cfg(test)]
mod gci_i9ng_tests;

#[cfg(test)]
mod gci_i10pp_tests;

/// gci i10/rh -- findings 1 and 2 of
/// `gci-i10rh-g1-humongous-rset-and-mixed-choice-review-findings-FIXED-20261016`, on the
/// `CRATONVM_G1_MIXED_HOTSPOT_CHOICE` arm against the default arm.
#[cfg(test)]
mod gci_i10_rh_tests;

/// gci i11/gm -- one old-CSet selector (finding 3), the compaction pause and
/// the mixed count (finding 8), eager humongous reclaim during marking
/// (finding 6). Filter: `cargo test -p cratonvm-gc --lib gci_i11gm -j 5`.
#[cfg(test)]
mod gci_i11_gm_tests;

#[cfg(test)]
mod gci_i11_go_tests;

/// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 1): the
/// concurrent-start pause scans what a frozen peer holds, so an UNLOGGED
/// overwrite after the pause (a compiled store whose SATB decision was taken
/// before the pause froze it) cannot hide the value the snapshot saw.
#[cfg(test)]
mod gci_i12_wb_tests;

/// gci i13/vg: G1's young set for the split root gather
/// (`G1Collector::parked_old_only_young_ranges`) and its refusals. Filter:
/// `cargo test -p cratonvm-gc gci_i13vg`.
#[cfg(test)]
mod gci_i13vg_g1_tests;

/// gci i15/vh: the G1 half of the mirror-keyed parked roots
/// (`G1Collector::parked_mirror_keyed_view`). Filter:
/// `cargo test -p cratonvm-gc gci_i15vh`.
#[cfg(test)]
mod gci_i15vh_g1_tests;

/// gci i16/mr: the G1 marking cycle's TAMS below the un-retired TLAB tails
/// the initial mark's take-over published, and the remark's row closure on
/// the real marker. Filter: `cargo test -j 5 -p cratonvm-gc gci_i16mr`.
#[cfg(test)]
mod gci_i16mr_g1_tests;

/// gci i17/rv -- review of wave i16's default-path changes
/// (`docs/feature-designs/concurrent-gc-maturation.md`).
/// Filter: `cargo test -j 5 -p cratonvm-gc --lib gci_i17rv_`.
#[cfg(test)]
mod gci_i17rv_g1_tests;

/// gci i14/yt — the HotSpot-shaped young model
/// (`CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`): the pure bounds, the arm's
/// count, the predictors and the record wiring. Filter:
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i14_yt_`.


/// gci i16/pl — finding 12 of
/// `gci-i15ra-g1-evacuation-additions-review-findings-FIXED-20261016`: the
/// HotSpot-shaped young model (`CRATONVM_G1_YOUNG_TARGET_HOTSPOT_MODEL`)
/// prices Eden regions from Eden-sourced copies only and the Survivor
/// regions in their own term. Filter:
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i16_pl_`.


/// gci i15/bg — `CRATONVM_G1_YOUNG_TARGET_BASE_AWARE`: the default young-target
/// loop with HotSpot's "base time is longer than pause time" case
/// (`gci-i14yt-g1-default-young-loop-has-no-base-exceeds-goal-case-FIXED-20261016`).


/// gci i19/ys — `CRATONVM_G1_YOUNG_TARGET_MIN_EDEN` (default ON): the default
/// young-target loop never leaves the target at or below the young regions
/// the last pause left (HotSpot 25's `absolute_min_young_length`), so the
/// young-target arm of `needs_gc` cannot fire before one Eden region has been
/// allocated (`gci-i2st-the-javac-stall-is-a-progressing-pause-regime-not-a-lost-wakeup-FIXED-20261017`,
/// G1 half; vi15-bg def rows).


/// gci i20/ys — the HotSpot young model's drain-census lift (default path,
/// the model being default ON since 2026-10-15), the opt-in humongous IHOP
/// request (`CRATONVM_G1_HUMONGOUS_IHOP_START`) and the opt-in young-only
/// survival sample (`CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY`).


/// gci i22/sz -- G1 survivor sizing against HotSpot 25's
/// `G1Policy::update_survivors_policy` (default path), the drain pass's
/// tenuring merge (default path), the `gc,age` facts, the opt-in mixed
/// minimum-old reserve (`CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD`) and the
/// humongous concurrent-start request (`CRATONVM_G1_HUMONGOUS_IHOP_START`).


/// gci i26/hu -- the opt-in humongous concurrent-start request
/// (`CRATONVM_G1_HUMONGOUS_IHOP_START`) measured as HotSpot 25 measures it:
/// old + humongous REGIONS against the 45 % initial IHOP (vi22-sz HC1 / HC3).


/// gci i23/tn -- G1's HotSpot tenuring flags (`-XX:MaxTenuringThreshold`,
/// `-XX:TargetSurvivorRatio`; default path), the pause's Survivor region cap
/// (`CRATONVM_G1_SURVIVOR_REGION_CAP`, default ON) and the mixed gate's
/// base-trigger waiver (`CRATONVM_G1_MIXED_WAIVER_BASE_TRIGGER`, opt-in).
/// Page: `docs/feature-designs/concurrent-gc-maturation.md`
/// items 4-5; `gci-i20ys-...-review-findings-20261015` item 6.


/// gci i27/gs: `gci-i23tn-g1-tenuring-flags-and-survivor-cap-residuals-FIXED-20261017`
/// item 2 -- the parallel evacuator's Survivor overflow into Old under the
/// existing opt-in `CRATONVM_G1_SURVIVOR_OVERFLOW_TO_OLD`.
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i27_gs_`.


/// gci i48/rv48 -- `gci-i24rv-i22-i23-default-path-review-findings-FIXED-20261017`
/// item 14a. `cargo test -j 5 -p cratonvm-gc --lib gci_i48_rv48_`.
#[cfg(test)]
mod gci_i48_rv48_tests;

/// gci i43/gy -- `gci-i42ta-g1-ignores-xmn-newsize-maxnewsize-newratio-FIXED-20261016`:
/// HotSpot 25's `G1YoungGenSizer` on G1.
#[cfg(test)]
mod gci_i43gy_g1_young_sizer_tests;

/// gci i45/gy (`gci-i43gy-g1-young-bounds-use-max-regions-not-committed-RETIRED-UNFIXED-20261018`):
/// the young bounds follow the committed regions (the soft capacity) and the
/// heap-size-change hook re-places the target; `CRATONVM_G1_YOUNG_BOUNDS_COMMITTED=0`
/// keeps the `-Xmx` grid.


/// gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): the
/// `System.gc()` door's G1 pause tenures every live young object, as
/// HotSpot's Full GC compacts all of them into old regions (no young region
/// survives it). Page
/// `gci-i47w-j46-system-gc-leaves-live-young-data-out-of-the-old-pool-FIXED-20261016`.
#[cfg(test)]
mod gci_i47w_sg47_tests;

#[cfg(test)]
mod r22_memory_maximal_compaction_tests;

/// JIT round 23 wave 6, lane L4: the parallel evacuator with the batched
/// termination counter (`CRATONVM_G1_EVAC_BATCHED_OUTSTANDING`) and the pooled
/// pointer-map merge (`CRATONVM_G1_PAR_POINTER_MAP_MERGE`), both default ON:
/// the `R23W3Keep` shape -- 64 linked chains hanging off one root array --
/// survives a multi-worker young pause whole, every node once.
#[cfg(test)]
mod r23_w6_g1_evac_tests;

/// JIT round 23 wave 7, lane L4: the serial recovery request
/// (`CRATONVM_G1_PARALLEL_UNDER_LOW_FREE_LATCH`) is the EXTERNAL note alone:
/// the ordinary pressure latch no longer asks for it, and a young pause
/// consumes it.
#[cfg(test)]
mod r23_w7_g1_serial_recovery_tests;

/// JIT round 23 wave 8, lane L9: the parallel evacuator's leaf screen
/// (`CRATONVM_G1_EVAC_LEAF_NO_PUSH`), its idle-probe `try_lock`
/// (`CRATONVM_G1_EVAC_IDLE_TRY_LOCK`). The `KeepLong` shape -- 64 chains of
/// nodes that each hold `next` and a `byte[]` payload -- survives a 4-worker
/// young pause whole on every arm: every node once, in order, every payload
/// copied with its bytes.
#[cfg(test)]
mod r23_w8_g1_evac_leaf_tests;

/// JIT round 23 wave 8, lane L9, follow-up: the walk size memo
/// (`CRATONVM_G1_WALK_SIZE_MEMO`) and the futile-young escalation
/// (`CRATONVM_G1_FUTILE_YOUNG_ESCALATION`).
#[cfg(test)]
mod r23_w8_g1_cleanup_and_escalation_tests;

/// JIT round 23 wave 8, lane L9: the serial evacuator's per-pause knobs
/// (`CRATONVM_G1_SERIAL_EVAC_SIZE_MEMO`, `CRATONVM_G1_EVAC_SHAPE_KEY`) copy
/// exactly what the plain path copies.
#[cfg(test)]
mod r23_w8_g1_serial_evac_knob_tests;

/// JIT round 23 wave 9, lane L9: `CRATONVM_G1_SERIAL_EVAC_SCREEN_ONCE`. For an
/// address the slot-candidate screen admits (`classify_candidate_header_view`
/// answers `Object`), the supply screen's verdict is exactly its
/// implausible-legacy-header half, which is all the screen-once arm still asks.
#[cfg(test)]
mod r23_w9_g1_screen_once_tests;

/// JIT round 23 wave 9, lane L9: `CRATONVM_G1_CLEANUP_PARALLEL_SCRUB`. The
/// pool-run cleanup scrub leaves exactly the heap the inline per-region scrub
/// leaves: every dead object's references nulled, every live one's kept.
#[cfg(test)]
mod r23_w9_g1_cleanup_parallel_scrub_tests;

/// JIT round 23 wave 10, lane L9: `CRATONVM_G1_OLD_PLAB` -- promotion copies in
/// PLAB chunks of a shared Old region, and the relaxed per-worker pool cap.
#[cfg(test)]
mod r23_w10_g1_old_plab_tests;









/// JIT round 21, lane vhcae (VHCAE21-1): the compiled post barrier's edge
/// filter ([`G1JitEdgeFilter`]) is written exactly when the barrier recorded
/// the edge, carries the epoch it was recorded in, and is retired by a bump.
#[cfg(test)]
mod jr21_vhcae_jit_edge_filter_tests;





/// g1ep7: a mixed phase whose only candidates left are excluded from every
/// pause (pinned by JIT or peer-stack words) ends, instead of spending its
/// budget on pauses that take no old region while no new mark can start.
#[cfg(test)]
mod g1ep7_pinned_candidates_tests;

mod value_from_unaligned_ptr;
#[allow(unused_imports)]
pub use value_from_unaligned_ptr::*;
#[allow(unused_imports)]
use value_from_unaligned_ptr::*;
mod config;
#[allow(unused_imports)]
pub use config::*;
#[allow(unused_imports)]
use config::*;
mod needs_gc_recount_due;
#[allow(unused_imports)]
pub use needs_gc_recount_due::*;
#[allow(unused_imports)]
use needs_gc_recount_due::*;
mod collector;
#[allow(unused_imports)]
pub use collector::*;
#[allow(unused_imports)]
use collector::*;
mod set_resolved_compact_primitive_field;
#[allow(unused_imports)]
pub use set_resolved_compact_primitive_field::*;
#[allow(unused_imports)]
use set_resolved_compact_primitive_field::*;
mod futile_young_escalation_count;
#[allow(unused_imports)]
pub use futile_young_escalation_count::*;
#[allow(unused_imports)]
use futile_young_escalation_count::*;
mod count_unretired_forwards;
#[allow(unused_imports)]
pub use count_unretired_forwards::*;
#[allow(unused_imports)]
use count_unretired_forwards::*;
mod collect_outgoing_cross_region_edges;
#[allow(unused_imports)]
pub use collect_outgoing_cross_region_edges::*;
#[allow(unused_imports)]
use collect_outgoing_cross_region_edges::*;
mod mark_root_grid_place;
#[allow(unused_imports)]
pub use mark_root_grid_place::*;
#[allow(unused_imports)]
use mark_root_grid_place::*;
mod record_collection_with_phases_within;
#[allow(unused_imports)]
pub use record_collection_with_phases_within::*;
#[allow(unused_imports)]
use record_collection_with_phases_within::*;
mod bump_satb_pre_barrier_epoch;
#[allow(unused_imports)]
pub use bump_satb_pre_barrier_epoch::*;
#[allow(unused_imports)]
use bump_satb_pre_barrier_epoch::*;
mod evac_copy_fits_a_region;
#[allow(unused_imports)]
pub use evac_copy_fits_a_region::*;
#[allow(unused_imports)]
use evac_copy_fits_a_region::*;
mod old_plab_bytes_for;
#[allow(unused_imports)]
pub use old_plab_bytes_for::*;
#[allow(unused_imports)]
use old_plab_bytes_for::*;
