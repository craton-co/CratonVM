// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Generational garbage-collected heap.
//!
//! Combines a young generation (semi-space copying) with an old generation
//! (non-moving free-list mark-sweep) and a card table for efficient
//! old→young reference tracking.
//!
//! ## Allocation
//! All new objects are allocated in the young generation's from-space via
//! bump-pointer allocation. Objects are promoted to the old generation
//! after surviving [`PROMOTION_AGE`] minor GC cycles.
//!
//! ## Minor GC
//! Triggered when young from-space exceeds [`YOUNG_GC_THRESHOLD_PERCENT`]%.
//! Copies live young objects to young to-space (incrementing age), or
//! promotes them to old gen if their age reaches the threshold.
//! Dirty card table entries are scanned for old→young references as
//! additional roots.
//!
//! ## Major GC
//! Triggered when old gen runs out of space during promotion. Performs a
//! mark-sweep of the old generation, freeing unreachable objects back to
//! the free list.

use std::backtrace::Backtrace;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicI32, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use std::sync::Arc;

use cratonvm_gc_core::arena::Arena;
use cratonvm_gc_core::card_table::CardTable;
use cratonvm_gc_core::collector::{GarbageCollector, MonitorCleanup};
use crate::concurrent_mark::{
    old_gen_allocated_total, ConcurrentGcState, ConcurrentStartPolicy, OldGenPolicy,
};
use cratonvm_gc_core::gc::GcResult;
use cratonvm_gc_core::heap::{
    array_data_size, array_element_type_from_tag, object_kind_from_tag, read_prim_element,
    write_prim_element, ArrayElementType, ObjectHeader, ObjectKind, ARRAY_DATA_OFFSET,
    AUTOBOX_CLASS_ID, GC_FLAG_HEADER, GC_FLAG_MARKED, GC_FLAG_OLD_GEN, HEADER_SIZE,
    REF_ELEMENT_SIZE, SLOT_SIZE,
};
use crate::old_gen::{MajorTrigger, OldGen, PinnedCompactRefusal};
#[cfg(test)]
mod gci_i11jd_ihash_witness_tests;

#[cfg(test)]
mod gci_i12sx_per_heap_switch_tests;
// instance fields are stored as 8-byte pointers per the per-class oop-map.
use cratonvm_gc_core::gc_flags;
use cratonvm_gc_core::satb::SatbQueue;
use cratonvm_gc_core::{compact_ref_fields_enabled, is_compact_object, object_instance_size};
use cratonvm_types::narrow_oop::{read_ref_slot, ref_element_size, ref_field_size, write_ref_slot};
use cratonvm_types::GC_FLAG_COMPACT;
use cratonvm_types::{ClassId, CompactLayout, FieldStorageKind, ObjectRef, Value};

// gen r4w5/oomjit5 (2026-09-24): `CRATONVM_DBG=oldmark-root-census`. A
// read-only, per-root-category attribution of what every stop-the-world major
// keeps alive, run from `old_gen_gc`'s seeding site and printed from
// `after_old_gen_collection`. Lives in its own file; see its module doc.
mod oldmark_census;
pub use oldmark_census::{
    census_enabled as oldmark_census_enabled,
    install_vm_labeller as install_oldmark_census_vm_labeller,
    stage_holder_markers as stage_oldmark_census_holder_markers,
    stage_root_sections as stage_oldmark_census_root_sections,
    take_last_holder_summary as take_oldmark_census_last_holders,
    HolderMarker as OldmarkCensusHolderMarker,
    StagedHolderMarkers as OldmarkCensusStagedHolderMarkers,
    StagedRootSections as OldmarkCensusStagedSections, VmRootLabeller as OldmarkCensusVmLabeller,
};

// gen r4w6/young6 (2026-09-24): HotSpot-style adaptive tenuring — the age
// table, the threshold rule and the per-heap state. Opt-in; see its module doc.
mod tenuring;
pub use tenuring::{TenuringConfig, DEFAULT_MAX_TENURING_THRESHOLD, DEFAULT_TARGET_SURVIVOR_RATIO};
// gcd d2/g: precise-root promotion (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, default ON since 2026-10-13, `=0` kills),
// published by the VM's root scan and consumed by `sweep_young_non_moving`.
pub use tenuring::{
    clear_precise_root_values, precise_root_promote_census_line, precise_root_promote_enabled,
    publish_precise_root_values, take_precise_root_values, PRECISE_ROOT_PROMOTE_FLAG,
};
// gcd d5/r: the scan seals the table with its final length; the sweep's
// sealed take refuses a table that does not describe its root list.
pub use tenuring::{seal_precise_root_values, take_sealed_precise_root_values};
// gci i13/cm item 6: the concurrent pauses' "no young sweep follows" scope.
pub use tenuring::{
    no_young_sweep_follows, with_no_young_sweep_following, CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG,
};
// gcj j3/rf3: the same scope, unswitched (the String table's in-cycle rule).
pub use tenuring::in_no_young_sweep_scope;
// gci i6/pn (conc10): which exemption moved a young root value, for the VM's
// root-remap audit (`CRATONVM_DBG_ROOT_REMAP_AUDIT`).
pub use tenuring::{
    take_young_root_exemptions, young_root_exemption_label, YoungRootExemptions,
};

// gen r5w5/old9 (2026-09-27) .. gcg g5/og (2026-10-02): the file that held the
// O(live) stop-the-world old-gen collection (`CRATONVM_GC_OLD_LIVE_SWEEP`,
// deleted by gcg g5/og: it lost to the walked collection on every measured
// shape). Lane d's remaining `gcd_d1d` tests now have their own test filename.
#[cfg(test)]
mod gcd_d1d_tests;

