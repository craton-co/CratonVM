#[allow(unused_imports)]
use super::*;


/// gcj yg3-residuals R3: the serial copy's old-gen destination for a survivor
/// that would take to-space (`to_used` bytes so far) past the cycle's cap
/// ([`PromotionQueue::to_space_cap`]), or `None`: under the cap, no cap, or
/// old gen refused (the copy then stays young, as without the cap).
pub fn geometry_cap_destination(
    promoted_worklist: &mut PromotionQueue,
    old_gen: &mut OldGen,
    to_used: usize,
    total_size: usize,
) -> Option<*mut u8> {
    if to_used.saturating_add(total_size) <= promoted_worklist.to_space_cap {
        return None;
    }
    let ptr = promoted_worklist.alloc(old_gen, total_size)?;
    GEOMETRY_CAP_PROMOTIONS.fetch_add(1, Ordering::Relaxed);
    Some(ptr)
}

/// gcj yg3-residuals R3 (`gcj-j1yg-proposal-serial-young-geometry-and-no-pause-goal`
/// part 1): `CRATONVM_GEN_SERIAL_YOUNG_GEOMETRY` -- default ON, `=0` kills.
/// Read once per heap, when the VM builds it
/// ([`GenerationalHeap::with_capacity_and_young`]).
///
/// HotSpot Serial's young generation `Y` (`-Xmn`, `NewRatio`, default
/// `Xmx / 3`) is an eden of `Y - 2S` plus two survivors `S`, and it collects
/// when EDEN is full. This collector's young generation was a semi-space pair
/// of `Y / 2` each (`Xmx / 4` by default), whose trigger was capped at 95 % of
/// the semi: under `-Xmx256m -Xmn128m` 61 MiB against HotSpot's 102 MiB, and
/// `GcfF3yYoungPremiumProbe` ran 103 young collections to HotSpot's 50 (101 to
/// 75 with no young flag). On: each semi reserves `eden + S`
/// ([`cratonvm_gc_core::gen_young_sizing::SerialPairGeometry`]), the old generation is
/// HotSpot's `Xmx - Y`, the trigger is eden on top of what the last cycle left
/// young (at most `S`, [`geometry_young_trigger`]), and a copy keeps at most
/// `S` young ([`PromotionQueue::to_space_cap`], `ParEvac::with_to_space_cap`),
/// tenuring the rest as HotSpot's survivor overflow does.
pub fn gen_serial_young_geometry_enabled() -> bool {
    true
}

/// gcj yg3-residuals R3: the Serial geometry for a `total` (`-Xmx`) heap with
/// the operator's `young` flags and `-XX:SurvivorRatio` (at least 1), and the
/// old generation it leaves
/// (`total - Y`), or `None` when the budget cannot hold an aligned eden and
/// two survivors or would leave old gen under its floor. Pure.
///
/// `Y` is the explicit plan's pair when a young flag was typed
/// ([`cratonvm_gc_core::gen_young_sizing::plan_young_gen`], with its clamps), else
/// HotSpot's default `Xmx / (NewRatio + 1)` ([`serial_young_gen_bytes`]).
pub fn serial_young_geometry_for(
    total: usize,
    young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    survivor_ratio: usize,
) -> Option<(cratonvm_gc_core::gen_young_sizing::SerialPairGeometry, usize)> {
    let y = match cratonvm_gc_core::gen_young_sizing::plan_young_gen(total, young) {
        Some(plan) => plan.young_pair(),
        None => serial_young_gen_bytes(total, young),
    };
    let geom = cratonvm_gc_core::gen_young_sizing::plan_serial_pair_geometry(
        y,
        survivor_ratio,
        SERIAL_SPACE_ALIGNMENT,
    )?;
    let old = total.checked_sub(geom.young_budget)?;
    let min_old = cratonvm_gc_core::gen_young_sizing::MIN_OLD_GEN_BYTES.min(total / 2);
    (old >= min_old && geom.semi_reserve >= cratonvm_gc_core::gen_young_sizing::YOUNG_SEMI_MIN)
        .then_some((geom, old))
}

/// gcj yg3-residuals R3: the young trigger under the Serial geometry -- eden
/// (`eden_trigger`, the heap's armed trigger) on top of the bytes the last
/// cycle left young (`carry`, at most one survivor), within 95 % of the semi
/// (`capacity`). HotSpot collects when eden is full whatever the survivor
/// space holds; here both live in the from-space. Pure.
pub fn geometry_young_trigger(
    eden_trigger: usize,
    carry: usize,
    survivor: usize,
    capacity: usize,
) -> usize {
    eden_trigger
        .saturating_add(carry.min(survivor))
        .min(capacity.saturating_mul(SERIAL_EDEN_FULL_PERCENT) / 100)
        .max(1)
}



/// gcj yg3-residuals: survivor bytes one copy worker is given before another
/// joins ([`copy_workers_for_survivors`]).
pub const COPY_SURVIVOR_BYTES_PER_WORKER: usize = 1 << 20;

/// gcj yg3-residuals (`gcj-j3yg3-young-collection-residuals` R2): the
/// parallel young copy's worker count. `young_gc_threads` sizes every young
/// phase by from-space OCCUPANCY, which is right for a walk or a sweep (their
/// work is the allocated bytes) and wrong for a copy, whose work is the
/// survivors. On the JIT-warm `GcfF3yYoungPremiumProbe` (~140 MB of eden, a few
/// MB of survivors a cycle) eight workers took the pinned copy to 1.17-1.33x
/// the sweep's process CPU while two took it to 0.85x at the same pause (rows
/// `mfC` of the yg3-residuals report). With `by_survivors`: one worker per
/// [`COPY_SURVIVOR_BYTES_PER_WORKER`] of the last cycle's survivors
/// (`survivors`), at least two (the parallel arm; one would be the serial
/// path) and at most `occupancy_workers`; below two it stays as asked.
/// Without it (`=0`, or an explicit `CRATONVM_GC_PAR_THREADS`), as before.
/// Pure.
pub fn copy_workers_for_survivors(
    occupancy_workers: usize,
    survivors: usize,
    by_survivors: bool,
) -> usize {
    if !by_survivors || occupancy_workers < 2 {
        return occupancy_workers;
    }
    survivors
        .div_ceil(COPY_SURVIVOR_BYTES_PER_WORKER)
        .clamp(2, occupancy_workers)
}



/// gcj j3/yg3: the floor [`GenerationalHeap::set_min_tlab_floor`] installs
/// for a VM's typed `TlabSizing::min_size()` (`typed`), pure: the typed size
/// on the 8-byte grid within `[tlab::frag_tlab_floor(), tlab::max_tlab_size()]`
/// with the switch on, the 8 KiB constant otherwise.
pub fn min_tlab_floor_for(typed: usize, enabled: bool) -> usize {
    if !enabled {
        return cratonvm_gc_core::tlab::min_tlab_size();
    }
    (typed & !7).clamp(cratonvm_gc_core::tlab::frag_tlab_floor(), cratonvm_gc_core::tlab::max_tlab_size())
}



/// Slots of [`GenerationalHeap::term4_offer_census`] (gcj j1/yg), printed as
/// `[GC] young_term4_offer:` by [`GenerationalHeap::term4_offer_census_line`]:
/// cycles on which term 4 fired.
pub const T4O_CYCLES: usize = 0;
/// Term-4 cycles offered to the pinned copy (`term4_alone`).
pub const T4O_OFFERED: usize = 1;
/// Not offered: a GPU critical section forbade relocation (first-wins order).
pub const T4O_GPU: usize = 2;
/// Not offered: an explicit full collection (`System.gc()`) sweeps young.
pub const T4O_EXPLICIT_FULL_GC: usize = 3;
/// Not offered: the promotion-OOM guard (both generations nearly full).
pub const T4O_PROMOTION_RISK: usize = 4;
/// Of the offered: offered only because
/// `CRATONVM_GEN_PINNED_COPY_TAKES_PROMOTION_RISK` admitted the guard's cycle.
pub const T4O_PROMOTION_RISK_ADMITTED: usize = 5;
/// Length of [`GenerationalHeap::term4_offer_census`].
pub const T4O_LEN: usize = 6;

/// gcj j1/yg: is a cycle on which term 4 fired offered to the pinned copy
/// (`term4_alone`), and which census slot names the first term that kept it
/// from the offer (or admitted it under the switch)? Pure.
/// `divert_for_incomplete_moving_coverage` cannot hold together with term 4
/// (term 4 needs `moving_young`, which that term clears); it is kept so the
/// predicate reads as the conjunction it replaces.
///
/// With `takes_promotion_risk` off, `offered` is exactly the conjunction
/// `collect_garbage_inner_with_pins` used before gcj j1/yg:
/// `!honor_promotion_oom_risk && !divert_for_incomplete_moving_coverage &&
/// !explicit_full_gc && !gpu_relocation_forbidden`.
pub fn term4_offer_verdict(
    honor_promotion_oom_risk: bool,
    divert_for_incomplete_moving_coverage: bool,
    explicit_full_gc: bool,
    gpu_relocation_forbidden: bool,
    takes_promotion_risk: bool,
) -> (bool, Option<usize>) {
    if divert_for_incomplete_moving_coverage {
        return (false, None);
    }
    if gpu_relocation_forbidden {
        return (false, Some(T4O_GPU));
    }
    if explicit_full_gc {
        return (false, Some(T4O_EXPLICIT_FULL_GC));
    }
    if honor_promotion_oom_risk {
        if takes_promotion_risk {
            return (true, Some(T4O_PROMOTION_RISK_ADMITTED));
        }
        return (false, Some(T4O_PROMOTION_RISK));
    }
    (true, None)
}

// gch h3/yc: `CRATONVM_GEN_TLAB_REFILL_SPARES_LARGE_BLOCKS` (gch h1/yy; free-list
// TLAB refills capped at 64 KiB to leave large blocks whole for large arrays,
// `gcg-g1c-...`) is DELETED. Alone it made `hashmap-pair` 3-8x slower with MORE
// young cycles (vh1-yy `yy1_p_prem_spare_*`: 255 vs 77-151, 329 futile-young
// verdicts); with SERIAL_EDEN it refused 47 065 of 49 521 refills and kept the
// finalize probe at 0/64 (vh2-ob B). g1c's answer is the pinned young copy
// (vh2-yg E: 151 -> 36 young cycles).

/// gcf f2/y: a free block smaller than this is "fragmented" space for
/// [`young_space_fragmented`]: `tlab::MIN_TLAB_SIZE`, below which a refill
/// cannot carve a regular TLAB and falls back to the mini-TLAB path.
pub const DEFRAG_SMALL_BLOCK: usize = 8 * 1024;

/// gcf f2/y: is a young arena fragmented, given the bytes of its free list
/// in blocks smaller than [`DEFRAG_SMALL_BLOCK`] (`small_bytes`), its whole
/// free list (`free_bytes`) and its un-bumped tail (`tail_bytes`)? Yes when
/// at least HALF of what the mutator can allocate before the next young
/// cycle lies in holes too small for a TLAB.
pub fn young_space_fragmented(small_bytes: usize, free_bytes: usize, tail_bytes: usize) -> bool {
    let allocatable = free_bytes.saturating_add(tail_bytes);
    allocatable > 0 && small_bytes.min(free_bytes).saturating_mul(2) >= allocatable
}

/// How many times a missed request the young free list must hold for
/// [`young_miss_is_fragmentation`]: the rule the deleted young-fragmentation
/// spill used (gcg g5/yg, `CRATONVM_GEN_YOUNG_FRAG_SPILL`).
pub const DEFRAG_MISS_FREE_MULTIPLE: usize = 8;

/// gch h2/yg (`gcg-g5yg-proposal-defrag-gate-counts-large-request-misses`):
/// is a young miss of `size` bytes (neither the bump tail nor any single free
/// block could take it) a FRAGMENTATION miss? Yes for a large request
/// (`>= DEFRAG_LARGEST_FREE_FLOOR`, 64 KiB) the free list could hold
/// [`DEFRAG_MISS_FREE_MULTIPLE`] times over: young is not full, it is cut up.
/// Pure.
pub fn young_miss_is_fragmentation(size: usize, free_list_bytes: usize) -> bool {
    size >= DEFRAG_LARGEST_FREE_FLOOR && free_list_bytes / DEFRAG_MISS_FREE_MULTIPLE >= size
}

/// gcg g6/yg: may this young cycle's copy run on the parallel evacuator?
/// `par_evac` is `CRATONVM_GC_PAR_EVAC`; a pinned in-place cycle needs its own
/// opt-in (`par_in_place`); and a moving cycle with both generations nearly
/// full (`promotion_oom_risk`) copies serially, because the parallel arm's
/// to-space region can run out where the serial semi-space cannot
/// (`gcg-g5oo-parallel-young-evacuation-aborts-when-promotion-and-plab-both-refuse-20261002`).
/// An in-place cycle copies into from-space spans and keeps what does not
/// fit, so the risk does not apply to it.
pub fn parallel_young_copy_permitted(
    par_evac: bool,
    in_place_cycle: bool,
    par_in_place: bool,
    promotion_oom_risk: bool,
) -> bool {
    if !par_evac {
        return false;
    }
    if in_place_cycle {
        return par_in_place;
    }
    !promotion_oom_risk
}


/// The value [`young_trigger_floor_after_collection`] stores to switch the
/// occupancy trigger off until the next completed collection: no `live` can
/// reach it, so `needs_gc` answers `false` and allocation failure — the
/// forced-GC ladder, which ends in `OutOfMemoryError` — decides instead.
pub const YOUNG_TRIGGER_DEFERRED: usize = usize::MAX;

/// Can the old generation absorb nothing worth collecting for?
///
/// `old_headroom` is `old capacity - old used`; `heap_capacity` is what
/// `VmHeap::heap_capacity` reports for this backend (one young semi-space plus
/// the old generation). "Wedged" is headroom below 2 % of that capacity. It is
/// the same yardstick, computed by this same function, as the free-space half
/// of the VM's GC-overhead limit (`note_gc_productivity`,
/// HIB-GCOVERHEAD-HALFFULL.1), so the trigger and the overhead limit cannot
/// disagree about when the old generation is full.
///
/// gen r4w4/oom (2026-09-24).
#[inline]
pub fn old_gen_is_wedged(old_headroom: usize, heap_capacity: usize) -> bool {
    // Widening: usize -> u128 cannot overflow the products below.
    (old_headroom as u128) * 100 < (heap_capacity as u128) * 2
}

/// The young trigger's anti-livelock floor, computed once per completed
/// collection from the live young occupancy the collection left.
///
/// Without the wedged arm this is exactly the floor `needs_gc` has always
/// armed: `0` below the trigger; at or above it, `live + capacity/16` capped at
/// the non-moving trigger (`NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT`), so a young
/// generation whose live set sits over the trigger waits for a sixteenth of
/// capacity of genuine growth before collecting again.
///
/// # gen r4w4/oom (2026-09-24): the cap is a livelock when old gen is wedged
///
/// When `live + capacity/16` is past the cap, the cap is what fires, and the
/// cap can sit a single TLAB above `live`. If the old generation can absorb
/// nothing (`old_gen_wedged`, evaluated only in this corner), no collection can
/// lower `live` except by finding young garbage, and every cycle then frees
/// about one TLAB carve: the mutator carves 256 KiB, `needs_gc` fires, the
/// collection copies the same ~30 MB of survivors, runs a major that frees
/// nothing, reclaims the barely-used carve, and the loop repeats. Measured on
/// `GenR4W3OomProbe` step `small` at `-Xmx128m`: `moving+major
/// young=30408704->30146560 old=67106728->67106728`, three ~300 ms cycles a
/// second, forever, never an `OutOfMemoryError`. The allocation succeeded after
/// every cycle, so the allocation-failure ladder (forced GC, overhead limit,
/// `last_ditch_reclaim`, OOME) was never entered.
///
/// In that corner the floor is [`YOUNG_TRIGGER_DEFERRED`]: the occupancy
/// trigger stands down until the next completed collection and allocation
/// failure takes over, which is HotSpot Serial's shape — eden fills, the
/// allocation fails, a full collection runs, and it either frees room or the
/// allocation throws. Collection is still never abandoned: the young
/// generation fills (at most `capacity - live` more, under a sixth of it,
/// since `live` is past `ceiling - capacity/16`), the refill or the young
/// attempt fails, and the ladder's forced collection runs. A forced collection
/// that frees young garbage moves `minor_gc_count`, so the next evaluation
/// recomputes an ordinary floor from the lower `live`.
///
/// The price, stated: in this corner a native wrapper allocation that cannot
/// report failure meets a FULL young generation more often than it met a 90 %
/// one. That is the same exposure every genuinely exhausted heap already has
/// (`docs/feature-designs/generative-gc-architecture.md`),
/// and the program is by construction one young generation away from
/// `OutOfMemoryError`.
///
/// Nothing changes while the old generation has room AND the last cycle could
/// promote (gcd d9/c: the caller ORs `GenerationalHeap::young_drain_blocked`
/// into `old_gen_wedged`, because a sweep whose promotion a refusing helper
/// window gated off cannot drain young either).
///
/// `old_gen_wedged` is a closure so the old-generation lock is taken only when
/// the cap bites.
///
/// gcd d9/b: this is [`young_trigger_floor_judged`] with the below-the-cap
/// arm off, kept for the stress trigger (which asks only whether the floor
/// DEFERS) and for the tests that pin the historical rule.
#[inline]
pub fn young_trigger_floor_after_collection(
    live: usize,
    threshold: usize,
    capacity: usize,
    old_gen_wedged: impl FnOnce() -> bool,
) -> usize {
    young_trigger_floor_judged(live, threshold, capacity, false, || {
        old_gen_wedged().then_some(usize::MAX)
    })
    .0
}

/// What [`young_trigger_floor_judged`] did with a wedged old generation.
/// gcd d9/b; counted per heap (`GenerationalHeap::young_wedged_census`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WedgedTriggerVerdict {
    /// The ordinary floor (the old generation was not consulted, or had room).
    Ordinary,
    /// Below the cap, old generation wedged: the floor was raised to the
    /// non-moving trigger (`CRATONVM_GEN_WEDGED_YOUNG_TRIGGER`).
    Raised,
    /// At the cap, old generation wedged: [`YOUNG_TRIGGER_DEFERRED`].
    Deferred,
}

/// The young trigger's post-collection floor, and whether a wedged old
/// generation changed it. [`young_trigger_floor_after_collection`]'s rule plus
/// one arm BELOW the cap. gcd d9/b (2026-09-28).
///
/// `wedged_raise_cap` is asked at most once, and only where the answer
/// matters: `None` = a young collection can drain young (the old generation
/// has room, and -- gcd d9/c -- the last cycle could promote); `Some(cap)` =
/// it cannot (old generation wedged per [`old_gen_is_wedged`], or the drain
/// blocked, `GenerationalHeap::young_drain_blocked`), and `cap` is the highest
/// young occupancy the raise may use (`usize::MAX`, or the
/// `CRATONVM_GC_OLD_BORROW_YOUNG` cap on a heap whose old generation grew into
/// the young budget). "Wedged" below means either.
///
/// # The arm below the cap (`raise_when_wedged`)
///
/// The deferral above covers a live young set within `capacity/16` of the
/// 90 % cap. It does not cover one that sits just under the MOVING trigger
/// (50 % of the semi-space by default; under the opt-in Serial-shaped
/// trigger, which starts at 95 %, above the cap, this applies once the
/// pause-goal loop has lowered it): there `live < threshold`, so the
/// ordinary floor is `0`, and the next collection fires as soon as `live`
/// crosses the threshold -- after one or two TLAB carves. With the old
/// generation wedged, no young collection can promote anything, so each such
/// cycle frees only what the mutators allocated since the previous one, and
/// every one of them also runs a major (old generation past 75 %) that frees
/// nothing either. Measured on `GenR4W5ThreadsOomProbe -Xmx128m` (a Windows
/// build of 2026-09-27, `--verbose:gc`): `moving+major
/// young=16756480->16494720 old=67108840->67108840 pause=309ms`, 65 cycles in
/// a row in the first shape alone, and 361 of the run's 564 collections (116
/// of its 179 s) were occupancy-triggered cycles on a wedged old generation.
/// HotSpot Serial's shape is an eden that fills, a failed allocation, one
/// full collection.
///
/// So when the level the ordinary rule would fire at is below the cap and the
/// old generation is wedged, the floor is raised to the cap (the non-moving
/// trigger, 90 %): the next collection waits for the young generation to be
/// nearly full, and each one then reclaims everything allocated since. A
/// young set within `capacity/16` of the cap gets the deferral above at the
/// next sample. Safe for the moving collector: to-space has the capacity of
/// from-space, so survivors of a 90 % from-space fit (the non-moving sweep
/// already fires at 90 %). The 10 % above the cap stays for allocations that
/// cannot collect on their own (the native allocators spill young -> old).
/// Nothing changes while the old generation has room.
///
/// `false` is [`young_trigger_floor_after_collection`] byte for byte.
#[inline]
pub fn young_trigger_floor_judged(
    live: usize,
    threshold: usize,
    capacity: usize,
    raise_when_wedged: bool,
    wedged_raise_cap: impl FnOnce() -> Option<usize>,
) -> (usize, WedgedTriggerVerdict) {
    let ceiling = capacity * NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT / 100;
    let rearm = live.saturating_add(capacity / 16);
    let ordinary = if live >= threshold {
        rearm.min(ceiling)
    } else {
        0
    };
    if rearm > ceiling {
        return if wedged_raise_cap().is_some() {
            (YOUNG_TRIGGER_DEFERRED, WedgedTriggerVerdict::Deferred)
        } else {
            (ordinary, WedgedTriggerVerdict::Ordinary)
        };
    }
    let fires_at = threshold.max(ordinary);
    if raise_when_wedged && fires_at < ceiling {
        if let Some(cap) = wedged_raise_cap() {
            let raised = ceiling.min(cap);
            if raised > fires_at {
                return (raised, WedgedTriggerVerdict::Raised);
            }
        }
    }
    (ordinary, WedgedTriggerVerdict::Ordinary)
}



// gcf f7/c: `CRATONVM_GEN_YOUNG_TRIGGER_EDEN` (gcf f3/o, the eden-rule budget
// `threshold - base`) is DELETED: vf3 never saw it engage (73/73 evac cycles
// entered by allocation failure) and vf5 c5 `_eden` was 0/3 on the finalize
// divergence it was the candidate fix for. Its rule kept the occupancy
// trigger's SIZE, which is what differs from HotSpot; the Serial-shaped
// trigger below changes the size instead.

/// gcf f7/c: `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` -- default ON since the
/// user's flip of 2026-10-13 (`runtime_flag_default_on`, `=0` kills; gci
/// i22/ed doc fix: this said "default OFF (`runtime_flag_on`)", the state
/// after gch h4/fl flipped it and reverted it: vh4-fl A passed 3/3, B failed
/// its wall and hashmap-pair cost terms), read once when a heap
/// is built from an `-Xmx` budget
/// ([`GenerationalHeap::arm_serial_eden_trigger`]): the young trigger and the
/// adaptive-tenuring survivor target follow HotSpot Serial's eden for the
/// heap's `-Xmx` / `-Xmn` / `NewRatio` instead of
/// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` (default 50 %) of a semi-space.
/// `docs/feature-designs/generative-gc-architecture.md`.
pub fn gen_young_trigger_serial_eden_enabled() -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
    // WARNING gci-i48-flipB-20261017: confirmed default ON (ours, with the gci age clock).
    true
}

/// HotSpot's default `-XX:NewRatio` (old / young): young = `-Xmx / 3`.
pub const SERIAL_DEFAULT_NEW_RATIO: usize = 2;

/// gcf f7/c: the most of a from-space the Serial-shaped trigger lets
/// allocation fill before it collects -- the top of the
/// `CRATONVM_GC_YOUNG_TRIGGER_PERCENT` clamp, the arm vf5 measured equal to
/// HotSpot on `OldGenFinalizeProbe` (c5 `_pct95`, 3/3). The rest is the
/// TLAB-refill slack the 90 % non-moving trigger also keeps.
pub const SERIAL_EDEN_FULL_PERCENT: usize = 95;

/// gcf f7/c: HotSpot Serial's young generation (eden plus both survivors), in
/// bytes, for a `total_bytes` (`-Xmx`) heap and the operator's young flags.
/// Pure.
///
/// HotSpot's resolution (`GenArguments::initialize_size_info`): `-Xmn` sets
/// it exactly; otherwise `MaxHeapSize / (NewRatio + 1)` (`NewRatio` default
/// 2), held between `-XX:NewSize` (floor) and `-XX:MaxNewSize` (ceiling; a
/// `NewSize` above it raises it). HotSpot also aligns to its 64 KiB space
/// alignment; [`serial_eden_bytes`] applies that alignment (gci i30/gv), so
/// this stays the unaligned young size its other callers size against.
///
/// Unlike [`cratonvm_gc_core::gen_young_sizing::plan_young_gen`], whose default base is
/// this collector's `-Xmx / 2` pair, the default here is HotSpot's `-Xmx / 3`:
/// the pair is this collector's geometry, the eden is HotSpot's behaviour.
pub fn serial_young_gen_bytes(
    total_bytes: usize,
    young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
) -> usize {
    if let Some(xmn) = young.xmn {
        return xmn.min(total_bytes);
    }
    let ratio = young.new_ratio.unwrap_or(SERIAL_DEFAULT_NEW_RATIO);
    let mut size = total_bytes / ratio.saturating_add(1);
    if let Some(max) = young.max_new_size {
        size = size.min(max.max(young.new_size.unwrap_or(0)));
    }
    if let Some(min) = young.new_size {
        size = size.max(min);
    }
    size.min(total_bytes)
}

/// gcf f7/c: HotSpot Serial's eden for a young generation of
/// `young_gen_bytes`: `young * SurvivorRatio / (SurvivorRatio + 2)` (eden is
/// `SurvivorRatio` survivors, and there are two survivors), i.e. 80 % at the
/// default `SurvivorRatio = 8` ([`tenuring::SURVIVOR_RATIO`]). Pure.
///
/// gci i30/gv (`gci-i27gs-serial-young-sizes-are-not-space-aligned-FIXED-20261016`):
/// with HotSpot's 64 KiB `SpaceAlignment` applied as HotSpot Serial does
/// (`GenArguments::scale_by_NewRatio_aligned`, `DefNewGeneration::
/// compute_survivor_size` / `compute_space_boundaries`): `Y` aligned down,
/// `S = n > align ? align_down(n) : align` with `n = Y / (SurvivorRatio + 2)`,
/// eden `Y - 2S`. At `-Xmx256m` that is HotSpot's 71 630 848 (the unaligned
/// 80 % was 71 582 788). A young generation too small to hold two aligned
/// survivors and an eden (under 192 KiB) keeps the unaligned 80 %.
pub fn serial_eden_bytes(young_gen_bytes: usize) -> usize {
    serial_eden_bytes_at(young_gen_bytes, tenuring::SURVIVOR_RATIO as usize)
}

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the consumer half): [`serial_eden_bytes`] at an explicit
/// `-XX:SurvivorRatio` (the heap's [`GenerationalHeap::serial_survivor_ratio`]),
/// HotSpot Serial's `DefNewGeneration::compute_space_boundaries` with the
/// operator's ratio: `S = Y / (SurvivorRatio + 2)` aligned, eden `Y - 2S`.
/// `survivor_ratio` is read as at least 1 (the launcher refuses 0; its
/// `AfterMemoryInit` constraint caps it at `MaxHeapSize / SpaceAlignment`).
/// Pure; the default ratio is [`serial_eden_bytes`] byte for byte.
pub fn serial_eden_bytes_at(young_gen_bytes: usize, survivor_ratio: usize) -> usize {
    let survivor_ratio = survivor_ratio.max(1);
    let sr = survivor_ratio as u128;
    let unaligned = ((young_gen_bytes as u128) * sr / (sr + 2)) as usize;
    let align = SERIAL_SPACE_ALIGNMENT;
    let y = young_gen_bytes & !(align - 1);
    let n = y / survivor_ratio.saturating_add(2);
    let s = if n > align { n & !(align - 1) } else { align };
    match s.checked_mul(2) {
        Some(two_s) if two_s < y => y - two_s,
        _ => unaligned,
    }
}

/// gci i27/gs (`gci-i14mp-memory-pool-and-gc-mxbean-review-findings-FIXED-20261016`
/// item 8): `CRATONVM_GEN_SURVIVOR_POOL_RESERVE` (default ON since
/// 2026-10-17, `=0` kills) -- the `Survivor Space` pool reports HotSpot Serial's survivor reserve as its
/// `max` (and a committed of at least that reserve's share of the young
/// commit), carved out of `Eden Space`; see [`survivor_reserve_split`]. Read
/// per pool sample (a reporting call, never on an allocation path).
pub fn survivor_pool_reserve_enabled() -> bool {
    true
}

/// gci i45/sc: `CRATONVM_GEN_SURVIVOR_RATIO_FLAG` (default ON, `=0` kills) --
/// a typed `-XX:SurvivorRatio` sizes this heap's Serial eden, survivor
/// reserve and tenuring target ([`GenerationalHeap::set_survivor_ratio`]).
/// `=0`: the constant 8, whatever was typed (the i43 behaviour). Read once
/// per heap, at construction; never on an allocation path.
pub fn gen_survivor_ratio_flag_enabled() -> bool {
    true
}

/// HotSpot's `SpaceAlignment` for Serial (`Generation::GenGrain`, 64 KiB):
/// a survivor space is a whole number of these.
pub const SERIAL_SPACE_ALIGNMENT: usize = 64 * 1024;

/// gci i27/gs: one HotSpot Serial survivor space for a semi-space of `semi`
/// bytes. This heap's semi-space is HotSpot's `eden + from` (its to-space is
/// the copy reserve, in no pool: `usable_committed_parts`), and HotSpot sizes
/// `survivor = young / (SurvivorRatio + 2)`, `eden = SurvivorRatio *
/// survivor` (`DefNewGeneration::compute_space_boundaries`), so `eden + from
/// = (SurvivorRatio + 1) * survivor`: the survivor is `semi / (SurvivorRatio
/// + 1)`, aligned down to `SpaceAlignment` and never below one alignment unit
/// (HotSpot's `if (survivor_size == 0) survivor_size = alignment`), never
/// above `semi`. Pure.
#[cfg_attr(not(test), allow(dead_code))] // gci i45/sc: default-ratio reference (tests); production passes the heap's ratio
pub fn serial_survivor_reserve(semi: usize) -> usize {
    serial_survivor_reserve_at(semi, tenuring::SURVIVOR_RATIO as usize)
}

/// gci i45/sc: [`serial_survivor_reserve`] at an explicit `-XX:SurvivorRatio`
/// (the heap's [`GenerationalHeap::serial_survivor_ratio`], read as at least
/// 1): `semi / (SurvivorRatio + 1)`, aligned as there. Pure; the default ratio
/// is [`serial_survivor_reserve`] byte for byte.
pub fn serial_survivor_reserve_at(semi: usize, survivor_ratio: usize) -> usize {
    let per = semi / survivor_ratio.max(1).saturating_add(1);
    let aligned = per & !(SERIAL_SPACE_ALIGNMENT - 1);
    if aligned == 0 {
        SERIAL_SPACE_ALIGNMENT.min(semi)
    } else {
        aligned
    }
}

/// gci i27/gs: the young pools' `max` / `committed` under
/// `CRATONVM_GEN_SURVIVOR_POOL_RESERVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurvivorReserveSplit {
    pub eden_max: usize,
    pub eden_committed: usize,
    pub survivor_max: usize,
    pub survivor_committed: usize,
}

/// gci i27/gs (`gci-i14mp-...` item 8): split one semi-space between `Eden
/// Space` and `Survivor Space` as HotSpot Serial's pools report it -- the
/// survivor's `max` is the survivor reserve of the largest semi-space
/// ([`serial_survivor_reserve`] of `semi_max`, HotSpot's
/// `max_survivor_size()`), eden's `max` the rest of it, and the survivor's
/// `committed` the reserve of the current young commit (HotSpot's from-space
/// capacity) -- while keeping every invariant the pools already hold:
///
/// * `max`: `eden_max + survivor_max == max(semi_max, young_total)`, so the
///   pools' maxes still sum to `Runtime.maxMemory()`
///   (`max_usable_excludes_the_copy_reserve_and_the_pool_maxes_sum_to_it`);
/// * `committed`: `eden_committed + survivor_committed == young_total`, so
///   they still sum to `Runtime.totalMemory()`;
/// * `used <= committed <= max` for both pools (`new MemoryUsage` throws
///   otherwise), with `eden_used = young_live - survivor_used`.
///
/// This young generation's eden is not bounded by `semi - reserve` (the
/// trigger may let it fill the semi-space) and its survivors are not bounded
/// by the reserve (to-space is a whole semi-space), so where either overflows
/// the reserve the survivor `max` gives way: it is the reserve, at least the
/// survivors, and at most what eden's usage leaves. Inputs: `young_total` is
/// the young commit as `jmx_memory_pools` computes it (`>= young_live`),
/// `survivor_used <= young_live`. Pure.
#[cfg_attr(not(test), allow(dead_code))] // gci i45/sc: default-ratio reference (tests); production passes the heap's ratio
pub fn survivor_reserve_split(
    semi_max: usize,
    young_total: usize,
    young_live: usize,
    survivor_used: usize,
) -> SurvivorReserveSplit {
    survivor_reserve_split_at(
        semi_max,
        young_total,
        young_live,
        survivor_used,
        tenuring::SURVIVOR_RATIO as usize,
    )
}

/// gci i45/sc: [`survivor_reserve_split`] with the reserve taken at an
/// explicit `-XX:SurvivorRatio` ([`serial_survivor_reserve_at`]). Same
/// invariants; the default ratio is [`survivor_reserve_split`] byte for byte.
pub fn survivor_reserve_split_at(
    semi_max: usize,
    young_total: usize,
    young_live: usize,
    survivor_used: usize,
    survivor_ratio: usize,
) -> SurvivorReserveSplit {
    let young_live = young_live.min(young_total);
    let survivor_used = survivor_used.min(young_live);
    let semi = semi_max.max(young_total);
    let eden_used = young_live - survivor_used;
    let survivor_max = serial_survivor_reserve_at(semi, survivor_ratio)
        .max(survivor_used)
        .min(semi - eden_used);
    let eden_max = semi - survivor_max;
    // `lo <= hi` by the bounds above (see the doc's invariants).
    let lo = survivor_used.max(young_total.saturating_sub(eden_max));
    let hi = (young_total - eden_used).min(survivor_max);
    let survivor_committed = serial_survivor_reserve_at(young_total, survivor_ratio)
        .min(hi)
        .max(lo);
    SurvivorReserveSplit {
        eden_max,
        eden_committed: young_total - survivor_committed,
        survivor_max,
        survivor_committed,
    }
}

/// gci i27/gs: `cargo test -j 5 -p cratonvm-gc --lib gci_i27_gs_`.


/// gcf f7/c: the Serial-shaped young trigger for a from-space of
/// `semi_capacity` bytes: HotSpot's eden, capped at
/// [`SERIAL_EDEN_FULL_PERCENT`] of the semi-space. Pure.
///
/// The cap binds on every heap the `-Xmx` constructors build, by arithmetic:
/// this collector's young PAIR is the young generation (or `-Xmx / 2` by
/// default), so one semi-space is half of it, while HotSpot's eden is 80 % of
/// a young generation of at least `-Xmx / 3` (`0.267 * Xmx > 0.25 * Xmx` at
/// the defaults; `0.8 * Y > 0.5 * Y` under `-Xmn` / `NewRatio`). So the
/// Serial shape here is "collect when the from-space is full", which is
/// HotSpot's own rule (collect when eden cannot serve an allocation) on a
/// smaller eden. The `min` keeps the rule honest if the geometry changes.
/// gcj j1/yg (`CRATONVM_GEN_PINNED_COPY_SURVIVOR_RESERVE`): the bytes a
/// JIT-warm young cycle leaves free in a from-space of `capacity` bytes.
///
/// The pinned in-place copy has no to-space: it copies survivors into the
/// spans of from-space that were free when the cycle began and promotes the
/// rest (`in_place_destination`'s overflow), whatever their age. A cycle
/// triggered at 90-95 % of the semi has 5-10 % to copy into, so a JIT-warm
/// program's survivors are promoted wholesale (4-thread javac: ~5k objects a
/// cycle, the old generation grew ~15 MB a cycle and its card scan became most
/// of every pause). HotSpot Serial copies into a survivor space of
/// `Y / (SurvivorRatio + 2)` and promotes only its overflow. The reserve is
/// that survivor (`serial_survivor`), or 5/4 of the last pinned cycle's
/// survivors when they were larger, never more than half the semi. Pure.
pub fn pinned_survivor_reserve(capacity: usize, serial_survivor: usize, last_survivors: usize) -> usize {
    serial_survivor
        .max(last_survivors.saturating_add(last_survivors / 4))
        .min(capacity / 2)
}




pub fn serial_eden_young_trigger(semi_capacity: usize, serial_eden: usize) -> usize {
    serial_eden
        .min(semi_capacity.saturating_mul(SERIAL_EDEN_FULL_PERCENT) / 100)
        .max(1)
}

// gci i22/ed (`gch-h4fl-proposal-serial-eden-follows-the-initial-heap-20261009`,
// `gch-h4fl-young-sizing-and-xlog-review-findings-FIXED-20261017` item 5): HotSpot
// Serial's young generation starts at the INITIAL heap's share and is resized
// after full collections. The Serial-shaped trigger above models only its
// MAXIMUM eden.

/// gci i22/ed: `CRATONVM_GEN_SERIAL_EDEN_INITIAL=1` -- default OFF
/// (`runtime_flag_on`), read once when the VM builds a Generational heap
/// ([`GenerationalHeap::follow_initial_heap_eden`]), and inert unless the
/// Serial-shaped trigger (`CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`) armed that
/// heap. On, the trigger starts at HotSpot Serial's eden for the INITIAL heap
/// ([`serial_initial_young_gen_bytes`]) and moves after every collection that
/// reclaimed the old generation, by HotSpot's `TenuredGeneration::compute_new_size`
/// followed by `DefNewGeneration::compute_new_size`
/// ([`SerialEdenFollow::after_full`]), never above the maximum eden the
/// trigger switch arms. The initial heap is what the VM was built with:
/// `-Xms` / `-XX:InitialHeapSize`, else the launcher's ergonomic `physical / 64`
/// unless `CRATONVM_GC_HOTSPOT_INITIAL_HEAP=0` (default ON on every collector
/// since gci i48, 2026-10-17; opt-in `=1` on Generational before), else
/// `VmConfig`'s 16 MiB default. gci i35/fz: the `cratonvm` launcher applied
/// the ergonomic initial heap whenever this switch was on and
/// `CRATONVM_GC_HOTSPOT_INITIAL_HEAP` was unset (`fz31_f_ed_only` had followed
/// the 16 MiB default: 0/64); since gci i48 that is the default itself. An
/// embedding still passes its own initial heap.
pub fn gen_serial_eden_initial_enabled() -> bool {
    false
}

/// HotSpot Serial's `GenAlignment` / `SpaceAlignment` (64 KiB).
pub const SERIAL_GEN_ALIGNMENT: usize = 64 * 1024;
/// HotSpot's default `NewSize` on a 64-bit VM, `ScaleForWordSize(1 * M)`
/// (`1M * 13 / 10`, word-aligned): the floor of the NewRatio-derived initial
/// young generation (`GenArguments::initialize_size_info`). Binds only below
/// a ~4 MiB initial heap.
pub const SERIAL_DEFAULT_NEW_SIZE: usize = 1_363_144;
/// HotSpot's `MinHeapDeltaBytes` on a 64-bit VM, `ScaleForWordSize(128 * K)`:
/// the old generation neither expands nor shrinks by less at a resize.
pub const SERIAL_MIN_HEAP_DELTA_BYTES: usize = 170_392;
/// HotSpot 25's `MinHeapFreeRatio` / `MaxHeapFreeRatio` defaults. The
/// Generational heap does not receive `-XX:Min/MaxHeapFreeRatio` (the
/// launcher validates them; only G1 reads them), so the defaults are modelled.
pub const SERIAL_MIN_HEAP_FREE_RATIO: usize = 40;
pub const SERIAL_MAX_HEAP_FREE_RATIO: usize = 70;
/// gci i22/ed: HotSpot Serial's INITIAL young generation (eden plus both
/// survivors), in bytes, for an `initial_bytes` (`InitialHeapSize`) start on a
/// `total_bytes` (`-Xmx`) heap. Pure.
///
/// `GenArguments::initialize_size_info`: when the initial heap is the maximum
/// the initial young generation IS the maximum ([`serial_young_gen_bytes`]);
/// otherwise a command-line `NewSize` (`-Xmn` sets it) is the initial size,
/// bounded by `InitialHeapSize - GenAlignment` and the maximum; otherwise
/// `InitialHeapSize / (NewRatio + 1)` held between the default `NewSize` and
/// the maximum. `0` (no initial heap known) reads as the maximum. HotSpot also
/// aligns the NewRatio share down to 64 KiB; not modelled, as in
/// [`serial_young_gen_bytes`].
pub fn serial_initial_young_gen_bytes(
    initial_bytes: usize,
    total_bytes: usize,
    young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
) -> usize {
    let max_young = serial_young_gen_bytes(total_bytes, young);
    if initial_bytes == 0 || initial_bytes >= total_bytes {
        return max_young;
    }
    if let Some(cmdline) = young.xmn.or(young.new_size) {
        // `bound_minus_alignment(NewSize, InitialHeapSize)`.
        let bound = initial_bytes
            .saturating_sub(SERIAL_GEN_ALIGNMENT)
            .max(SERIAL_GEN_ALIGNMENT);
        return cmdline.min(bound).min(max_young).max(1);
    }
    let ratio = young.new_ratio.unwrap_or(SERIAL_DEFAULT_NEW_RATIO);
    let scaled = initial_bytes / ratio.saturating_add(1);
    // `min(.., max_young)` keeps the clamp's bounds ordered.
    scaled
        .clamp(SERIAL_DEFAULT_NEW_SIZE.min(max_young), max_young)
        .max(1)
}

/// gci i22/ed: HotSpot Serial's `TenuredGeneration::compute_new_size` (25,
/// `ShrinkHeapInSteps` on): the old generation's capacity after a full
/// collection that left `used_after` bytes in it, from `capacity` (what it
/// had entering the collection). Grows to `used / (1 - MinHeapFreeRatio)`
/// (at least the initial size) when that is `MinHeapDeltaBytes` or more away;
/// shrinks toward `used / (1 - MaxHeapFreeRatio)` damped by `shrink_factor`
/// (0 % on the first call, then 10 %, 40 %, 100 %; reset by any call that
/// does not shrink). Never above `max`. Pure but for the factor.
///
/// Not modelled: HotSpot's extra shrink of an expansion made for promotion
/// within this same collection (`_capacity_at_prologue`), and page alignment.
pub fn serial_old_compute_new_size(
    capacity: usize,
    used_after: usize,
    initial: usize,
    max: usize,
    shrink_factor: &mut usize,
) -> usize {
    let current_factor = *shrink_factor;
    *shrink_factor = 0;
    let used = used_after as u128;
    let cap_usize = |v: u128| usize::try_from(v).unwrap_or(usize::MAX);
    let min_desired =
        cap_usize(used * 100 / (100 - SERIAL_MIN_HEAP_FREE_RATIO) as u128).max(initial);
    if capacity < min_desired {
        if min_desired - capacity >= SERIAL_MIN_HEAP_DELTA_BYTES {
            return min_desired.min(max).max(capacity);
        }
        return capacity;
    }
    let max_desired =
        cap_usize(used * 100 / (100 - SERIAL_MAX_HEAP_FREE_RATIO) as u128).max(initial);
    if capacity > max_desired {
        let shrink = (capacity - max_desired) / 100 * current_factor;
        *shrink_factor = if current_factor == 0 {
            10
        } else {
            (current_factor * 4).min(100)
        };
        if shrink >= SERIAL_MIN_HEAP_DELTA_BYTES {
            return capacity - shrink;
        }
    }
    capacity
}

/// gci i22/ed: HotSpot Serial's `DefNewGeneration::compute_new_size` after a
/// full collection (both survivors empty): `old_capacity / NewRatio`, aligned
/// up to 64 KiB, held between the initial and the maximum young generation.
/// Pure. Not modelled: `NewSizeThreadIncrease` (about 5 KiB per non-daemon
/// thread, below the alignment for any ordinary thread count).
pub fn serial_young_compute_new_size(
    old_capacity: usize,
    young_initial: usize,
    young_max: usize,
    new_ratio: usize,
) -> usize {
    let candidate = old_capacity / new_ratio.max(1);
    let aligned = candidate
        .div_ceil(SERIAL_GEN_ALIGNMENT)
        .saturating_mul(SERIAL_GEN_ALIGNMENT);
    aligned.clamp(young_initial, young_max.max(young_initial))
}

/// gci i22/ed: HotSpot Serial's generation sizes as
/// `CRATONVM_GEN_SERIAL_EDEN_INITIAL` follows them, per heap
/// ([`GenerationalHeap::follow_initial_heap_eden`]). Sizes are HotSpot's
/// geometry (young = eden + two survivors; old = `-Xmx` less the maximum
/// young), not this collector's arenas.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SerialEdenFollow {
    /// HotSpot's young generation now, bytes.
    pub young_now: usize,
    pub young_initial: usize,
    pub young_max: usize,
    /// `NewRatio` (old / young), at least 1.
    pub new_ratio: usize,
    /// HotSpot's old-generation capacity as modelled.
    pub old_capacity: usize,
    pub old_initial: usize,
    pub old_max: usize,
    /// `TenuredGeneration::_shrink_factor`.
    pub shrink_factor: usize,
    /// The most old-generation bytes seen in use since the last resize: a
    /// lower bound on the capacity HotSpot's promotion expansions reached.
    pub old_used_peak: usize,
    /// Resizes that grew / shrank the young generation (census).
    pub grows: u64,
    pub shrinks: u64,
}