// gcf f2/c (2026-10-01): unit tests of the requested-major compaction
// (`CRATONVM_GC_REQUESTED_MAJOR_COMPACTS`) and the mark admission (its
// switch deleted by gcg g4/og; the bitmap form is the shared start set's).
// A file of its own so the shared
// working tree's lanes do not edit the same test module.
#[cfg(test)]
mod gcf_f2c_tests;

// gcg g1/c (2026-10-01): unit tests of lane c's wave-1 readings of the young
// trigger (the Serial-eden switch's sweep trigger). Its own file, like the
// f2/c tests above.
#[cfg(test)]
mod gcg_g1c_tests;

// gcg g3/e (2026-10-01): unit tests of lane e's wave-3 old-generation work
// (the shared start set, the live sweep under the true-root seed, the
// unrewritable-only plan's per-thread veto). A file of its own, as above.
#[cfg(test)]
mod gcg_g3e_tests;

// gch h4/pc (2026-10-09): unit tests of the true-root young rounds' cost cut
// (`CRATONVM_GC_TRUE_ROOT_ROUNDS_IN_BFS`, `CRATONVM_GC_TRUE_ROOT_SKIP_IDLE_POST_CLOSE`).
#[cfg(test)]
mod gch_h4pc_tests;

// gci i28/cl (2026-10-15): the incremental post-resurrection closure's
// heap-level twin (`CRATONVM_GC_OLD_POST_CLOSE_INCREMENTAL`).
#[cfg(test)]
mod gci_i28cl_tests;

// gcj j2/rf2 (2026-10-19): ec47, the major's deferred finalizer seeds
// (`CRATONVM_GEN_MAJOR_DEFERS_FINALIZER_SEEDS`).
#[cfg(test)]
mod gcj_j2rf2_tests;

// gcj j3/rf3 (2026-10-19): ec47, the legacy young seed's half
// (`CRATONVM_GEN_LEGACY_SEED_DEFERS_RESURRECTED`).
#[cfg(test)]
mod gcj_j3rf3_tests;

// gci i10/gr (2026-10-13): sizer10 (a) -- the unrecorded-young-root guard over
// the dirty-card and overlay values (`CRATONVM_GEN_UNRECORDED_ROOT_GUARD_CARDS`,
// opt-in), called from `collect_garbage_inner_with_pins` right after the card
// take. Its own file so the shared working tree's lanes do not collide.
mod gci_i10gr;

// gen r5w6/conc10 (2026-09-27): the concurrent cycle's young→old seeds from
// the LIVE young set (`CRATONVM_GEN_Y2O_LIVE_SEED`, default ON since the gce
// close; gch h3/mj: plus g4og's step-0 census, opt-in), and the hidden
// referent slot of young `Reference`s. Called by the VM's concurrent driver in
// place of `collect_young_to_old_roots`. Lives in its own file; see its
// module doc.
mod y2o_live;
pub use y2o_live::{
    y2o_live_seed_enabled, y2o_old_holder_census_enabled, Y2oFallback, Y2oOldHolderCensus,
    YoungToOldSeedRequest, YoungToOldSeeds, Y2O_LIVE_SEED_FLAG, Y2O_OLD_HOLDER_CENSUS_FLAG,
};

/// gcd d5/s: slots of [`GenerationalHeap::pinned_takeover_declines`].
///
/// `REACHED` counts every young cycle that reached the take-over arm's test
/// (both flags on, term 3 fired, not the forced test path); each other slot
/// counts the declines whose FIRST failing condition it names, in
/// [`pinned_takeover_decline_reason`]'s order. So `REACHED` minus the sum of
/// the rest is the arm's `ptko_candidates`.
mod takeover_decline {
    /// Cycles that reached the arm's test.
    pub(super) const REACHED: usize = 0;
    /// Term 3 fired with no take-over population at all: nothing frozen, no
    /// helper window, nobody unread (another coverage obligation fired it).
    pub(super) const NO_TAKEOVER_PEER: usize = 1;
    /// A peer frozen in compiled code (a different contract; left non-moving).
    pub(super) const FROZEN: usize = 2;
    /// A peer nobody read (still running, or a blocked peer no slot reached).
    pub(super) const UNREAD: usize = 3;
    /// A helper window the pass could not pin whole (its band or shadow
    /// window was incomplete), or a take-over stack read short.
    pub(super) const WINDOW_REFUSED: usize = 4;
    /// Derived pointers not resolved (`CRATONVM_XT_HELPER_WINDOW_PIN_RESOLVE=0`).
    pub(super) const DERIVED_UNRESOLVED: usize = 5;
    /// The take-over licence was fine but another coverage obligation is open
    /// (`moving_young_coverage_incomplete` or an unregistered frame): the
    /// decision histogram's sub-reason names it.
    pub(super) const OTHER_COVERAGE: usize = 6;
    /// Another divert term (promotion-OOM risk, explicit full GC, GPU veto;
    /// gci i11/tk: not `CRATONVM_OLDGEN_COMPACT`, see
    /// `takeover_other_divert_terms`), forced non-moving roots, or moving
    /// young not requested.
    pub(super) const OTHER_DIVERT: usize = 7;
    /// Slots in the census.
    pub(super) const LEN: usize = 8;
}