impl SerialEdenFollow {
    /// The state for a heap of `total_bytes` started at `initial_bytes`
    /// (`0` = unknown, read as the maximum).
    pub fn arm(
        initial_bytes: usize,
        total_bytes: usize,
        young: &cratonvm_gc_core::gen_young_sizing::YoungGenSizing,
    ) -> Self {
        let young_max = serial_young_gen_bytes(total_bytes, young);
        let young_initial =
            serial_initial_young_gen_bytes(initial_bytes, total_bytes, young).min(young_max);
        let initial = if initial_bytes == 0 {
            total_bytes
        } else {
            initial_bytes.min(total_bytes)
        };
        let old_max = total_bytes.saturating_sub(young_max);
        let old_initial = initial.saturating_sub(young_initial).min(old_max);
        Self {
            young_now: young_initial,
            young_initial,
            young_max,
            new_ratio: young
                .new_ratio
                .unwrap_or(SERIAL_DEFAULT_NEW_RATIO)
                .max(1),
            old_capacity: old_initial,
            old_initial,
            old_max,
            ..Self::default()
        }
    }

    /// Record the old generation's occupancy after a collection.
    pub fn note_old_used(&mut self, used: usize) {
        self.old_used_peak = self.old_used_peak.max(used);
    }

    /// One full collection that left `old_used_after` bytes in the old
    /// generation: HotSpot resizes the old generation, then the young one
    /// from the old one's new capacity.
    pub fn after_full(&mut self, old_used_after: usize) {
        let capacity_in = self
            .old_capacity
            .max(self.old_used_peak)
            .min(self.old_max.max(self.old_capacity));
        let old = serial_old_compute_new_size(
            capacity_in,
            old_used_after,
            self.old_initial,
            self.old_max,
            &mut self.shrink_factor,
        );
        self.old_capacity = old;
        self.old_used_peak = old_used_after;
        let young =
            serial_young_compute_new_size(old, self.young_initial, self.young_max, self.new_ratio);
        if young > self.young_now {
            self.grows += 1;
        } else if young < self.young_now {
            self.shrinks += 1;
        }
        self.young_now = young;
    }
}



/// gci i1/yg (`gcf-f5c-young-trigger-is-half-of-hotspot-serial-eden-...`):
/// `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK=1` -- default OFF (`runtime_flag_on`),
/// read once when a heap is built from an `-Xmx` budget
/// ([`GenerationalHeap::arm_serial_eden_trigger`]).
///
/// The f5c divergence is WHEN medium-lived objects tenure: an object's age
/// counts this collector's young cycles, and at the same `-Xmx` this
/// collector runs about twice HotSpot Serial's young cycles (its trigger is
/// half a semi-space, HotSpot's is the whole eden), so an object reaches the
/// tenuring threshold after about half the allocation it takes on HotSpot.
/// `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` fixes that by collecting at the
/// eden, which moves the in-place sweep to 90 % of a fuller semi-space and
/// failed its cost rule (vh4-fl B). This switch fixes the AGE instead and
/// leaves the trigger alone: survivors age by one per HotSpot Serial eden of
/// young allocation ([`age_clock_advance`]), so a young cycle that comes
/// before a whole eden has been allocated ages nobody and tenures nobody by
/// age ([`clock_promotion_age`]). The tenuring rule, the threshold and the
/// survivor target (HotSpot's eden, [`GenerationalHeap::tenuring_eden_for`])
/// are unchanged; only the clock they are read against is HotSpot's.
pub fn gen_serial_eden_age_clock_enabled() -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
    true
}

/// gci i3/yg: the reader of `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`
/// (opt-in, `runtime_flag_on`; read once per heap, when the age clock arms,
/// and inert unless `CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK` armed it).
///
/// `gci-i2yg-young-trigger-loop-and-age-clock-review-findings` item 4: the
/// clock charges `young live now - young live after the last collection`, so
/// an allocation that goes straight to the old generation -- an array above
/// `is_humongous`'s threshold, or the young-exhausted spill of
/// `try_alloc_array_humongous` / `try_alloc_object_old` -- never ticks it,
/// while HotSpot Serial allocates anything below its young capacity in eden
/// (`should_try_older_generation_allocation`) and counts it toward its next
/// young collection. With the switch every such allocation smaller than the
/// clock's eden is charged as eden allocation at the next collection.
pub fn gen_serial_eden_age_clock_old_direct_enabled() -> bool {
    // WARNING gci-user-flip-20261013: default flipped ON by the user's decision, pending apps triage.
    true
}

/// gci i1/yg: the HotSpot age clock of one heap
/// ([`gen_serial_eden_age_clock_enabled`]). Held in a mutex taken twice per
/// collection (before and after it, by `collect_garbage_inner`) and once by
/// the young sweep; never on an allocation path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AgeClock {
    /// Young live bytes (`used - free list`) right after the last collection.
    pub exit_live: usize,
    /// Young allocation since the last tick, always below one eden after a
    /// collection ([`age_clock_advance`]).
    pub credit: usize,
    /// The age step of the collection in progress (or the last one): how many
    /// HotSpot young collections this one stands for. 1 until the first
    /// collection, so a cycle the clock did not see ages as before.
    pub step: u8,
    /// `age_clock_begin`'s inputs, to give a REFUSED cycle its allocation
    /// back without consuming a tick.
    pub entry_live: usize,
    pub credit_before: usize,
    /// Collections that aged their survivors (step >= 1) / that did not.
    pub ticks: u64,
    pub holds: u64,
    /// gci i3/yg (`CRATONVM_GEN_SERIAL_EDEN_AGE_CLOCK_OLD_DIRECT`): the
    /// old-direct bytes `age_clock_begin` charged to the collection in
    /// progress (refunded with the rest on a refused cycle), and their total
    /// over the heap's life. Both 0 with the switch off.
    pub entry_old_direct: usize,
    pub old_direct_total: u64,
}

impl Default for AgeClock {
    fn default() -> Self {
        Self {
            exit_live: 0,
            credit: 0,
            step: 1,
            entry_live: 0,
            credit_before: 0,
            ticks: 0,
            holds: 0,
            entry_old_direct: 0,
            old_direct_total: 0,
        }
    }
}

/// gci i1/yg: advance the age clock by `alloc` bytes of young allocation
/// against a HotSpot eden of `eden` bytes. Returns the new credit (below one
/// eden) and the step: the number of whole edens the credit now holds, i.e.
/// how many young collections HotSpot Serial would have run, at most
/// [`cratonvm_types::MAX_GC_AGE`] (an age saturates there anyway). `eden == 0`
/// is the unarmed clock: step 1 every cycle, no credit. Pure.
pub fn age_clock_advance(credit: usize, alloc: usize, eden: usize) -> (usize, u8) {
    if eden == 0 {
        return (0, 1);
    }
    let total = credit.saturating_add(alloc);
    let edens = total / eden;
    let step = edens.min(usize::from(cratonvm_types::MAX_GC_AGE)) as u8;
    (total % eden, step)
}

/// gci i1/yg: `should_tenure`'s `promotion_age` for a cycle whose survivors
/// age by `step` instead of 1. `should_tenure` asks `age + 1 >= promotion_age`;
/// a survivor that ages by `step` reaches the threshold when
/// `age + step >= promotion_age`, i.e. `age + 1 >= promotion_age - (step - 1)`
/// (never below 1, which tenures every survivor). Step 0 -- a cycle that is
/// not a HotSpot young collection -- tenures nobody by age (`u8::MAX`: an age
/// saturates at `MAX_GC_AGE`, 15); a forced promotion
/// (`force_promote_all`) still does. Step 1 is the identity. Pure.
pub fn clock_promotion_age(promotion_age: u8, step: u8) -> u8 {
    match step {
        0 => u8::MAX,
        1 => promotion_age,
        // gci i27/gs (`gci-i23tn-...` item 1): `MaxTenuringThreshold=16`
        // (HotSpot's `NeverTenure`, `promotion_age` 17 since TN-H1) never
        // tenures by age: HotSpot's age saturates at `MAX_GC_AGE` (15) before
        // it reaches 16, however many collections a step stands for. The
        // subtraction below would bring 17 down to an age a survivor CAN
        // carry (step 2: tenure at 15).
        _ if promotion_age > cratonvm_types::MAX_GC_AGE + 1 => u8::MAX,
        s => promotion_age.saturating_sub(s - 1).max(1),
    }
}

/// gc wave 9 (lane gcC, `gcf-f5c-...`): `CRATONVM_GEN_AGE_BY_SERIAL_EDEN` --
/// DEFAULT ON since the w9c host gate (`=0` kills: C1 conformance 3/3 + 3/3,
/// C2 cost rule, C3 soundness on 8x repeats), read once when a heap is built
/// from an `-Xmx` budget ([`GenerationalHeap::arm_serial_eden_trigger`]). A young survivor
/// ages once per HotSpot-Serial-eden of young allocation instead of once per
/// young cycle: the trigger (and so the cycle count and each cycle's cost) is
/// unchanged, but an object tenures after the allocation volume HotSpot
/// Serial would have tenured it after. f5c's divergence is that the default
/// trigger (50 % of a `-Xmx / 4` semi-space) runs about twice HotSpot's young
/// collections per byte, so a medium-lived object reaches the tenuring
/// threshold in half the allocation and dies old (`OldGenFinalizeProbe`
/// 0/64 against HotSpot's 64/64); the Serial-shaped trigger that fixes it
/// (`CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN`) failed its cost rule (gch
/// h4/fl, vh4-fl B), because a fuller semi-space sweeps slower.
pub fn gen_age_by_serial_eden_enabled() -> bool {
    true
}

/// gc wave 9 (lane gcC): the `promotion_age` a young cycle that does NOT age
/// its survivors runs with ([`GenerationalHeap::cycle_promotion_age`]). No
/// survivor can reach it (`MAX_GC_AGE` is far below it), so on such a cycle
/// only a forced tenure (`force_promote_all`: promote-on-pressure, the defrag
/// escalation) promotes, and [`cycle_age_step`] keeps every age where it is.
/// Only `CRATONVM_GEN_AGE_BY_SERIAL_EDEN` ever produces it.
pub const NON_AGING_PROMOTION_AGE: u8 = u8::MAX;

/// gc wave 9 (lane gcC): the age a survivor of a cycle run with
/// `promotion_age` gains: 0 on a non-aging cycle
/// ([`NON_AGING_PROMOTION_AGE`]), else 1. Every young aging site (the sweep's
/// survivor words and deferred bumps, the serial and parallel copies) adds
/// this instead of a literal 1, so the two cannot disagree. Pure.
#[inline]
pub fn cycle_age_step(promotion_age: u8) -> u8 {
    u8::from(promotion_age != NON_AGING_PROMOTION_AGE)
}

/// merge-dev-20261015: the age step of a young cycle run with `promotion_age`
/// (gcC's [`GenerationalHeap::cycle_promotion_age`]) under the gci age clock's
/// `clock_step` ([`age_clock_advance`]): 0 on a cycle gcC's epoch holds, else
/// the clock's step. At most one of the two is armed on a heap
/// (`arm_serial_eden_trigger`), so this is `clock_step` on a clock heap,
/// [`cycle_age_step`] on an epoch heap and 1 on an unarmed one; every young
/// aging site and `clock_promotion_age` read this one value. Pure.
#[inline]
pub fn merged_age_step(promotion_age: u8, clock_step: u8) -> u8 {
    if cycle_age_step(promotion_age) == 0 {
        0
    } else {
        clock_step
    }
}

/// gc wave 9 (lane gcC): one young cycle's aging verdict under
/// `CRATONVM_GEN_AGE_BY_SERIAL_EDEN`. `credit` is the young allocation carried
/// since the last aging cycle, `allocated` what was allocated in young since
/// the previous cycle ended, `eden` HotSpot Serial's eden. The cycle ages when
/// the two together reach an eden (HotSpot ages a survivor once per eden-full
/// of allocation), and the remainder carries to the next cycle so the RATE is
/// exactly one age per eden; otherwise everything carries. Returns
/// `(ages, credit after this cycle)`. An `eden` of 0 always ages (unarmed).
/// Pure.
pub fn serial_eden_age_epoch(credit: usize, allocated: usize, eden: usize) -> (bool, usize) {
    if eden == 0 {
        return (true, 0);
    }
    let total = credit.saturating_add(allocated);
    if total >= eden {
        (true, total - eden)
    } else {
        (false, total)
    }
}


/// Will the NEXT ordinary young collection certainly take the non-moving
/// sweep? Only used to size the collection trigger (the moving cycle needs
/// Cheney copy headroom; the non-moving sweep does not).
///
/// arch-2026-07-26 (`moving-young-precise-roots`): the former
/// `allow_moving_young` parameter — fed by `CRATONVM_ALLOW_MOVING_YOUNG`, a
/// *second* opt-in that had to be set IN ADDITION to the moving-young gate —
/// is gone, along with the flag itself. Its practical effect was that
/// `CRATONVM_MOVING_YOUNG=1` alone never ran a moving cycle under a live JIT
/// frame, which is the only case the feature exists for. A live JIT frame now
/// forces the non-moving sweep exactly when moving-young is not in effect;
/// when it IS in effect, the per-cycle coverage proof decides, not a flag.
///
/// # Why `moving_young_requested` is not the whole answer
///
/// Requesting moving-young does not make the next cycle moving. While
/// [`cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT`] is `false`,
/// `conservative_roots::refresh_moving_young_coverage_for_current_thread`
/// vetoes moving-young for the whole process as soon as any compiled code
/// exists, so a cycle taken with a live JIT frame — which is exactly what the
/// `jit_active || unregistered || jit_allocation_frame` term above requires —
/// is **guaranteed** to divert to the non-moving sweep. Saying otherwise costs
/// real throughput and buys nothing:
///
/// the answer here picks the young-GC *trigger*. `false` selects the Cheney
/// threshold, which must reserve the unused semi-space as worst-case survivor
/// headroom; `true` selects `NON_MOVING_YOUNG_GC_THRESHOLD_PERCENT` (90%),
/// because an in-place sweep needs no headroom. So on a default JIT run the
/// heap was collecting at the copying-collector's early threshold and then
/// diverting every one of those collections to the in-place sweep — roughly
/// double the collections, each a whole-heap sweep, for a copy that never
/// happened. Allocation-heavy workloads feel it directly: it is what kept
/// `type.temporal` over its cap (>1500 s vs 777 s under
/// `CRATONVM_NO_MOVING_YOUNG=1`) after four codegen suspects had been
/// eliminated by bisection.
///
/// Fail-safe direction: this must only report `true` when the sweep is
/// *certain*, since a moving cycle taken against the 90% trigger could exhaust
/// its copy reserve. The veto provides that certainty, and both halves read the
/// same constant, so they cannot disagree.
///
/// # gengc-round1-core (2026-09-20): the certainty argument above is STALE
///
/// The "veto" it rests on was the process-wide one in
/// `conservative_roots::refresh_moving_young_coverage_for_current_thread` that
/// fired as soon as compiled code merely existed. That blanket became the
/// opt-in `CRATONVM_MOVING_YOUNG_NO_JIT` (see
/// `types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT`'s own parenthetical, and
/// `docs/GC.md`: "the older 'any JIT frame ⇒ non-moving' rule is reachable
/// today only under `CRATONVM_NO_MOVING_YOUNG` or
/// `CRATONVM_MOVING_YOUNG_NO_JIT=1`"), leaving the per-cycle proof as the
/// default authority. So this predicate no longer *proves* anything: it is a
/// heuristic that happens to agree with `collect_garbage_inner`'s
/// `divert_non_moving` most of the time, and
/// [`GenerationalHeap::adapt_young_trigger_to_pause`]'s own doc records a
/// measured run where it did not ("the prediction is not even right: those
/// cycles run the MOVING path").
///
/// What keeps the fail-safe property today is NOT this function. It is
/// `collect_garbage_inner`'s Cheney invariant: the semispaces are equalised
/// (`young_to.grow(young_from.capacity())` when to-space is empty and smaller)
/// and the cycle is REFUSED outright when `to_headroom < young_from.used()`.
/// With `to.capacity() >= from.capacity() >= from.used()` and promotion only
/// ever removing demand, a moving cycle taken at the 90 % trigger still cannot
/// run out of destination — so a wrong prediction costs pause time, not
/// correctness.
///
/// # gengc-round2-core2 (2026-09-20): the PREDICTION is right again, for a
/// # different reason, and the behaviour is deliberately left alone
///
/// The round-1 note above is correct that the cited veto is gone. It stops one
/// step short of the answer: a second mechanism re-established the same
/// implication in the meantime. `divert_non_moving`'s term 4,
/// `unrewritable_conservative_jit_roots` (2026-09-06, added for the QDox
/// SIGSEGV), is
///
/// ```text
/// moving_young && !CRATONVM_GC_NO_PEER_PIN_DIVERT && has_conservative_roots
///     && gc_quiescence::conservative_jit_scans() > 0
/// ```
///
/// and its own comment records that the scan count "is positive whenever
/// anything is compiled", because every mutator deposits on its way into the
/// pause. So "a live compiled frame ⇒ the cycle takes the non-moving sweep" is
/// true again on the shipped default — via term 4 when moving-young is on, and
/// via term 1 when it is off. (Round 3, 2026-09-21: term 1 lost its
/// `has_conservative_roots` conjunct and is now plain `!moving_young`, so with
/// the gate off the sweep is certain whether or not anything is compiled. That
/// makes the implication this paragraph argues for STRONGER, not weaker.)
/// Which is exactly what this function computes.
///
/// The measured counter-example quoted in
/// [`GenerationalHeap::adapt_young_trigger_to_pause`] (`non_moving=true` on
/// cycles that reported `cheney_drain`) predates term 4; it is evidence about
/// the collector as it was, not as it is.
///
/// Where the prediction can still be wrong, and it is a short list:
/// `CRATONVM_GC_NO_PEER_PIN_DIVERT=1` and `CRATONVM_GC_PRECISE_ONLY_ROOTS=1`,
/// the two documented ways to disarm term 4. Both are diagnostic A/B switches,
/// and on both the cost of the wrong prediction is pause SHAPE, bounded by the
/// equalisation argument two paragraphs up — never a failed relocation.
///
/// Nothing here is changed, and that is a decision rather than an omission.
/// The obvious tightening (make the third clause `!moving_young_requested`, so
/// the prediction stops claiming certainty under moving-young) would move the
/// default JIT-warm trigger from 90 % to 50 % — halving it against a live set
/// the nursery cannot shrink, which is the regression shape this file records
/// twice: `with_capacity`'s note and `binarytrees-bt18-half-gap-20260730.md`
/// for large `-Xmx`, and the >1500 s vs 777 s `type.temporal` result above.
/// Making that trade needs the A/B this lane cannot run. See
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// # gengc-round3 (2026-09-21): a DIFFERENT tightening is now provable, and is
/// # also not taken here
///
/// The opt-out repair widened `divert_non_moving`'s term 1 to plain
/// `!moving_young`, so `!moving_young_requested` now IMPLIES the non-moving
/// sweep with certainty — no JIT frame required, and `force_moving` is already
/// this function's first clause. The provable form is therefore a DISJUNCT, not
/// a tightening of the third clause:
///
/// ```text
/// !force_moving
///     && (!moving_young_requested
///         || ((jit_active || unregistered_jit_frame || jit_allocation_frame)
///             && !cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT))
/// ```
///
/// It cannot touch the default path (`moving_young_requested == true`) at all;
/// it only stops a `CRATONVM_NO_MOVING_YOUNG` run with nothing compiled from
/// collecting at the Cheney 50 % trigger for a copy that can no longer happen —
/// roughly double the collections, each a whole-heap sweep, which is the exact
/// pathology the `type.temporal` paragraph above measures.
///
/// **Not taken here, on purpose.** `young_gc_trigger_bytes`'s own test asserts
/// `!next_young_gc_is_guaranteed_non_moving(false, false, false, false, false)`
/// ("JIT-quiescent collection is moving by default") — an assertion that was
/// TRUE of the collector until the opt-out repair, and that this disjunct
/// inverts. Moving it is a trigger change with a measurable cost on a lane
/// nobody in this round can A/B, and the fail-safe direction is unaffected
/// either way (predicting `false` only costs collection count, never
/// relocation headroom). Whoever takes it must move that assertion in the same
/// commit and say which lane priced it.
///
/// # gen r4w2/youngpolicy (2026-09-23): the round-3 disjunct is TAKEN
///
/// Applied exactly as written above, and the test assertion moved with it
/// (`young_gc_trigger_preserves_moving_headroom_but_fills_non_moving_space`
/// now asserts `true` for the JIT-quiescent opt-out case). The default path is
/// untouched by construction — every change is behind
/// `moving_young_requested == false`, i.e. `CRATONVM_NO_MOVING_YOUNG`, where
/// TERM 0 of `divert_non_moving` makes the in-place sweep certain whatever is
/// compiled. The fail-safe direction holds: `true` is returned only when the
/// sweep is certain (`force_moving`, the one override of TERM 0, is still the
/// first clause). What it buys: an opt-out run that has not tiered up — the
/// shape every bisection repro has — stops collecting at the Cheney 50 %
/// trigger for a copy that cannot happen, so sweep measurements taken under
/// the opt-out stop over-counting collections by about 2x against a JIT-warm
/// production run. Priced by the youngpolicy lane's review
/// (`docs/feature-designs/generative-gc-architecture.md`, probe P3).
#[inline]
pub const fn next_young_gc_is_guaranteed_non_moving(
    jit_active: bool,
    unregistered_jit_frame: bool,
    jit_allocation_frame: bool,
    moving_young_requested: bool,
    force_moving: bool,
) -> bool {
    !force_moving
        && (!moving_young_requested
            || ((jit_active || unregistered_jit_frame || jit_allocation_frame)
                && !cratonvm_types::flags::JIT_PUBLISHES_RELOCATION_CONTRACT))
}

/// "Humongous" object threshold as a percentage of the young semi-space
/// capacity.  Allocations whose total in-memory footprint
/// (`HEADER_SIZE + array_data_size`) exceeds this fraction of one young
/// semi-space are routed directly to the old generation, bypassing
/// the young from-space entirely (G1-style humongous handling).
///
/// Without this routing, a single large array of size `A` would force
/// the user to size `--Xmx` to at least `4 * A` just so the array can
/// fit in *one* young semi-space (each semi is `Xmx / 4` —
/// see [`with_capacity`]).  A 2 GiB int[] at `--Xmx 16g` (4 GiB
/// semi) fits; at `--Xmx 12g` (3 GiB semi) it would not, even though
/// the heap has 12 GiB free.  HotSpot avoids the same trap by
/// sending oversized arrays straight to old gen / a humongous region.
///
/// Threshold chosen at 50%: large enough that small surviving sets
/// in young can still copy without colliding with a humongous tail
/// (Cheney needs to be able to fit copies into to-space), small enough
/// that any single array bigger than half the young semi-space takes
/// the humongous path instead of guaranteeing a copying-collector OOM.
pub const HUMONGOUS_YOUNG_FRACTION_PERCENT: usize = 50;