#[cfg(test)]
mod gci_i30wy_pinned_watched_seed_tests;
#[cfg(test)]
mod gci_i31fz_pinned_finalizable_seed_tests;
#[cfg(test)]
mod t1w8_ygzero_claim_tests;
/// gcf f2/y: the mark word a header-marked young survivor of the non-moving
/// sweep is left with -- `GC_FLAG_MARKED` cleared and the age one higher,
/// saturating at `MAX_GC_AGE` -- and that new age. Exactly the word
/// `clear_gc_flags(GC_FLAG_MARKED)` followed by
/// `set_gc_age(gc_age() + 1)` leaves (unit test
/// `f2y_the_folded_survivor_word_is_the_two_rmws_word`), as one pure function
/// so the sweep writes it with one load and one plain store.
/// gci i1/yg: the sweep calls [`sweep_survivor_mark_by`] with the cycle's age
/// step; this step-1 form is what the bit-identity test pins.
#[cfg(test)]
#[inline]
fn sweep_survivor_mark(mark: u32) -> (u32, u8) {
    sweep_survivor_mark_by(mark, 1)
}

#[cfg(test)]
mod gcj_j1yg_term4_offer_tests;

#[cfg(test)]
mod gcg_g6yg_tests;

// gcg g6/yg: `CRATONVM_GEN_SWEEP_TRIGGER_OWN_GOAL` (gcd d5/q, the sweep's own
// pause loop, `sweep_own_trigger_bytes`) is DELETED: it failed alone (vg1
// `y1_d4m`: minor -19 %, run time up, majors +6) and in the pair with
// SERIAL_EDEN (vg3 `c1b_tc_*`: +17 % on the premium probe).

#[cfg(test)]
mod gcd_d5q_sweep_trigger_tests;
// gch h2/ob: `serial_eden_sweep_moving_threshold` (gcg h1/ob, the in-place
// sweep's trigger capped at the percent knob under the Serial-shaped trigger)
// is deleted. Its premise -- "the finalize divergence is a moving-cycle
// probe" -- is refuted by vh1-ob: `OldGenFinalizeProbe` cycles were 11/15,
// 25/26 and 13/16 sweeps (`decision histogram: non_moving=`), and the run that
// was nearly all sweeps (`ob1_se_ofin_se_2`) fired them at the default arm's
// 50 % and finalized 0/64, like the default. The SWEEP's cadence is what ages
// the medium-lived objects into the old generation, so the cap removed the
// switch's parity with its cost. The cost it removed is the forced-sweep
// regime (`gcg-g1c-jit-warm-young-forces-a-sweep-per-large-array`), whose
// cause the pinned young copy addresses (gch h1/yy's refill cap was deleted
// by gch h3/yc after vh2-ob `ob2_se_*` measured the pair). The pinning test is
// `gen_heap_gcg_g1c_tests::gcg_g1c_serial_eden_moves_the_sweep_trigger_to_ninety_percent`.

#[cfg(test)]
mod gcf_f7c_serial_eden_trigger_tests;

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the consumer half): `-XX:SurvivorRatio` sizes the Serial eden, the survivor
/// reserve and the tenuring target. `cargo test -j 5 -p cratonvm-gc --lib gci_i45_sc_`.
#[cfg(test)]
mod gci_i45_sc_survivor_ratio_tests;

/// gci i1/yg: the HotSpot age clock (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK`,
/// `gcf-f5c-...`). `cargo test -j 5 -p cratonvm-gc --lib gci_i1yg_`.
#[cfg(test)]
mod gci_i1yg_age_clock_tests;

/// gci i5/yg: the engagement census of the in-band trial judgement
/// (`adapt_in_band_trials=` on `[GC] young_trigger:`), so a row can tell an
/// unexercised `CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND` from one that
/// changed nothing (vi2-yg T was VOID on engagement).
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i5yg_`.
#[cfg(test)]
mod gci_i5yg_tests;

/// gci i2/yg: the pause-goal loop's in-band trial judgement
/// (`CRATONVM_GC_YOUNG_PAUSE_TRIAL_JUDGED_IN_BAND`).
/// `cargo test -j 5 -p cratonvm-gc --lib gci_i2yg_`.
#[cfg(test)]
mod gci_i2yg_tests;

/// gci i22/dr (P2/yy): `CRATONVM_GEN_TLAB_REFILL_SMALLEST_WHOLE_BLOCK`, the
/// best-fit-by-size young TLAB refill. `cargo test -j 5 -p cratonvm-gc --lib gci_i22dr_`.






#[cfg(test)]
mod gc21c_age_by_serial_eden_tests;



#[cfg(test)]
mod gcf_f3o_refill_gate_tests;

/// gcd d9/b: the slots of `GenerationalHeap::oome_ladder_census`, the
/// allocation-failure ladder's census (`[GC] oome_ladder:`). The VM feeds them
/// (`vm/src/runtime/interpreter/gc_and_alloc.rs`); the heap only keeps and
/// prints them, per heap (one VM), never process-wide.
///
/// Majors are a subset of forced collections (a ladder major IS an
/// allocation-failure-door collection with a major requested), and the
/// `*_US` slots are wall time around the door, pause included.
pub mod oome_ladder {
    /// Heap `OutOfMemoryError`s raised (`Java heap space`, every door).
    pub const OOMES: usize = 0;
    /// Allocation-failure-door collections this VM ran (the thread won the
    /// pause): every caller of the forced door but the compiled refill
    /// trigger, whose door is an occupancy verdict (see `THRESHOLD_WEDGED`).
    pub const FORCED: usize = 1;
    /// ...their wall time, microseconds.
    pub const FORCED_US: usize = 2;
    /// Forced-door collections the GC-overhead verdict judged unproductive
    /// (every forced-door collection is judged, the compiled refill
    /// trigger's included).
    pub const FORCED_UNPRODUCTIVE: usize = 3;
    /// Occupancy-triggered collections (`maybe_gc`, and the compiled refill
    /// trigger's `tlab-alloc-shaped` forced door) that started with the old
    /// generation wedged: the crawl of `gcd-d4j-threads-oom-probe-crawls`.
    pub const THRESHOLD_WEDGED: usize = 4;
    /// ...their wall time, microseconds.
    pub const THRESHOLD_WEDGED_US: usize = 5;
    /// Ladder majors that ran on the requesting thread (every rung:
    /// overhead-limit, last-ditch, second major, native debt, exceptions,
    /// JIT helpers).
    pub const MAJORS: usize = 6;
    /// ...their wall time, microseconds.
    pub const MAJORS_US: usize = 7;
    /// Ladder major requests whose every attempt lost the pause race.
    pub const MAJORS_LOST: usize = 8;
    /// `majors_to_decide_oome` calls whose first major ran.
    pub const DECIDING: usize = 9;
    /// ...that ran a second major (the first freed under 2 % of the heap).
    pub const SECOND_MAJORS: usize = 10;
    /// Native-funnel OOME debt payments.
    pub const DEBT_PAYMENTS: usize = 11;
    /// Futile-young verdicts (`note_forced_young_cycle_outcome`).
    pub const FUTILE_VERDICTS: usize = 12;
    /// Forced young cycles the futile-young backoff skipped.
    pub const FUTILE_SKIPS: usize = 13;
    /// gce e2/o: latched-overhead exits that took their one graced attempt
    /// after the deciding major instead of throwing at once
    /// (`gc_and_alloc::latched_overhead_grace`).
    pub const LATCHED_GRACE: usize = 14;
    /// ...whose attempt allocated (each is an `OutOfMemoryError` the grace
    /// saved: HotSpot Serial would have allocated there too).
    pub const LATCHED_GRACE_OK: usize = 15;
    /// Number of slots.
    pub const LEN: usize = 16;
}

#[cfg(test)]
mod gcd_d9b_wedged_trigger_tests;
#[cfg(test)]
mod gci_wr_heap_stats_tests;
/// JIT round 23 wave 3, lane L4 (`docs/feature-designs/concurrent-gc-maturation.md`):
/// what the compiled G1 post-write barrier needs to ask "is the HOLDER in a
/// young region?" -- HotSpot's `g1_young_card_val` filter -- read at run time
/// by the inline sequence (`cratonvm_jit` `g1_young_holder_skip_bytes`):
///
/// * `[0]` the address of G1's `Vec<G1Region>` buffer (created once in
///   `G1Collector::new` and never resized, so address-stable for the
///   collector's life);
/// * `[1]` the arena base (the region index is `(holder - base) >> shift`;
///   the same base as [`JIT_G1_BARRIER`] `[0]`, kept here so the owner check
///   and the reader need only this table);
/// * `[2]` the OWNER claim: `0` free, the owning collector's arena base, or
///   [`JIT_G1_YOUNG_CONTENDED`] once two G1 collectors have coexisted;
/// * `[3]` the region shift -- the LIVENESS word, stored last and cleared
///   first: `0` means "do not filter", and the emitted sequence compares it
///   against the shift it was compiled with;
/// * `[4]` the region count (the index bound).
///
/// Published only when G1's young-source remembered-set prerequisite and
/// filter are on (`g1::g1_young_source_filter_enabled`): the prerequisite is
/// what makes skipping a young holder's store sound (every young region left
/// out of a collection set is re-walked for its outgoing edges at the pause,
/// and cleanup scrubs young regions unscreened). The region-type byte is read
/// at `regions + index * G1_REGION_STRIDE + G1_REGION_TYPE_OFFSET` (`g1.rs`).
///
/// # One owner per process (r23 wave 3 review)
///
/// The compiled sequence does not read `[1]`: it indexes `[0]` with an offset
/// taken against [`JIT_G1_BARRIER`]'s base, a DIFFERENT table that the newest
/// G1 collector overwrites. Two live G1 collectors (embedding, the vm test
/// binaries) could therefore pair one collector's region array with the other
/// one's base for the instant between the two publishes -- and a wrong index
/// can read "young" for an old holder, which loses a remembered-set edge. So
/// the table is claimed (`[2]`, compare-and-swap from `0`), a second claimant
/// POISONS it (shift `0`, claim [`JIT_G1_YOUNG_CONTENDED`], sticky for the
/// process) instead of overwriting it, and `G1Collector::new` publishes this
/// table BEFORE the barrier table so the poison is visible first. Poisoned,
/// every compiled store falls through to `jit_g1_post_write_barrier`, whose
/// own young test is per collector; nothing is lost but the inline skip.
///
/// # Where the table lives (r23 wave 3 build fix)
///
/// The words are defined in `cratonvm_types::jit_g1_young`, below both this
/// crate and the JIT (which reaches `cratonvm-gc` only as a dev-dependency and
/// reads the table at compile time through
/// `cratonvm_types::jit_g1_young::jit_g1_young_geometry`). Words `[5]`-`[7]`
/// carry the region array stride, the type byte's offset and the largest
/// young type byte -- [`crate::g1::G1_REGION_STRIDE`],
/// [`crate::g1::G1_REGION_TYPE_OFFSET`], [`crate::g1::G1_REGION_YOUNG_MAX`] --
/// published with the rest, so no copy of `G1Region`'s layout exists outside
/// this crate. This module keeps the policy: claim, fill, poison, teardown.
pub use cratonvm_types::jit_g1_young::{
    jit_g1_young_addr, jit_g1_young_shift, JitG1YoungTable, JIT_G1_YOUNG, JIT_G1_YOUNG_CONTENDED,
};

#[cfg(test)]
mod r4w6_young_commit_screen_tests;
#[cfg(test)]
mod fragmented_pinned_compaction_policy_tests;
/// gci i8/pa (P-A of `gengc-core-system-gc-forces-non-moving-20260920`,
/// `CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES`, opt-in) — the YOUNG values of
/// the side-table registries that a licensed moving `System.gc()` must copy:
/// every `mirror_pin` value, every `metadata_pin` value and every pinned
/// loader (`loader_pin`; a non-strong hidden class's row names its mirror)
/// that `is_young` places in this heap's young from-space. Non-null, 8-aligned,
/// ascending, deduplicated.
///
/// Why all of them, owner or not: the licensed root scan left these values out
/// of the root set because their owner keeps them, but the Cheney closure
/// follows no `mirror_pin` / `metadata_pin` edge (and `loader_pin` only from a
/// copied instance, never from an old one it does not scan). Forwarding them
/// keeps every one for the young phase, which is what the unlicensed scan did
/// by rooting them; they are not roots of the same cycle's major, which
/// follows them from their owners (re-keyed by post-copy owner under P-C). A
/// row of another VM never names this heap's from-space, so `is_young`
/// filters it out.
///
/// gci i9/yl: no longer the cycle's seed set (it kept a dead young loader
/// alive through its young mirror, residual 3c of `i8-pa-report.md`); the
/// cycle follows [`side_table_young_rows_of`] instead. Kept for the tests as
/// the superset every followed value belongs to.
#[cfg(test)]
fn young_side_table_values(
    mirror_rows: Option<&FxHashMap<usize, Vec<usize>>>,
    metadata_rows: Option<&FxHashMap<usize, Vec<usize>>>,
    loaders: &[usize],
    is_young: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let mut out: Vec<usize> = mirror_rows
        .into_iter()
        .flat_map(|m| m.values())
        .chain(metadata_rows.into_iter().flat_map(|m| m.values()))
        .flat_map(|v| v.iter().copied())
        .chain(loaders.iter().copied())
        .filter(|&v| v != 0 && v % 8 == 0 && is_young(v))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The hand-rolled mark-clear walk as it stood before gen r4w6/young6 moved
/// [`clear_all_mark_bits_in_arena`] onto [`YoungGridCursor`]. Test-only: the
/// drift tests run both and require the same objects to come out unmarked.
#[cfg(test)]
fn clear_all_mark_bits_in_arena_legacy(arena: &mut Arena, side_sorted: &[usize]) {
    let base = arena.base_ptr() as usize;
    let used = arena.used();
    let free_blocks = arena.free_blocks_sorted();
    let mut free_iter = free_blocks.iter().peekable();
    let mut cursor: usize = 0;
    while cursor < used {
        // Skip known free blocks — their bytes are stale and must not be
        // parsed as object headers. DoHead walk-desync hardening
        // (2026-07-02): robust skip — the old exact-match wedged after one
        // overshoot and then CLEARED "mark bits" (a byte write at header
        // offset 21) through phantom headers inside live objects.
        if skip_free_blocks(&mut cursor, &mut free_iter).0 {
            continue;
        }
        // `cursor` is within `used`; the arena's `[base, base+used)` region
        // is backed by mapped, allocated memory. The integer-to-pointer cast
        // itself is safe; only the header deref on the next line requires
        // `unsafe`.
        let obj_ptr = (base + cursor) as *mut u8;
        // SAFETY: `obj_ptr` is 8-byte-aligned (bump arena) and points at the
        // start of an object header within the live region.
        let header = unsafe { &mut *(obj_ptr as *mut ObjectHeader) };
        // Bug-D fix (2026-06-12): stride over a GAP-filler sentinel (a
        // sub-`HEADER_SIZE` TLAB tail) the same way the sweep does. Normally
        // these are already on the free list (skipped above), but if the
        // sweep broke out early one may remain in place; recognise it here so
        // this mark-clearing walk does not desync on it. No mark bit to clear
        // (it is dead filler).
        if header.class_id.as_u32() == cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32() {
            // SAFETY: offset 4 lies within the >=8-byte gap.
            let gap =
                unsafe { std::ptr::read((obj_ptr as *const u8).add(4) as *const u32) } as usize;
            if (8..HEADER_SIZE).contains(&gap) && gap & 7 == 0 && cursor + gap <= used {
                cursor += gap;
                continue;
            }
        }
        // DoHead walk-desync hardening (2026-07-02): an unlisted zeroed span
        // is not parseable — re-anchor at the next free block (carries no
        // mark bits to clear) instead of striding it as phantom objects and
        // writing the mark-clear byte into live-object interiors.
        //
        // …but a run of EMPTY objects is not that (`zero_run_empty_object_resume`),
        // and re-anchoring on one is not free HERE in a way it is nowhere else:
        // the marks in the skipped stretch are LEFT SET, and the next
        // non-moving sweep treats a set `GC_FLAG_MARKED` as live regardless of
        // reachability (the bug-C5 note at the call site). So the false
        // positive feeds itself — measured at ~800 re-anchors per young cycle
        // on the hibernate repro. Step over the run instead: it can
        // hold no mark to clear, because a header carrying `GC_FLAG_MARKED` is
        // by definition not all-zero, and the predicate refuses any run with a
        // marked base inside it.
        // SAFETY: reads the first header word at `obj_ptr` (`cursor < used`),
        // already dereferenced in-bounds as a header above.
        let word0 = unsafe { *(obj_ptr as *const u64) };
        let mut anomaly = false;
        if word0 == 0 {
            let limit = free_iter
                .peek()
                .map(|&&(off, _)| off)
                .unwrap_or(used)
                .min(used);
            let run_end = zero_run_end(base, cursor, limit);
            // A marked base AT the run start is a live `ClassId(0)` container
            // that DOES carry a mark bit — parse it normally below.
            let vouched_live = side_sorted.binary_search(&(base + cursor)).is_ok();
            if run_end - cursor >= HEADER_SIZE && !vouched_live {
                if let Some(resume) =
                    zero_run_empty_object_resume(base, cursor, run_end, used, side_sorted, None)
                {
                    SWEEP_ZERO_SPAN_EMPTY_RUNS.fetch_add(1, Ordering::Relaxed);
                    EMPTY_RUN_BYTES_CYCLE.fetch_add((resume - cursor) as u64, Ordering::Relaxed);
                    cursor = resume;
                    continue;
                }
                anomaly = true;
            }
        }
        let total_size = if anomaly {
            0
        } else {
            gen_object_total_size(header)        };
        // gen r4w4/young4 (2026-09-24): the hole-crossing rule, the one grid
        // rule `YoungGridCursor` (`GridAnomaly::CrossesFreeBlock`) had and
        // this walk did not. A parsed header whose extent STRADDLES the next
        // free block cannot be an object — a free block is carved from dead
        // spans, never from inside a live one — and striding it lands past the
        // hole, mid-object: every header after that is parsed off the grid,
        // their marks are left set (bug C5's over-retention) and the
        // mark-clear write lands in object interiors. Treat it as the anomaly
        // it is: re-anchor at the hole, clear nothing here. Unreachable on a
        // healthy arena; pinned by
        // `tests::gen_r4w4_young4::the_mark_clear_walk_reanchors_at_a_straddled_free_block`.
        let straddles_free_block = !anomaly
            && free_iter.peek().is_some_and(|&&(foff, _)| {
                foff > cursor && foff < cursor.saturating_add(total_size)
            });
        if anomaly || straddles_free_block || total_size < HEADER_SIZE || cursor + total_size > used
        {
            // Corruption — same defence as the sweep loop: re-anchor at the
            // next free block rather than risk parsing arbitrary bytes as a
            // header (marks in the skipped stretch remain — retention only).
            if resync_to_next_free_block(&mut cursor, &mut free_iter) {
                continue;
            }
            break;
        }
        header.clear_gc_flags(GC_FLAG_MARKED);
        cursor += total_size;
    }
}

/// gen r4/alloc (2026-09-23): the allocation / trigger-policy lane's tests.
/// A module of its own, at the end of the file, so seven lanes appending to
/// `mod tests` concurrently do not all conflict on its closing brace.
#[cfg(test)]
mod gengc_r4_alloc_tests;

/// gen r4w2/alloc2 (2026-09-23): wave 2 of the allocation lane — young sizing
/// (`-Xmn` / `NewRatio`), the pause-goal loop's old-gen skip, large-object
/// zeroing outside the young lock and the opt-in pristine-chunk skip. Its own
/// module at the end of the file for the same reason `gengc_r4_alloc_tests` is.


/// gen r4w3/alloc3 (2026-09-23). A separate module so parallel lanes appending
/// tests do not collide.
#[cfg(test)]
mod gengc_r4w3_alloc3_tests;

/// gen r4w3/cards3 (2026-09-23): the remembered set's summary map, the old
/// generation's block-offset walk and element-precise reference-array cards,
/// checked against the whole-old-generation scan. A module of its own, at the
/// end of the file, so the wave's lanes do not all append to `mod tests`.
#[cfg(test)]
mod gen_r4w3_cards3_tests;



/// gen r4w6/tlab6 (2026-09-24): the Generational TLAB tail sink
/// (`CRATONVM_GEN_TLAB_TAIL_SINK`) and the carve-aware `CRATONVM_DBG_GC_STRESS`
/// bound. A module of its own, away from the end of the file, so the wave's
/// parallel lanes do not all append at the same closing brace.
#[cfg(test)]
mod gen_r4w6_tlab6_tests;

/// gen r5w5/sizer9 (2026-09-27): the share sizer's KEEP sequence against the
/// real young generation with the tail sink on, the cursor-retraction
/// invariant beside a live buffer, and the `-Xms` floor of the evacuated
/// semi-space's uncommit. A module of its own beside the tail sink's, so
/// parallel lanes do not meet at a closing brace.
///
/// The first two are the invariants
/// `docs/feature-designs/generative-gc-architecture.md`
/// asks to be pinned: no byte is handed to two owners, and a retraction never
/// lowers the cursor under a buffer that is still bumping. They hold on this
/// base (the page's mechanism was not found by reading — see its STATUS); a
/// change that breaks either fails here without a JVM.
#[cfg(test)]
mod gen_r5w5_sizer9_tests;

/// gen r4w4/cards4 (2026-09-24): the per-heap reference-store plan, the JIT
/// card view, the element-card slow path the inline barrier calls, and the
/// element-aware `CRATONVM_DBG_RSET_AUDIT` card test. A module of its own at
/// the end of the file, so parallel lanes do not conflict on a closing brace.
#[cfg(test)]
mod gen_r4w4_cards4_tests;

/// gen r4w5/review5 (2026-09-24): young-memory readers whose "inside the
/// semispace, so mapped" argument stopped holding once the young arenas
/// commit lazily and `uncommit_evacuated_young` (default ON) hands the
/// evacuated semi-space back to the OS. A module of its own at the end of the
/// file, like the earlier waves'.
#[cfg(test)]
mod gen_r4w5_review5_tests;

/// gci i10/gy (2026-10-13): `gengc-r4w5-pinned5-in-place-copy-limits` L3 ("a
/// pinned cycle never grows young") is REFUTED by reading for the VM: no
/// heap the VM builds has young room to grow, so no cycle grows young.
#[cfg(test)]
mod gci_i10gy_tests;

/// gen r5w1/oldgen5 (2026-09-26): the old-generation lane of gen round 5,
/// wave 1. A module of its own, at the end of the file, so the wave's lanes
/// do not all append to `mod tests`.
#[cfg(test)]
mod gen_r5w1_oldgen5_tests;

/// gen r5w2/alloc6 (2026-09-26): the allocation / old-generation lane of gen
/// round 5, wave 2 — the humongous refusal's arming, and the pinned
/// compaction's pin settlement and attribution. A module of its own at the
/// end of the file, like the earlier waves'.
#[cfg(test)]
mod gen_r5w2_alloc6_tests;

/// gen r5w2/roots6 (2026-09-26): the young-collection lane of gen round 5,
/// wave 2. A module of its own, at the end of the file, like wave 1's.
#[cfg(test)]
mod gen_r5w2_roots6_tests;

/// gcd d2/h (2026-09-27): the unrecorded-precise-root refusal
/// (`gengc-r5w6-sizer10-refused-young-root-dangles`) and the promoted holders'
/// element-precise card deferral (`gengc-r4w3-cards3-precise-array-cards-residuals`
/// item 3, producer (c)).
#[cfg(test)]
mod gcd_d2h_tests;

/// gcd d3/m: the evacuator's refusal ledger is per heap, and the pinned copy's
/// take-over arm takes only a take-over-alone divert.
#[cfg(test)]
mod gcd_d3m_tests;

/// gci i11/tk (2026-10-13): the pinned copy's take-over arm under the
/// compaction default (`gci-i8rv-default-flip-review-findings-RETIRED-RULING-20261018` item
/// 7). Pure: the arm's flag is a process `OnceLock`, so a unit test cannot
/// drive a take-over cycle; these pin the two decisions the fix couples.
#[cfg(test)]
mod gci_i11tk_tests;

/// gcd d5/s (2026-09-28): the cards3 item 3 producer (g) and its opt-in
/// consumer, and the take-over arm's decline census. A module of its own at
/// the end of the file, like the earlier waves'.
#[cfg(test)]
mod gcd_d5s_tests;

/// gce e1/y (GC design-and-performance round, young lane): the evacuated
/// semi-space's keep-for-survivors give-back, and the sweep's known-zero
/// publication. A module of its own at the end of the file, so parallel lanes
/// do not conflict on a closing brace.
#[cfg(test)]
mod gce_e1y_tests;

/// gcj j2/yg2 (`CRATONVM_GEN_YOUNG_KNOWN_ZERO_CARRY`, default ON): the sweep
/// keeps claiming the previous known-zero set's bytes that are still free,
/// and a tail sink return does not forget the set.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j2yg2_`.