/// Absolute ceiling on the humongous threshold, independent of heap size.
///
/// The `HUMONGOUS_YOUNG_FRACTION_PERCENT`-of-young-semi rule scales the
/// threshold with `--Xmx` (young semi = `Xmx / 4`), so on a *large* heap a
/// merely-large array stays below it and lands in young — where the Cheney
/// collector must **copy** it into to-space on every minor GC until it ages
/// out (`PROMOTION_AGE` cycles). A handful of ~1 GiB arrays then either
/// overflow to-space (hard OOM in the relocation path) or, when old gen is
/// also full and humongous routing falls back to young, get corrupted under
/// pressure. Concretely a 1 GiB array is only "humongous" when
/// `Xmx <= 8g` (semi/2 < 1 GiB); at `--Xmx 16g` it is young and gets copied.
/// (Observed: `TestEncryptInterceptorLargeHeap` needs `--Xmx 16g` to pass,
/// AES-GCM `AuthenticationFailed` / relocation-OOM below that.)
///
/// Capping the threshold means any array larger than this ALWAYS routes
/// directly to old gen and is never young-copied, mirroring HotSpot G1's
/// humongous handling (large objects bypass the copying young generation).
/// 256 MiB is well above the default-heap young-semi fraction
/// (256 MiB heap → 64 MiB semi → 32 MiB threshold), so small/default heaps
/// are unaffected; it only changes where genuinely large (>256 MiB) arrays
/// live on multi-GiB heaps, which is exactly the case the fraction rule
/// mishandles.
pub const HUMONGOUS_ABSOLUTE_CAP_BYTES: usize = 256 * 1024 * 1024;

/// Maximum allowed heap expansion factor (4x the initial size).
///
/// This is only ONE of the two ceilings on young growth, and on the
/// production path it is not the binding one: heaps built from `-Xmx` via
/// [`GenerationalHeap::with_capacity`] are additionally clamped so that
/// `2 * young_semi + old_gen <= Xmx` (see `max_heap_bytes`), which under that
/// constructor's 50/50 split lands exactly on the initial semi. This factor
/// still governs the explicit-size constructors ([`GenerationalHeap::new`],
/// [`GenerationalHeap::with_sizes`]), which take absolute arena sizes and
/// therefore have no budget to divide.
pub const MAX_HEAP_EXPANSION_FACTOR: usize = 4;

/// If GC reclaims less than this fraction of young gen, expand the heap.
pub const GC_EXPANSION_THRESHOLD_PERCENT: usize = 25;

/// If GC reclaims less than this fraction of young gen, arm the
/// "promote-on-pressure" flag so the NEXT minor GC promotes ALL
/// survivors to old gen regardless of age. Mirrors HotSpot's
/// "premature promotion" / "always tenure" behaviour for high-survival
/// cycles: when the survival rate is this high (>75%) the working set
/// is effectively long-lived, and another semi→semi copy would just
/// repeat the same problem on the next cycle.
pub const GC_PROMOTE_PRESSURE_PERCENT: usize = 25;

/// Whether a young cycle that found `occupied` bytes in an arena of
/// `capacity` and left `survived` of them young arms promote-on-pressure for
/// the next cycle: it freed under [`GC_PROMOTE_PRESSURE_PERCENT`] of an arena
/// at least half full. The one rule for both collectors, the moving copy and
/// the non-moving sweep (`gen-gcd1-pinned-callee-oome-probe-crawls-unaged-young-sweeps`:
/// a JIT-warm program whose young cycles were all sweeps never armed, and
/// kept a full young generation young for thousands of sweeps). Pure.
pub fn promote_pressure_verdict(occupied: usize, survived: usize, capacity: usize) -> bool {
    let freed_percent = if occupied > 0 {
        occupied.saturating_sub(survived) * 100 / occupied
    } else {
        100
    };
    freed_percent < GC_PROMOTE_PRESSURE_PERCENT && occupied >= capacity / 2
}

// ---------------------------------------------------------------------------
// GenerationalHeap
// ---------------------------------------------------------------------------

/// Statistics for the generational heap.  All counters are
/// monotonically non-decreasing; use [`HeapStats::snapshot`] to capture a
/// consistent view at one point in time.  This is Phase H (RH.1) — a
/// hardening-only addition so tests can assert that allocation pressure
/// does not corrupt promotion bookkeeping (every object is accounted
/// for exactly once).
/// DBG: count of corrupt headers the non-moving sweep has detected. The VM's
/// `maybe_gc` reads this and, under `CRATONVM_DBG_CORRUPT_FRAMES`, dumps the
/// mutator's Java stack the first time it increases — close to the corruptor
/// when run with a tiny young gen (frequent GC).
pub static SWEEP_CORRUPTION_HITS: AtomicU64 = AtomicU64::new(0);

/// A2 forensic probe (CRATONVM_DBG_A2) — limits the per-run detail dump to the
/// first few corruption hits so the log is not flooded.
pub static A2_PROBE_HITS: AtomicU64 = AtomicU64::new(0);

/// Count of OVERLAPPING free blocks merged by the post-sweep coalescer (off <
/// previous block end). A non-zero count proves the free list was handing out —
/// or about to hand out — the same young region twice (the A2 object-overlap /
/// walk-desync root). Exposed for the gated diagnostic + tests.
pub static A2_FL_OVERLAP_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening (2026-07-02) — count of times a linear young
/// from-space walk OVERSHOT into a known free block (`cursor > off`). Each hit
/// is proof the walk grid and the free list disagreed upstream (an unlisted
/// zeroed span or a mis-sized header) — the seed of the phantom-stride /
/// mid-live-object free corruption family. Kept as a plain counter so a
/// repro run can grep it without a debug gate.
pub static SWEEP_WALK_OVERSHOOT_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening — count of all-zero spans (word0 == 0 at a
/// walk-grid offset, at least `HEADER_SIZE` of zero bytes) encountered by a
/// young walk OUTSIDE the free list. These are freed-then-reused-then-
/// clobbered slots or freed-but-unlisted residue; they are SKIPPED (never
/// re-freed, never zeroed — the span may be a live allocation whose header a
/// stale register-held reference clobbered, so re-freeing would double-serve
/// live memory).
pub static SWEEP_ZERO_SPAN_HITS: AtomicU64 = AtomicU64::new(0);

/// Zero runs a young walk recognised as a run of EMPTY objects and stepped
/// over on-grid, instead of treating as a desync — see
/// [`zero_run_empty_object_resume`]. Summed across the three walks that ask:
/// the sequential sweep, the parallel [`sweep_chunk`], and
/// [`clear_all_mark_bits_in_arena`]. Deliberately a SEPARATE counter from
/// [`SWEEP_ZERO_SPAN_HITS`], which now counts only the runs that still take the
/// unwind: this shape is normal and frequent (147 per young cycle on the
/// hibernate repro), so folding the two together would turn its
/// arrival into apparent corruption. Like its siblings above it has no printer
/// — it is read from a debugger or an instrumented repro build.
pub static SWEEP_ZERO_SPAN_EMPTY_RUNS: AtomicU64 = AtomicU64::new(0);

/// Objects the young sequential walk sized whose header does NOT carry
/// [`cratonvm_types::GC_FLAG_HEADER`].
///
/// The flag is set by `ObjectHeader::new` and by both JIT inline-allocation
/// emitters, and is never cleared, so on a sound VM this is exactly zero. A
/// non-zero value names an allocation path that publishes a header without it
/// — which is not itself a defect today (nothing reads the flag on a path
/// where absence costs anything), but IS the thing that must be zero before
/// anything is allowed to. See `GC_FLAG_HEADER`'s "what must NOT be built on
/// it" and `header_reserved_fields_plausible`.
///
/// Printed unconditionally in the young-sweep exit census, for the H2-CID0
/// reason: a counter printed only when non-zero cannot distinguish "clean"
/// from "the walk never ran", and here that is the whole question.
pub static SWEEP_NO_HEADER_FLAG: AtomicU64 = AtomicU64::new(0);

/// Bytes in the runs [`SWEEP_ZERO_SPAN_EMPTY_RUNS`] counts, accumulated for the
/// CURRENT cycle by whichever walkers covered the arena (the sequential walk
/// and the parallel chunks partition it, so both add).
pub static EMPTY_RUN_BYTES_CYCLE: AtomicU64 = AtomicU64::new(0);

/// [`EMPTY_RUN_BYTES_CYCLE`] as of the end of the last completed sweep — the
/// standing retention, because every cycle re-skips the same runs. A
/// cumulative total would just multiply this by the cycle count.
pub static EMPTY_RUN_BYTES_LAST: AtomicU64 = AtomicU64::new(0);

/// Young `used` at the end of the last sweep, so the figure above can be read
/// as a fraction without a second run.
pub static EMPTY_RUN_YOUNG_USED_LAST: AtomicU64 = AtomicU64::new(0);

/// Sweeps whose standing empty-object-run retention exceeded
/// [`EMPTY_RUN_RETENTION_BUDGET_PERMILLE`] of the young generation.
///
/// An accepted empty-object run is stepped OVER, not parsed, so each dead empty
/// object in it stays until a moving cycle resets from-space -- and under a
/// permanent non-moving sweep there is no moving cycle. The obvious worry is
/// that this accumulates. Measured across seven heap sizes and 4..27 young
/// collections it does NOT: the figure is flat at 11.5-29 KB with no trend in
/// cycle count, a ceiling of 0.045% of the young generation, because every
/// cycle re-skips the SAME runs rather than adding new ones.
///
/// The decision that followed is to keep stepping over rather than reclaim:
/// pushing the run as a dead region would be the first change in this family
/// to FREE something the previous code retained, and trading that property for
/// <=0.045% of young is a bad trade. That decision was correct on the numbers
/// and would have stayed a remembered opinion.
///
/// This counter is what makes it a RATCHET instead. The mechanism's own
/// prediction -- "re-skipping the same runs, so no growth with cycle count" --
/// is now checked by the binary on every sweep, and a workload that ever
/// contradicts it says so in the log rather than waiting to be re-measured by
/// hand. Non-zero is new evidence and reopens the trade.
pub static EMPTY_RUN_RETENTION_EXCEEDED: AtomicU64 = AtomicU64::new(0);

/// Bounded report counter for [`EMPTY_RUN_RETENTION_EXCEEDED`].
pub static EMPTY_RUN_RETENTION_REPORTS: AtomicU64 = AtomicU64::new(0);

/// Retention budget for stepped-over empty-object runs, in PERMILLE of the
/// young generation's `used` bytes.
///
/// 10 permille = 1%, against a measured ceiling of 0.45 permille (0.045%) at
/// the smallest heap tested. The ~22x headroom is deliberate: this is a
/// trend detector, not a tuning knob, and it must not fire on the ordinary
/// scatter that six heap sizes already showed.
pub const EMPTY_RUN_RETENTION_BUDGET_PERMILLE: u64 = 10;

/// Does this sweep's standing retention exceed the budget?
///
/// A tiny young generation makes any fraction noisy, so a floor of 64 KiB
/// applies: below that the absolute figure is smaller than one TLAB and no
/// growth trend could be read out of it anyway. Split out from the sweep so
/// the threshold itself is unit-testable.
pub fn empty_run_retention_exceeded(retained: u64, young_used: u64) -> bool {
    const FLOOR_BYTES: u64 = 64 * 1024;
    if retained < FLOOR_BYTES {
        return false;
    }
    retained.saturating_mul(1000) > young_used.saturating_mul(EMPTY_RUN_RETENTION_BUDGET_PERMILLE)
}

/// `CRATONVM_DBG_MARK_WHY_CLASS` straddle reports emitted — the sweep walk
/// striding OVER the watched base, inside some earlier object's computed
/// extent. Bounds the log: a desynced grid can straddle one watched address on
/// many consecutive objects.
pub static MARKWHY_STRADDLE_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead walk-desync hardening — count of forwarded young objects whose
/// forwarding target failed validation (not inside old gen). Such a header is
/// a phantom write from a desynced walk or corruption; the span is retained
/// instead of zeroed+freed.
pub static SWEEP_BAD_FORWARD_HITS: AtomicU64 = AtomicU64::new(0);

/// DoHead comb-7 fix (2026-07-03) — count of mark-phase candidates rejected
/// because the header's claimed EXTENT (`gen_object_total_size`) does not fit
/// inside its generation at that address. The per-field plausibility gate
/// admits `array_length` up to i32::MAX, and the mark BFS scan iterates that
/// count with the header as its only bound — a corrupt header (e.g. packed
/// pointer bytes misparsed as `kind=Array, array_length=<pointer low 32>`) ran
/// the scan tens of MB off the mapped arena (observed SIGSEGV at the region
/// boundary). A real object's extent always fits inside the arena it was
/// allocated from, so out-of-extent headers are rejected without marking.
pub static SWEEP_BAD_EXTENT_HITS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-01) — non-moving young collections whose ANCHOR ORACLE left
/// at least one conservative root candidate unresolved, so the late
/// grid-resolution pass had to run.
///
/// The oracle maps a conservative candidate to the BASE of the object that
/// contains it. It works interval by interval and discards an interval whose
/// header chain does not land exactly on its upper anchor; every candidate in a
/// discarded interval then fell back to being side-marked at its RAW address.
/// The sweep matches side marks against object STARTS, so that fallback retains
/// NOTHING for an interior candidate and the containing object is reclaimed —
/// while a conservative root still points into it. That is a premature
/// reclamation, and it is silent: the object's span is zeroed, so the next read
/// through the stale reference sees an all-zero header, i.e. `ClassId(0)` /
/// `java.lang.Object`. See old-sweep-liveness.md section 7 for the
/// old-generation twin of this, which was the H2 `MVStore` cache defect.
///
/// Non-zero here means the oracle's interval verification failed on a live
/// heap; the accompanying `LATE_RESOLVE_BASES` is how many objects were saved.
pub static LATE_RESOLVE_CYCLES: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — object bases recovered by the late grid-resolution pass. Each one
/// is an object the sweep would otherwise have reclaimed with a conservative
/// root pointing into it.
pub static LATE_RESOLVE_BASES: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — candidates the late pass could not resolve either (the grid walk
/// desynced before reaching them). These are handed to the sweep as spans it
/// must not reclaim; over-retention, the safe direction.
pub static LATE_RESOLVE_PINS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — bounded report counter for the late-resolution warning.
pub static LATE_RESOLVE_REPORTS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — objects the sweep retained ONLY because an unresolvable
/// conservative candidate pointed inside them. Non-zero means the grid walk
/// could not reach the candidate at all, so this is the last line of defence.
pub static LATE_PIN_RETENTIONS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — sweep anchors that the SEQUENTIAL walk proved are NOT object
/// bases.
///
/// The parallel sweep prefix splits the from-space at these offsets and walks
/// each interval independently. Its whole soundness argument is "a bad anchor
/// cannot corrupt, because each chunk re-proves its anchor by chaining onto the
/// next one" — but that re-proof is probabilistic, not a proof: a walk that
/// starts off-grid can mis-parse a run of objects and still land exactly on the
/// upper anchor, and every object subsumed by a phantom header is then
/// classified by the phantom's mark bit. An unmarked phantom spanning live
/// objects reclaims and ZEROES them, which is precisely the all-zero-header
/// (`ClassId(0)` / `java.lang.Object`) face this counter was added to explain.
///
/// Counted only on cycles where the sequential walk covered the whole arena
/// (i.e. the parallel prefix did not run), because only then does the walk
/// visit every base. Zero means every anchor really is an object base and the
/// re-proof never has to save anything.
pub static SWEEP_ANCHOR_NOT_A_BASE: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — cycles on which the parallel sweep prefix was attempted, and of
/// those, how many it completed. A large abort count means chunks routinely
/// disagree with the grid, which is the same evidence from the other side.
///
/// # gengc-round2-core2 (2026-09-20) — READ THIS WITH ITS DENOMINATOR
///
/// `par_attempts=0` does NOT mean the engagement predicate is unreachable or
/// inert. This counter lives inside
/// [`GenerationalHeap::sweep_young_non_moving`], so its denominator is
/// "non-moving young cycles", which is
/// [`selective_promotion_census`]'s first field (`sp_sweeps=` on the shutdown
/// line) — **not** the number of collections. A run that took the MOVING path
/// on every cycle never reaches this code at all, and the `young_sweep:` line
/// will read `par_attempts=0 par_accepts=0` while the collector worked
/// perfectly.
///
/// That is exactly what the 2026-09-20 `BinT 14` soak measured: ~1500
/// collections, `moving_cycles_total=1510 non_moving_cycles_total=2`, so this
/// sweep ran twice and the prefix predicate was evaluated at most twice.
///
/// On those two it correctly DECLINED, for a second reason worth knowing when
/// designing a stress recipe: the predicate requires `sweep_anchors.len() > 2`
/// (more than one chunk — with two anchors there is exactly one chunk and no
/// parallelism to be had), and [`sweep_anchor_stride`] defaults to **8 MiB**
/// while `CRATONVM_DBG_GC_STRESS=250000` collects a ~250 KB nursery. The
/// subsample then yields `[0, used]` and nothing else. `sweep_anchor_stride`'s
/// own doc says so: "Lowering it is how the GC-stress matrix drives the
/// parallel sweep on a small young gen, where the 8 MiB default would produce
/// a single chunk and silently never exercise it."
///
/// So to exercise this path a soak needs BOTH
/// `CRATONVM_GC_SWEEP_ANCHOR_STRIDE` lowered (64 KiB or less at these sizes)
/// and a configuration that actually takes the non-moving sweep
/// (`CRATONVM_NO_MOVING_YOUNG=1`, or a JIT-warm workload). Neither was set.
pub static PAR_SWEEP_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0 — companion to [`PAR_SWEEP_ATTEMPTS`]: attempts that were accepted.
pub static PAR_SWEEP_ACCEPTS: AtomicU64 = AtomicU64::new(0);

/// Which check abandoned a parallel sweep chunk, by reason.
///
/// [`PAR_SWEEP_ACCEPTS`] is a per-CYCLE verdict and one `None` from any chunk
/// discards the whole attempt, so a shortfall there says only "something
/// disagreed with the grid somewhere". The seven reasons mean entirely
/// different things — a hole-crossing extent is a free-list/grid disagreement,
/// a phantom extent is the corruption family, and a chain that misses its
/// anchor is an anchor that was never an object start — and picking the wrong
/// one to chase costs a whole investigation.
///
/// Index: `0` free-block overshoot, `1` malformed GAP filler, `2` unlisted zero
/// span (the ones [`zero_run_empty_object_resume`] refuses), `3` implausible
/// header size, `4` extent crosses a free hole, `5` phantom extent (subsumes a
/// live object), `6` the chain did not land exactly on the next anchor.
pub static PAR_CHUNK_BAILS: [AtomicU64; 7] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Count a [`PAR_CHUNK_BAILS`] reason and abandon the chunk. Every `return
/// None` in `sweep_chunk` goes through here so a new bail cannot be added
/// without choosing a reason for it.
#[inline]
pub fn chunk_bail(reason: usize) -> Option<SweepChunkResult> {
    PAR_CHUNK_BAILS[reason].fetch_add(1, Ordering::Relaxed);
    None
}

/// Which check refused a chunk of the PARALLEL young object-start walk
/// ([`objstart_chunk`], moving path) — the twin of [`PAR_CHUNK_BAILS`].
///
/// gen r4w2/youngmark (2026-09-23). One refusal abandons the whole parallel
/// attempt and re-walks sequentially from offset 0, and the only record used to
/// be `objstart_parallel=0` beside a non-zero `objstart_chunks=` in the
/// `[gcpause]` counts — a refusal with nothing to chase it with. See
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// Index: `0` the chunk's anchor lies inside a free/TLAB-skip block, `1` a
/// skip block's extent overflows, `2` the cursor landed inside a skip block,
/// `3` malformed GAP filler, `4` implausible object size, `5` an object's
/// extent crosses a skip block, `6` misaligned start (the bitmap refused it),
/// `7` the chain did not land exactly on the next anchor.
pub static OBJSTART_CHUNK_BAILS: [AtomicU64; 8] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Count an [`OBJSTART_CHUNK_BAILS`] reason and refuse the chunk. Every
/// refusal in `objstart_chunk` goes through here, as every `sweep_chunk` bail
/// goes through [`chunk_bail`].
#[inline]
pub fn objstart_bail(reason: usize) -> Option<()> {
    OBJSTART_CHUNK_BAILS[reason].fetch_add(1, Ordering::Relaxed);
    None
}

/// H2-CID0 — bounded report counter for the off-grid-anchor warning.
pub static ANCHOR_OFF_GRID_REPORTS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — reclaim spans the young non-moving sweep refused to free because a
/// LIVE (side-marked, non-forwarded) object sat inside them.
///
/// Non-zero is a walk-grid break: a mis-sized header subsumed its neighbours
/// and the phantom's own disposition condemned them. Before this check the span
/// was zeroed and published, which is the silent premature reclamation behind
/// the all-zero-header (`ClassId(0)` / `java.lang.Object`) family.
pub static LIVE_IN_DEAD_SPANS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — bounded report counter for [`LIVE_IN_DEAD_SPANS`].
pub static LIVE_IN_DEAD_REPORTS: AtomicU64 = AtomicU64::new(0);

/// H2-GCVAR (2026-08-10) — headers whose claimed extent SUBSUMED a live
/// (marked) object, caught DURING the non-moving sweep walk rather than by the
/// end-of-sweep [`LIVE_IN_DEAD_SPANS`] merge.
///
/// The merge is a safety net: it retains the span, but by the time it runs the
/// walk has already strided the phantom and everything after it was decided on
/// a grid that never resynchronised. Catching it at the header lets the walk
/// re-anchor on an allocator-recorded object start and keep reclaiming, which
/// is the difference between "this cycle freed the tail of the arena" and
/// "this cycle freed nothing past the first phantom".
pub static SWEEP_PHANTOM_EXTENTS: AtomicU64 = AtomicU64::new(0);

/// Bounded report counter for [`SWEEP_PHANTOM_EXTENTS`].
pub static SWEEP_PHANTOM_REPORTS: AtomicU64 = AtomicU64::new(0);

/// Marks subsumed by a reported phantom extent that may NOT be object bases.
///
/// [`SWEEP_PHANTOM_EXTENTS`] rests on "`side_sorted` holds object BASES, and
/// live objects never nest". The first half has one documented exception: when
/// no anchor interval resolves a CONSERVATIVE candidate, `mark_young`'s
/// fallback side-marks the RAW candidate address, which is a base only if the
/// candidate happened to be one. A conservative candidate is frequently an
/// object-INTERIOR word (a field address, a derived pointer, a spilled register
/// mid-object), and the late-resolution pass that marks the real base cannot
/// unmark the raw address — so `side_sorted` can carry both.
///
/// An interior mark of a perfectly VALID object satisfies the phantom check's
/// premise exactly, which would make the verdict a false positive that unwinds
/// every reclamation since the last anchor: a throughput cost, not a
/// correctness one (`live_in_dead` stayed 0 throughout the 2026-08-13 finding).
///
/// This counter does not change the verdict — it says whether that explanation
/// is the right one. `unresolved_snapshot` is exactly the set of marks that may
/// not be bases; everything else came from a precise ref-slot value (a base by
/// construction) or from `resolve_candidate_bases`. Read it beside
/// `phantom_extents`: equal counts say the guard is firing on interior marks,
/// zero says the subsumed marks were real bases and the walk really did leave
/// the object grid.
pub static SWEEP_PHANTOM_INTERIOR_MARKS: AtomicU64 = AtomicU64::new(0);

/// Raw conservative-candidate side marks the late base-resolution pass PROVED
/// are object-INTERIOR and therefore cleared (2026-08-16).
///
/// This is the producer side of [`SWEEP_PHANTOM_INTERIOR_MARKS`]: every address
/// counted here would otherwise have entered `side_sorted` as a "live base"
/// that is not one. Non-zero on a cycle whose `phantom_extents` is zero is the
/// fix working; the two can no longer both be non-zero for the same address,
/// because the mark is gone before `side_sorted` is materialised.
pub static LATE_RESOLVE_RAW_INTERIOR_CLEARED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — reclaim spans the sweep refused to publish because they already
/// overlapped a free block. Publishing one is a double free: the allocator can
/// hand the same bytes to two objects, and the second allocation zeroes them
/// under the first.
pub static DOUBLE_FREE_SPANS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — bounded report counter for [`DOUBLE_FREE_SPANS`].
pub static DOUBLE_FREE_REPORTS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — bounded report counter for [`COMPACT_DROPPED_INTERIOR_ROOT`].
pub static COMPACT_INTERIOR_REPORTS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 — bounded report counter for [`COMPACT_DOWNGRADED_INTERIOR_ROOT`].
pub static COMPACT_DOWNGRADE_REPORTS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0-BLOCKED — spans the young non-moving sweep was about to free while
/// a ROOT still pointed into them. Every one is a use-after-free the mark
/// phase let through; they are retained instead. See the invariant in
/// `sweep_young_non_moving`.
pub static ROOT_IN_DEAD_SPANS: AtomicU64 = AtomicU64::new(0);

/// Object bases recovered by the `CRATONVM_GC_LATE_RESOLVE_DROPPED` arm --
/// candidates `mark_young` dropped as free/gap space inside a PROVED span that
/// the arena object grid then said were real objects. Every one is a live
/// object the default arm would have freed, so a non-zero reading here is the
/// anchor oracle's proof being wrong, not this arm being generous.
pub static LATE_RESOLVE_DROPPED_RECOVERED: AtomicU64 = AtomicU64::new(0);
/// Bounded report counter for [`LATE_RESOLVE_DROPPED_RECOVERED`].
pub static LATE_RESOLVE_DROPPED_REPORTS: AtomicU64 = AtomicU64::new(0);
/// Bounded report counter for [`ROOT_IN_DEAD_SPANS`].
pub static ROOT_IN_DEAD_REPORTS: AtomicU64 = AtomicU64::new(0);
/// H2-CID0-BLOCKED — the benign half of the same measurement: a root pointing
/// at a young source selective promotion EVACUATED. Not a violation (the copy
/// is live in old gen and the root is rewritten), counted so a campaign can
/// tell "the check never had anything to look at" from "the check looked and
/// everything it saw was a promotion".
pub static ROOT_IN_EVACUATED_SPANS: AtomicU64 = AtomicU64::new(0);

// ---------------------------------------------------------------------------
// Old-generation reclamation ring (H2-CID0, 2026-08-01)
//
// `OldGen::free` returns a block to the free list and leaves the bytes in
// place, so a still-live reference into it keeps reading the old object —
// until the allocator reuses the block, and reuse ZEROES it before writing the
// new header. That is how a prematurely reclaimed old-gen object comes back as
// an all-zero `ClassId(0)` header, which renders as `java.lang.Object` and
// fails a `checkcast` minutes later on another thread. By then the class is
// gone, and the sweep-ring that would have known it
// (`CRATONVM_DBG_SWEEP_ZERO`) only records the YOUNG sweep AND has to have
// been enabled before the run.
//
// This ring is the old-gen answer, and it is ALWAYS ON: an in-place old sweep
// only runs past 75 % occupancy, and a record is one relaxed `fetch_add` plus
// four relaxed stores. Lock-free, so it cannot serialise a sweep the way the
// young sweep-zero ring's mutex does.
// ---------------------------------------------------------------------------

/// [`record_old_freed`] site: the in-place old-gen sweep's free loop.
pub const OLD_FREED_SITE_INPLACE_SWEEP: u8 = 1;
/// [`record_old_freed`] site: mark-compact dropped the object (its storage
/// became part of the zeroed tail, or was slid over).
pub const OLD_FREED_SITE_COMPACT: u8 = 2;

/// Ring capacity, as a power of two.
///
/// H2-CID0 follow-up (2026-08-02): this was 14 (16 K entries), and 16 K is far
/// too small to answer the question the ring exists for. An in-place old sweep
/// frees thousands of blocks per cycle and the failing read is many cycles
/// later, so the record naming the victim had always been overwritten by the
/// time a `checkcast` asked — the one reproduction that reached the widened
/// reporter (`Page$Leaf cannot be cast to Chunk`) got no ring hit for exactly
/// that reason. 1 M entries x 32 B = 32 MB of BSS, untouched (and therefore
/// unmapped) until something is actually freed.
pub const OLD_FREED_RING_BITS: usize = 20;
pub const OLD_FREED_RING_LEN: usize = 1 << OLD_FREED_RING_BITS;

pub struct OldFreedRec {
    pub addr: AtomicU64,
    /// Block extent, so a lookup can answer for an INTERIOR address too. The
    /// receiver a failing `checkcast` holds is frequently not the freed block's
    /// base — a field address, an array element, a derived pointer — and an
    /// exact-address ring answered `None` for every one of those.
    pub size: AtomicU64,
    pub class_id: std::sync::atomic::AtomicU32,
    pub kind: std::sync::atomic::AtomicU8,
    pub site: std::sync::atomic::AtomicU8,
    /// [`OLD_FREED_FLAG_WATCHED`] | [`OLD_FREED_FLAG_INTERIOR_ROOT`] — WHY the
    /// mark could plausibly have missed this block. The reclaiming collector
    /// knows both facts and the reader, minutes later, can recover neither.
    pub flags: std::sync::atomic::AtomicU8,
    pub seq: AtomicU64,
}

impl OldFreedRec {
    pub const fn new() -> Self {
        Self {
            addr: AtomicU64::new(0),
            size: AtomicU64::new(0),
            class_id: std::sync::atomic::AtomicU32::new(0),
            kind: std::sync::atomic::AtomicU8::new(0),
            site: std::sync::atomic::AtomicU8::new(0),
            flags: std::sync::atomic::AtomicU8::new(0),
            seq: AtomicU64::new(0),
        }
    }
}

/// [`record_old_freed`] flag: at reclamation time this block was a WATCHED
/// referent — some `java.lang.ref.Reference` named it. An unmarked watched
/// block is *expected*: weak reachability is not reachability. What must then
/// hold is that post-GC reference processing CLEARS every `Reference` to it,
/// and a stale non-null `Reference.get()` is what a set flag here points at.
pub const OLD_FREED_FLAG_WATCHED: u8 = 0x1;
/// [`record_old_freed`] flag: an INTERIOR conservative root pointed into this
/// block when it was reclaimed. On the in-place arm the pin makes this
/// unreachable; on the compacting arm it would mean conservative roots reach a
/// collector that assumes precise ones.
pub const OLD_FREED_FLAG_INTERIOR_ROOT: u8 = 0x2;

pub static OLD_FREED_RING: [OldFreedRec; OLD_FREED_RING_LEN] =
    [const { OldFreedRec::new() }; OLD_FREED_RING_LEN];
pub static OLD_FREED_NEXT: AtomicU64 = AtomicU64::new(0);

/// Record that an old-generation block is being reclaimed. See the module note
/// above for why this is unconditional.
#[inline]
pub fn record_old_freed(addr: usize, size: usize, class_id: u32, kind: u8, site: u8, flags: u8) {
    let seq = OLD_FREED_NEXT.fetch_add(1, Ordering::Relaxed);
    let r = &OLD_FREED_RING[(seq as usize) & (OLD_FREED_RING_LEN - 1)];
    // Publish `addr` LAST: a reader that sees the address has already seen the
    // rest of this record (the slot may still be torn against a wrapping
    // writer, which is why `old_freed_lookup` is a diagnostic and not a proof).
    r.size.store(size as u64, Ordering::Relaxed);
    r.class_id.store(class_id, Ordering::Relaxed);
    r.kind.store(kind, Ordering::Relaxed);
    r.site.store(site, Ordering::Relaxed);
    r.flags.store(flags, Ordering::Relaxed);
    r.seq.store(seq, Ordering::Relaxed);
    r.addr.store(addr as u64, Ordering::Release);
}

/// Did an old-gen reclamation free a block at `addr`, and what did it hold?
/// Returns `(class_id, kind, site, seq)` for the most recent matching record.
pub fn old_freed_lookup(addr: usize) -> Option<(u32, u8, u8, u64)> {
    let a = addr as u64;
    let mut best: Option<(u32, u8, u8, u64)> = None;
    // Round 9 wave 8b (vmrt8 R2): scan only the written prefix. Writers claim
    // slots from index 0 in `seq` order, so before the ring wraps no slot at or
    // past `NEXT` has ever been written (its `addr` is 0 and would be skipped
    // anyway); a process that freed nothing scans nothing and never faults in
    // the ring's BSS. A record claimed after the load is missed exactly as an
    // unpublished one already was.
    let n = (OLD_FREED_NEXT.load(Ordering::Relaxed) as usize).min(OLD_FREED_RING_LEN);
    for r in OLD_FREED_RING[..n].iter() {
        if r.addr.load(Ordering::Acquire) != a {
            continue;
        }
        let seq = r.seq.load(Ordering::Relaxed);
        if best.is_none_or(|(_, _, _, b)| seq > b) {
            best = Some((
                r.class_id.load(Ordering::Relaxed),
                r.kind.load(Ordering::Relaxed),
                r.site.load(Ordering::Relaxed),
                seq,
            ));
        }
    }
    best
}

/// Like [`old_freed_lookup`], but answers for an address anywhere INSIDE a
/// freed block, not only at its base.
///
/// Returns `(class_id, kind, site, seq, base, size)` for the most recent
/// covering record. A failing `checkcast` holds whatever reference the Java
/// code had, which is the object base far less often than it looks — an array
/// element, a `Reference` referent slot, a derived pointer from compiled code.
/// The exact-address form answered `None` for all of those and the reporter
/// stayed silent about a block it had a full record for.
pub fn old_freed_lookup_covering(addr: usize) -> Option<(u32, u8, u8, u64, usize, usize, u8)> {
    let a = addr as u64;
    let mut best: Option<(u32, u8, u8, u64, usize, usize, u8)> = None;
    // Written prefix only; see `old_freed_lookup` (vmrt8 R2).
    let n = (OLD_FREED_NEXT.load(Ordering::Relaxed) as usize).min(OLD_FREED_RING_LEN);
    for r in OLD_FREED_RING[..n].iter() {
        let base = r.addr.load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let size = r.size.load(Ordering::Relaxed);
        if a < base || a >= base.saturating_add(size.max(1)) {
            continue;
        }
        let seq = r.seq.load(Ordering::Relaxed);
        if best.is_none_or(|(_, _, _, b, _, _, _)| seq > b) {
            best = Some((
                r.class_id.load(Ordering::Relaxed),
                r.kind.load(Ordering::Relaxed),
                r.site.load(Ordering::Relaxed),
                seq,
                base as usize,
                size as usize,
                r.flags.load(Ordering::Relaxed),
            ));
        }
    }
    best
}

// ---------------------------------------------------------------------------
// Young-generation span-reclamation ring (H2-CID0, 2026-08-02)
//
// The old-gen ring above answers "what was in this block, and which
// reclamation freed it" for ONE generation. The young non-moving sweep zeroes
// and publishes coalesced dead spans, and its existing forensic records
// (`record_swept`, `zero_forensics::record`) are both flag-gated AND
// mutex-backed — so they have to have been armed before the run that
// reproduces, and arming them silently swaps the collector off its parallel
// sweep prefix (`retain_dead_objects`) and takes a process-global lock per
// reclaimed object. The instrument changed the thing it measured, which is a
// documented cause of this family's "reproduces on plain runs, never on
// instrumented ones" history.
//
// This ring is unconditional and lock-free, and records ONE entry per
// COALESCED span rather than per object — a sweep publishes tens of spans, not
// millions of objects — so the cost stays bounded no matter the workload.
// ---------------------------------------------------------------------------

pub const YOUNG_FREED_RING_BITS: usize = 16; // 64 K spans, ~2 MB
pub const YOUNG_FREED_RING_LEN: usize = 1 << YOUNG_FREED_RING_BITS;

pub struct YoungFreedRec {
    pub base: AtomicU64,
    pub size: AtomicU64,
    pub cycle: AtomicU64,
    pub seq: AtomicU64,
    /// H2-CID0 (2026-08-05) — packed cross-thread root coverage of the sweep
    /// that published this span. See [`pack_xt_coverage`].
    pub xt: AtomicU64,
}

impl YoungFreedRec {
    pub const fn new() -> Self {
        Self {
            base: AtomicU64::new(0),
            size: AtomicU64::new(0),
            cycle: AtomicU64::new(0),
            seq: AtomicU64::new(0),
            xt: AtomicU64::new(0),
        }
    }
}

/// Marks a packed coverage word as actually written.
///
/// Zero is a legitimate reading (`passes=0` = the takeover never ran, which
/// `gc_quiescence::XT_PASSES_LAST_CYCLE` warns is the OPPOSITE conclusion from
/// "ran and found nothing"), so absence needs its own encoding.
pub const XT_CAPTURED: u64 = 1 << 48;

/// Pack `(passes, taken_over, unclassified)` into one word, saturating each at
/// 16 bits — the counts are per-cycle peer tallies, so anything near the cap is
/// already far past "a few threads".
#[inline]
pub fn pack_xt_coverage(passes: u64, taken: u64, unclassified: u64) -> u64 {
    XT_CAPTURED | (passes.min(0xFFFF) << 32) | (taken.min(0xFFFF) << 16) | unclassified.min(0xFFFF)
}

/// `(passes, taken_over, unclassified)`, or `None` if nothing was captured.
#[inline]
pub fn unpack_xt_coverage(w: u64) -> Option<(u64, u64, u64)> {
    if w & XT_CAPTURED == 0 {
        return None;
    }
    Some(((w >> 32) & 0xFFFF, (w >> 16) & 0xFFFF, w & 0xFFFF))
}

pub static YOUNG_FREED_RING: [YoungFreedRec; YOUNG_FREED_RING_LEN] =
    [const { YoungFreedRec::new() }; YOUNG_FREED_RING_LEN];
pub static YOUNG_FREED_NEXT: AtomicU64 = AtomicU64::new(0);

/// Record that the young non-moving sweep zeroed and published `[base, base +
/// size)`. Unconditional — see the module note above for why the gated
/// alternatives cannot answer.
#[inline]
pub fn record_young_span_freed(base: usize, size: usize, cycle: u64) {
    let seq = YOUNG_FREED_NEXT.fetch_add(1, Ordering::Relaxed);
    let r = &YOUNG_FREED_RING[(seq as usize) & (YOUNG_FREED_RING_LEN - 1)];
    r.size.store(size as u64, Ordering::Relaxed);
    r.cycle.store(cycle, Ordering::Relaxed);
    r.seq.store(seq, Ordering::Relaxed);
    // H2-CID0: the coverage of THIS sweep, captured here because this call
    // happens inside it — after the take-over pass published its outcome and
    // before `reset_xt_cycle` clears it for the next one. Read back by the
    // reclaim guard for the exact span it reports, so a stale receiver can say
    // whether the sweep that freed it had seen every running peer's JIT frames.
    let (passes, taken, unclassified, _roots, _hw, _hw_roots) =
        cratonvm_gc_core::gc_quiescence::xt_cycle_coverage();
    r.xt.store(
        pack_xt_coverage(passes, taken, unclassified),
        Ordering::Relaxed,
    );
    // Publish `base` LAST, as in the old-gen ring.
    r.base.store(base as u64, Ordering::Release);
}

/// [`record_young_span_freed`] for every span of one pinned in-place cycle's
/// rebuild (gcd d1/e, 2026-09-27).
///
/// That rebuild frees one span per GAP between survivors, which on a
/// scattered survivor set is hundreds of thousands per cycle, not the sweep's
/// "tens" the module note budgets for; per span, the single form paid a
/// process-wide `fetch_add` and a `xt_cycle_coverage` read (a thread-local
/// ledger borrow and six loads) that cannot change inside the loop. Here the
/// coverage is read once and the sequence numbers are reserved with ONE
/// `fetch_add`, and a record the same batch would overwrite (more spans than
/// the ring holds) is not written at all. The ring ends in exactly the state
/// the per-span calls leave: same `seq` for every span, same fields, same
/// write order per record.
pub fn record_young_spans_freed(base: usize, spans: &[(usize, usize)], cycle: u64) {
    if spans.is_empty() {
        return;
    }
    let (passes, taken, unclassified, _roots, _hw, _hw_roots) =
        cratonvm_gc_core::gc_quiescence::xt_cycle_coverage();
    let xt = pack_xt_coverage(passes, taken, unclassified);
    // Widening: a span count on a 64-bit (or narrower) target.
    let first = YOUNG_FREED_NEXT.fetch_add(spans.len() as u64, Ordering::Relaxed);
    let skip = spans.len().saturating_sub(YOUNG_FREED_RING_LEN);
    for (i, &(off, len)) in spans.iter().enumerate().skip(skip) {
        let seq = first.wrapping_add(i as u64);
        let r = &YOUNG_FREED_RING[(seq as usize) & (YOUNG_FREED_RING_LEN - 1)];
        r.size.store(len as u64, Ordering::Relaxed);
        r.cycle.store(cycle, Ordering::Relaxed);
        r.seq.store(seq, Ordering::Relaxed);
        r.xt.store(xt, Ordering::Relaxed);
        // Publish `base` LAST, as the single form does.
        r.base
            .store(base.wrapping_add(off) as u64, Ordering::Release);
    }
}

/// Did the young sweep reclaim a span covering `addr`? Returns
/// `(base, size, cycle, seq, xt_coverage)` for the most recent covering record,
/// where `xt_coverage` is `(passes, taken_over, unclassified)` for the sweep
/// that published the span — `None` when nothing was captured.
#[allow(clippy::type_complexity)]
pub fn young_freed_lookup(
    addr: usize,
) -> Option<(usize, usize, u64, u64, Option<(u64, u64, u64)>)> {
    let a = addr as u64;
    let mut best: Option<(usize, usize, u64, u64, Option<(u64, u64, u64)>)> = None;
    // Written prefix only; see `old_freed_lookup` (vmrt8 R2).
    let n = (YOUNG_FREED_NEXT.load(Ordering::Relaxed) as usize).min(YOUNG_FREED_RING_LEN);
    for r in YOUNG_FREED_RING[..n].iter() {
        let base = r.base.load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let size = r.size.load(Ordering::Relaxed);
        if a < base || a >= base.saturating_add(size.max(1)) {
            continue;
        }
        let seq = r.seq.load(Ordering::Relaxed);
        if best.is_none_or(|(_, _, _, b, _)| seq > b) {
            best = Some((
                base as usize,
                size as usize,
                r.cycle.load(Ordering::Relaxed),
                seq,
                unpack_xt_coverage(r.xt.load(Ordering::Relaxed)),
            ));
        }
    }
    best
}

/// H2-CID0 (2026-08-02) — conservative roots that are INTERIOR words of a live
/// old-generation object, and so pinned the object they point into.
///
/// Non-zero means the hole this counter was added for is live in the workload:
/// before the pin, every one of these was a live old-gen object with no mark
/// bit. See the seed loop in [`GenerationalHeap::old_gen_gc`].
pub static OLDMARK_INTERIOR_ROOT_PINS: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-02) — blocks the in-place old sweep freed even though an
/// interior conservative root pointed into them.
///
/// **Zero by construction** unless `CRATONVM_GC_NO_OLD_INTERIOR_PINS` is set,
/// which disables the pin while keeping the accounting: that is the negative
/// control, and it is what turns "the pin fires a lot" into "without the pin
/// these exact blocks are freed under a live root".
pub static OLD_SWEEP_FREED_INTERIOR_PINNED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-02) — blocks the old-gen MARK-COMPACT dropped that were
/// WATCHED referents at the time.
///
/// Not a defect on its own: weak reachability is not reachability, so dropping
/// a watched block is the normal outcome for a dead weak referent. It is the
/// number to read beside a `checkcast` verdict, because a stale non-null
/// `Reference.get()` over a dropped block means the post-GC CLEAR did not
/// happen for it.
pub static COMPACT_DROPPED_WATCHED: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-02) — blocks the old-gen MARK-COMPACT dropped that an
/// INTERIOR conservative root pointed into.
///
/// This was *predicted* to be zero — "the compacting arm runs only from the
/// moving young cycle, whose collector rewrites every root it is handed" — and
/// the H2 `MVStore` workload refuted the prediction on the first run. It is
/// what turned this family's diagnosis from "which sweep freed it" into "the
/// compactor is deciding liveness from a root set it cannot rewrite".
///
/// Now zero by construction: a cycle whose root set contains an interior
/// address does not compact at all (see [`COMPACT_DOWNGRADED_INTERIOR_ROOT`]).
/// Non-zero again means the downgrade regressed.
pub static COMPACT_DROPPED_INTERIOR_ROOT: AtomicU64 = AtomicU64::new(0);

/// H2-CID0 (2026-08-02) — major collections that asked to COMPACT and were
/// downgraded to the in-place sweep because a root was an interior address.
///
/// The cost side of the fix. Compare against `[GC] generational: major=N`: if
/// this is a small fraction the generation still gets compacted regularly, and
/// if it is most of them the in-place sweep's coalescer is carrying the
/// fragmentation (`[GC] oldgen_coalesce`).
pub static COMPACT_DOWNGRADED_INTERIOR_ROOT: AtomicU64 = AtomicU64::new(0);

/// Old-gen free-list entries that OVERLAP their neighbour, observed by the
/// coalescer (which already sorts the list, so this costs one comparison per
/// block).
///
/// A free list can only contain overlapping blocks if something double-freed or
/// over-freed a span. The allocator would then serve the same bytes to two
/// objects and zero them under the first owner — the same all-zero-header face
/// as a premature free, from the other direction. The young sweep has had this
/// check since `DOUBLE_FREE_SPANS`; old gen had none.
pub static OLD_FREE_LIST_OVERLAPS: AtomicU64 = AtomicU64::new(0);