/// gcj j2/yg2 (`CRATONVM_GEN_SERIAL_EDEN_NO_PAUSE_GOAL`, default ON since gcj
/// j1/yg): a heap the Serial-shaped trigger armed runs no young pause goal
/// unless one is set.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j2yg2_`.
#[cfg(test)]
mod gcj_j2yg2_serial_no_pause_goal_tests;

/// gcj j3/yg3 (`CRATONVM_GEN_TYPED_MIN_TLAB_FLOOR`, default ON): the heap's
/// smallest-useful-TLAB floor follows the VM's typed `-XX:MinTLABSize`.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j3yg3_`.


/// gcj j3/yg3 (`CRATONVM_GEN_CHENEY_OVERFLOW_PROMOTES`, default ON): the serial
/// Cheney copy's to-space overflow destination is old gen through the
/// promotion buffer, counted; `=0` (or a full old gen) is the abort's `None`.
/// `cargo test -j 5 -p cratonvm-gc --lib gcj_j3yg3_`.


/// gcj yg3-residuals R3 (`CRATONVM_GEN_SERIAL_YOUNG_GEOMETRY`): the Serial
/// geometry's plan, trigger and survivor cap, pure.
#[cfg(test)]
mod gcj_yg3r_serial_young_geometry_tests;

/// gcj j3/yg3 (`CRATONVM_GEN_YOUNG_MIRROR_DEFER_MOVING_FOLLOWS`, default ON): the
/// moving young phase builds its side-table rows when the root scan asked it
/// to, not only on a licensed moving `System.gc()`, while the Phase 5 P-C
/// re-key stays the `System.gc()` licence's. Needles split with `concat!`.
#[cfg(test)]
mod gcj_j3yg3_moving_follows_rows_tests;

/// gce e2/y: the adaptive-tenuring gate's counters on the `[GC] gen-alloc:`
/// line. A module of its own at the end of the file.
#[cfg(test)]
mod gce_e2y_tests;

/// gcf f1/y (2026-10-01): the young lane's tests for this wave.
#[cfg(test)]
mod gcf_f1y_tests;

/// gcf f2/p (2026-10-01): `set_field`'s compact arm resolves the layout once,
/// and `set_resolved_compact_ref_field` (the `jit_putfield_object` door) stores
/// exactly what `set_field` stores, falling back to it for every receiver its
/// resolved arm does not cover.
#[cfg(test)]
mod gcf_f2p_set_field_tests;
#[cfg(test)]
mod gcf_f2y_tests;

/// gcf f3/q (2026-10-01): the young-allocation-premium cuts of the non-moving
/// sweep -- the per-walk size memo, the batched selective-promotion census,
/// fix-up 3a's replay of the mark set (3c's listed-slot rewrite and the
/// pre-reserved `evac_map` were deleted by gcg g4/yg) -- and the stale-mark
/// aging fix (f2/y finding 4).
#[cfg(test)]
mod gcf_f3q_tests;

#[cfg(test)]
mod gcf_f4y_tests;



#[cfg(test)]
mod gci_i2fr_presence_reader_tests;