/// Old-gen mark-worklist entries whose `kind` byte is not a valid `ObjectKind`
/// discriminant — i.e. the mark BFS was handed an address that is not an object
/// base and decoded whatever bytes were there as an `ObjectHeader`.
///
/// See `gc-old-gen-mark-accepts-unvalidated-addresses-FIXED.md`.
/// A non-zero value here means a side table is holding a dangling old-gen
/// address, or a reference slot holds a non-base word.
pub static OLDMARK_BAD_KIND_HITS: AtomicU64 = AtomicU64::new(0);

/// Old-gen mark-worklist candidates rejected by
/// [`old_gen_mark_candidate_plausible`]. Non-zero means a reference slot or a
/// side table held an in-range address that is not an old-gen object base —
/// before the screen existed, each of these was a blind `gc_flags` write into
/// whatever occupied that address.
pub static OLDMARK_REJECTED_CANDIDATES: AtomicU64 = AtomicU64::new(0);

/// GCAUD-8 — old-gen mark candidates that [`old_gen_mark_candidate_plausible`]
/// rejected but that `OldGen::walk_objects()` had just yielded as an object
/// BASE, so they were admitted anyway.
///
/// The plausibility screen was written for *conservative* register/stack
/// guesses, where a reject costs nothing because a failing candidate was never
/// an object. At a **precise** push site — a reference slot, an overlay owner,
/// a loader/mirror/metadata pin — its own doc says the opposite: "at a precise
/// site a false reject IS a premature free". The screen is a set of heuristics
/// about a header's bytes; walk membership is a *derivation* of the object grid
/// from the free list. Where the two disagree about an address the walk
/// produced, the walk wins and the object is retained.
///
/// Non-zero here means the screen would have dropped a live edge. It is the
/// counter to read together with `old_gen::COMPACT_ESCAPE_HITS`.
pub static OLDMARK_RESCUED_BY_WALK: AtomicU64 = AtomicU64::new(0);

/// GCAUD-8 — objects the in-place old-gen sweep's live-set closure promoted:
/// unmarked blocks that a MARKED old-gen object still referenced, i.e. blocks
/// the mark phase missed and this sweep would otherwise have freed while live.
///
/// Zero on every healthy cycle (a transitively correct marker leaves the
/// closure nothing to do). Non-zero names a real mark-phase gap and is the
/// number to watch when `old_gen::COMPACT_ESCAPE_HITS` stops climbing.
pub static OLD_SWEEP_CLOSURE_RESCUES: AtomicU64 = AtomicU64::new(0);

/// GCAUD-8 — in-place old-gen sweeps that observed a live old-gen object
/// referencing an in-old-gen address the object walk did not yield: a block
/// that is *already* on the free list, or one behind a `scan_region` header
/// anomaly. Unlike the compactor, this sweep cannot repair it (it never had the
/// block) and does not abort on it — it only refuses to write a mark bit
/// through the address, and reports.
pub static OLD_SWEEP_ESCAPE_HITS: AtomicU64 = AtomicU64::new(0);

/// Conservative root candidates may be interior heap addresses.  Only the
/// opt-in A2 forensic mode reports rejected candidates; normal collection
/// silently discards an address that is not an object start.
#[inline]
pub fn emit_conservative_candidate_diagnostic(hit: u64, a2_enabled: bool) -> bool {
    a2_enabled && hit < 8
}

/// DoHead walk-desync hardening — count of selective-promotion UNWIND events:
/// the evacuation walk saw a grid anomaly (zero span, implausible header,
/// free-block overshoot, or a span crossing a free hole) and dropped the
/// candidates collected since the last trustworthy anchor (a free-block
/// boundary). Their forwarding pointers were never installed (installs are
/// deferred until a stretch is anchor-verified); the already-copied old-gen
/// bytes are unreachable garbage for the next major GC. Candidates on
/// verified stretches still promote, so a persistent unparseable span cannot
/// starve promotion.
pub static SWEEP_PROMOTION_ABORT_HITS: AtomicU64 = AtomicU64::new(0);

/// DBG (CRATONVM_DBG_WATCHREF): trace the RandomizedContext WeakHashMap fix —
/// which sweep path ran, which kept-in-place survivors matched a watched
/// Weak/Soft/Phantom referent, and whether each was found alive or dead.
/// Cached (checked once, not per-object-visited) so enabling it cannot itself
/// perturb GC timing enough to mask/create the races it's meant to diagnose.
#[inline]
pub fn watchref_dbg() -> bool {
    gc_flags().dbg_watchref
}