/// gci i12/wb (`gci-i12wb-jit-barrier-review-findings-FIXED-20261015` item 2): the
/// process SATB pre-gate byte can no longer be cleared by a stale disarm while
/// a marker holds a count. The byte is process-global and other tests arm and
/// disarm it concurrently, so every assertion is one that holds whatever they
/// do: while THIS test holds a count, the byte reads armed.
#[cfg(test)]
mod gci_i12_wb_pre_gate_tests;

/// gci i13/vg (`CRATONVM_GC_VT_PARKED_GATHER_FILTER`): the Generational half
/// of the split root gather -- the young set it offers and the old-only
/// section it adds back to a same-pause major. Filter:
/// `cargo test -p cratonvm-gc gci_i13vg`.
#[cfg(test)]
mod gci_i13vg_tests;

/// gci i17/fz (i16/fr's residual): the stop-the-world door takes its probe
/// referents back out of the concurrent remark's finalizer candidates.
#[cfg(test)]
mod gci_i17fz_retract_finalizer_candidates_tests;

/// gci i47w/sg47 (`CRATONVM_GC_SYSTEM_GC_TENURES_ALL`, default ON since the 2026-10-17 flip): a
/// `System.gc()`-door collection tenures every movable live young object on
/// its FIRST cycle, on the default in-place sweep and on the moving variant,
/// as HotSpot Serial's mark-compact leaves young empty. Page
/// `gci-i47w-j46-system-gc-leaves-live-young-data-out-of-the-old-pool-FIXED-20261016`.
#[cfg(test)]
mod gci_i47w_sg47_tests;

/// r23 wave 3 review: the one-owner / poison protocol of [`JIT_G1_BARRIER`]
/// (`publish_jit_g1_barrier_in`, `clear_jit_g1_barrier_owned_by_in`), driven
/// on a private table and owner word so the process statics (claimed by every
/// collector this binary builds) do not interfere. The twin with two real
/// collectors is `g1::tests::r23_w3_two_live_g1_collectors_poison_the_jit_tables`.
#[cfg(test)]
mod r23_w3_g1_barrier_owner_tests;

/// JIT round 23 wave 4, lane L4: `poison_dead_object_headers`, the in-place
/// cycle's replacement for zeroing every dead byte.
#[cfg(test)]
mod r23_w4_in_place_lazy_zero_tests;

/// JIT round 23 wave 4, lane L4: the lazy young object-start grid
/// (`CRATONVM_GEN_LAZY_YOUNG_OBJECT_STARTS`) -- its bounds and its poisoning.
#[cfg(test)]
mod r23_w4_lazy_young_starts_tests;

/// JIT round 23 wave 6, lane L4: the fused promotion/sweep walk
/// (`CRATONVM_GEN_SWEEP_FUSED_WALK`) decides exactly what `sweep_chunk` does.
#[cfg(test)]
mod r23_w6_fused_sweep_walk_tests;

/// JIT round 23 wave 8, lane W8-L3: the dead-chunk skip of the fused walk
/// (`CRATONVM_GEN_SWEEP_DEAD_CHUNK_SKIP`, proposal 14).
#[cfg(test)]
mod r23_w8_dead_chunk_skip_tests;

/// t1 wave 8, lane PIN10: `CRATONVM_GEN_PINNED_COPY_AGES_IN_PLACE`.
#[cfg(test)]
mod t1w8_pin10_tests;

#[cfg(test)]
mod gcj_pcp_sorted_live_offsets_tests;

mod young_from_guard;
#[allow(unused_imports)]
pub use young_from_guard::*;
#[allow(unused_imports)]
use young_from_guard::*;
mod geometry_cap_destination;
#[allow(unused_imports)]
pub use geometry_cap_destination::*;
#[allow(unused_imports)]
use geometry_cap_destination::*;
mod publish_young_commit_bits;
#[allow(unused_imports)]
pub use publish_young_commit_bits::*;
#[allow(unused_imports)]
use publish_young_commit_bits::*;
mod set_resolved_compact_primitive_field;
#[allow(unused_imports)]
pub use set_resolved_compact_primitive_field::*;
#[allow(unused_imports)]
use set_resolved_compact_primitive_field::*;
mod set_field_full;
#[allow(unused_imports)]
pub use set_field_full::*;
#[allow(unused_imports)]
use set_field_full::*;
mod non_moving_young_cycle;
#[allow(unused_imports)]
pub use non_moving_young_cycle::*;
#[allow(unused_imports)]
use non_moving_young_cycle::*;
mod collect_garbage_inner_with_pins;
#[allow(unused_imports)]
pub use collect_garbage_inner_with_pins::*;
#[allow(unused_imports)]
use collect_garbage_inner_with_pins::*;
mod young_drain_blocked;
#[allow(unused_imports)]
pub use young_drain_blocked::*;
#[allow(unused_imports)]
use young_drain_blocked::*;
mod sweep_young_non_moving;
#[allow(unused_imports)]
pub use sweep_young_non_moving::*;
#[allow(unused_imports)]
use sweep_young_non_moving::*;
mod sweep_old_gen_non_moving;
#[allow(unused_imports)]
pub use sweep_old_gen_non_moving::*;
#[allow(unused_imports)]
use sweep_old_gen_non_moving::*;
mod mark_young_to_old_refs_deferring;
#[allow(unused_imports)]
pub use mark_young_to_old_refs_deferring::*;
#[allow(unused_imports)]
use mark_young_to_old_refs_deferring::*;
mod cell_corrupt_diag_enabled;
#[allow(unused_imports)]
pub use cell_corrupt_diag_enabled::*;
#[allow(unused_imports)]
use cell_corrupt_diag_enabled::*;
mod note_rejected_old_mark_candidate;
#[allow(unused_imports)]
pub use note_rejected_old_mark_candidate::*;
#[allow(unused_imports)]
use note_rejected_old_mark_candidate::*;
mod zero_run_verdict;
#[allow(unused_imports)]
pub use zero_run_verdict::*;
#[allow(unused_imports)]
use zero_run_verdict::*;