thread_local! {
    /// Phase marks recorded by the MOVING (Cheney) young cycle for the current
    /// collection, drained by `collect_garbage_inner`'s pause timer.
    ///
    /// A thread-local rather than a value threaded through the cycle because
    /// that function has many exit paths (young-skip backstops, the non-moving
    /// diversion, promotion-OOM bailouts); a returned value would report only
    /// the one that falls through, which is not the interesting case.
    pub static MOVING_PHASE_MARKS: std::cell::RefCell<Vec<(&'static str, u128)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Render one per-collection `--verbose:gc` line.
///
/// Split out of [`GenerationalHeap::verbose_gc_end`] so the FORMAT can be
/// unit-tested: the emitter itself reads a live heap and three global side
/// tables, none of which a test can pose. Every field an operator reads comes
/// through here.
///
/// `young` / `old` are `(before, after)` byte occupancies; `pause_us` is the
/// whole stop-the-world window. `divert` is the `divert_non_moving` reason
/// label on a non-moving cycle and `None` otherwise — a moving cycle has
/// nothing to explain, and printing `divert=` on one would invite the reading
/// that it nearly did not move.
pub fn render_verbose_gc_line(
    cycle: u64,
    kind: &str,
    major: bool,
    young: (usize, usize),
    old: (usize, usize),
    young_free: (usize, usize),
    pause_us: u128,
    divert: Option<&str>,
) -> String {
    let mut line = format!(
        "[GC] generational: cycle={cycle} kind={kind}{major} \
         young={}->{} young_free={}->{} old={}->{} pause={:.3}ms",
        young.0,
        young.1,
        young_free.0,
        young_free.1,
        old.0,
        old.1,
        pause_us as f64 / 1000.0,
        major = if major { "+major" } else { "" },
    );
    if let Some(reason) = divert {
        // Same slot, two keys. A REFUSED cycle was not diverted anywhere —
        // neither collector ran — so calling its reason a `divert=` would say
        // the non-moving sweep took over, which is the exact false report
        // `kind=refused` exists to prevent. 2026-09-21.
        line.push_str(if kind == "refused" {
            " refusal="
        } else {
            " divert="
        });
        line.push_str(reason);
    }
    line
}

/// Pre-collection state for one `--verbose:gc` line.
///
/// Captured by [`GenerationalHeap::verbose_gc_begin`] before the pause and
/// consumed by [`GenerationalHeap::verbose_gc_end`] after it, so the `->`
/// arrows on the rendered line are a genuine before/after pair rather than two
/// reads of the same post-collection arena.
///
/// gen r4/plumbing (2026-09-23): no longer carries its own start `Instant`.
/// The entry points time the pause ONCE, around `collect_garbage_inner` only,
/// and hand that one figure to this line, to the pause-goal loop and to the
/// JMX pause total — so the three cannot disagree, and the before-image's two
/// arena locks and the line's own `eprintln!` are charged to none of them.
#[derive(Clone, Copy)]
pub struct VerboseGcCycle {
    pub young_used: usize,
    pub old_used: usize,
    /// Young free-list bytes before the collection. See
    /// [`GenerationalHeap::young_from_free_bytes`] for why `young_used` alone
    /// cannot show a non-moving cycle's reclamation.
    pub young_free: usize,
}

/// Record one moving-young phase duration, in MICROSECONDS. Only ever called
/// when `CRATONVM_DBG=gcpause` is on.
///
/// gengc-round2-core2 (2026-09-20): the unit was milliseconds, and it was
/// milliseconds *here*, i.e. the precision was already gone by the time the
/// renderer saw it. A cycle made of twenty sub-millisecond phases printed
/// twenty `=0ms` rows against a 12 ms total, and a genuine zero and a lost
/// 0.9 ms were indistinguishable. See
/// `docs/feature-designs/generative-gc-architecture.md`.
pub fn moving_phase_marks_push(name: &'static str, us: u128) {
    MOVING_PHASE_MARKS.with(|m| m.borrow_mut().push((name, us)));
}

/// Take and clear this thread's moving-young phase marks (microseconds).
pub fn moving_phase_marks_take() -> Vec<(&'static str, u128)> {
    MOVING_PHASE_MARKS.with(|m| std::mem::take(&mut *m.borrow_mut()))
}

/// Render one `[gcpause]` line from a total and the phase marks, both in
/// microseconds.
///
/// # The partition property
///
/// `docs/GC.md` records this being found and fixed for G1, and says why it is
/// load-bearing: "A table whose rows do not sum to the total cannot be used to
/// argue that a cost was removed rather than moved". G1's `[GC-STAT]` line now
/// satisfies `roots + rset + closure + fixup + free + verify + other == pause`
/// exactly; the generational `[gcpause]` breakdown never had that property.
/// Each `mv_phase!` records the delta since the previous mark, so the rows
/// covered only `[first mark .. last mark]`: the safepoint spin, the arena
/// locks, `join_evacuated_wipe` and the whole `divert_non_moving` decision
/// block were charged to no phase at all, and the line offered no `other=` to
/// hold them.
///
/// `other` is printed UNCONDITIONALLY, including zero. That is the whole point
/// — it is what makes a *future* uncovered phase visible instead of silent.
///
/// Split out of `PauseTimer::drop` so the property can be unit-tested; the
/// drop path is untestable by construction.
pub fn render_gcpause_line(
    total_us: u128,
    marks: &[(&'static str, u128)],
    counts: &[(&'static str, u128)],
) -> String {
    let covered: u128 = marks.iter().map(|(_, d)| *d).sum();
    // `saturating_sub`: the marks are taken inside the window the total
    // measures, so `covered <= total_us` holds — but the two clocks are read
    // at different instants and a pathological reordering must print a zero
    // residual rather than panic in a release build.
    let other_us = total_us.saturating_sub(covered);
    debug_assert!(
        covered <= total_us,
        "[gcpause] phase marks ({covered}us) exceed the measured pause ({total_us}us): \
         a phase is being double-counted, which makes every row meaningless",
    );
    let ms = |us: u128| us as f64 / 1000.0;
    let mut line = format!("[gcpause] collection took {:.3}ms", ms(total_us));
    if !marks.is_empty() {
        line.push_str("  ");
        for (n, d) in marks {
            line.push_str(&format!("{n}={:.3}ms ", ms(*d)));
        }
        // The residual. Emitted beside the rows rather than appended to one of
        // them, because attributing it would be a guess.
        line.push_str(&format!("other={:.3}ms", ms(other_us)));
        if !counts.is_empty() {
            line.push_str("  ");
            let tally = counts
                .iter()
                .map(|(n, v)| format!("{n}={v}"))
                .collect::<Vec<_>>()
                .join(" ");
            line.push_str(&tally);
        }
    }
    line
}

thread_local! {
    /// Unit-less counts for the current moving-young cycle (objects copied,
    /// bytes copied, …), reported beside the phase durations.
    ///
    /// A duration alone cannot say whether a slow `cheney_drain` means "too
    /// many survivors" or "the copy loop is too slow per object", and those
    /// have opposite fixes. The count is what separates them.
    pub static MOVING_PHASE_COUNTS: std::cell::RefCell<Vec<(&'static str, u128)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Default young-collection pause goal, in milliseconds.
///
/// Matches `G1CollectorConfig::max_gc_pause_ms` (and HotSpot's own
/// `-XX:MaxGCPauseMillis` default), so the two collectors in this VM answer
/// the same question the same way. Before this had a value, the generational
/// young collector had no pause target at all: on the OAuth2 issuer-URI
/// workload at `--Xmx 2g` it collected a ~272 MB young generation in
/// **469-629 ms** (median 553, 24 samples over the 100 ms print threshold),
/// against a 500 ms socket read budget that Spring Security hard-codes and
/// that HotSpot meets — the whole `SocketTimeoutException: Read timed out`
/// flake. With the goal armed the same repro measures **median 147 ms, and 2
/// of 170 collections over 500 ms** (both of them the pre-adaptation cycles
/// that run before the feedback loop has seen a pause).
pub const DEFAULT_YOUNG_PAUSE_GOAL_MS: u64 = 200;

/// `CRATONVM_GC_YOUNG_PAUSE_MS=N` — young-collection pause goal in ms, driving
/// [`GenerationalHeap::adapt_young_trigger_to_pause`]. `0` turns it off.
///
/// **Default [`DEFAULT_YOUNG_PAUSE_GOAL_MS`]**, changed from off on 2026-08-11.
/// The reason it was off is on record — this collector has a history of
/// young-sizing changes that helped one workload and cost another
/// (`with_capacity`'s note: capping the initial semi was a NET REGRESSION for
/// large-`-Xmx` workloads) — so the flip is backed by the throughput control
/// that objection asks for, `bench/BinTreesClassic.java`, run interleaved
/// A,B,B,A so host drift cannot be read as an effect:
///
/// | arm | bt18 `--Xmx 2g` (n=3) | bt20 `--Xmx 4g` (n=2) |
/// |---|---|---|
/// | goal off | mean 4.05 s | mean 16.77 s |
/// | goal 200 | mean 4.10 s | mean 17.30 s |
///
/// Both differences sit inside the run-to-run drift of the block they came
/// from, so no throughput cost is established — and the feedback loop cannot
/// touch a workload whose pauses are already inside the goal, which is the
/// structural reason it cannot repeat the earlier regression.
///
/// gci i4/yg: read once PER HEAP, at construction, into
/// `GenerationalHeap::young_pause_goal_ms` (every reader goes through that
/// field). It was a process-wide `OnceLock`, so a second heap in the process
/// (an embedding, a unit test under `with_thread_overrides`) used the first
/// heap's goal while its `[GC] young_trigger:` line printed it as its own
/// (`gci-i3yg-young-collection-review-findings` item 1). A value that does not
/// parse still means the default, as before.
pub fn young_pause_goal_ms_from_env() -> u64 {
    None::<std::ffi::OsString>
        .and_then(|v| v.to_str().and_then(|s| s.trim().parse::<u64>().ok()))
        .unwrap_or(DEFAULT_YOUNG_PAUSE_GOAL_MS)
}

/// gci i4/yg: the reader of `CRATONVM_GEN_YOUNG_EXHAUSTED_REFILL_COLLECTS`
/// (default on, `runtime_flag_on`; read once per heap, at construction).
///
/// i4/fj's O4 (`vi2-jn` `jn2_a_roots_def_1`, Generational, a JIT-compiled
/// `byte[4096]` churn beside a foreign attached thread, every young cycle a
/// non-moving sweep): `tlab_refill_attempts=28218940 tlab_refill_ok=4723016`
/// and `gen_old_allocs=23498933` -- 83 % of the compiled refills found young
/// unable to hold the object and the object went straight to the old
/// generation (96 GB of old-direct allocation, 4436 concurrent old cycles in
/// 175 s). The refill-time young trigger (`tlab_alloc_shaped_inner`) consults
/// the occupancy only every 4 MiB of SUCCESSFUL refills
/// (`NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES`, a quarter of this 16 MiB semi),
/// a failed refill adds nothing to that count, and the floor a refusing helper
/// window raises to 90 % (`ytw_raised=5763`) sits inside the last consult
/// window -- so young runs out between two consults and every allocation until
/// the next one is tenured at birth. HotSpot Serial collects young on the
/// failed eden allocation. With the switch, the compiled refill that young
/// could not serve asks [`GenerationalHeap::young_exhausted_refill_wants_collection`]
/// and, granted, collects young and retries the refill once. default on because it
/// changes when young collections run (pause shape).
pub fn gen_young_exhausted_refill_collects_enabled() -> bool {
    true
}

/// gci i22/dr (P2/yy, `gch-h1yy-proposal-refill-takes-the-smallest-whole-free-block-20261008`;
/// opt-in, measure-first): a young TLAB refill takes the SMALLEST free block
/// that makes a useful TLAB (at least `tlab::min_tlab_size()` and the object
/// that asked), WHOLE, before the default carve best-fits the request -- so the
/// large blocks a sweep coalesced stay whole for large arrays until the medium
/// holes are gone ([`GenerationalHeap::refill_smallest_whole_block`],
/// `Arena::take_smallest_block_at_least`). Opt-in because it changes TLAB
/// sizes and so the young cycle count; deciding rows `vi22-dr.list` block YY.
///
/// WARNING flip (gcj j2, 2026-10-19, `triage-j1.md` vi22-dr YY): default ON
/// (`=0` kills). On j1, 9 interleaved reps: hashmap-pair 125.8 vs 1289.6
/// ns/op, 20 vs 230 young GCs; alloc-ring 19.79 vs 17.30 ns (1.14x, over the
/// 1.05x bar -- recorded, wave j2 lane yg2 owns the cost).
pub fn gen_tlab_refill_smallest_whole_enabled() -> bool {
    true
}

/// What [`GenerationalHeap::young_exhausted_refill_wants_collection`] answers
/// for an exhausted refill. gci i4/yg.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExhaustedRefillVerdict {
    /// Collect young now (then retry the refill once).
    Collect,
    /// Less than [`EXHAUSTED_REFILL_PROGRESS_DIVISOR`]th of the semi was
    /// carved into TLABs since this young cycle's first refill: a collection
    /// now would reclaim little (young holds live data, or the cycle just
    /// ran), so the object takes the old-generation spill as before.
    NoProgress,
    /// This young cycle already collected on an exhausted refill.
    Repeat,
}

/// A young cycle earns an exhausted-refill collection once TLAB carves since
/// its first refill reach `capacity / EXHAUSTED_REFILL_PROGRESS_DIVISOR` (a
/// quarter of the semi). The storm guard: a young generation full of LIVE data
/// fails its refills before carving that much, so it never collects on this
/// path more than once in a row. gci i4/yg.
pub const EXHAUSTED_REFILL_PROGRESS_DIVISOR: u64 = 4;

/// The pure rule of [`GenerationalHeap::young_exhausted_refill_wants_collection`]:
/// `carved_since` = TLAB bytes carved since the current young cycle's first
/// refill, `capacity` = one semi, `fired_this_cycle` = this cycle already
/// collected on an exhausted refill. gci i4/yg.
pub fn exhausted_refill_verdict(
    carved_since: u64,
    capacity: u64,
    fired_this_cycle: bool,
) -> ExhaustedRefillVerdict {
    if fired_this_cycle {
        ExhaustedRefillVerdict::Repeat
    } else if capacity == 0 || carved_since < capacity / EXHAUSTED_REFILL_PROGRESS_DIVISOR {
        ExhaustedRefillVerdict::NoProgress
    } else {
        ExhaustedRefillVerdict::Collect
    }
}

pub fn moving_phase_count_push(name: &'static str, value: u128) {
    MOVING_PHASE_COUNTS.with(|m| m.borrow_mut().push((name, value)));
}

pub fn moving_phase_counts_take() -> Vec<(&'static str, u128)> {
    MOVING_PHASE_COUNTS.with(|m| std::mem::take(&mut *m.borrow_mut()))
}

thread_local! {
    /// The last collection's `(pause_us, phase marks)` as `collect_garbage_inner`'s
    /// pause timer drained them, kept for the VM's JFR `jdk.GCPhasePause`
    /// emitter. gen r4w2/obs (2026-09-23).
    ///
    /// A thread-local, like the marks themselves: the timer drops on the
    /// collecting thread, which is the VM initiator that goes on to call
    /// `gc_event_finish` — so the producer and the consumer are the same
    /// thread and no other collection can interleave. Only written while a JFR
    /// recording is running.
    pub static LAST_PAUSE_PHASES: std::cell::RefCell<Option<PausePhases>> =
        const { std::cell::RefCell::new(None) };
}

/// `(pause_us, [(phase, us)])` -- see [`take_last_pause_phases`]. gen r4w2/obs.
pub type PausePhases = (u128, Vec<(&'static str, u128)>);



/// gcg h1/ob (`gcg-g3x-xlog-escalated-young-collection-prints-one-pause-full`):
/// the young half of a pause that also ran an UNREQUESTED major, as the VM's
/// `-Xlog` / JFR / GC-notification split needs it (HotSpot Serial reports such
/// a pause as `Pause Young` then `Pause Full`, two ids). Booked by
/// `note_collection_pause` (only when the `Copy` bean booked the young half
/// too, so the bean counts and the split agree) and taken once by the VM
/// inside the pause ([`GenerationalHeap::take_escalation_split`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EscalationSplit {
    /// The instant the young half ended and the major began.
    pub at: std::time::Instant,
    /// The old generation's used bytes at that instant: after the young
    /// half's promotions, before the major (read by `major_trigger_decision`
    /// under the old-generation lock, the decision that started the major).
    pub old_used_at_mark: usize,
}

/// gcg h1/ob: the per-heap escalation state of the cycle in flight (see
/// [`EscalationSplit`]). `decided_old_used` is written by
/// `major_trigger_decision` when an unrequested major will run, `mark` by
/// `note_escalation_mark` as it starts, `booked` by `note_collection_pause`
/// after the cycle. Every field is cleared at both cycle entries.
#[derive(Clone, Copy, Debug, Default)]
pub struct EscalationState {
    pub decided_old_used: Option<usize>,
    pub mark: Option<std::time::Instant>,
    pub booked: Option<EscalationSplit>,
}

/// gcg g6/yg: the `(all, old)` pause totals [`cratonvm_gc_core::gc_metrics::serial_collectors`]
/// splits into `Copy` and `MarkSweepCompact`, with the young halves of
/// escalated pauses (`esc` = `(count, us)`) moved from the full collector's
/// time to the young one's and counted once more under `Copy`. Pure.
pub fn serial_totals_with_escalations(
    all: (u64, u64, u64),
    old: (u64, u64, u64),
    esc: (u64, u64),
) -> ((u64, u64, u64), (u64, u64, u64)) {
    let esc_us = esc.1.min(old.1);
    (
        (all.0.saturating_add(esc.0), all.1, all.2),
        (old.0, old.1 - esc_us, old.2),
    )
}

pub fn last_pause_phases_store(pause_us: u128, marks: Vec<(&'static str, u128)>) {
    LAST_PAUSE_PHASES.with(|m| *m.borrow_mut() = Some((pause_us, marks)));
}

/// Take (and clear) this thread's last generational collection's phase marks:
/// `(pause_us, [(phase, us)])`, the rows summing to at most `pause_us` (the
/// residual is what `render_gcpause_line` prints as `other=`). `None` when no
/// generational collection has stashed any since the last take — the stash is
/// only armed while `cratonvm_jfr::is_enabled()`. gen r4w2/obs.
pub fn take_last_pause_phases() -> Option<PausePhases> {
    LAST_PAUSE_PHASES.with(|m| m.borrow_mut().take())
}

/// gci i14/mp: `GenerationalHeap::last_collection_usage[2]` bits -- a
/// collection has stamped the pools' collection usage ...
pub const COLLECTION_RECORDED: usize = 1;
/// ... and one that reclaimed the old generation has stamped `Tenured Gen`'s.
pub const TENURED_COLLECTION_STAMPED: usize = 2;

/// The old generation's committed size as this heap REPORTS it (`Tenured Gen`
/// committed, and the old half of `Runtime.totalMemory()`): its committed
/// granules on the reserving store; on the wholly committed fallback, where
/// every byte is committed from construction, its touched prefix
/// (`high_water`) — counting the whole store there made the old generation
/// alone report half of `-Xmx`. Never below the bytes it holds.
///
/// gen r5w2/obs6: the one derivation `VmHeap::committed_bytes` (it lived
/// inline there) and `GenerationalHeap::jmx_memory_pools` (which reported
/// the reservation) now share. Equal to `OldGen::committed_capacity` on the
/// reserving store.
pub fn reported_old_committed(og: &OldGen) -> usize {
    let capacity = og.capacity();
    let committed = og.committed_bytes();
    let in_use = if committed >= capacity {
        og.high_water().min(capacity)
    } else {
        committed
    };
    in_use.max(og.used())
}

/// DBG (`CRATONVM_DBG_PROMO_SEED`): report how many of THIS cycle's young→old
/// promotion destinations the in-place old sweep seeded its mark from, and how
/// many `old_gen_mark_candidate_plausible` rejected. A nonzero rejection count
/// is the measurement the first attempt at that seeding inferred but never
/// took — see `sweep_old_gen_non_moving`.
pub fn promo_seed_dbg() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| false)}

/// **Current default: ON** (WARNING gci-user-flip-20261013: `GcFlags::oldgen_compact`
/// parses with `truthy_word_default_true`; `CRATONVM_OLDGEN_COMPACT=0` restores the
/// in-place arm). On a moving cycle's Phase 5 major the verdict is
/// `moving_major_compaction_plan`'s (gci i8/pa): a major P-C can arm reclaims in
/// place, every other moving major compacts. The history below is why it was off
/// from 2026-08-03 to 2026-10-13; "Default disabled" and "Unset ... off" in it
/// describe that period (gci i20/gn docs pass, `gci-i8rv-default-flip-review-findings-RETIRED-RULING-20261018`).
///
/// 2026-08-03 (`HIB-MAPRESIZE-STALE.1`, see
/// map-resize-unpinned-chain-cursors-nojit-segv-20260731-FIXED.md
/// Follow-up 4): whether `OldGen::compact` (the sliding mark-compact
/// collector) is permitted to run at all. Default **disabled** (until 2026-10-13) —
/// `major_gc` now runs the in-place, non-compacting arm of `old_gen_gc`
/// unconditionally instead.
///
/// This was found by bisection, not by reading: `walk_objects` covering
/// hundreds of MB fewer bytes than `used_bytes` (fixed defensively in
/// `OldGen::compact`'s new walk-coverage guard, see `COMPACT_WALK_GAP_HITS`)
/// proved real corruption was already present the FIRST time compaction ran
/// in a session — but forcing every old-gen major GC through the
/// non-compacting arm instead (`CRATONVM_DBG_FORCE_OLDGEN_NO_COMPACT=1`, the
/// flag this replaced) made `DefaultCatalogAndSchemaTest` complete cleanly,
/// twice, at exact parity with HotSpot (`found=132`) — where every prior
/// compact-enabled run of the same class crashed or came up short. That
/// implicates something in `compact`'s own Phase 1-3 (forwarding-pointer
/// assignment / reference-slot rewrite / slide), not just the walk-coverage
/// gap, but the exact mechanism was not found by code review (Phase 1-3 and
/// the overlay `pointer_map` remap in
/// `native_collections::gc_update_collection_overlay_refs` were both
/// audited; both look correct in isolation).
///
/// `compact` still exists, still self-tests, and is not deleted: a future
/// session that finds the real bug can flip this back with
/// `CRATONVM_OLDGEN_COMPACT=1` to re-enable it (or remove this gate once
/// fixed). Until then, over-retaining memory the in-place sweep's own
/// `coalesce_free_blocks` fallback cannot fully defragment is judged the
/// safer failure mode than the memory corruption this replaces — the same
/// "over-retention over corruption" direction every other fail-safe check
/// added during this investigation already takes.
///
/// gen r4/oldgen (2026-09-23): read with `runtime_flag_on`, not presence.
/// `runtime_var_os(..).is_some()` turned compaction ON for
/// `CRATONVM_OLDGEN_COMPACT=0` / `=false` / `=off` — the value someone writes
/// precisely to say "keep it off", on the collector's most corruption-prone
/// arm. Same defect class as `jit-presence-only-flag-reads-FIXED.md`. Unset
/// (the default) is unchanged: off.
pub fn oldgen_compact_enabled() -> bool {
    gc_flags().oldgen_compact
}

/// gcd d4/n (2026-09-28) — may a MOVING young cycle's Phase 5 major slide old
/// gen on a fragmentation request while compiled frames are live? Default ON
/// guard (it may not); `CRATONVM_GC_MOVING_MAJOR_JIT_GUARD=0` restores the
/// slide. Read per call (the declared-flag snapshot; cheap, and a per-thread
/// test override reaches it). See
/// `docs/feature-designs/generative-gc-architecture.md`.
pub fn moving_major_jit_guard_enabled() -> bool {
    true
}



/// DBG: optional young-GC stress threshold (bytes). Read from
/// `CRATONVM_DBG_GC_STRESS`, or `CRATONVM_GC_STRESS` as an accepted alias
/// (the latter is what several handoff/repro docs use; without the alias the
/// documented `CRATONVM_GC_STRESS=<bytes> …` command silently does nothing).
/// `CRATONVM_DBG_GC_STRESS` wins if both are set.
pub fn gc_stress_threshold() -> Option<usize> {
    gc_flags().gc_stress_bytes
}

/// The `CRATONVM_DBG_GC_STRESS=<t>` young trigger: `(fires, floor_to_store)`.
///
/// gen r4w2/youngmark (2026-09-23), see
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// The trigger used to be `used >= t` on every path. After a MOVING cycle that
/// is "every `t` bytes of allocation": the survivors were compacted and the
/// bump cursor restarts near zero. After a NON-MOVING cycle `used` is the
/// arena's bump HIGH-WATER mark, which the in-place sweep never retreats, so
/// once it passed `t` the predicate was true for the rest of the run. Every
/// interpreted `new` calls `maybe_gc` right after allocating, so every single
/// allocation collected: the cycle retired the thread's fresh 256 KiB TLAB, the
/// sweep reclaimed it, the next `new` carved another one, and the mutator
/// advanced one object per ~2 ms pause (`OldToYoungEdgeProbe 16 20` under
/// `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_DBG=gc-stress=250000`: 54 414 cycles
/// in 120 s, `young=6293840->6293840`, never finished).
///
/// After a non-moving cycle (`after_non_moving`) the metric is the LIVE
/// occupancy `used - free_list_bytes`, which the sweep does lower. That alone
/// is not enough: a TLAB carve counts as live the moment it is carved, and the
/// default TLAB (256 KiB) is larger than the documented `t = 250000`, so the
/// first carve after a cycle crosses `t` and the very next `maybe_gc` would
/// collect again, wasting the TLAB. So the first time the metric reaches `t`
/// in an epoch (`sample_now`: no floor sampled since the last completed
/// collection) the trigger RE-ARMS at `live + t` instead of firing. A further
/// carve can only happen once the carved TLAB is exhausted, so every stress
/// cycle now buys at least one full TLAB of mutator progress. The price is a
/// cadence of roughly `2t` rather than `t` on this path — stress is a
/// bug-hunting knob, and a knob that stops the program is no knob at all.
///
/// **After a MOVING cycle too** (gen r4w3/young2, 2026-09-23). Wave 2 kept
/// that arm at the bare `used >= t`, on the belief that a compacted cursor
/// cannot livelock. It can, by the same carve: after a moving cycle `used` is
/// the survivor volume `S`, the first interpreted `new` carves a TLAB
/// (256 KiB — `initial_refill_size`, which every refill after the cycle's
/// TLAB retire takes), and `S + 262 144 >= t` holds for EVERY `t` below the
/// carve, whatever `S` is. The next `maybe_gc` collects that TLAB, the cycle
/// copies `S` again, and the mutator advances one object per pause. That is
/// the measured shape: `OldToYoungEdgeProbe 16 20` with moving young ON timed
/// out at 900 s under `gc-stress=250000` and passed at `500000` (where
/// `S + 262 144 < t` while `S` stays under ~238 KB); and `BinT 10` at
/// `250000` was measured at 1 512 moving cycles (the no-moving-young-optout
/// FIXED page) for a run that allocates 10 x 2 047 `Node`s — about 14 objects
/// per collection, where "every `t` bytes" would be a handful of cycles in
/// all. It finished only because it is that small. So both arms
/// now re-arm once per epoch; the moving arm's metric stays `used` (a compacted
/// from-space has no free list to subtract).
///
/// **The re-arm is `min(t, max_tlab_size())`, not `t`.** Its only job is to
/// let the carve that crossed be consumed before the next collection, and no
/// carve exceeds `max_tlab_size()` (1 MiB). For `t <= 1 MiB` — every stress
/// value in the repro docs — this is wave 2's `metric + t` exactly; for a
/// larger `t` it keeps the cadence near `t` instead of doubling it. A JIT
/// refill checks BEFORE carving, so on compiled allocation the first crossing
/// is a genuine one and the re-arm costs at most one extra TLAB per cycle.
///
/// The metric counts CARVED bytes, not allocated ones, so when two or more
/// INTERPRETED threads each carve right after a cycle, the second carve can
/// cross `metric + re-arm` while the first thread's TLAB is still almost
/// unused. Since gen r4w6/tlab6 the caller vetoes such a verdict with
/// [`gc_stress_carve_bound`] (and `refill_tlab` caps stress carves at
/// [`stress_tlab_carve_cap`]); this function is unchanged.
pub fn gc_stress_decision(
    used: usize,
    free_list_bytes: usize,
    t: usize,
    after_non_moving: bool,
    sample_now: bool,
    floor: usize,
) -> (bool, Option<usize>) {
    let metric = if after_non_moving {
        used.saturating_sub(free_list_bytes)
    } else {
        used
    };
    if sample_now {
        if metric >= t {
            let rearm = t.min(cratonvm_gc_core::tlab::max_tlab_size());
            return (false, Some(metric.saturating_add(rearm)));
        }
        return (false, None);
    }
    (metric >= t.max(floor), None)
}

/// How many stress-capped carves make one `t` (gen r4w6/tlab6). See
/// [`stress_tlab_carve_cap`] and [`gc_stress_carve_bound`].
pub const STRESS_CARVES_PER_THRESHOLD: usize = 8;

/// The largest TLAB `refill_tlab` carves under `CRATONVM_DBG_GC_STRESS=<t>`:
/// `t / 8`, clamped to `[min_tlab_size(), max_tlab_size()]`, on the 8-byte grid
/// (gen r4w6/tlab6).
///
/// Why cap at all: the carve bound below has to allow for buffers that are
/// carved but not yet used, and that allowance is a number of carves. With the
/// default 256 KiB carve against the documented `t = 250000`, a single carve
/// already exceeds `t`, so no allowance can both keep one thread's cadence near
/// `t` and stop a second thread's carve from collecting the first's buffer.
/// Capped at `t / 8`, eight carves are one `t`.
pub fn stress_tlab_carve_cap(t: usize) -> usize {
    let min = cratonvm_gc_core::tlab::min_tlab_size();
    let max = cratonvm_gc_core::tlab::max_tlab_size().max(min);
    (t / STRESS_CARVES_PER_THRESHOLD).clamp(min, max) & !7
}

/// The carve-aware veto on [`gc_stress_decision`]'s verdict (gen r4w6/tlab6,
/// `docs/feature-designs/generative-gc-architecture.md`).
///
/// `fires` is the per-epoch verdict (`metric >= floor`), `metric` the stress
/// metric now, `floor` the sampled floor (`metric_at_epoch + rearm`), and
/// `carved_since` the NET carve bytes since that sample
/// (`GenerationalHeap::young_stress_net_carved` delta). A carve raises the metric
/// by the whole buffer the moment it is carved, so the metric's growth since
/// the epoch is `direct + carved_since`, where `direct` is what went in without
/// a carve (slow-path objects, humongous spills into young). The trigger keeps
/// a verdict only when
///
/// * `direct >= t` — direct allocation alone reached `t`; or
/// * `carved_since >= t + 8 x stress_tlab_carve_cap(t)` — the carves exceed `t`
///   by more than eight capped buffers could still hold unused.
///
/// So N interpreted threads that each carve after a collection no longer
/// collect each other's fresh buffers: with the cap, up to eight threads' idle
/// buffers fit in the allowance and every cycle still buys at least `t` of
/// allocation. A single thread's cadence stays about `2t` of carves, what the
/// per-epoch re-arm already gave it.
pub fn gc_stress_carve_bound(
    fires: bool,
    metric: usize,
    floor: usize,
    t: usize,
    carved_since: u64,
) -> bool {
    if !fires {
        return false;
    }
    let rearm = t.min(cratonvm_gc_core::tlab::max_tlab_size());
    let base = floor.saturating_sub(rearm);
    let grown = metric.saturating_sub(base);
    let carved = usize::try_from(carved_since).unwrap_or(usize::MAX);
    let direct = grown.saturating_sub(carved);
    let allowance = STRESS_CARVES_PER_THRESHOLD.saturating_mul(stress_tlab_carve_cap(t));
    direct >= t || carved >= t.saturating_add(allowance)
}

// ---------------------------------------------------------------------------
// "Zeroed-a-live-object" detector (CRATONVM_DBG_SWEEP_ZERO)
//
// The non-moving sweep zeroes every UNMARKED young object. When a live object
// is reclaimed because its only reference is invisible to the marker (a
// register/native-stack root — `CRATONVM_DBG_SWEEP_EDGES` stays silent, ruling
// out heap/root/card edges), the bug surfaces LATER as an all-zero-header
// receiver / ClassCastException on some other thread. By then the header is
// zeroed, so the object's class is lost.
//
// This records the (address, class_id, kind) of every object the sweep zeroes
// into a bounded ring BEFORE the zeroing write. A consumer (e.g. the
// interpreter's all-zero-header detection) calls `sweep_zero_lookup(addr)` to
// recover the ORIGINAL class of a reclaimed object — i.e. "this swept slot was
// a java/util/concurrent/ForkJoinTask", which names the root-coverage gap.
// Gated + cheap (one masked ring write per dead object only when enabled).
// ---------------------------------------------------------------------------

/// One swept-object record: heap address, original class_id, original kind byte.
#[derive(Clone, Copy)]
pub struct SweptRec {
    pub addr: u64,
    pub class_id: u32,
    pub kind: u8,
    pub cycle: u32,
    /// GC context at the time of reclamation (CRATONVM_DBG_MTROOTS), so the
    /// sweep-zero detector can name WHICH GC reclaimed a live object: the
    /// reason (1=System.gc / 2=alloc-young / 3=forced-alloc), the initiating
    /// thread id, and how many threads were parked (blocked) at that STW.
    pub reason: u8,
    pub initiator: u32,
    pub blocked: u32,
}

pub const SWEPT_RING_BITS: usize = 18; // 256K entries
pub const SWEPT_RING_LEN: usize = 1 << SWEPT_RING_BITS;
pub const SWEPT_RING_MASK: usize = SWEPT_RING_LEN - 1;

pub struct SweptRing {
    pub buf: Vec<SweptRec>,
    pub next: usize,
}

pub static SWEPT_RING: std::sync::OnceLock<parking_lot::Mutex<SweptRing>> = std::sync::OnceLock::new();
pub static SWEEP_ZERO_CYCLE: AtomicU64 = AtomicU64::new(0);

/// GC context for the IN-PROGRESS collection, published by the VM root-gathering
/// path just before `collect_garbage` (CRATONVM_DBG_MTROOTS). Read by
/// `record_swept` so a reclaimed-live record names the responsible GC.
/// reason: 1=System.gc, 2=alloc-young (maybe_gc), 3=forced-alloc (maybe_gc_forced).
pub static GC_CTX_REASON: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static GC_CTX_INITIATOR: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub static GC_CTX_BLOCKED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Publish the current collection's context (VM-side, before `collect_garbage`).
pub fn set_gc_context(reason: u8, initiator: u32, blocked: u32) {
    GC_CTX_REASON.store(reason as u32, Ordering::Relaxed);
    GC_CTX_INITIATOR.store(initiator, Ordering::Relaxed);
    GC_CTX_BLOCKED.store(blocked, Ordering::Relaxed);
}

/// Current young-sweep cycle stamp (the value the next sweep will record).
pub fn current_sweep_cycle() -> u32 {
    SWEEP_ZERO_CYCLE.load(Ordering::Relaxed) as u32
}

pub fn sweep_zero_enabled() -> bool {
    gc_flags().dbg_sweep_zero
}

pub fn swept_ring() -> &'static parking_lot::Mutex<SweptRing> {
    SWEPT_RING.get_or_init(|| {
        parking_lot::Mutex::new(SweptRing {
            buf: vec![
                SweptRec {
                    addr: 0,
                    class_id: 0,
                    kind: 0,
                    cycle: 0,
                    reason: 0,
                    initiator: 0,
                    blocked: 0
                };
                SWEPT_RING_LEN
            ],
            next: 0,
        })
    })
}

/// Record that the sweep is about to zero `addr` (a young object with the given
/// header), so a later all-zero-header consumer can recover its original class.
/// No-op unless `CRATONVM_DBG_SWEEP_ZERO` is set.
#[inline]
pub fn record_swept(addr: usize, class_id: u32, kind: u8, cycle: u32) {
    if !sweep_zero_enabled() {
        return;
    }
    // Skip synthetic filler sentinels (TLAB/GAP fillers are *supposed* to be
    // reclaimed) — they only mask the real reclaimed object's record when a
    // freed slot is later reused as a filler at the same address.
    if class_id == cratonvm_gc_core::tlab::TLAB_FILLER_CLASS_ID.as_u32()
        || class_id == cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32()
    {
        return;
    }
    let mut r = swept_ring().lock();
    let i = r.next & SWEPT_RING_MASK;
    r.buf[i] = SweptRec {
        addr: addr as u64,
        class_id,
        kind,
        cycle,
        reason: GC_CTX_REASON.load(Ordering::Relaxed) as u8,
        initiator: GC_CTX_INITIATOR.load(Ordering::Relaxed),
        blocked: GC_CTX_BLOCKED.load(Ordering::Relaxed),
    };
    r.next = r.next.wrapping_add(1);
}

/// Look up whether `addr` was recently zeroed by the non-moving sweep. Returns
/// `(original_class_id, original_kind, gc_cycle, reason, initiator_tid, blocked)`
/// of the most recent matching record, or `None`. Used by the all-zero-header
/// detection to name a reclaimed (register/native-root-invisible) live object
/// AND the GC that reclaimed it. No-op unless the gate is set.
pub fn sweep_zero_lookup(addr: usize) -> Option<(u32, u8, u32, u8, u32, u32)> {
    if !sweep_zero_enabled() {
        return None;
    }
    let a = addr as u64;
    let r = swept_ring().lock();
    let mut best: Option<SweptRec> = None;
    for rec in r.buf.iter() {
        if rec.addr == a {
            match best {
                Some(b) if b.cycle >= rec.cycle => {}
                _ => best = Some(*rec),
            }
        }
    }
    best.map(|b| {
        (
            b.class_id,
            b.kind,
            b.cycle,
            b.reason,
            b.initiator,
            b.blocked,
        )
    })
}

#[derive(Default, Debug)]
pub struct HeapStats {
    /// Number of minor GC cycles completed.
    pub minor_gc_count: AtomicU64,
    /// Number of major (old-gen mark-sweep) GC cycles completed.
    pub major_gc_count: AtomicU64,
    /// Total bytes copied in the young gen across all minor GCs.
    pub bytes_copied_young: AtomicU64,
    /// Total objects copied in the young gen across all minor GCs.
    pub objects_copied_young: AtomicU64,
    /// Total bytes promoted from young→old across all minor GCs.
    pub bytes_promoted: AtomicU64,
    /// Total objects promoted from young→old across all minor GCs.
    pub objects_promoted: AtomicU64,
    /// Total bytes freed by major GC sweep phases.
    pub bytes_freed_old: AtomicU64,
    /// Young allocations served by the arena SLOW path
    /// (`try_alloc_young_initialized` and its twins) since startup.
    ///
    /// gen r4/plumbing (2026-09-23): NOT "every young allocation", which is
    /// what this used to say. A TLAB refill is one arena hand-out, and every
    /// object bump-allocated inside it — the interpreter's and the JIT's
    /// inline `new` — never reaches this counter. On a JIT-warm workload it is
    /// a count of refills and slow-path objects, orders of magnitude below the
    /// allocation count, and must not be read as an allocation rate.
    pub young_allocations: AtomicU64,
    /// Objects allocated DIRECTLY in the old generation since startup: the
    /// humongous-array path, the young-exhausted spill of a native wrapper
    /// allocation (`try_alloc_object_old`), and its batched form.
    ///
    /// gen r4/plumbing (2026-09-23): the previous doc said "currently all
    /// allocations go through young, … exposed for future humongous
    /// handling" — the counter has had three live writers for a long time.
    pub old_allocations: AtomicU64,
    /// gen r4w4/alloc4 — bytes of young hand-outs (TLAB chunks and slow-path
    /// objects) that were NOT memset because they came off the bump tail,
    /// which the young wipe / a fresh commit already zeroed
    /// (`GcFlags::gen_zero_once`). One relaxed add per refill, never per object.
    pub young_zero_skipped_bytes: AtomicU64,
    /// gen r4w4/alloc4 — bytes of young hand-outs that WERE memset: free-list
    /// reuse, the zero-once switch off, or a repair (below).
    pub young_zeroed_bytes: AtomicU64,
    /// gen r4w4/alloc4 — bump-tail hand-outs the zero-once skip refused to
    /// trust because a word it read was non-zero (the refill tripwire, or the
    /// debug-build sample), and zeroed after all. Non-zero is a finding: some
    /// writer reached the young bump tail, which the one-zeroing-point
    /// invariant says nothing may.
    pub young_zero_repairs: AtomicU64,
    /// gce e1/o — the part of `young_zeroed_bytes` whose hand-out did NOT come
    /// off the bump tail (a free-list block, the fragmentation fallback's
    /// mini-TLAB): memory the non-moving sweep had already zeroed when it
    /// listed the span (`zero_spans_parallel`), zeroed a second time here.
    /// Counted whatever `CRATONVM_GEN_ZERO_ONCE` says, so
    /// `young_zeroed_bytes - young_zeroed_reuse_bytes` is the bump-tail memset
    /// (the switch off, or a repair). On a JIT-warm run every young cycle is
    /// the in-place sweep, the cursor never falls back, and after the first
    /// pass through young nearly every refill is a reuse: this is why probe Z
    /// of `gengc-r4-alloc-tlab-memory-is-zeroed-twice-FIXED-20260929.md` shows the
    /// skip at ~1.5 %. One relaxed add per REUSED refill, never per object.
    /// gce e1/y: a hand-out the known-zero free list lets skip its memset
    /// (`CRATONVM_GEN_ZERO_ONCE_FREE_LIST`) is NOT counted here; it goes to
    /// `young_zero_skipped_bytes` like a bump-tail skip.
    pub young_zeroed_reuse_bytes: AtomicU64,
    /// t1 lane F (`gcj-j2yg2` re-measure) — the part of the reused hand-outs'
    /// memset that covered bytes the arena's lazy-dirty set named
    /// (`Arena::take_lazy_dirty_counted`, exact clipped overlap). Under the
    /// lazy sweep / lazy in-place defaults those are the reclaimed bytes'
    /// ONE zeroing; `young_zeroed_reuse_bytes` minus this is memset written
    /// over bytes no lazy-dirty span named (zero already wherever the free
    /// list's other doors kept their promise). Census only.
    pub young_lazy_dirty_zeroed_bytes: AtomicU64,
    /// gen r4w4/alloc4 — humongous arrays whose body was zeroed AFTER the
    /// old-generation lock dropped (`GcFlags::gen_humongous_zero_unlocked`).
    pub humongous_unlocked_zeroes: AtomicU64,
    /// gen r4w4/alloc4 (cross-lane request from lane `oom`) — bytes carved out
    /// of the young from-space as TLAB chunks, BOTH arms of `refill_tlab` (the
    /// main carve and the fragmentation fallback's mini-TLAB), cumulative since
    /// the heap was built. Read through
    /// [`GenerationalHeap::young_stress_carved`]; see there.
    pub young_tlab_carved_bytes: AtomicU64,
    /// gen r4w6/tlab6 — bytes of retiring TLAB tails this heap took back
    /// (`GenerationalHeap::return_tlab_tail`, `CRATONVM_GEN_TLAB_TAIL_SINK`),
    /// cumulative. `young_tlab_carved_bytes - young_tlab_tail_returned_bytes`
    /// is what the carves still hold.
    pub young_tlab_tail_returned_bytes: AtomicU64,
    /// gen r4w6/tlab6 — returned tails that retracted the bump cursor.
    pub young_tlab_tail_retracts: AtomicU64,
    /// gen r4w6/tlab6 — returned tails that went on the young free list.
    pub young_tlab_tail_free_listed: AtomicU64,
    /// gen r4w6/tlab6 — tails offered while the sink was on and NOT taken
    /// (the `young_from` lock was busy, the tail was too small to list, or it
    /// was not this from-space's): those got the filler as before.
    pub young_tlab_tail_declined: AtomicU64,
}

/// Immutable snapshot of [`HeapStats`] at a moment in time.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HeapStatsSnapshot {
    pub minor_gc_count: u64,
    pub major_gc_count: u64,
    pub bytes_copied_young: u64,
    pub objects_copied_young: u64,
    pub bytes_promoted: u64,
    pub objects_promoted: u64,
    pub bytes_freed_old: u64,
    pub young_allocations: u64,
    pub old_allocations: u64,
    /// gen r4w4/alloc4 — see `HeapStats::young_zero_skipped_bytes`.
    pub young_zero_skipped_bytes: u64,
    /// gen r4w4/alloc4 — see `HeapStats::young_zeroed_bytes`.
    pub young_zeroed_bytes: u64,
    /// gen r4w4/alloc4 — see `HeapStats::young_zero_repairs`.
    pub young_zero_repairs: u64,
    /// gce e1/o — see `HeapStats::young_zeroed_reuse_bytes`.
    pub young_zeroed_reuse_bytes: u64,
    /// t1 lane F — see `HeapStats::young_lazy_dirty_zeroed_bytes`.
    pub young_lazy_dirty_zeroed_bytes: u64,
    /// gen r4w4/alloc4 — see `HeapStats::humongous_unlocked_zeroes`.
    pub humongous_unlocked_zeroes: u64,
    /// gen r4w4/alloc4 — see `HeapStats::young_tlab_carved_bytes`.
    pub young_tlab_carved_bytes: u64,
    /// gen r4w6/tlab6 — see `HeapStats::young_tlab_tail_returned_bytes`.
    pub young_tlab_tail_returned_bytes: u64,
    /// gen r4w6/tlab6 — see `HeapStats::young_tlab_tail_retracts`.
    pub young_tlab_tail_retracts: u64,
    /// gen r4w6/tlab6 — see `HeapStats::young_tlab_tail_free_listed`.
    pub young_tlab_tail_free_listed: u64,
    /// gen r4w6/tlab6 — see `HeapStats::young_tlab_tail_declined`.
    pub young_tlab_tail_declined: u64,
}

impl HeapStats {
    pub fn snapshot(&self) -> HeapStatsSnapshot {
        // `Relaxed` is fine for monotonic counters sampled from outside
        // a GC pause — snapshot consistency is not guaranteed and
        // callers only use this for observability / regression tests.
        HeapStatsSnapshot {
            minor_gc_count: self.minor_gc_count.load(Ordering::Relaxed),
            major_gc_count: self.major_gc_count.load(Ordering::Relaxed),
            bytes_copied_young: self.bytes_copied_young.load(Ordering::Relaxed),
            objects_copied_young: self.objects_copied_young.load(Ordering::Relaxed),
            bytes_promoted: self.bytes_promoted.load(Ordering::Relaxed),
            objects_promoted: self.objects_promoted.load(Ordering::Relaxed),
            bytes_freed_old: self.bytes_freed_old.load(Ordering::Relaxed),
            young_allocations: self.young_allocations.load(Ordering::Relaxed),
            old_allocations: self.old_allocations.load(Ordering::Relaxed),
            young_zero_skipped_bytes: self.young_zero_skipped_bytes.load(Ordering::Relaxed),
            young_zeroed_bytes: self.young_zeroed_bytes.load(Ordering::Relaxed),
            young_zero_repairs: self.young_zero_repairs.load(Ordering::Relaxed),
            young_zeroed_reuse_bytes: self.young_zeroed_reuse_bytes.load(Ordering::Relaxed),
            young_lazy_dirty_zeroed_bytes: self
                .young_lazy_dirty_zeroed_bytes
                .load(Ordering::Relaxed),
            humongous_unlocked_zeroes: self.humongous_unlocked_zeroes.load(Ordering::Relaxed),
            young_tlab_carved_bytes: self.young_tlab_carved_bytes.load(Ordering::Relaxed),
            young_tlab_tail_returned_bytes: self
                .young_tlab_tail_returned_bytes
                .load(Ordering::Relaxed),
            young_tlab_tail_retracts: self.young_tlab_tail_retracts.load(Ordering::Relaxed),
            young_tlab_tail_free_listed: self.young_tlab_tail_free_listed.load(Ordering::Relaxed),
            young_tlab_tail_declined: self.young_tlab_tail_declined.load(Ordering::Relaxed),
        }
    }

    /// gci i2/wr — collections booked (minor + major) in two relaxed loads, the
    /// same answer as `snapshot()`'s two fields without its other 17 loads.
    /// `VmHeap::collection_count` (and so every address-memo epoch lookup on
    /// Generational) reads this.
    #[inline]
    pub fn collection_count(&self) -> u64 {
        self.minor_gc_count.load(Ordering::Relaxed) + self.major_gc_count.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero.  Useful in tests that want to
    /// observe a single GC cycle in isolation.
    pub fn reset(&self) {
        self.minor_gc_count.store(0, Ordering::Relaxed);
        self.major_gc_count.store(0, Ordering::Relaxed);
        self.bytes_copied_young.store(0, Ordering::Relaxed);
        self.objects_copied_young.store(0, Ordering::Relaxed);
        self.bytes_promoted.store(0, Ordering::Relaxed);
        self.objects_promoted.store(0, Ordering::Relaxed);
        self.bytes_freed_old.store(0, Ordering::Relaxed);
        self.young_allocations.store(0, Ordering::Relaxed);
        self.old_allocations.store(0, Ordering::Relaxed);
        self.young_zero_skipped_bytes.store(0, Ordering::Relaxed);
        self.young_zeroed_bytes.store(0, Ordering::Relaxed);
        self.young_zero_repairs.store(0, Ordering::Relaxed);
        self.young_zeroed_reuse_bytes.store(0, Ordering::Relaxed);
        self.young_lazy_dirty_zeroed_bytes.store(0, Ordering::Relaxed);
        self.humongous_unlocked_zeroes.store(0, Ordering::Relaxed);
        self.young_tlab_carved_bytes.store(0, Ordering::Relaxed);
        self.young_tlab_tail_returned_bytes
            .store(0, Ordering::Relaxed);
        self.young_tlab_tail_retracts.store(0, Ordering::Relaxed);
        self.young_tlab_tail_free_listed.store(0, Ordering::Relaxed);
        self.young_tlab_tail_declined.store(0, Ordering::Relaxed);
    }
}


pub use cratonvm_gc_core::jit_bounds::{jit_region_bounds_addr, CommitScreen, CommitScreens, JitRegionBoundsTable, JIT_REGION_BOUNDS, YOUNG_COMMIT_SCREENS, YOUNG_COMMIT_SCREEN_READ_ATTEMPTS};

/// Bytes the generational collector has returned to the OS, over the process.
///
/// The engagement number for `CRATONVM_GEN_UNCOMMIT`. A zero with the switch ON
/// is a fact worth seeing — it means every collection found the evacuated
/// semi-space had no whole granule to give back — and it is indistinguishable
/// from the switch being off unless it is printed.
/// How long a granule of a young semi-space must sit unused before
/// [`GenerationalHeap::uncommit_evacuated_young`] hands it back to the OS --
/// HotSpot's `ZUncommitDelay` default, and the delay ZGC's arena uses.
///
/// It was zero: every young collection released the whole evacuated
/// semi-space, which is the NEXT cycle's to-space and the cycle after's
/// from-space, so an allocation-heavy program faulted its whole nursery back in
/// every other cycle. Measured (r16 zgcarray, `R16ZgcarrayAlloc
/// -XX:+UseGenerationalGC -Xmx1g -Dlens=8192`, Linux, `perf -e cpu-clock`):
/// 31% of the process in page faults and 6.6% in the `madvise` that dropped
/// the pages, which is most of the 2.4-3x per-array premium over HotSpot that
/// `docs/feature-designs/zgc-architecture.md`
/// left unattributed. A granule the nursery keeps reusing never goes idle for
/// this long (`Arena::commit_range_for_use` resets its clock), so it stays
/// committed; one a workload stopped using goes back at the first collection
/// after the delay, as before but later. `CRATONVM_GEN_UNCOMMIT=0` still turns
/// the give-back off entirely.
pub const GEN_YOUNG_UNCOMMIT_DELAY: std::time::Duration =
    std::time::Duration::from_secs(300);

pub static YOUNG_BYTES_UNCOMMITTED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Bytes returned to the OS by the generational young collector so far.
pub fn young_bytes_uncommitted() -> u64 {
    YOUNG_BYTES_UNCOMMITTED.load(Ordering::Relaxed)
}

/// `is_object_address` answers the exact bitmap gave, and answers it declined.
///
/// The engagement pair for `CRATONVM_GC_OBJECT_STARTS`. `hits` counts the
/// candidates the bitmap accepted without reading a header byte; `misses`
/// counts the ones it had no bit for and which therefore paid the full
/// deduction anyway. A high miss ratio is not a fault -- it is what a
/// conservative scan over zeroes, small integers and long bit patterns looks
/// like, and it is also what TLAB-allocated objects look like, since a TLAB
/// chunk is one `hand_out` and its contents never reach the bitmap.
///
/// Separating those two populations is the next measurement, and it needs a
/// workload rather than another counter.
///
/// # Striped, not one shared word (gen r4/plumbing, 2026-09-23)
///
/// These three were plain `AtomicU64`s bumped by `fetch_add` on every
/// bitmap-answered `is_object_address` call -- "the VM's hottest validator",
/// reached per operand-stack object on every object-returning native call from
/// every mutator at once. That is one shared cache line taken exclusive by
/// every thread, the exact cost `gc_metrics.rs`'s module header explains
/// gating the card counter for, and it is charged ONLY to the
/// `CRATONVM_GC_OBJECT_STARTS=1` arm -- the arm `arena.rs` records as slower
/// in 7 of 8 concurrent 8-thread pairs. An instrument must not bill its own
/// cost to the treatment it measures, so each thread now bumps its own
/// cache-line stripe and the (shutdown-only) reader sums them.
pub static OBJECT_START_HITS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();
pub static OBJECT_START_MISSES: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();
/// Misses that the header-shaped deduction then ACCEPTED.
///
/// This is the recoverable half of the miss population, and it needs no new
/// plumbing to measure: a candidate the bitmap had no bit for, which the full
/// deduction went on to certify as a real object base, is by definition an
/// object this arena allocated and the bitmap never saw. There is exactly one
/// way for that to happen -- a TLAB chunk is one `hand_out` and the objects
/// bump-allocated inside it never reach it.
///
/// The other half of a miss is a genuine non-object: a zero, a small integer, a
/// long bit pattern. Those SHOULD miss, and no amount of work on the bitmap will
/// change them. Reporting the two together is what made the raw miss count
/// unactionable -- it could not say whether the bitmap was leaving coverage on
/// the table or simply being asked about rubbish.
pub static OBJECT_START_MISSED_OBJECTS: cratonvm_types::striped_counter::StripedCounter =
    cratonvm_types::striped_counter::StripedCounter::new();

/// `(hits, misses, missed_objects)` for the exact object-start fast path.
///
/// `missed_objects` is the subset of `misses` the deduction went on to accept --
/// the coverage the bitmap is leaving on the table, as opposed to the rubbish it
/// is correctly declining. See [`OBJECT_START_MISSED_OBJECTS`].
pub fn object_start_counts() -> (u64, u64, u64) {
    (
        OBJECT_START_HITS.get() as u64,
        OBJECT_START_MISSES.get() as u64,
        OBJECT_START_MISSED_OBJECTS.get() as u64,
    )
}

/// Candidates [`GenerationalHeap::is_object_address`] declined because their
/// header reads FORWARDED and the forward is not one this heap's collector
/// installs: it leaves the heap, is null / misaligned / implausible, or stays
/// a BUSY claim (gen r5w5/pin9; the last two since gen r5w6/pin10, see
/// `ObjectHeader::screen_shape`).
///
/// Only a rejected candidate bumps it, and a healthy run rejects few, so a
/// plain shared atomic costs nothing on the accept path. A non-zero reading is
/// the population that used to SIGSEGV at `addr=<target>+4` or spin 262 144
/// rounds (see `forward_chain_stays_inside`); the BUSY share alone is
/// `ObjectHeader::screen_busy_declined()`.
pub static FORWARD_SCREEN_DECLINED: AtomicU64 = AtomicU64::new(0);

/// `(forward_declined, busy_declined, busy_wait_abandoned)` for the shutdown
/// census (gen r5w6/pin10):
///
/// - `forward_declined`: [`FORWARD_SCREEN_DECLINED`], every candidate
///   `is_object_address` refused on its forward chain;
/// - `busy_declined`: `ObjectHeader::screen_busy_declined()`, the part of
///   those (and of the old-gen screens' refusals) whose mark stayed BUSY --
///   the population that used to spin 262 144 rounds each;
/// - `busy_wait_abandoned`: `ObjectHeader::forwarding_busy_wait_abandoned()`,
///   BUSY waits some typed accessor still gave up on. After this change a
///   screen never reaches that wait, so a non-zero value names a caller that
///   sizes an unproven header through `num_slots()` / `array_length()`.
///
/// A run that prints all three as 0 never met the ASLR-dependent population
/// at all, which is what a gate needs to know before reading a pass as a fix.
pub fn header_screen_counts() -> (u64, u64, u64) {
    (
        FORWARD_SCREEN_DECLINED.load(Ordering::Relaxed),
        ObjectHeader::screen_busy_declined(),
        ObjectHeader::forwarding_busy_wait_abandoned(),
    )
}

/// Whether `header`'s forward chain, if any, is one a collector of this heap
/// could have installed: every hop's target accepted by `inside`, no BUSY claim
/// that never publishes, at most 8 hops (gen r5w5/pin9; gen r5w6/pin10).
///
/// Since gen r5w6/pin10 this is `ObjectHeader::screen_shape(inside).is_some()`:
/// ONE screen-only resolution, shared with `is_object_address`, which uses the
/// resolved shape itself instead of re-reading it. Two things changed against
/// pin9's walk, both refusals of bytes no collector writes:
///
/// - a mark that stays `FORWARDED | BUSY` for `SCREEN_BUSY_REREADS` reads is
///   refused. pin9's walk answered `true` there, and the caller's `num_slots()`
///   then spun `resolved_shape` x `forwarding_address` = 262 144 rounds per
///   candidate
///   (`docs/feature-designs/generative-gc-architecture.md`);
/// - a FORWARDED (not SELF, not BUSY) mark whose target is null, the header
///   itself, misaligned or beyond 2^47 is refused. pin9's walk answered `true`
///   there because `shape_source` stops without a dereference; the typed
///   accessors then read the garbage second word as the shape. No collector
///   publishes such a forward (`set_forwarding_address` asserts the target),
///   so the bytes are not a forwarded object.
///
/// Callers: `is_object_address` (through `screen_shape` directly), and the two
/// old-gen screens `old_gen_mark_candidate_plausible` /
/// `victim8_neighbor_explains_zero_prefix` (lane old's region), before their
/// own `num_slots()` / `array_length()`.
///
/// # The pin9 rationale, kept
///
/// `ObjectHeader::num_slots` and `array_length` resolve a long header's shape
/// word THROUGH a forward (`resolved_shape` -> `shape_source`), and the only
/// screen on the target is the context-free `plausible_heap_pointer`: non-null,
/// 8-aligned, below 2^47. That is right for a real object and wrong for a
/// conservative candidate, whose bytes merely LOOK like a header. The netty
/// buffer-suite `SIGSEGV addr=0x100000004` is this, exactly:
///
/// - the candidate is the payload word of a legacy `Value::Object` cell, a
///   JIT-derived field address (`obj + off + 8`) held in a register;
/// - its "class id" is the referent's low 32 bits and its "mark" the high 32,
///   `0x7673` for a heap at `0x7673_xxxx_xxxx`, whose low two bits read
///   `MARK_FORWARDED` (and bits 2..3, `SELF`/`BUSY`, read clear): every other
///   header screen passes on those bits;
/// - its "second word" is the next cell's first word, `Value::Int(1)` =
///   `0x1_0000_0000`: aligned and below 2^47, so the forward is followed and
///   the mark read at `0x1_0000_0004` faults.
///
/// It depends on where ASLR places the heap (the low nibble of the address's
/// high half must be `0b0011`), which is why it came and went between runs and
/// arms. A collector only ever forwards into its own arenas, so `false` never
/// declines a real object: it names bytes that are not a header.
///
/// `inside(t)` must accept only addresses at which a whole header is readable
/// (the caller's arenas, bounded by their commit maps).
pub fn forward_chain_stays_inside(header: &ObjectHeader, inside: impl Fn(usize) -> bool) -> bool {
    header.screen_shape(inside).is_some()
}



/// Process-global **read-side** mirror of whatever address range this heap has
/// mapped — same six-word `[b0, e0, b1, e1, b2, e2]` layout as
/// [`JIT_REGION_BOUNDS`], so the emitted containment sequence is byte-identical
/// and only the baked address differs.
///
/// # Why a second table rather than filling the first
///
/// [`JIT_REGION_BOUNDS`] is doing two jobs. Its *documented* job is the
/// READ-side question "is this address mapped, so a raw load cannot fault".
/// Its load-bearing job since `g1-audit.md` §8.1 (G1-2) is the
/// STORE-side question "may an inline reference store skip the collector's
/// write barrier" — and G1/ZGC answer that by leaving the table **empty**, so
/// that under those collectors no inline reference-store fast path is
/// reachable and a JNI-pinned, CSet-excluded region cannot lose its
/// remembered-set edge.
///
/// One table, two questions, opposite answers: filling it to make *loads*
/// inline would silently re-enable those *stores*. Hence the sibling. Only
/// `emit_guarded_getfield_receiver_check` and `ir_lower`'s copy of it read
/// this one; `region_bounds_are_live` keeps reading [`JIT_REGION_BOUNDS`] and
/// keeps gating every store path.
///
/// # Who publishes, and who deliberately does not
///
/// * **Generational** — its three arenas, alongside the store-side publish.
/// * **G1** — the single contiguous backing arena, in slot 0. G1's N regions
///   are carved out of one `Box` (`arena_base + i*region_size`), so one
///   `[base, end)` pair covers all of them and the three slots are ample. A
///   reference field under G1 is a plain pointer, so admitting inline
///   reference loads there is sound.
/// * **ZGC** — its single arena envelope, in slot 0, **as of 2026-08-19**.
///   This row previously read "not published, on purpose", on the grounds
///   that a compact reference slot under ZGC holds
///   `Z_COLORED_TAG | colour | offset` rather than a pointer. That is true of
///   the RELOCATING ZGC `feature-designs/zgc-jit-load-barrier.md` designs and
///   false of the one that runs: `feature-designs/zgc-reference-slot-
///   representation.md` measured it as *"Reference slots are plain pointers;
///   nothing in the heap stores a colored word"*, and `set_barrier_color` —
///   the only writer of the colored state — has no non-test caller. The
///   publish is coupled to that fact rather than betting on it: arming the
///   barrier CLEARS this table, which is the only mechanism that can disarm
///   an inline sequence in already-compiled code. See
///   `gc/src/zgc.rs::zgc_jit_read_bounds_enabled`, and
///   `CRATONVM_ZGC_NO_JIT_READ_BOUNDS` to restore helper-only reads.
///
/// All-zero matches nothing, so an unpublished collector degrades to exactly
/// today's behaviour: every guarded site falls through to the checked helper.
pub use cratonvm_gc_core::jit_bounds::*;

/// How many identity hash codes a thread claims per global `fetch_add`.
/// See [`GenerationalHeap::next_hash`] for the trade-off this number sets.
pub const IDENTITY_HASH_BLOCK: i32 = 64;

thread_local! {
    /// `(next value to serve, values left in this thread's block)`.
    /// `(0, 0)` forces the first call on a thread to claim a block.
    pub static IDENTITY_HASH_CURSOR: std::cell::Cell<(i32, i32)> =
        const { std::cell::Cell::new((0, 0)) };
}

pub use cratonvm_gc_core::jit_bounds::{addr_in_published_young_regions, published_young_regions_are_live, CommitScreenTable};









