#[allow(unused_imports)]
use super::*;


/// gci i19/yc: can an evacuation copy of `size` bytes fit ANY destination
/// region of `region_size` bytes? `false` only for a size strictly larger than
/// a region (a header misread: no evacuated object is that large). An unknown
/// geometry (`region_size == 0`) answers `true`, never a refusal. Pure; see
/// `SharedEvac::tlab_alloc`.
#[inline]
pub fn evac_copy_fits_a_region(size: usize, region_size: usize) -> bool {
    region_size == 0 || size <= region_size
}

/// The chunk a Survivor PLAB claim asks for: a PLAB, or the object when it is
/// larger, never more than the 8-aligned region length. Pure.
#[inline]
pub fn survivor_plab_chunk(need: usize, region_len: usize) -> usize {
    SURVIVOR_PLAB_BYTES.max(need).min(region_len & !7)
}

/// gci i16/pl: HotSpot's `ParallelGCBufferWastePct` default, the share of a
/// PLAB (percent) at or above which an object that does not fit the current
/// PLAB is allocated directly instead of retiring the PLAB.
pub const SURVIVOR_PLAB_DIRECT_WASTE_PCT: usize = 10;

/// gci i16/pl (finding 4): does an object of `size` bytes that missed its
/// PLAB chunk go straight to the shared region (HotSpot's
/// `word_sz * 100 >= plab_word_size * ParallelGCBufferWastePct`)? Pure.
#[inline]
pub fn survivor_plab_wants_direct(size: usize) -> bool {
    size.saturating_mul(100) >= SURVIVOR_PLAB_BYTES * SURVIVOR_PLAB_DIRECT_WASTE_PCT
}

/// Smallest closure work (copied + rset-scanned bytes) a young pause must
/// have for its `closure_us` to teach the per-byte price: below it the
/// division is dominated by the fixed cost of starting the closure.
pub const YT_MIN_WORK_BYTES: u64 = 64 * 1024;

/// Fold `sample` into a 7/8 decaying average held in `cell`; an empty cell
/// takes the sample whole (the file's EMA convention).
#[inline]
pub fn yt_ema_into(cell: &AtomicU64, sample: u64) {
    let prev = cell.load(Ordering::Relaxed);
    let next = if prev == 0 {
        sample
    } else {
        prev.saturating_mul(7).saturating_add(sample) / 8
    };
    cell.store(next, Ordering::Relaxed);
}

/// Survivor bytes as whole regions, rounded up: what PLAB-packed survivors
/// would occupy. Pure.
#[inline]
pub fn survivor_equiv_regions(surv_bytes: u64, region_size: usize) -> usize {
    let region = (region_size as u64).max(1);
    usize::try_from(surv_bytes.div_ceil(region)).unwrap_or(usize::MAX)
}

/// The young count the HotSpot-shaped arm compares with the target: Eden
/// claimed since the last pause (`young` minus the young regions that pause
/// left) plus the packed survivor footprint. Saturating. Pure.
#[inline]
pub fn hotspot_young_arm_count(young: usize, after_pause: usize, survivor_equiv: usize) -> usize {
    young.saturating_sub(after_pause).saturating_add(survivor_equiv)
}

/// HotSpot's base time for one pause: the pause minus its copy at the
/// predicted price (`ps_per_byte` picoseconds per byte). Saturating; with no
/// price yet the whole pause is base (the conservative direction: a larger
/// base never grants more Eden). Pure.
#[inline]
pub fn young_pause_base_us(pause_us: u64, bytes_copied: u64, ps_per_byte: u64) -> u64 {
    pause_us.saturating_sub(bytes_copied.saturating_mul(ps_per_byte) / 1_000_000)
}

/// Predicted cost of one more Eden region, in nanoseconds: its predicted
/// survivors times the price per byte. 0 means "no prediction yet". Pure.
#[inline]
pub fn young_region_cost_ns(surv_bytes_per_region: u64, ps_per_byte: u64) -> u64 {
    surv_bytes_per_region.saturating_mul(ps_per_byte) / 1_000
}

/// gci i16/pl (finding 12) — a young pause's copied bytes split into
/// `(eden_sourced, survivor_sourced)`, given the Eden-sourced figure (the
/// age-1 survivor bytes), clamped so the two always sum to `copied`. Pure.
#[inline]
pub fn split_young_copied(copied: u64, eden_copied: u64) -> (u64, u64) {
    let eden = eden_copied.min(copied);
    (eden, copied - eden)
}

/// gci i16/pl (finding 12) — Eden regions of a young collection set: the
/// set minus the Survivor regions the previous census left (a pinned
/// Survivor region kept out of the set makes this an under-count, i.e. a
/// higher per-region cost: the conservative direction). Pure.
#[inline]
pub fn young_eden_cset_regions(cset_regions: u32, surv_regions_in: usize) -> u64 {
    u64::from(cset_regions).saturating_sub(surv_regions_in as u64)
}

/// gci i16/pl (finding 12) — per mille of the Survivor bytes a pause took in
/// that it copied again (or promoted), capped at 1000; 0 for no intake. Pure.
#[inline]
pub fn survivor_survival_permille(surv_copied: u64, surv_in: u64) -> u64 {
    if surv_in == 0 {
        return 0;
    }
    (surv_copied.saturating_mul(1_000) / surv_in).min(1_000)
}

/// gci i16/pl (finding 12) — HotSpot's `predict_survivor_regions_evac_time`
/// in microseconds: `surv_bytes` at the learned survival (per mille), times
/// the closure price (picoseconds per byte). Saturating. Pure.
#[inline]
pub fn survivor_copy_us(surv_bytes: u64, survival_permille: u64, ps_per_byte: u64) -> u64 {
    (surv_bytes.saturating_mul(survival_permille.min(1_000)) / 1_000).saturating_mul(ps_per_byte)
        / 1_000_000
}

/// gci i14/yt — HotSpot 25 `G1Policy::calculate_young_desired_length` with
/// the MMU term at its default (disabled: `GCPauseIntervalMillis` =
/// `MaxGCPauseMillis + 1`), in regions. Pure.
///
/// * `absolute_min = max(min_sizer, survivors + 1)` (G1NewSizePercent of the
///   heap, and at least one Eden region beyond the young regions already
///   there -- right after a pause, HotSpot's survivors);
/// * `absolute_max = max(max_sizer, absolute_min)`;
/// * Eden by pause (`calculate_desired_eden_length_by_pause`): the minimum
///   Eden when the predicted base time is at or above the goal (HotSpot logs
///   "base time is longer than pause time" and does the same), the maximum
///   when no per-region cost has been measured, else the largest Eden whose
///   predicted cost fits in `goal - base`;
/// * the result is `survivors + eden`, clamped to `[absolute_min,
///   absolute_max]`.
pub fn hotspot_young_target(
    min_sizer: usize,
    max_sizer: usize,
    survivors: usize,
    goal_us: u64,
    base_us: u64,
    region_ns: u64,
) -> usize {
    let absolute_min = min_sizer.max(survivors.saturating_add(1));
    let absolute_max = max_sizer.max(absolute_min);
    let min_eden = absolute_min - survivors.min(absolute_min);
    let max_eden = absolute_max - survivors.min(absolute_max);
    let eden = if base_us >= goal_us {
        min_eden
    } else if region_ns == 0 {
        max_eden
    } else {
        let fit = (goal_us - base_us).saturating_mul(1_000) / region_ns;
        usize::try_from(fit).unwrap_or(usize::MAX).clamp(min_eden, max_eden)
    };
    survivors
        .saturating_add(eden)
        .clamp(absolute_min, absolute_max)
}

/// gci i15/bg — `CRATONVM_G1_YOUNG_TARGET_BASE_AWARE`, default OFF: give the
/// DEFAULT young-target loop HotSpot's "base time is longer than pause time"
/// case (`G1Collector::update_young_target_base_aware`,
/// [`base_aware_young_target`]). Without it the loop takes 20 % off on every
/// overrun whatever the overrun is made of, so a pause whose young-independent
/// cost alone exceeds `MaxGCPauseMillis` walks the target to its floor one
/// step per pause (vi7-g1y def_r1: 307 -> 25 in 12 pauses), and one between
/// 50 % and 100 % of the goal never regrows. A pause-shape policy, hence
/// opt-in until its rows (`vi15-bg.list`) decide. Read per record, not
/// latched, so tests can set it with `with_thread_overrides`.
pub fn g1_young_target_base_aware() -> bool {
    false
}

/// gci i22/sz -- `CRATONVM_G1_YOUNG_RESERVES_MIXED_MIN_OLD`, default OFF
/// (finding 4 of `gci-i20ys-g1-young-model-and-mixed-futile-reset-as-default-review-findings-RETIRED-UNFIXED-20261018`):
/// on the HotSpot young model, size Eden before a mixed pause in what the
/// goal leaves after the base AND the minimum old collection set (HotSpot 25
/// `calculate_desired_eden_length_before_mixed`). A pause-shape change, hence
/// opt-in until `vi22-sz.list` MR decides. Read per record and at `cleanup`
/// (not latched), so tests can set it with `with_thread_overrides`.
pub fn g1_young_reserves_mixed_min_old() -> bool {
    false
}

/// gci i22/sz -- HotSpot 25 `calculate_desired_eden_length_before_mixed`'s
/// old term, in microseconds. Pure.
///
/// * `min_old = min(calc_min_old_cset_length, remaining)`: HotSpot's minimum
///   is `ceil(candidates at the mark's end / G1MixedGCCountTarget)`
///   ([`mixed_choice::min_old_regions`], constant over the phase), capped at
///   the candidates still left (`candidates - selected`, the regions the
///   phase has already taken);
/// * each region is priced at the census' mean live bytes times the old copy
///   price (`ns_per_byte`), as HotSpot sums `predict_group_total_time_ms`
///   over the first `min_old` candidates.
///
/// 0 with no candidates, none left, or no price.
pub fn mixed_min_old_reserve_us(
    candidates: usize,
    selected_since: u64,
    count_target: u8,
    live_bytes: u64,
    ns_per_byte: u64,
) -> u64 {
    if candidates == 0 {
        return 0;
    }
    let remaining = (candidates as u64).saturating_sub(selected_since);
    let min_old = (mixed_choice::min_old_regions(candidates, count_target) as u64).min(remaining);
    let per_region = live_bytes / candidates as u64;
    min_old
        .saturating_mul(per_region)
        .saturating_mul(ns_per_byte)
        / 1_000
}

/// gci i15/bg — the default loop's next young target with the predicted base
/// split out (HotSpot 25 `calculate_young_desired_length` for the
/// base-over-goal case, the default loop's steps otherwise). Pure; regions.
///
/// * `floor = max(min, young_after_pause + 1)`, capped at `max` --
///   HotSpot's `absolute_min_young_length` (`G1NewSizePercent` of the heap,
///   and at least one Eden region beyond the young regions the pause left).
///   Every result is at or above it;
/// * `base_us >= goal_us` -> `floor`: the minimum, at once, and no lower
///   (equal to `hotspot_young_target(min, max, young_after_pause, goal, base,
///   _)` capped at `max`);
/// * `pause_us > goal_us` (base under the goal) -> 20 % off;
/// * `pause - base` under half of `goal - base` -> `+ cur / 8 + 1`, capped at
///   `max` (with `base_us == 0` this is the default loop's `2 * pause < goal`);
/// * otherwise `cur`.
pub fn base_aware_young_target(
    cur: usize,
    min: usize,
    max: usize,
    young_after_pause: usize,
    goal_us: u64,
    pause_us: u64,
    base_us: u64,
) -> usize {
    let floor = young_target_absolute_min(min, max, young_after_pause);
    if base_us >= goal_us {
        return floor;
    }
    let next = if pause_us > goal_us {
        cur.saturating_mul(4) / 5
    } else if pause_us.saturating_sub(base_us).saturating_mul(2) < goal_us - base_us {
        cur.saturating_add(cur / 8 + 1).min(max)
    } else {
        cur
    };
    next.max(floor)
}



/// gci i19/ys — HotSpot's `absolute_min_young_length` in this collector's
/// census: `max(min, young_after_pause + 1)` (the `G1NewSizePercent` floor,
/// and one Eden region beyond the young regions the pause left), capped at
/// the ceiling `max` (HotSpot's `absolute_max = max(max_by_sizer,
/// absolute_min)`; a target at the ceiling leaves the young-target arm inert
/// and the Free arms in charge). Pure; regions.
#[inline]
pub fn young_target_absolute_min(min: usize, max: usize, young_after_pause: usize) -> usize {
    min.max(young_after_pause.saturating_add(1)).min(max)
}

/// gci i20/ys — `CRATONVM_G1_YOUNG_SURVIVAL_YOUNG_ONLY`, default OFF
/// (`gci-i19ys-g1-young-sizing-residuals-FIXED-20261017` item 1): the young
/// survival ratio behind the evacuation-headroom arm of `needs_gc`
/// (`young_survival_permille`, [`G1Collector::young_exceeds_evac_headroom`])
/// is sampled only from young-only pauses, over the bytes young held (Eden
/// handed out since the last pause plus the packed survivor footprint).
/// Default: every pause but a humongous compaction samples, over every young
/// region counted full -- so a mixed pause's or a full compaction's OLD copies
/// read as young survival (the EMA takes a higher sample whole) and the
/// pinned-kept / sparse young regions dilute a young-only pause's ratio.
/// HotSpot predicts survivors per Eden region from young regions only
/// (`G1SurvRateGroup`). A pause-shape change (when the headroom arm fires),
/// hence opt-in until its rows decide. Read per pause.
pub fn g1_young_survival_young_only() -> bool {
    false
}



/// gci i20/ys — the switch arm's survival denominator: Eden handed out since
/// the last pause plus the last census's packed survivor footprint (whole
/// regions), never above the gross `young regions x region size`. Pure.
#[inline]
pub fn young_survival_denominator(
    gross: usize,
    eden_since_pause: usize,
    surv_equiv_regions: usize,
    region_size: usize,
) -> usize {
    eden_since_pause
        .saturating_add(surv_equiv_regions.saturating_mul(region_size))
        .min(gross)
}

/// gci i20/ys — the denominator a finished pause folds into the young
/// survival ratio, or `None` for no sample. A humongous compaction never
/// samples (both arms, as before). Switch off: the gross count, every other
/// pause. Switch on: young-only pauses only, over `net`. Pure.
#[inline]
pub fn young_survival_sample(
    young_only_switch: bool,
    compacting_humongous: bool,
    young_only_pause: bool,
    gross: usize,
    net: usize,
) -> Option<usize> {
    if compacting_humongous {
        return None;
    }
    if !young_only_switch {
        return Some(gross);
    }
    if !young_only_pause {
        return None;
    }
    Some(net)
}



/// gci i20/ys — HotSpot 25 `G1Policy::need_to_start_conc_mark`'s level test:
/// `old + request rounded up to whole regions > threshold` (a humongous object
/// uses whole regions). A zero threshold (IHOP not armed) never fires. Pure.
#[inline]
pub fn humongous_request_crosses_ihop(
    old_bytes: usize,
    request_bytes: usize,
    region_size: usize,
    threshold: usize,
) -> bool {
    if threshold == 0 {
        return false;
    }
    let region = region_size.max(1);
    let request = request_bytes.div_ceil(region).saturating_mul(region);
    old_bytes.saturating_add(request) > threshold
}

/// gci i26/hu — HotSpot 25's `InitiatingHeapOccupancyPercent` default (45),
/// the initial `G1AdaptiveIHOPControl` threshold HotSpot's humongous request
/// test (`need_to_start_conc_mark`) compares against before the model has
/// samples. Our collector's own default is 70 (`G1CollectorConfig::default`,
/// T19.3.G1), which on the `GciOo19NativeStringHumongousProbe` shape (G1
/// `-Xmx64m`, 800 000-byte Strings dying young) is never reached: the
/// Free-fraction young pause runs at ~35 MiB used, under 70 % (44.8 MiB),
/// where HotSpot crosses 45 % (28.8 MiB) at the ~27th String.
pub const G1_HOTSPOT_DEFAULT_IHOP_PERCENT: usize = 45;

/// gci i26/hu — the threshold the opt-in humongous request test uses: the
/// collector's own marking threshold, held to HotSpot's 45 % of the region
/// grid when the IHOP percent is still our default (`default_percent`), i.e.
/// no `-XX:InitiatingHeapOccupancyPercent` changed it. An explicit percent
/// is HotSpot's own answer and is used unchanged (as `marking_threshold`
/// already carries it). Residual: an explicit `=70` reads as the default
/// (the config does not record explicitness). A zero `marking_threshold`
/// (IHOP not armed) stays zero. Pure.
#[inline]
pub fn humongous_request_ihop_threshold(
    marking_threshold: usize,
    grid_bytes: usize,
    ihop_percent: u8,
    default_percent: u8,
) -> usize {
    if marking_threshold == 0 || ihop_percent != default_percent {
        return marking_threshold;
    }
    let hotspot = grid_bytes / 100 * G1_HOTSPOT_DEFAULT_IHOP_PERCENT
        + grid_bytes % 100 * G1_HOTSPOT_DEFAULT_IHOP_PERCENT / 100;
    marking_threshold.min(hotspot.max(1))
}

/// gci i26/hu — HotSpot's `non_young_capacity_bytes()` for the humongous
/// request test: the regions that are neither Free nor young (Old +
/// Humongous), in bytes, from the maintained counters (no table walk; the
/// humongous claim books its span in `free_region_count` under the exclusive
/// guard). Never below `old_gen_bytes` (our byte count of Old + Humongous), so
/// a counter drift can only make the test fire later than the byte count
/// would, never earlier than HotSpot's region count. Pure.
#[inline]
pub fn humongous_request_occupancy(
    table_regions: usize,
    free_regions: usize,
    young_regions: usize,
    region_size: usize,
    old_gen_bytes: usize,
) -> usize {
    table_regions
        .saturating_sub(free_regions)
        .saturating_sub(young_regions)
        .saturating_mul(region_size)
        .max(old_gen_bytes)
}

/// gci i8/gs — `CRATONVM_G1_YOUNG_TARGET_NET_OF_HEAP_COSTS`, default OFF:
/// feed the young pause-goal loop (`update_young_target`) the pause NET of
/// `verify_us` and `fixup_us` (`gci-i7orch-g1-young-sizing-and-door-label-review-findings`
/// item 2).
///
/// Both phases scale with the old generation, not with young: the fix-up is
/// the whole-heap reference walk (45-61 ms median on G1 4-thread javac
/// -Xmx512m, 89 ms at 1 GiB, vi5g-g1f), the verifier is a fixed budget or,
/// under `--verbose:gc` before gci i7/g1y, a whole-heap sweep. Charged to the
/// young target they drive it to its floor while no young size can bring the
/// pause under the goal. Read once per pause record, not latched, so tests
/// can set it with `with_thread_overrides`. A pause-shape policy (bigger young
/// generation, fewer pauses), hence opt-in until its rows decide.
pub fn g1_young_target_net_of_heap_costs() -> bool {
    false
}

/// The pause the young-target loop is fed: `pause_us`, or with `net` the pause
/// minus the verifier and the whole-heap fix-up (saturating). Pure.
#[inline]
pub fn young_target_sample_us(pause_us: u64, phases: &G1PausePhases, net: bool) -> u64 {
    if !net {
        return pause_us;
    }
    pause_us
        .saturating_sub(phases.verify_us)
        .saturating_sub(phases.fixup_us)
}

/// gci i8/gs — `CRATONVM_G1_YOUNG_TARGET_FUTILE_BELOW_REGION`, default OFF:
/// the young pause-goal loop's anti-storm reset (target back to its ceiling)
/// fires on a pause that freed less than one region, not only on one that
/// copied and freed exactly nothing (`gci-i7orch-...` item 3).
///
/// vi5g-g1f J r1 pause 2835 freed 69 104 bytes, counted as productive, and
/// still tightened the target; `note_pause_productivity` has called a
/// less-than-one-region pause futile for the Free arms since 2026-09-30. Read
/// once per pause record, not latched. A sizing policy, hence opt-in.
pub fn g1_young_target_futile_below_region() -> bool {
    false
}

/// Is this young or mixed pause futile for the young-target loop? Exactly
/// nothing copied and nothing freed; with `below_one_region`, any pause that
/// freed less than `region_size` bytes (a superset). Pure.
#[inline]
pub fn young_target_pause_futile(stats: &GcStats, region_size: usize, below_one_region: bool) -> bool {
    if below_one_region {
        stats.bytes_freed < region_size
    } else {
        stats.objects_copied == 0 && stats.bytes_freed == 0
    }
}

/// gci i8/gs — `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS`, default OFF: the young
/// floor of the low-Free triggers (`low_free_trigger_allowed`: the
/// Free-fraction and evacuation-headroom arms of `needs_gc` and the
/// native-pressure latch) counts young NET of the regions the last young or
/// mixed pause kept under a pin (`gci-i7orch-...` item 4).
///
/// On a nearly full heap ~20 pinned young regions satisfy `young >= free/4`
/// with no collectable young at all, which is the futile-pause shape the
/// floor exists to refuse. The evacuation-headroom predicate itself keeps the
/// gross count: a pin released since the last pause makes its region
/// collectable again, and to-space must be reserved for it. Latched like the
/// file's other `needs_gc` switches (it is read on the allocation path).
///
/// gci i12/sx: latched per COLLECTOR, not per process: read once at
/// construction into `G1Collector::free_trigger_net_of_pins`, so the
/// allocation path pays one field load and a collector built under a test's
/// `with_thread_overrides` carries that test's answer.
pub fn g1_free_trigger_net_of_pins() -> bool {
    false
}

/// The young count the low-Free floor compares under
/// `CRATONVM_G1_FREE_TRIGGER_NET_OF_PINS`: `young` minus the regions the last
/// pause kept pinned, saturating. Pure.
#[inline]
pub fn free_trigger_young(young: usize, pinned_kept: usize) -> usize {
    young.saturating_sub(pinned_kept)
}

/// `CRATONVM_G1_FREE_TRIGGER_YOUNG_FLOOR=0` -- let the low-Free triggers
/// (`needs_gc`'s Free-fraction and evacuation-headroom arms, and the
/// native-pressure latch) fire whatever the young generation's size, the
/// behaviour before 2026-09-30 (see the note at the Free-fraction arm).
/// Default on.
pub fn g1_free_trigger_young_floor() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// `CRATONVM_G1_RSET_DEAD_HOLDER_FILTER=0` -- never skip a remembered-set
/// holder as proven dead (see [`MarkProvenDead`]); every holder of a source
/// region is scanned. Default on.
pub fn g1_rset_dead_holder_filter() -> bool {
    None::<std::ffi::OsString>
        .and_then(|v| v.into_string().ok())
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !(v == "0" || v == "false" || v == "off" || v == "no")
        })
        .unwrap_or(true)
}

/// `CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG=0` -- let the marker's gray
/// gate ([`classify_mark_scan_target`]) accept a header without
/// `GC_FLAG_HEADER` again, i.e. decode-only, the gate before r11w19 g1store
/// (proposal W18-1). Default ON. Value-parsed like [`g1_mark_root_screen`].
pub fn g1_mark_gray_requires_header_flag() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// `CRATONVM_G1_MARK_CAP_SEED_SCREEN=0` -- let the gray-set cap arms mark a
/// seed black without first asking the gray gate, the behaviour before r11w19
/// g1store (proposal W18-3; see [`G1Collector::cap_arm_seed_passes_gray_gate`]),
/// default ON, value-parsed like [`g1_mark_root_screen`].
pub fn g1_mark_cap_seed_screen() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        None::<std::ffi::OsString>
            .and_then(|v| v.into_string().ok())
            .map(|v| {
                let v = v.trim().to_ascii_lowercase();
                !(v == "0" || v == "false" || v == "off" || v == "no")
            })
            .unwrap_or(true)
    })
}

/// r11w18 g1store — [`G1Collector::mark_root_seed`]'s answer for one root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkRootSeed {
    /// Push this object START (the root itself, or the object containing it).
    Object(usize),
    /// The root names no allocated object; push nothing.
    Nothing,
    /// The root is inside allocated memory the grid cannot place; push
    /// nothing, and the caller takes the cleanup fail-safe.
    Unresolved,
}

/// r11w18 g1store — where an address falls in its region's object grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridPlace {
    /// The address is an object start.
    Start,
    /// The address is inside the object starting at this REGION OFFSET.
    Interior(usize),
    /// The address is in a TLAB filler, a gap sentinel, a skip span, or past
    /// the grid.
    Unallocated,
    /// The walk met bytes that are not a header before reaching the address.
    Desync,
    /// The address is in (or at) a decodable header that lacks
    /// `GC_FLAG_HEADER`, so it is not a published object start (r12w1
    /// g1store, proposal W19-3). Seeding it would push a gray the marker's
    /// gate refuses as torn one pop later; the seed is `Unresolved` instead,
    /// which reaches the same cleanup fail-safe without the push.
    Flagless,
}

/// r11w18 g1store — one region's object grid, walked once per `remark` call.
pub struct MarkRootGrid {
    /// The region's index.
    pub idx: usize,
    /// `(start, end, published)` region offsets of every OBJECT the walk met,
    /// in address order, and whether its header carries `GC_FLAG_HEADER`
    /// (always `true` under `CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG=0`).
    /// TLAB fillers, gap sentinels and skip spans are not objects and do not
    /// appear.
    pub objects: Vec<(usize, usize, bool)>,
    /// The offset the walk reached.
    pub reached: usize,
    /// The walk reached the cursor without meeting bytes it could not decode.
    pub complete: bool,
}

impl MarkRootGrid {
    pub fn place(&self, off: usize) -> GridPlace {
        if off >= self.reached {
            return if self.complete {
                GridPlace::Unallocated
            } else {
                GridPlace::Desync
            };
        }
        let pp = self.objects.partition_point(|&(s, _, _)| s <= off);
        match pp.checked_sub(1).map(|i| self.objects[i]) {
            Some((s, e, published)) if off < e => {
                if !published {
                    GridPlace::Flagless
                } else if off == s {
                    GridPlace::Start
                } else {
                    GridPlace::Interior(s)
                }
            }
            _ => GridPlace::Unallocated,
        }
    }
}

/// r11w18 g1store — the grids one `remark` call has walked, plus the TLAB
/// skip spans they were walked against (fetched on first use).
#[derive(Default)]
pub struct MarkRootGrids {
    pub skips: Option<Vec<(usize, usize)>>,
    pub walked: Vec<MarkRootGrid>,
}

/// Orphans per remark that [`G1Collector::jit_pin_mark_seeds`] prints with
/// their provenance under `CRATONVM_DBG_JIT_ROOTSCAN=1`.
pub const JIT_PIN_ORPHAN_SAMPLES: usize = 16;

/// r12w2 g1store — what [`G1Collector::jit_pin_mark_seeds`] made of one
/// remark's JIT pin snapshot. Counts of DISTINCT pins.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub struct JitPinSeedTally {
    /// Distinct published pins.
    pub pins: usize,
    /// Pins whose object a root of this pause already seeds.
    pub covered: usize,
    /// Pins placed inside an object rather than at its start.
    pub interior: usize,
    /// Pins that name no allocated object (a Free region, past the cursor, a
    /// filler).
    pub nothing: usize,
    /// Pins the grid could not place (seeded nothing; `remark` takes the
    /// cleanup fail-safe for them, as for a root).
    pub unresolved: usize,
    /// Pins whose object no root names: seeded by the pin alone.
    pub orphans: usize,
    /// Orphans that are humongous objects.
    pub orphan_humongous: usize,
    /// Orphans reached through an interior word (a derived pointer).
    pub orphan_interior: usize,
}

impl JitPinSeedTally {
    pub fn report(&self) {
        tracing::debug!("[g1] remark JIT pin seeds: {self:?}");
        if g1_dbg_jit_rootscan() {
            eprintln!(
                "[g1-jitpin-census] pins={} covered={} interior={} nothing={} unresolved={} \
                 orphans={} orphan_humongous={} orphan_interior={}",
                self.pins,
                self.covered,
                self.interior,
                self.nothing,
                self.unresolved,
                self.orphans,
                self.orphan_humongous,
                self.orphan_interior,
            );
        }
    }
}

/// Regions one [`G1Collector::remark`] call may walk to place roots that are
/// not published object starts. A root past the budget is `Unresolved`, which
/// retains every region for the cycle: conservative, and a signal worth a look
/// if it ever appears in steady state. Each walk is O(region) and the result
/// is reused for every later root in the same region.
pub const MARK_ROOT_GRID_REGION_BUDGET: usize = 64;

/// Roots [`G1Collector::mark_root_seed`] had to place through a region grid.
/// Read only to throttle its debug line (the count is the line's `#n`).
pub static MARK_ROOT_PLACED: AtomicUsize = AtomicUsize::new(0);

// LANE W2-C (2026-09-20) removed `plausible_mark_scan_target`, a one-line
// `classify_mark_scan_target(..).is_ok()` wrapper that had no callers anywhere
// in the workspace -- only two doc comments naming it as the concept, both now
// re-pointed at `classify_mark_scan_target`, which is the gate they were
// describing. No dead-code warning fired for it, so nothing would have caught
// it either. The tombstone is here so the next reader of those comments'
// history does not re-add a third form of the same test.

/// The marker's gray gate: may `obj_addr` be scanned as an object start in
/// `region`? See [`GrayRefusal`] for what each refusal means.
///
/// `flagless` is the collector's `mark_gray_without_header_flag` counter,
/// bumped for each refusal of a header without `GC_FLAG_HEADER`.
pub fn classify_mark_scan_target(
    region: &G1Region,
    obj_addr: usize,
    flagless: &AtomicU64,
) -> Result<(), GrayRefusal> {
    if obj_addr & 0x7 != 0 {
        // Every object start is 8-aligned, so this address was never one.
        return Err(GrayRefusal::NotAllocated);
    }
    // A humongous object begins at offset 0 of its `HumongousStart` region;
    // its continuations carry `cursor = 0` and hold only payload. A reference
    // names an object START, so a gray landing in a continuation is an
    // interior or stale address and never a live object.
    if region.region_type() == RegionType::HumongousContinuation {
        return Err(GrayRefusal::NotAllocated);
    }
    let base = region.data.as_ptr() as usize;
    let off = obj_addr.wrapping_sub(base);
    // Header must be fully inside the allocated prefix. (Real objects
    // satisfy start + size <= cursor, so start + HEADER_SIZE <= cursor.)
    if off
        .checked_add(HEADER_SIZE)
        .is_none_or(|end| end > region.cursor())
    {
        return Err(GrayRefusal::NotAllocated);
    }
    // r11w19 g1store (proposal W18-1) -- the header must carry
    // `GC_FLAG_HEADER`. Every allocator stamps it and nothing clears it, and it
    // is exactly the bit a field word lacks: the `obj + 8` word of a compact
    // instance (a reference field) reads as a flag-less NEUTRAL legacy header,
    // which the size decode below accepts (flags `0` are a subset of the known
    // flags), and the marker then walked `num_slots` 16-byte "cells" across the
    // object and its neighbours. Refused as `TornHeader`, never `NotAllocated`:
    // the address IS inside the allocated prefix, and a real object that
    // somehow lacked the flag must cost the cycle its reclamation (the cleanup
    // fail-safe), never its liveness -- the flag's own rule that its absence is
    // never read as "not a live object" where a wrong answer frees memory.
    // `CRATONVM_G1_MARK_GRAY_REQUIRES_HEADER_FLAG=0` restores the decode-only
    // gate.
    if g1_mark_gray_requires_header_flag() {
        // SAFETY: the header span was just confirmed inside this region's
        // allocated prefix; the mark word is read atomically.
        let flags = unsafe { (*(obj_addr as *const ObjectHeader)).gc_flags() };
        if flags & cratonvm_types::GC_FLAG_HEADER == 0 {
            flagless.fetch_add(1, Ordering::Relaxed);
            return Err(GrayRefusal::TornHeader);
        }
    }
    // SAFETY: the header span was just confirmed inside this region's
    // allocated prefix; the validator reads it field-by-field unaligned.
    let Some(size) =
        cratonvm_gc_core::concurrent_mark::concurrent_mark_object_size(obj_addr as *const ObjectHeader)
    else {
        // Inside the allocated prefix and undecodable: a real torn header.
        return Err(GrayRefusal::TornHeader);
    };
    if region.region_type() != RegionType::HumongousStart
        && off
            .checked_add(size)
            .is_none_or(|end| end > region.cursor())
    {
        // The header decodes but claims an extent running past everything this
        // region has ever handed out. Also a torn header, not a stale address.
        return Err(GrayRefusal::TornHeader);
    }
    Ok(())
}

/// gci i16/mr — a region's TAMS (as an offset from `base`) at the start of a
/// marking cycle: its `cursor`, lowered to the start of the lowest published
/// un-retired TLAB tail (`spans`, absolute `[start, end)` pairs, the
/// [`G1Collector::set_jit_tlab_skip_regions`] set the initial mark's take-over
/// just published) that begins inside `[base, base + cursor)`.
///
/// A tail is carved out of the region below its cursor, but its owner (a peer
/// the take-over froze in compiled code) keeps bumping it after the pause:
/// those objects postdate the snapshot, are neither marked nor SATB-logged,
/// and with TAMS at the cursor `is_live_after_mark` read them dead at a remark
/// that no young pause preceded (the `System.gc()` full cycle). From the tail's
/// start up they are implicitly live, as HotSpot's young regions are. Bytes
/// between a tail and the cursor that predate the snapshot become implicitly
/// live too: an over-retention for one cycle, never an under-mark. Spans
/// outside the region (another region, another heap) change nothing.
pub fn tams_below_reserved_tails(base: usize, cursor: usize, spans: &[(usize, usize)]) -> usize {
    spans
        .iter()
        .filter(|&&(s, e)| s < e && s >= base && s - base < cursor)
        .map(|&(s, _)| s - base)
        .fold(cursor, usize::min)
}

/// gci i17/rv (finding 1 of `gci-i17rv-i16-default-path-review-findings-RETIRED-UNFIXED-20261018`)
/// -- push onto `out` every published object start in `[base + from, base +
/// cursor)` outside the reserved tails `spans`: the objects a TAMS lowered by
/// [`tams_below_reserved_tails`] puts ABOVE TAMS although they predate the
/// snapshot (carved after the frozen tail, before the pause). Above TAMS they
/// are implicitly live, so the marker must trace them or what only they name
/// is freed at cleanup while they stay reachable through a weak reference.
/// Same skip / gap / filler / extent rules as [`scrub_dead_refs_below_tams`];
/// answers whether the walk reached `cursor` (`false`: it stopped at an extent
/// it could not trust, and the rest of the region was not examined).
///
/// # Safety
/// Stop-the-world, regions guard held; `cursor` is the region's cursor and
/// `from <= cursor`.
pub unsafe fn pre_snapshot_starts_above_lowered_tams(
    base: usize,
    from: usize,
    cursor: usize,
    spans: &[(usize, usize)],
    out: &mut Vec<usize>,
) -> bool {
    let mut offset = from;
    while offset < cursor {
        let addr = base + offset;
        if let Some(skip) = jit_tlab_skip_span_len(spans, addr) {
            offset += skip;
            continue;
        }
        if let Some(gap) = gap_filler_len(addr as *const u8) {
            offset += gap;
            continue;
        }
        // SAFETY: `offset < cursor`, inside the region's allocated prefix.
        let header = unsafe { &*(addr as *const ObjectHeader) };
        if is_humongous_filler(header) {
            return false;
        }
        let size = object_total_size(header);
        if size < cratonvm_types::MIN_OBJECT_SIZE || offset + size > cursor {
            return false;
        }
        if !is_tlab_filler_header(header) {
            out.push(addr);
        }
        offset += size;
    }
    true
}

/// g1pin: why a captured peer word could not be resolved to an object start,
/// so that its region is pinned whole
/// (`G1Collector::peer_words_to_object_starts`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerWordFallback {
    /// The region is not Eden, Survivor or Old (humongous or Free).
    NotWalkable = 0,
    /// The word lies at or above the region's cursor.
    AboveCursor = 1,
    /// The walk met a stretch it cannot parse before the word.
    Unparseable = 2,
}

impl PeerWordFallback {
    pub const COUNT: usize = 3;
}

/// g1pin: one step of the peer-word region walk
/// (`G1Collector::peer_words_to_object_starts`).
pub enum PeerWalkStep {
    /// An object of this many bytes; a word inside it pins it.
    Object(usize),
    /// A frozen peer's TLAB tail, a gap filler or a TLAB filler of this many
    /// bytes; a word inside it names no object and pins nothing.
    NoObject(usize),
    /// The walk cannot go on (a humongous filler or an implausible size).
    Stop,
}

impl PeerWalkStep {
    /// `(bytes, whether a word inside pins an object)`, or `None` for `Stop`.
    pub fn extent(self) -> Option<(usize, bool)> {
        match self {
            PeerWalkStep::Object(len) => Some((len, true)),
            PeerWalkStep::NoObject(len) => Some((len, false)),
            PeerWalkStep::Stop => None,
        }
    }
}

/// g1pin: `(base, cursor)` of a region the peer-word walk may resolve words
/// in: Eden, Survivor or Old. Humongous and Free regions keep the region pin.
pub fn peer_walkable_extent(region: &G1Region) -> Option<(usize, usize)> {
    matches!(
        region.region_type(),
        RegionType::Eden | RegionType::Survivor | RegionType::Old
    )
    .then(|| (region.data.as_ptr() as usize, region.cursor()))
}

/// g1pin: classify the `room` bytes at `addr`, an object boundary of a region
/// walk below its cursor, inside a stop-the-world pause.
pub fn peer_walk_step(addr: usize, room: usize, jit_skips: &[(usize, usize)]) -> PeerWalkStep {
    if let Some(skip) = jit_tlab_skip_span_len(jit_skips, addr) {
        return PeerWalkStep::NoObject(skip);
    }
    if let Some(gap) = gap_filler_len(addr as *const u8) {
        return PeerWalkStep::NoObject(gap);
    }
    // SAFETY: the caller's walk is at an object boundary below the cursor.
    let header = unsafe { &*(addr as *const ObjectHeader) };
    if is_humongous_filler(header) {
        return PeerWalkStep::Stop;
    }
    let size = object_total_size(header);
    if size < cratonvm_types::MIN_OBJECT_SIZE || size > room {
        return PeerWalkStep::Stop;
    }
    if is_tlab_filler_header(header) {
        PeerWalkStep::NoObject(size)
    } else {
        PeerWalkStep::Object(size)
    }
}

/// INT-3 — if `addr` falls inside a published frozen-peer TLAB tail (see
/// [`G1Collector::set_jit_tlab_skip_regions`]), return the distance to the
/// span's end so the walker strides past it. The span is reserved,
/// UNINITIALIZED memory below the region cursor: it carries no walkable
/// filler (its owner was frozen mid-JIT before its safepoint retire), so it
/// must be skipped BEFORE any header/sentinel byte is interpreted — random
/// tail bytes could even alias `GAP_FILLER_CLASS_ID` and desync the walk. A
/// linear walk lands exactly on a span's start (objects fill the TLAB
/// contiguously up to `cursor`), but the check is range-based rather than
/// exact-start as defense-in-depth. `spans` is empty on every cycle without
/// frozen/blocked un-retired TLABs, making this a length check per object.
#[inline]
pub fn jit_tlab_skip_span_len(spans: &[(usize, usize)], addr: usize) -> Option<usize> {
    spans
        .iter()
        .find(|&&(s, e)| addr >= s && addr < e)
        .map(|&(_, e)| e - addr)
}

/// Whether a desync landing address is covered by a published frozen-peer TLAB
/// skip span, and how far it sits from the nearest one.
///
/// This is the question that separates the two candidate producers of an
/// unwalkable hole. If the address IS inside a published span, some live
/// thread owns that memory and the walkers simply were not told in time. If it
/// is NOT — and especially if it sits a small fixed distance past a span's end
/// — the hole belongs to no live thread, which points at a TLAB whose tail was
/// never filled (an abandoned/`Tlab::new`-trimmed span) rather than a walker
/// gap. Printing the neighbourhood rather than a bare yes/no is deliberate: the
/// interesting case so far is a gap of a few bytes adjacent to a legitimate
/// span, which a boolean would hide.
pub fn describe_skip_coverage(
    spans: &[(usize, usize)],
    addr: usize,
    region_base: *mut u8,
    region_cursor: usize,
) -> String {
    let off = addr.saturating_sub(region_base as usize);
    if let Some(&(s, e)) = spans.iter().find(|&&(s, e)| addr >= s && addr < e) {
        return format!(
            "addr={addr:#x} off={off:#x} INSIDE published skip span [{s:#x},{e:#x}) \
             (len={}) — a live thread owns this memory",
            e - s
        );
    }
    // Nearest span by absolute distance to either endpoint, so an adjacent
    // sliver shows up as `dist=8` rather than as "uncovered".
    let nearest = spans
        .iter()
        .map(|&(s, e)| {
            let d = if addr < s {
                s - addr
            } else {
                addr.saturating_sub(e)
            };
            (d, s, e)
        })
        .min_by_key(|(d, _, _)| *d);
    match nearest {
        Some((d, s, e)) => format!(
            "addr={addr:#x} off={off:#x} region_cursor={region_cursor:#x} NOT covered by any of \
             {} published skip span(s); nearest [{s:#x},{e:#x}) dist={d:#x}",
            spans.len()
        ),
        None => format!(
            "addr={addr:#x} off={off:#x} region_cursor={region_cursor:#x} NOT covered — \
             ZERO published skip spans this pause"
        ),
    }
}

/// 32 bytes before and 48 bytes from a walk-break offset, as 8-byte words.
///
/// The break offset alone cannot distinguish "these bytes are garbage" from
/// "these bytes are a real object header the walker arrived at off-grid". The
/// words on either side settle it: a real header shows a plausible class_id in
/// the low dword of word 0, and the preceding words show whether the zero run
/// that led here is 8, 16 or more bytes long.
pub fn hexdump_around(base: *mut u8, cursor: usize, offset: usize) -> String {
    let lo = offset.saturating_sub(32);
    let hi = (offset + 48).min(cursor);
    let mut out = String::from("bytes[");
    let mut o = lo & !7;
    while o + 8 <= hi {
        // SAFETY: `[lo, hi)` is clamped to `[0, cursor)`, inside the region's
        // committed data; reads are 8-aligned and within the same allocation.
        let w = unsafe { std::ptr::read_unaligned(base.add(o) as *const u64) };
        if o == offset {
            out.push_str(&format!(" >>{o:#x}={w:#018x}<<"));
        } else {
            out.push_str(&format!(" {o:#x}={w:#018x}"));
        }
        o += 8;
    }
    out.push(']');
    out
}

/// How many walked objects [`WalkTrail`] keeps.
pub const WALK_TRAIL_LEN: usize = 8;

/// The last few objects a linear region walk consumed, so a break can name the
/// step that desynchronized the grid instead of only the offset it landed on.
///
/// A linear walk derives each step from the previous object's RECORDED size. So
/// when the walk lands on bytes that are not a header, the defect is almost
/// never at that offset — it is the last object whose recorded size understated
/// (or overstated) the space it actually occupies. Printing only the landing
/// offset, as the two break sites did, names the victim and hides the culprit.
///
/// Recorded only under `CRATONVM_G1_DBG_REACH=1`; one branch per object
/// otherwise, and nothing is rendered unless a walk actually breaks.
#[derive(Default)]
pub struct WalkTrail {
    /// `(offset, class_id, kind_tag, recorded_size)`, oldest-to-newest once
    /// wrapped. Fixed-size so the trail cannot allocate inside a GC pause.
    pub entries: [(usize, u32, u8, usize); WALK_TRAIL_LEN],
    pub len: usize,
    pub next: usize,
}

impl WalkTrail {
    #[inline]
    pub fn record(&mut self, offset: usize, class_id: u32, kind: u8, size: usize) {
        self.entries[self.next] = (offset, class_id, kind, size);
        self.next = (self.next + 1) % WALK_TRAIL_LEN;
        self.len = (self.len + 1).min(WALK_TRAIL_LEN);
    }

    /// Oldest-first rendering of the trail, `off=+size(cid,kind)` per step, so
    /// the arithmetic that produced the landing offset can be checked by eye.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let start = if self.len == WALK_TRAIL_LEN {
            self.next        } else {
            0
        };
        for i in 0..self.len {
            let (off, cid, kind, size) = self.entries[(start + i) % WALK_TRAIL_LEN];
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&format!("{off:#x}+{size:#x}(cid={cid},k={kind})"));
        }
        out
    }
}

/// How many cursor advances [`BumpTrail`] keeps per region.
///
/// Sized from the first run that used it: a 12-entry ring resolved `OWNER` on
/// none of four breaks, because a region takes hundreds of `obj:*` advances
/// after the `tlab:*` carve that actually committed the span, and they evict it.
/// Widening alone does not fix that (the ratio, not the depth, is the problem) —
/// see [`G1Region::tlab_trail`] for the half that does.
pub const BUMP_TRAIL_LEN: usize = 32;

pub static NEXT_BUMP_TID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// Dense per-thread id for [`BumpTrail`]. Not an OS tid — it exists only so
    /// a rendered trail can say "these two carves came from the SAME thread"
    /// without a syscall or an allocation on the allocation path.
    pub static BUMP_TID: u64 = NEXT_BUMP_TID.fetch_add(1, Ordering::Relaxed);
}

#[inline]
pub fn bump_tid() -> u64 {
    BUMP_TID.with(|t| *t)
}

/// One recorded cursor advance: who moved `region.cursor()`, from where, by how
/// much, and in which incarnation of the region.
#[derive(Default, Clone, Copy)]
pub struct BumpEntry {
    pub epoch: u64,
    pub offset: usize,
    pub size: usize,
    pub tid: u64,
    pub site: &'static str,
}

/// The last few [`G1Region::bump_alloc`] calls against one region.
///
/// [`WalkTrail`] names the last objects a broken walk CONSUMED; this names the
/// calls that COMMITTED the span the walk broke inside. The two answer different
/// halves of the same question, and only this half can distinguish the possible
/// producers of a committed-but-headerless span:
///
/// * `bump_alloc` is the only production writer that advances `cursor` upward
///   (`reset` sets 0, the humongous path sets `size`/0, and the parallel
///   evacuator's `retire_tlab` is opt-in and off), and it zero-fills exactly
///   what it hands out. So a zeroed span below `cursor` is always a chunk some
///   caller took and never wrote a header into.
/// * A per-object carve (`site="obj:*"`) is followed by a header write by the
///   same caller, so it can only leave a hole if that write did not happen.
/// * A TLAB carve (`site="tlab:*"`) is EXPECTED to be headerless above its
///   owner's private cursor — but then the owner must either retire it (filler)
///   or publish it (`reserved_tail`). A hole inside a `tlab:*` extent with zero
///   published skip spans names an owner that did neither.
///
/// Recorded only under `CRATONVM_G1_DBG_REACH=1`. The ring is fixed-size so it
/// never allocates under the regions lock, and entries carry `epoch` so a trail
/// surviving a [`G1Region::reset`] cannot be misread as describing the current
/// incarnation.
/// Ten-findings item 5: the ring is a `Vec` that stays EMPTY until the first
/// `record`, which only `CRATONVM_G1_DBG_REACH=1` ever makes.
///
/// It used to be an inline `[BumpEntry; BUMP_TRAIL_LEN]` — 32 entries of 48
/// bytes, twice per region (`bump_trail` and `tlab_trail`) — so every
/// `G1Region` carried 3 KiB of diagnostic ring whether or not the flag was on,
/// and every linear pass over the region table (the CSet filters, the census,
/// the humongous `any()` checks, the free-region searches, cleanup) streamed
/// megabytes of it. The region entry is now a few hundred bytes; see
/// `a_region_table_entry_stays_small`. Capacity is reserved once, on first use,
/// so the diagnostic path allocates exactly one buffer per ring, not one per
/// advance.
#[derive(Default)]
pub struct BumpTrail {
    pub entries: Vec<BumpEntry>,
    pub len: usize,
    pub next: usize,
}

impl BumpTrail {
    #[inline]
    pub fn record(&mut self, epoch: u64, offset: usize, size: usize, site: &'static str) {
        let entry = BumpEntry {
            epoch,
            offset,
            size,
            tid: bump_tid(),
            site,
        };
        if self.entries.len() < BUMP_TRAIL_LEN {
            if self.entries.is_empty() {
                self.entries.reserve_exact(BUMP_TRAIL_LEN);
            }
            self.entries.push(entry);
        } else {
            self.entries[self.next] = entry;
        }
        self.next = (self.next + 1) % BUMP_TRAIL_LEN;
        self.len = (self.len + 1).min(BUMP_TRAIL_LEN);
    }

    pub fn iter_oldest_first(&self) -> impl Iterator<Item = &BumpEntry> {
        let start = if self.len == BUMP_TRAIL_LEN {
            self.next        } else {
            0
        };
        (0..self.len).map(move |i| &self.entries[(start + i) % BUMP_TRAIL_LEN])    }

    /// Oldest-first rendering, `site@off+size(e=epoch,t=tid)` per advance.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (i, e) in self.iter_oldest_first().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            out.push_str(&format!(
                "{}@{:#x}+{:#x}(e={},t={})",
                e.site, e.offset, e.size, e.epoch, e.tid
            ));
        }
        out
    }

    /// The recorded advance whose extent contains `offset` in incarnation
    /// `epoch` — i.e. the call that committed the bytes a walk broke on.
    ///
    /// `None` is not "nobody committed it": the ring is short, so an old enough
    /// advance is simply gone. The rendering says which, so a missing owner is
    /// never read as evidence.
    pub fn owner_of(&self, epoch: u64, offset: usize) -> Option<&BumpEntry> {
        self.iter_oldest_first()
            .find(|e| e.epoch == epoch && offset >= e.offset && offset < e.offset + e.size)
    }

    /// `describe_owner` against `self` first, falling back to `other`.
    ///
    /// Used to ask the `tlab:*`-only ring before the shared one: a large hole's
    /// owner is a carve, and the shared ring is the one that loses it.
    pub fn describe_owner_or(&self, other: &BumpTrail, epoch: u64, offset: usize) -> String {
        if self.owner_of(epoch, offset).is_some() {
            return self.describe_owner(epoch, offset);
        }
        other.describe_owner(epoch, offset)
    }

    pub fn describe_owner(&self, epoch: u64, offset: usize) -> String {
        match self.owner_of(epoch, offset) {
            Some(e) => format!(
                "OWNER={} extent=[{:#x},{:#x}) size={:#x} tid={} epoch={}",
                e.site,
                e.offset,
                e.offset + e.size,
                e.size,
                e.tid,
                e.epoch
            ),
            None if self.len == 0 => {
                "OWNER=unknown (no bump advances recorded for this region)".to_string()
            }
            None => format!(
                "OWNER=unknown (not in the last {} advances; oldest kept starts at {:#x})",
                self.len,
                self.iter_oldest_first()
                    .next()
                    .map(|e| e.offset)
                    .unwrap_or(0)
            ),
        }
    }
}

/// TLAB-retire GAP sentinel probe (see `Tlab::retire`, tlab.rs): a
/// sub-`HEADER_SIZE` TLAB tail cannot hold a walkable `int[]` filler, so it
/// is stamped with `GAP_FILLER_CLASS_ID` at offset 0 and the exact gap
/// length at offset 4. Such a span is NOT a walkable object — its "kind"
/// byte is the low byte of the gap length. G1 refills TLABs from Eden
/// regions, so every linear region walker in this file must skip these
/// spans by their recorded length; treating one as an object header either
/// desyncs the stride or (via the defensive size check) `break`s the walk
/// and silently skips the REST of the region — fatal when the walk is a
/// pinned-region source scan or the Phase-4 reference fix-up (missed
/// evacuations / stale pointers). Returns the 8-aligned gap length when
/// `ptr` points at a gap sentinel.
///
/// SAFETY contract: caller guarantees `ptr` points at >= 8 readable bytes
/// inside a region (every walk loop checks `offset < cursor` first, and a
/// gap is always a trailing 8..=32-byte span fully inside the region).
#[inline]
pub fn gap_filler_len(ptr: *const u8) -> Option<usize> {
    let cid = unsafe { std::ptr::read(ptr as *const u32) };
    if cid == cratonvm_gc_core::tlab::GAP_FILLER_CLASS_ID.as_u32() {
        let len = unsafe { std::ptr::read((ptr as usize + 4) as *const u32) } as usize;
        // Defensive: round a corrupt length up to a positive 8-multiple so
        // the walk can never wedge in place.
        Some(((len + 7) & !7).max(8))
    } else {
        None
    }
}

/// True iff a region of this type can be a member of a collection set —
/// `Eden`/`Survivor` (every young or mixed CSet) or `Old` (a mixed CSet). The
/// What a Phase-4 walk learned about which humongous spans are still referenced.
///
/// Humongous spans are never evacuated, so nothing in a young or mixed pause
/// otherwise forms an opinion about whether they are alive. This is that
/// opinion, and it is deliberately shaped as a **death certificate that has to
/// be earned**: `complete` starts `false` (via `Default`), so any path that
/// fails to produce a full census — an early return, a region walk that aborted
/// on an implausible object size — leaves the collector unable to free
/// anything. An incomplete `referenced` set is an UNDER-approximation of what
/// is live, and acting on one frees a reachable object.
#[derive(Default)]
pub struct HumongousCensus {
    /// `HumongousStart` region indices reached by a reference from some walked
    /// object. Over-approximates liveness: a reference from a dead holder still
    /// counts, which costs one extra cycle of retention and never a UAF.
    pub referenced: RegionSet,
    /// Item 1 — whether a census was taken at all. Only a WIDE fix-up walk
    /// takes one; after a narrow walk `referenced` is empty and meaningless,
    /// and eager reclaim consults the spans' remembered sets instead.
    pub taken: bool,
    /// Every region the walk was supposed to cover ran to its natural end.
    pub complete: bool,
    /// G1AUD-10 — how much heap the fix-up walk actually covered: non-CSet
    /// regions visited and bytes stepped over. `fixup_us` without this cannot
    /// distinguish a slow walk from a big old generation.
    pub walked_regions: u32,
    pub walked_bytes: u64,
}

/// The `HumongousStart` index of the span containing `idx`, if any.
///
/// A reference always names an object's base address, which for a humongous
/// object lives in its `HumongousStart` region — so the backward scan is
/// defensive rather than load-bearing. It exists because
/// `lookup_region_for_addr` is a pure address-range query with no idea what a
/// span is, and a single interior address treated as its own region would make
/// a live span look unreferenced.
pub fn humongous_span_start(regions: &[G1Region], idx: usize) -> Option<usize> {
    match regions.get(idx)?.region_type() {
        RegionType::HumongousStart => Some(idx),
        RegionType::HumongousContinuation => {
            let mut i = idx;
            while i > 0 {
                i -= 1;
                match regions[i].region_type() {
                    RegionType::HumongousStart => return Some(i),
                    RegionType::HumongousContinuation => continue,
                    // A continuation run that does not begin at a start is a
                    // corrupt region table. Report nothing rather than guess:
                    // the caller reads `None` as "not a humongous target",
                    // which can only over-retain.
                    _ => return None,
                }
            }
            None
        }
        _ => None,
    }
}

/// gci i14/pn -- does a pin of region `idx` (the region holding an array's
/// header) keep the whole payload `[.., end)` in place? See
/// [`G1Collector::pin_array_payload_for_critical`]. `arena_base` and `shift`
/// are the region grid (`lookup_region_for_addr`'s). Pure.
pub fn critical_payload_is_covered(
    regions: &[G1Region],
    idx: usize,
    arena_base: usize,
    shift: u32,
    end: usize,
) -> bool {
    let Some(r) = regions.get(idx) else {
        return false;
    };
    let region_end = arena_base.saturating_add((idx + 1) << shift);
    match r.region_type() {
        RegionType::Eden | RegionType::Survivor | RegionType::Old => end <= region_end,
        RegionType::HumongousStart => {
            if end <= region_end {
                return true;
            }
            // The last byte's region, then every region up to it must be a
            // continuation of this span.
            let last = (end - 1 - arena_base) >> shift;
            (idx + 1..=last).all(|i| {
                regions
                    .get(i)
                    .is_some_and(|c| c.region_type() == RegionType::HumongousContinuation)
            })
        }
        RegionType::HumongousContinuation | RegionType::Free => false,
    }
}

/// gci i11/gm finding 6 -- is the object at a humongous span's base a
/// PRIMITIVE array (HotSpot's `typeArray`: no reference slots, built-in
/// metadata)? A reference array, a plain object and a `HumongousFiller` all
/// answer `false`.
///
/// The caller passes the base of a well-formed `HumongousStart` region whose
/// cursor is at least `HEADER_SIZE`, under the regions write guard; the header
/// was written before the cursor was published (`alloc_in_region_locked`).
pub fn span_holds_primitive_array(base: usize) -> bool {
    // SAFETY: see above -- `base` is the committed start of a live span and
    // at least a header's worth of it is allocated.
    let header = unsafe { &*(base as *const ObjectHeader) };
    header.kind() == ObjectKind::Array && header.element_type() != ArrayElementType::Reference
}

/// Phase-4 rset rebuild records cross-region edges into these region types so a
/// later young/mixed GC scans the holder as a remembered-set source; edges into
/// non-collectable regions (`Free`, `HumongousStart`/`HumongousContinuation`)
/// are never consulted and so carry no rebuilt rset entry.
///
/// Humongous targets are not simply dropped, though: they are recorded in the
/// [`HumongousCensus`] instead, which is what an evacuation pause reads to
/// decide whether a span can be reclaimed eagerly. The rset is the wrong home
/// for them precisely because a humongous region can never enter a collection
/// set, so nothing would ever consult the entry.
#[inline]
pub fn is_collectable_region_type(region_type: RegionType) -> bool {
    matches!(
        region_type,
        RegionType::Eden | RegionType::Survivor | RegionType::Old
    )
}

/// F-02 — resolve a reference slot's forwarding target, without hashing the
/// slots that cannot have one.
///
/// # Why the CSet test comes first
///
/// Phase 4 walks the NON-collection-set regions — in a young pause, the whole
/// old generation — and asked `pointer_map.get(raw)` for every non-null
/// reference slot in it. Almost none of those can be answered: an object is
/// only in the forwarding map if it was EVACUATED, and only a collection-set
/// resident is ever evacuated, so on a heap with a large old generation the
/// overwhelming majority of those probes are hash-and-miss over slots pointing
/// at objects that never moved.
///
/// `lookup_region_for_addr` is O(1) arithmetic (a shift, since F-09) and CSet
/// membership is a bitset test (since F-03), so screening on "does this
/// reference even point into the collection set?" turns the common case from a
/// hash probe into four instructions. It is exactly equivalent, not an
/// approximation: every key of `pointer_map` is a from-space address, and
/// `evacuate_object` is only ever called for collection-set residents — the
/// self-forwarded (`k == v`) entries included.
///
/// The map, not the from-space object's forwarding mark word, stays the
/// authority here. The header would answer in one load, but `raw` at this point
/// is an unvalidated word out of a reference slot: it is only known to be
/// in-region, and reading a header at an arbitrary in-region address could
/// decode stale bytes as a forward and rewrite a live slot to a bogus address.
/// The map cannot do that.
pub struct ForwardLookup<'a> {
    pub collector: &'a G1Collector,
    pub cset: &'a RegionSet,
    pub map: &'a cratonvm_types::PointerMap,
    /// The region table, so [`update_object_refs`] can clamp its two walks to
    /// the holder's own region. Phase 4 already holds it; passing it here is
    /// what lets the bound be a property of the WALK rather than of its one
    /// caller's loop condition.
    pub regions: RegionView<'a>,
}

impl ForwardLookup<'_> {
    #[inline]
    pub fn resolve(&self, raw: usize) -> Option<usize> {
        let idx = self.collector.lookup_region_for_addr(raw)?;
        if !self.cset.contains(&idx) {
            return None;
        }
        self.map.get(&raw).copied()
    }
}

/// Update reference fields in an object using the forwarding map.
///
/// # Both arms are bounded by the holder's own region
///
/// This walk WRITES every slot it resolves, and until 2026-09-08 neither arm
/// had a bound of any kind: the array arm iterated `array_length` raw `u64`
/// slots and the object arm went through
/// [`for_each_flat_object_reference_trusting_header`], whose own doc says it
/// "bounds it by nothing". `g1-eight-byte-write-at-a-live-objects-base-20260906`
/// names this as the AMPLIFIER of the corrupt-header family: one refusal in its
/// census named a holder declaring 102736 legacy slots -- a 1.6 MB body in a
/// 1 MiB region, rewritten cell by cell with forwarding addresses.
///
/// Its single caller (Phase 4's linear region walk) does break on
/// `offset + obj_size > cursor`, so in practice the count it hands over already
/// fits the region -- but that is a property of ONE caller, restated nowhere,
/// and the counts here come from the same `shape` dword the family corrupts.
/// The clamp makes it a property of the walk. It cannot drop a live reference:
/// a reference field of an object is inside that object, and a slot past the
/// holder's own region is not one.
pub fn update_object_refs(obj_ptr: *mut u8, header: &ObjectHeader, forwards: &ForwardLookup<'_>) {
    let data_start = unsafe { obj_ptr.add(ARRAY_DATA_OFFSET) };
    let extent = holder_extent(obj_ptr, header);

    if header.kind() == ObjectKind::Array {
        if header.element_type() == ArrayElementType::Reference {
            let walkable = forwards.collector.holder_walkable_slots_view(
                forwards.regions,
                obj_ptr,
                header.array_length() as usize,
                8,
                HolderBound::Cursor,
            );
            for i in 0..walkable {
                let slot_ptr = unsafe { data_start.add(i * 8) };
                let raw: u64 = unsafe { std::ptr::read(slot_ptr as *const u64) };
                if raw != 0 {
                    if let Some(new_addr) = forwards.resolve(raw as usize) {
                        note_ref_write(
                            slot_ptr as usize,
                            new_addr as u64,
                            obj_ptr as usize,
                            extent,
                            8,
                        );
                        unsafe {
                            std::ptr::write(slot_ptr as *mut u64, new_addr as u64);
                        }
                    }
                }
            }
        }
    } else {
        let walkable = if cratonvm_types::is_compact_object(header) {
            0
        } else {
            forwards.collector.holder_walkable_slots_view(
                forwards.regions,
                obj_ptr,
                header.num_slots() as usize,
                SLOT_SIZE,
                HolderBound::Cursor,
            )
        };
        for_each_flat_object_reference_capped(
            obj_ptr,
            header,
            0,
            walkable,
            |slot, raw, compact| {
                if let Some(new_addr) = forwards.resolve(raw) {
                    write_flat_object_reference_watched(slot, new_addr, compact, obj_ptr, extent);
                }
            },
        );
    }
}

// gci i11/gm finding 12 -- `select_evacuation_candidates` (T5.5.4) and its
// `RegionId` alias were deleted here: a fourth old-CSet selector over the
// deprecated `region::Region` prototype, ranked `garbage / live` descending,
// reached only by its own six tests. The production choice is
// `G1Collector::choose_old_cset`.

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The pin a rejected conservative band word owes, from
/// [`G1Collector::band_reject_pin`] (round 12 wave 1, lane g1store).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BandRejectPin {
    /// The address to publish in the pin set: the span base for a humongous
    /// word, otherwise the word itself.
    pub addr: usize,
    /// Whether the word lies in a humongous span.
    pub humongous: bool,
    /// The region `addr` pins (the `HumongousStart` region for a span).
    pub region: usize,
}

/// What kind of live span an in-arena address falls in, as
/// `G1Collector::live_span_at` reports it.
///
/// The kind is what lets `is_object_address` prove an object START in
/// humongous memory with no extra lookup: a humongous object begins at offset 0
/// of its `HumongousStart` region and nothing else is ever allocated in its
/// span (`claim_humongous_span_locked`), so `base` is the one start in a
/// `HumongousStart` region and a `HumongousContinuation` region holds none.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LiveSpan {
    /// An Eden / Survivor / Old region, below its cursor (or a recorded
    /// self-forwarded object of a kept region). Any 8-aligned word may start
    /// an object as far as this lookup can tell.
    Allocated,
    /// Below the cursor of a `HumongousStart` region whose data begins at
    /// `base`.
    HumongousStart { base: usize },
    /// Anywhere in a `HumongousContinuation` region: payload only.
    HumongousContinuation,
}

/// W3-B — what the whole-heap remembered-set check found, by SHAPE.
///
/// # Why a count of edges was not enough
///
/// `dbg_verify_rset_completeness` reported `edges=N missing=0` and nothing
/// else. On `tools/probes/G1RsetWideProbe` — a probe written specifically to
/// produce thousands of old-to-young remembered-set sources — that check said
/// `edges=13583 missing=0` while the pause immediately after it reported
/// `rset_sources=1`. Read together those two numbers look like a remembered set
/// that is being emptied by a filter, which is a correctness question; read
/// with this breakdown they say something else entirely, because
/// `old_to_young` on that workload is a handful of edges and `examined` is
/// dominated by `young_to_young`.
///
/// So the distinction this struct exists to draw is:
///
/// * **`examined`** counts every cross-region reference in the heap. Most of
///   them can never be a remembered-set source for a young pause — a
///   young-to-young edge needs no entry at all, because a young pause collects
///   young WHOLESALE and therefore scans the source as part of the collection
///   set rather than on behalf of it.
/// * **`old_to_young_sources`** counts the regions that a young pause would
///   actually have to walk. That, and not `examined`, is the number a pause's
///   `rset_sources=` should be compared against.
///
/// A gap between `old_to_young_sources.len()` and the pause's reported
/// `rset_sources` is the only thing that means "entries are being filtered
/// out". Equality means the remembered set is naming exactly the sources the
/// heap contains, and a small number is then a fact about the WORKLOAD.
///
/// `young_to_old` is counted too, and it is the column that should stay near
/// zero under a generational hypothesis; a large one says the workload is
/// storing old references into young objects, which is a different shape from
/// the one the remembered set is tuned for.
#[derive(Default)]
pub struct RsetEdgeCensus {
    /// Every cross-region reference into a collectable region.
    pub examined: usize,
    /// Those the target's remembered set did not name, or named only with a
    /// stale generation stamp. Non-zero is a use-after-free in waiting.
    pub missing: usize,
    /// Source outside a young generation, target inside one. The only column a
    /// young pause's remembered set has to carry.
    pub old_to_young: usize,
    /// Both ends young: covered by collecting young wholesale.
    pub young_to_young: usize,
    /// Both ends old: carried for MIXED pauses, not young ones.
    pub old_to_old: usize,
    /// Source young, target old. Not a remembered-set obligation in either
    /// direction; counted so the four columns sum to `examined`.
    pub young_to_old: usize,
    /// The distinct source regions behind `old_to_young`.
    pub old_to_young_sources: RegionSet,
}

// --- lane A helpers ---
//
// Remembered-set / card-table levers. These read their environment variable
// through a `OnceLock` rather than through `cratonvm_types::GcFlags`, following
// `cratonvm_gc_core::region::rset_source_cap`: both are knobs whose only readers are the
// remembered set's own code, and adding a field to the process-wide flag struct
// for each one pushes a lane's private policy into a type every collector
// shares.

// ---------------------------------------------------------------------------
// ONE source-region walk (wave 2, lane A)
// ---------------------------------------------------------------------------

/// The diagnostic site tags one ARM's source walk reports under.
///
/// Everything else about the two walks is now shared; these are not, and
/// deliberately. Every one of these strings ends up in a log line whose first
/// job is to say WHICH walk produced it, and a report that cannot tell the
/// serial arm's refusal from the parallel arm's has lost the fact that starts
/// the investigation.
pub struct SourceWalkSites {
    /// [`G1Collector::note_implausible_legacy_header_view`]'s site.
    pub walk: &'static str,
    /// The CSet-candidate screen's site, reference-array arm.
    pub array: &'static str,
    /// The CSet-candidate screen's site, flat-object arm.
    pub flat: &'static str,
    /// How the `EVAC_SOURCE_WALK_DESYNC` warning names this arm.
    pub desync: &'static str,
}

pub const SERIAL_SOURCE_WALK_SITES: SourceWalkSites = SourceWalkSites {
    walk: "rset-source-walk",
    array: "rset-source-scan[array]",
    flat: "rset-source-scan[object]",
    desync: "rset-source",
};

pub const PARALLEL_SOURCE_WALK_SITES: SourceWalkSites = SourceWalkSites {
    walk: "parallel-seed-walk",
    array: "parallel-seed[array]",
    flat: "parallel-seed[object]",
    desync: "PARALLEL rset-source",
};

/// What [`walk_source_region_for_cset_refs`] needs from the arm running it.
///
/// # Why this exists, and why it is a deletion rather than an addition
///
/// `G1Collector::scan_source_region_for_cset_refs` (serial) and
/// `SharedEvac::seed_source_region` (parallel) were the same walk written
/// twice. Both carried a comment saying the two arms must agree —
/// `CRATONVM_G1_PARALLEL_EVAC` is supposed to change how FAST a pause runs,
/// not what it FINDS — and nothing enforced it. **Defect G1-9 was a divergence
/// between these two walks**, and the 2026-09-20 audit found five more:
///
/// * a clamped slot walk cleaning its own card (a use-after-free, fixed on the
///   serial arm first and on the parallel arm eight hours later);
/// * a per-object card query whose result was thrown away when the screen was
///   off, which charged the A/B's control arm for the feature it does not have;
/// * the card cursor, on the serial arm only;
/// * `candidate_header_is_plausible` + the `EVAC_SOURCE_WALK_DESYNC`
///   diagnostic, on the serial arm only — so the same corruption produced a
///   logged, bounded refusal on one arm and a wild read on the other;
/// * `note_humongous_reference`, on the serial arm only, so item 1's
///   span-liveness mark simply did not exist on the arm every young pause
///   takes.
///
/// Every one of those is the same bug: two bodies, one specification, no
/// mechanism. The mechanism is this trait. Where the arms genuinely differ the
/// difference is a method here; everything else has exactly one copy, and a
/// screen that has one copy cannot diverge from itself.
///
/// # What genuinely differs
///
/// Five things, and it is worth naming why each is irreducible:
///
/// * **how a region is reached** — a borrowed `&[G1Region]` against a raw
///   `regions_base` pointer. That is [`RegionView`], which already existed for
///   exactly this reason;
/// * **where a to-space copy comes from** — `evacuate_object` into a
///   `PointerMap` against `SharedEvac::evacuate` into a worker TLAB;
/// * **where a fresh child goes** — a serial `work_list` against the shared
///   gray queue, and the serial arm deliberately pushes unconditionally where
///   the parallel arm gates on `fresh`;
/// * **how far a holder's body may reach** — [`HolderBound`], whose own docs
///   explain why the parallel arm cannot use the cursor bound;
/// * **whether the header screens are armed** — the serial arm has had them
///   since 2026-09-02 unconditionally; the parallel arm's are behind
///   `CRATONVM_G1_PARALLEL_EVAC_SCREEN`, which exists to reproduce the crash
///   clusters they were added for.
pub trait SourceWalkArm {
    /// The region table, as the shared header screens want it.
    fn view(&self) -> RegionView<'_>;

    /// Are the header-plausibility screens armed on this arm?
    fn screens_armed(&self) -> bool;

    /// How far a holder's declared body may reach before the clamp fires.
    fn holder_bound(&self) -> HolderBound;

    /// `CRATONVM_G1_CARD_RSET` as this arm reads it (the parallel arm latches
    /// it per pause; see `SharedEvac::card_rset`).
    fn card_rset(&self) -> bool;

    /// `CRATONVM_G1_CARD_CLEAN` as this arm reads it.
    fn card_clean(&self) -> bool;

    /// This arm's diagnostic site tags.
    fn sites(&self) -> &'static SourceWalkSites;

    /// Evacuate one CSet-resident referent; `Some((new_ptr, fresh))` exactly as
    /// both arms' own evacuators return it, `None` on evacuation failure.
    ///
    /// SAFETY: `referent` has passed this arm's candidate screen and points at
    /// the start of an object in a collection-set region.
    unsafe fn evacuate(&mut self, referent: *mut u8) -> Option<(*mut u8, bool)>;

    /// This arm's bookkeeping for a referent that was just evacuated, AFTER the
    /// holder's slot has been rewritten.
    fn note_evacuated(&mut self, old_ptr: *mut u8, new_ptr: *mut u8, fresh: bool);

    /// Item 1's span-liveness mark for a slot that points into region `idx`.
    ///
    /// Called only when `CRATONVM_G1_HUMONGOUS_MARKS` is on — the walk hoists
    /// that flag out of the per-slot loop, because this used to be a
    /// `gc_flags()` `OnceLock` deref per reference slot on the hottest walk in
    /// the pause.
    fn note_humongous_reference(&self, idx: usize);

    /// May this CSet-resident candidate be dereferenced and evacuated?
    fn candidate_ok(&self, site: &'static str, holder: *mut u8, slot: usize, raw: usize) -> bool;

    #[inline]
    fn candidate_screen(
        &self,
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> (bool, bool) {
        (self.candidate_ok(site, holder, slot, raw), false)
    }

    #[inline]
    unsafe fn evacuate_screened(
        &mut self,
        referent: *mut u8,
        _proved: bool,
    ) -> Option<(*mut u8, bool)> {
        self.evacuate(referent)
    }
}

/// How much of a source region's card range may be dirty before the monotone
/// cursor stops paying for itself.
///
/// # Where the number comes from
///
/// The cursor ([`crate::g1_cards::CardSet::next_dirty_addr`]) replaces a
/// per-object bitset probe with two address compares, and re-derives itself
/// only when the walk has stepped past the card it names. So its total cost
/// over one region is `2 * objects` compares plus `dirty_cards` derivations,
/// against the query's `1..2 * objects` probes. The cursor therefore wins on
/// the per-object term always, and loses only when `dirty_cards` grows large
/// enough for the derivations to eat the difference — i.e. when the clean runs
/// it exists to skip are shorter than the objects stepping over them.
///
/// At 512-byte cards and the 32-to-48-byte objects these workloads allocate,
/// one card holds roughly a dozen objects. A region that is 75% dirty still
/// has one clean card in four, so a derivation is still amortised over ten-odd
/// objects; a region that is essentially all dirty amortises it over one. 75%
/// is where the amortisation is still clearly positive and the measurement
/// (see `lane_a_card_cursor`) can no longer separate the two arms.
///
/// The threshold is deliberately generous, because the two arms are EXACTLY
/// equivalent in what they scan — this is a throughput choice between two
/// identical answers, so picking the wrong side costs microseconds and never
/// correctness.
pub const LANE_A_CURSOR_MAX_DIRTY_PERCENT: usize = 75;

/// Is this region's dirty-card population sparse enough for the cursor?
///
/// Reads the SNAPSHOT, not the table: the snapshot is a dense bitset the walk
/// has already paid to build, so the density is `count_ones` over 32 words for
/// a 1 MiB region — two cache lines, once per region, against the thousands of
/// per-object probes the answer governs.
///
/// The two-level summary table cannot answer this. A summary byte is a
/// `clean`/`maybe-dirty` bit, not a count (a count would be a
/// read-modify-write on the write barrier, which is the one thing
/// `G1CardTable` refuses — see its field docs), so it can say WHICH chunks to
/// look at and not HOW FULL they are.
#[inline]
pub fn lane_a_card_region_is_sparse(snapshot: &crate::g1_cards::CardSet) -> bool {
    snapshot.is_at_most_percent_dirty(LANE_A_CURSOR_MAX_DIRTY_PERCENT)
}

/// Item 1's span-liveness mark against a [`RegionView`], so both arms of the
/// unified source walk can take it.
///
/// `G1Collector::note_humongous_reference` takes `&[G1Region]` and belongs to
/// the humongous lane; this is the same three tests against a view, so the
/// parallel arm can stop being the one arm on which item 1 silently does
/// nothing. The flag check is the CALLER's (the walk hoists it), because this
/// used to be a `OnceLock` deref per reference slot.
pub fn lane_a_note_humongous_reference_view(gc: &G1Collector, regions: RegionView<'_>, idx: usize) {
    let is_humongous = matches!(
        regions.get(idx).map(|r| r.region_type()),
        Some(RegionType::HumongousStart) | Some(RegionType::HumongousContinuation)
    );
    if !is_humongous {
        return;
    }
    // The backward scan to the span's `HumongousStart`, as
    // `humongous_span_start` does it for a slice. A continuation run that does
    // not begin at a start is a corrupt region table: report nothing rather
    // than guess, which can only over-retain.
    let mut i = idx;
    let span = loop {
        match regions.get(i).map(|r| r.region_type()) {
            Some(RegionType::HumongousStart) => break Some(i),
            Some(RegionType::HumongousContinuation) => {
                if i == 0 {
                    break None;
                }
                i -= 1;
            }
            _ => break None,
        }
    };
    if let Some(span) = span {
        gc.humongous_referenced.lock().insert(span);
    }
}

/// **THE** remembered-set source walk: evacuate every CSet-bound reference out
/// of one non-CSet source region and rewrite the slot in place.
///
/// Both arms call this and only this. See [`SourceWalkArm`] for the five ways
/// the arms differ and for the history that says why sharing the rest is the
/// point.
///
/// `cursor_limit` is the PRE-DISPATCH cursor when the caller runs beside other
/// evacuation workers, and `None` for a single-threaded walk. A source region
/// can legitimately also be a DESTINATION region this pause (`SharedEvac`
/// hands partly-filled non-CSet Old regions out as promotion TLABs, and
/// `shared_dest_alloc` bumps into any non-CSet region of the destination
/// type), so a walk that read the live cursor could sweep over to-space
/// objects another worker is at that instant still copying in. Clamping to the
/// cursor the driver snapshotted before any worker started keeps the walk
/// strictly inside the bytes that were already there — which is the whole and
/// only set of bytes a remembered-set source is about.
///
/// SAFETY: `gc`'s region table must be quiescent for the duration (a
/// stop-the-world safepoint, with `cursor_limit` supplied when other
/// evacuation workers are live), and `arm`'s view must describe it.
pub unsafe fn walk_source_region_for_cset_refs<A: SourceWalkArm>(
    gc: &G1Collector,
    arm: &mut A,
    cset: &RegionSet,
    jit_skips: &[(usize, usize)],
    source_idx: usize,
    card_screen: bool,
    cursor_limit: Option<usize>,
    precise_rset_liveness: Option<&MarkProvenDead>,
) {
    // Source must be a live non-CSet region; rset sources can include indices
    // that have since been reclassified.
    if cset.contains(&source_idx) {
        return;
    }
    let sites = arm.sites();
    let (cursor, base) = {
        let Some(r) = arm.view().get(source_idx) else {
            return;
        };
        if r.region_type() == RegionType::Free {
            return;
        }
        let live = r.cursor();
        (
            match cursor_limit {
                Some(limit) => live.min(limit),
                None => live,
            },
            r.data.addr() as *mut u8,
        )
    };

    // F-05 — the whole-region screen. A source region none of whose cards is
    // dirty holds no cross-region reference at all, so it cannot hold one into
    // the collection set, and the megabyte walk below is skipped entirely.
    // This is the case F-05 is about: a remembered-set entry is additive and
    // outlives the edge that created it by up to a full mark cycle, and a
    // COARSENED rset makes every live region a nominal source, so most of what
    // Phase 2 walks has nothing in it.
    let screen = card_screen && arm.card_rset();
    // gci i15/rs (`CRATONVM_G1_YOUNG_CARD_SCREEN=1`, default OFF) -- when the
    // collection set holds only young regions, screen by YOUNG-dirty cards
    // (`== G1_CARD_DIRTY`) and step over the OLD_ONLY cards the previous young
    // pauses' fix-ups demoted (`G1_CARD_OLD_ONLY`: examined in full, no
    // reference into a young region). Every reader below -- the region screen,
    // the snapshot, the cursor, the block-offset jump, the per-object query --
    // then reads the young table; nothing else changes. A mixed collection set
    // (any Old member) keeps the sticky `!= CLEAN` screen. `young_redirty`: a
    // holder whose slot this walk rewrites is re-dirtied, so the fix-up's young
    // screen always walks it (HotSpot's redirty of the scanned card). See
    // `fixup_walk_region` for the invariant this rests on.
    let young_redirty =
        young_card_screen_armed(young_card_screen_switch(), arm.card_rset(), arm.card_clean());
    let young_cards = screen
        && young_redirty
        && cset_is_young_only(cset, |i| arm.view().get(i).map(|r| r.region_type()));
    let cards = gc.cards();
    gc.card_regions_offered.fetch_add(1, Ordering::Relaxed);
    let region_has_cards = || {
        if young_cards {
            cards.any_young_dirty_in(base as usize, cursor)
        } else {
            cards.any_dirty_in(base as usize, cursor)
        }
    };
    if screen && !region_has_cards() {
        gc.card_regions_skipped.fetch_add(1, Ordering::Relaxed);
        gc.card_bytes_skipped
            .fetch_add(cursor as u64, Ordering::Relaxed);
        return;
    }

    // Card cleaning — the snapshot the per-object screen reads, and the set of
    // cards this walk will leave dirty behind it.
    //
    // Without cleaning, F-05's table only ever GAINS bits: a card is cleared in
    // exactly one place, `G1Region::reset`, so a card dirtied by a store that
    // was later overwritten stays dirty until its region is recycled and a
    // long-lived Old region saturates.
    //
    // The snapshot is what makes cleaning safe to decide from — see
    // `G1CardTable::snapshot` for why reading the live table while rewriting it
    // answers its own next question wrongly.
    //
    // THE SNAPSHOT IS BUILT EXACTLY WHEN A READER NEEDS IT, and this is where
    // the two arms used to disagree in a way that unifying them could have
    // changed the default path underneath its own measurement.
    //
    // The serial walker took the snapshot whenever the SCREEN was on. The
    // parallel walker took it only when CLEANING was on — which is why the card
    // cursor, whose only input is the snapshot, could not engage at all on the
    // arm every young pause takes. See `lane_a_card_cursor` for the 180-run
    // battery that came out of, and for what it says about the wave-1 table
    // that was read off a lever in that state.
    //
    // Adopting either arm's rule here would silently change the other arm's
    // cost, so this adopts NEITHER. The snapshot has exactly two readers — card
    // cleaning, and the cursor (including the adaptive gate's density test,
    // which has to see the bitset to measure it) — so it is built when one of
    // them is present and not otherwise. With everything at its default
    // (cleaning off, cursor off) that means no snapshot on either arm and a
    // live-table probe per object, which is byte-for-byte what the DEFAULT path
    // did before this unification.
    //
    // Reading a snapshot rather than the live table is not a relaxation when
    // one is built: Phase 2 runs at a stop-the-world safepoint, every card
    // producer (`post_write_barrier_rset`, `record_outgoing_rset_edges`,
    // `collect_outgoing_cross_region_edges`) runs either on a parked mutator or
    // later in this same pause, and `clean_and_redirty` below is the only
    // writer while the walk is in flight. The snapshot and the table are the
    // same bits, so which one the screen reads cannot change what it decides.
    let cleaning = screen && arm.card_clean();
    let cursor_mode = lane_a_card_cursor();
    // W4-A — the block-offset jump is a THIRD reader of the snapshot, and it
    // reads the same thing the cursor does (the address of the next dirty card
    // at or after a position). It is listed here rather than folded into
    // `cursor_mode` because the two are separable levers: the cursor changes
    // how the per-object screen is EVALUATED and is exactly equivalent to the
    // query it replaces, while this changes which objects are visited at all.
    // A reader comparing the two arms of either A/B needs to know which one
    // paid for the snapshot.
    // W5-B — AND NOT DISARMED. One relaxed-acquire load per source region,
    // not per object. `block_offsets_disarmed` is the latch a card producer
    // sets when it names an address whose sixteen bytes do not carry
    // `GC_FLAG_HEADER`, i.e. when the producer rule this jump rests on is
    // observed breaking. Once it is set the walk goes back to stepping over
    // every object linearly, for the rest of the process, which is exactly
    // what it does with the lever off — the only direction a remembered set
    // may fail in. See `ObjectStart` in `gc/src/g1_cards.rs`.
    let block_offsets =
        screen && lane_a_block_offsets() && !crate::g1_cards::block_offsets_disarmed();
    // W5-B — the AUDIT arm. Off by default and not cheap: it walks the bytes
    // the jump just skipped, which is precisely the work the jump exists to
    // avoid, and asks of every object starting in them whether it holds a
    // reference into another region. A single hit is the producer rule
    // breaking in the one shape the vouch above cannot see — a producer that
    // names a real object start belonging to the WRONG object. See
    // `lane_a_block_offset_audit`.
    // gci i15/rs: not under the young screen. The audit's predicate is "any
    // cross-region reference" in the skipped bytes, and an OLD_ONLY card the
    // young snapshot steps over legitimately holds Old -> Old ones; auditing it
    // would disarm the jump process-wide on a healthy heap.
    let block_audit = if block_offsets && !young_cards {
        lane_a_block_offset_audit()
    } else {
        0
    };
    let snapshot = (screen && (cleaning || block_offsets || cursor_mode != CardCursorMode::Off))
        .then(|| {
            if young_cards {
                cards.snapshot_young(base as usize, cursor)
            } else {
                cards.snapshot(base as usize, cursor)
            }
        });
    let mut keep = cleaning.then(|| cards.empty_set(base as usize, cursor));

    // LANE A — the card cursor, now DENSITY-ADAPTIVE.
    //
    // `next_dirty` is the start address of the first dirty card at or after the
    // walk's current position, or `None` once the rest of the region is clean.
    // It is re-derived only when the walk steps past the card it names, so a
    // whole region walk costs O(cards) cursor work rather than O(objects)
    // card-range arithmetic. See `CardSet::next_dirty_addr` for why the cursor
    // and `any_in_span` decide identically — it is a reindexing of one bitset,
    // so neither arm scans an object the other skips.
    //
    // Which arm is taken is a pure throughput choice between two identical
    // answers. `lane_a_card_cursor` says whether to make it per region (the
    // density test) or to pin it; `LANE_A_CURSOR_MAX_DIRTY_PERCENT` carries the
    // argument for where the line sits.
    let use_cursor = screen
        && match cursor_mode {
            CardCursorMode::Off => false,
            // `snapshot` is `Some` whenever `cursor_mode != Off` and the screen
            // is on, so the `is_some_and`s below are the cursor's own guard
            // rather than a fallback: no snapshot means no cursor, and the
            // per-object query takes over unchanged.
            CardCursorMode::Always => snapshot.is_some(),
            CardCursorMode::Adaptive => snapshot.as_ref().is_some_and(lane_a_card_region_is_sparse),
        };
    // W4-A — the jump drives the SAME cursor variable the per-object screen
    // does, so that a region walking with both armed maintains one cursor and
    // not two. `block_offsets` therefore seeds it even when `use_cursor` is
    // false; the per-object screen still takes its own arm below (`use_cursor`
    // alone decides that), so seeding it here cannot change what any object is
    // screened by.
    let mut next_dirty: Option<usize> = if use_cursor || (block_offsets && snapshot.is_some()) {
        snapshot.as_ref().and_then(|s| s.first_dirty_addr())
    } else {
        None
    };
    // W4-A — has a candidate entry failed its header screen in this region? A
    // mis-entry is a fact about this region's table, so the refusal is scoped
    // to this region: the rest of it is walked linearly, from the offset the
    // walk was already at, and the next region gets a fresh chance. Latched
    // rather than retried because a table that is wrong once in a region is
    // most likely wrong repeatedly in it, and each retry costs a full
    // `candidate_header_is_plausible`.
    let mut block_jumps_refused_here = false;
    let mut block_jumps_here = 0u64;
    let mut block_bytes_jumped_here = 0u64;
    let mut block_tail_bytes_here = 0u64;
    let mut block_tail_break_here = false;
    // W5-B follow-up — the walk had a dirty card AHEAD of it and the table
    // could not name a target for it: no entry, an entry that does not move
    // the walk forward, or one past the pre-dispatch clamp. Distinct from a
    // REFUSAL (the entry decoded to something that is not a header) and from a
    // DECLINE (the entry is inside a frozen-peer TLAB span), both of which
    // already have their own accounting and mean different things.
    let mut block_no_entry_here = 0u64;
    // W5-B — the audit arm's two counters, accumulated locally and published
    // once, for the same reason the jump's own are.
    // W5-B — has the audit caught a violation in THIS region? The
    // process-wide disarm latch is read once per walk, so it cannot stop the
    // walk that found it; this does.
    let mut block_audit_stop = false;
    let mut block_audit_spans_here = 0u64;
    let mut block_audit_objects_here = 0u64;
    let mut block_audit_violations_here = 0u64;

    // Bytes this walk actually examined. Cleaning is bounded by it, never by
    // `cursor`: a walk that broke early on an unsizeable header has not looked
    // at the bytes past the break, and cleaning those would drop edges nothing
    // has seen.
    let mut walked_upto = 0usize;
    // Bytes whose reference slots this walk visited, and bytes it stepped over
    // because the object touched no dirty card. Accumulated locally and
    // published once, so the screen does not put an atomic RMW in the
    // per-object loop it exists to make cheaper.
    let mut scanned_bytes = 0u64;
    let mut skipped_bytes = 0u64;

    let dbg_walk = gc_flags().g1_dbg_reach;
    // Item 1's mark, hoisted out of the per-SLOT loop. It is default-off and
    // measured `marked_live=0` on every workload; leaving the `gc_flags()`
    // `OnceLock` deref inside the slot loop put a cold line per slot back on
    // the old->old path that dominates a source walk.
    let humongous_marks = gc_flags().g1_humongous_marks;
    let screens_armed = arm.screens_armed();
    let holder_bound = arm.holder_bound();
    let mut trail = WalkTrail::default();
    let mut offset = 0usize;
    'walk: while offset < cursor {
        // ------------------------------------------------------------------
        // W4-A — THE BLOCK-OFFSET JUMP.
        //
        // This is the only place in the walk that moves `offset` by anything
        // other than an object it has sized, so it is the only place that can
        // desynchronise the walk on a healthy heap. Everything below is
        // arranged around that one sentence.
        //
        // The walk is at `here`, a boundary it is certain of (offset 0, or the
        // end of an object it decoded). It asks the snapshot for the next
        // dirty card at or after `here` and then, if that card starts strictly
        // after `here`, asks the block-offset table where that card's object
        // grid may be entered.
        //
        // WHY SKIPPING THE BYTES IN BETWEEN IS SOUND. Those bytes lie under
        // cards that are clean, plus the head of the dirty card below the
        // recorded offset. A clean card means no producer named an object
        // starting there, and every producer names the START of the object it
        // is recording — so no object starting in that run holds a
        // cross-region reference, and none of them can be a collection-set
        // edge. The head of the dirty card is the same argument by minimality:
        // the recorded value is the LOWEST start any producer named in that
        // card, so an object starting below it was never named either.
        //
        // An object that STRADDLES into the dirty card from the left is not an
        // exception and does not need one: it starts in some earlier card, and
        // if it held a cross-region reference that card would be dirty and the
        // walk would have stopped at it rather than jumped past it.
        //
        // WHY THIS CANNOT MIS-ENTER. `offset` is not assigned until the
        // candidate address has passed `candidate_header_is_plausible`, which
        // is the same screen the linear walk applies below and which validates
        // the tag bytes and the containment. A candidate that fails is counted,
        // reported and DISCARDED — the walk is still standing on the boundary
        // it was certain of, so there is nothing to rewind and the fallback is
        // simply the walk this function has always done. That is deliberately a
        // counted fail-safe and not a `debug_assert!`: an assertion that fires
        // inside a GC pause manufactures a worse bug than it reports.
        //
        // Note the screen here runs REGARDLESS of `screens_armed`. On the
        // parallel arm that flag is `CRATONVM_G1_PARALLEL_EVAC_SCREEN`, which
        // is default ON (`on_unless_zero`; gci i19/yc corrected this comment,
        // which said default-off), so the linear walk there header-screens
        // every object unless `=0` -- and with `=0` a jump is still one screen
        // per jump rather than one per object, and it is the jump that needs
        // it. This adds a guard on that arm; it removes none.
        // The jump is a LABELLED BLOCK rather than a chain of `else`s, so that
        // the three ways it can decline — no entry, an entry that does not move
        // the walk forward, an entry inside a frozen-peer TLAB span — all leave
        // by the same door and land on the linear walk below. The `else`-chain
        // form put the refusal counter on whichever branch happened to fall
        // through, and that counter's whole value is that it means one thing.
        'block_jump: {
            if !block_offsets || block_jumps_refused_here || block_audit_stop {
                break 'block_jump;
            }
            let here = base as usize + offset;
            // Re-derive only when the walk has stepped past the card the
            // cursor names — the same amortisation the per-object screen uses,
            // so the cursor costs O(dirty cards) over the region and not
            // O(objects).
            if next_dirty.is_none_or(|d| d.wrapping_add(crate::g1_cards::G1_CARD_BYTES) <= here) {
                next_dirty = snapshot.as_ref().and_then(|s| s.next_dirty_addr(here));
            }
            match next_dirty {
                // NOTHING FROM HERE TO THE END OF THE REGION IS DIRTY. By the
                // producer rule above, no object starting in that run holds a
                // cross-region reference, so there is nothing left in this
                // region for a collection-set scan to find.
                //
                // `walked_upto` is deliberately NOT advanced to `cursor` here.
                // It means "bytes this walk examined", and card cleaning is
                // bounded by it for a reason the module states plainly: a walk
                // that did not look at some bytes must not rewrite the cards
                // covering them. Cleaning fewer cards leaves them dirty, which
                // is the safe direction; claiming coverage the walk does not
                // have is the other one.
                None => {
                    block_tail_bytes_here = (cursor - offset) as u64;
                    block_tail_break_here = true;
                    break 'walk;
                }
                Some(d) if d > here => {
                    match cards.block_entry_addr(d) {
                        // The table has an entry, it is ahead of the walk, and
                        // it is inside the bytes this walk is allowed to touch
                        // (`cursor` already carries the pre-dispatch clamp).
                        Some(entry) if entry > here && entry < base as usize + cursor => {
                            // INT-3 — an entry inside a published frozen-peer
                            // TLAB span is DECLINED, and it is not counted as a
                            // refusal.
                            //
                            // It is a reachable and entirely legitimate state:
                            // a mutator can allocate an object in its TLAB,
                            // store a cross-region reference into it — which
                            // records that object's start here — and then be
                            // frozen with that TLAB unretired, so the whole
                            // span including the object is published as
                            // unwalkable. The linear walk's own guard at the
                            // top of this loop would step over the span, so
                            // jumping into it would be reading bytes the walk
                            // has been told not to interpret.
                            //
                            // Declining rather than refusing, because the two
                            // mean different things and the counter has to keep
                            // saying so: a refusal means the table named
                            // something that is not an object and is a reason
                            // to turn the lever off, while this means the walk
                            // was told to leave that memory alone and did.
                            if jit_tlab_skip_span_len(jit_skips, entry).is_some() {
                                // DECLINED. Fall through to the linear walk,
                                // whose own INT-3 guard skips the span's exact
                                // length — and leave the refusal counter alone.
                                break 'block_jump;
                            }
                            if gc.candidate_header_is_plausible_view(arm.view(), entry) {
                                // W5-B — THE AUDIT RUNS BEFORE THE JUMP IS
                                // TAKEN, over exactly the bytes about to be
                                // skipped, while the walk is still standing on
                                // a boundary it is certain of.
                                //
                                // That ordering is the difference between
                                // preventing a dropped edge and reporting one.
                                // A violation here means an object in those
                                // bytes holds a cross-region reference, so the
                                // jump is REFUSED: the walk falls through to
                                // the linear path and visits it. The sampled
                                // arm therefore prevents the edge loss on
                                // every jump it audits and contains it on the
                                // rest, because the same finding also disarms
                                // the jump process-wide inside
                                // `audit_block_jump_span`.
                                let mut audited_bad = 0u64;
                                if block_audit != 0 && block_audit_due(block_audit) {
                                    block_audit_spans_here += 1;
                                    let (objs, bad) = audit_block_jump_span(
                                        gc,
                                        arm.view(),
                                        jit_skips,
                                        source_idx,
                                        here,
                                        entry,
                                    );
                                    block_audit_objects_here += objs;
                                    block_audit_violations_here += bad;
                                    audited_bad = bad;
                                }
                                if audited_bad != 0 {
                                    // Stop jumping in this region as well as
                                    // in the process. The process latch is
                                    // read once per walk, so it cannot take
                                    // effect until the next region; this one
                                    // must not keep jumping in the meantime.
                                    //
                                    // Deliberately NOT `block_jumps_refused_here`:
                                    // that latch feeds `entries_refused`, whose
                                    // whole value since W4-A is that it means
                                    // exactly one thing — the table named
                                    // something that is not an object header.
                                    // This is a different finding and has its
                                    // own counter.
                                    block_audit_stop = true;
                                    break 'block_jump;
                                }
                                block_jumps_here += 1;
                                block_bytes_jumped_here += (entry - here) as u64;
                                offset = entry - base as usize;
                                continue 'walk;
                            }
                            block_jumps_refused_here = true;
                            let n = BLOCK_ENTRY_REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
                            if n <= 8 || n.is_power_of_two() {
                                tracing::warn!(
                                    "[g1] {} block-offset ENTRY REFUSED (#{n}): region={source_idx} \
                                     here=0x{here:x} card=0x{d:x} entry=0x{entry:x} cursor={cursor:#x} \
                                     — the block-offset table named an address that is not an object \
                                     header. No jump was taken and the walk continues LINEARLY from \
                                     0x{here:x}; nothing is skipped and nothing is aborted. A non-zero \
                                     count here on a healthy heap means the producers' \
                                     \"a card names its object's START\" rule is not holding and \
                                     CRATONVM_G1_BLOCK_OFFSETS must go back to off.",
                                    sites.desync,
                                );
                            }
                        }
                        // No entry, or one that does not move the walk forward.
                        // Both are "walk linearly", which is what the rest of
                        // this loop body already is — and both are counted,
                        // because "the cursor found a gap and the table had no
                        // answer for it" is the diagnosis `jumps=0` could not
                        // previously distinguish from "there was no gap".
                        _ => block_no_entry_here += 1,
                    }
                }
                // Already standing in (or before the end of) a dirty card:
                // nothing to jump over. NOT counted — there was no gap here,
                // so the table was never asked anything.
                _ => {}
            }
        }
        let obj_ptr = base.add(offset);
        // INT-3 — frozen-peer TLAB tail: uninitialized, no walkable filler;
        // must be skipped before any byte is interpreted.
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, obj_ptr as usize) {
            if dbg_walk {
                trail.record(offset, u32::MAX, b'S', skip);
            }
            offset += skip;
            continue;
        }
        // TLAB-retire gap sentinel: skip its exact span (NOT a walkable header
        // — see `gap_filler_len`; `break`ing here would skip the rest of a
        // possibly-pinned source region's objects).
        if let Some(gap) = gap_filler_len(obj_ptr) {
            if dbg_walk {
                trail.record(offset, u32::MAX, b'G', gap);
            }
            offset += gap;
            continue;
        }
        // VALIDATE BEFORE DECODING, AND BREAK.
        //
        // The walk derives every step from bytes it has not validated. Its only
        // rejection below is an implausible SIZE, and a ZEROED header passes
        // that: `class_id=0`, `num_slots=0`, `kind=Object` gives exactly
        // `HEADER_SIZE`. So a source region whose object grid no longer
        // describes its contents is walked 16 bytes at a time through reclaimed
        // memory until some stale bytes decode as an array with a large length
        // — and the element loop below then reads off the end of the region.
        //
        // BREAK rather than skip forward: a desynchronized linear walk cannot
        // resynchronize without a block-start table, and continuing is
        // precisely what turns the desync into the wild read.
        //
        // The serial arm has had this since 2026-09-02 unconditionally; the
        // parallel arm gates it on `CRATONVM_G1_PARALLEL_EVAC_SCREEN`, which is
        // the same-binary A/B for the crash clusters it was added for. That is
        // the one screen the two arms are still allowed to disagree about, and
        // the flag's name says which arm it governs.
        if screens_armed && !gc.candidate_header_is_plausible_view(arm.view(), obj_ptr as usize) {
            let n = EVAC_SOURCE_WALK_DESYNC.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 || n.is_power_of_two() {
                if let Some(r) = arm.view().get(source_idx) {
                    tracing::warn!(
                        "[g1] {} walk DESYNCED (#{n}): region={source_idx} type={:?} reuse_epoch={} recycled_in_generation={} offset={offset:#x} cursor={cursor:#x} obj=0x{:x} — the bytes there are not an object header, so this region's object grid does not describe its contents. Abandoning the walk; the pause continues. trail=[{}]",
                        sites.desync,
                        r.region_type(),
                        r.reuse_epoch,
                        r.recycled_in_generation(),
                        obj_ptr as usize,
                        trail.render(),
                    );
                    eprintln!(
                        "[g1][DESYNC-COVER] {} {} bumps=[{}]",
                        describe_skip_coverage(jit_skips, obj_ptr as usize, base, cursor),
                        r.tlab_trail.lock().describe_owner_or(
                            &r.bump_trail.lock(),
                            r.reuse_epoch,
                            offset
                        ),
                        r.bump_trail.lock().render(),
                    );
                }
            }
            break;
        }
        let header = &*(obj_ptr as *const ObjectHeader);
        // The same screen on the LINEAR walk. Note the standing caveat: this
        // walk visits dead objects too (it walks the region, not the live set),
        // so a hit here is weaker evidence than one at `worklist-holder` --
        // which is exactly why the two sites are named apart.
        gc.note_implausible_legacy_header_view(arm.view(), obj_ptr, header, sites.walk);
        // Round-9 gc CRIT-1: humongous continuation filler covers the entire
        // region; skip without trying to follow any oops.
        if is_humongous_filler(header) {
            break;
        }
        let obj_size = object_total_size(header);
        if obj_size < HEADER_SIZE || offset + obj_size > cursor {
            if dbg_walk {
                eprintln!(
                    "[g1][WALKBRK] {} region={source_idx} off={offset:#x} \
                     cursor={cursor:#x} obj_size={obj_size:#x} \
                     cid={} kind={} alen={} slots={} trail=[{}]",
                    sites.desync,
                    header.class_id.as_u32(),
                    ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                    header.array_length(),
                    header.num_slots(),
                    trail.render(),
                );
                if let Some(r) = arm.view().get(source_idx) {
                    eprintln!(
                        "[g1][WALKBRK-COVER] {} {} {}",
                        describe_skip_coverage(jit_skips, obj_ptr as usize, base, cursor),
                        r.tlab_trail.lock().describe_owner_or(
                            &r.bump_trail.lock(),
                            r.reuse_epoch,
                            offset
                        ),
                        hexdump_around(base, cursor, offset),
                    );
                    eprintln!(
                        "[g1][WALKBRK-BUMPS] region={source_idx} epoch={} carves=[{}] bumps=[{}]",
                        r.reuse_epoch,
                        r.tlab_trail.lock().render(),
                        r.bump_trail.lock().render(),
                    );
                }
            }
            break;
        }
        // A remembered set says this *region* once held an edge into the
        // CSet; it does not say this particular holder is live. Under
        // allocation pressure a holder the last trustworthy completed mark
        // proves dead is skipped, so a dropped holder's stale subtree is not
        // copied (see `MarkProvenDead`). JNI-pinned sources are never filtered.
        //
        // gci i17/nr (i16/nc residual R1): the skipped holder's non-null slots
        // are NULLED before it is stepped over. Skipping alone leaves its slots
        // into this collection set un-rewritten, the CSet regions are freed at
        // the end of the pause and reused, and a holder `cleanup` did not scrub
        // (one behind `scrub_dead_refs_below_tams`'s `break`, or any holder
        // under `CRATONVM_G1_CLEANUP_SCRUB=0`) then keeps a slot into reused
        // memory, which the next cycle's pauses (no filter while marking) walk
        // and follow -- the `corrupt Value cell` shape of finding 13. The proof
        // that lets the walk skip the holder is the evidence `cleanup` scrubs
        // dead holders on, so nulling its slots is the scrub, done here.
        // Only non-null slots are written: a holder cleanup already scrubbed
        // costs a read per slot.
        let source_is_unpinned = arm.view().get(source_idx).is_some_and(|r| !r.pinned);
        if source_is_unpinned
            && precise_rset_liveness.is_some_and(|dead| {
                arm.view()
                    .get(source_idx)
                    .is_some_and(|r| dead.proves_dead(r, source_idx, obj_ptr as usize))
            })
        {
            // SAFETY: `[obj_ptr, obj_ptr + obj_size)` is one object inside
            // this source region's walked prefix (sized and bounded above), the
            // world is stopped, and no other worker walks this source region.
            let nulled = unsafe {
                null_reference_slots_naming(obj_ptr as usize, header, obj_size, &|raw: usize| {
                    raw != 0
                })
            };
            if nulled > 0 {
                gc.scrub_counters
                    .rset_dead_holder_slots_nulled
                    .fetch_add(nulled as u64, Ordering::Relaxed);
            }
            walked_upto = offset + obj_size;
            skipped_bytes += obj_size as u64;
            offset += obj_size;
            continue;
        }
        if dbg_walk {
            trail.record(
                offset,
                header.class_id.as_u32(),
                ObjectHeader::kind_tag(header.mark_word.load(Ordering::Relaxed)),
                obj_size,
            );
        }

        // F-05 — the per-object screen.
        //
        // WHAT IS AND IS NOT SKIPPED. The object walk still steps
        // object-by-object: without a block-start table this walk cannot
        // resynchronise at an arbitrary card boundary, so the header read and
        // `object_total_size` above are unavoidable. What the screen removes is
        // everything that is actually expensive — the per-slot reads, an
        // `evacuation_candidate_is_an_object` validation per non-null slot, and
        // a `lookup_region_for_addr` per candidate — which is where a
        // source-region walk spends its time. Removing the linear step needs
        // the block-offset table in
        // `docs/feature-designs/concurrent-gc-maturation.md`.
        //
        // The test is over the object's WHOLE byte range, not just its header's
        // card, because an object may straddle several cards and a store into
        // its last slot dirties the card that slot's OBJECT starts in, not the
        // card the slot itself lies in. Over-approximating here is free;
        // under-approximating is a use-after-free.
        //
        // W3-B — WHAT THE PRODUCERS ACTUALLY GUARANTEE, because the
        // block-offset table's soundness rests on it and this comment used to
        // deny it. It said "every producer is free to name any of them", which
        // is not true of any producer in the tree. All five pass an object
        // START address:
        //
        //   * `post_write_barrier_rset`        — `src_obj.as_ptr()`, the holder
        //   * `record_outgoing_rset_edges`     — `obj_addr`, the seed object
        //   * `collect_outgoing_cross_region_edges`, humongous arm — `obj_ptr`
        //   * `collect_outgoing_cross_region_edges`, collectable arm — `obj_ptr`
        //   * this walk's own re-dirty (`keep.insert_addr`) — `obj_ptr`
        //
        // and `post_write_barrier_rset`'s three production callers (`set_field`,
        // `set_array_element`, `write_barrier`) all hand it the object the
        // store targeted, never an interior address.
        //
        // `a_store_into_a_straddling_objects_last_slot_dirties_its_START_card`
        // is the tripwire for that rule. It is stated here rather than asserted
        // in `dirty_addr` because `G1CardTable` has no way to ask whether an
        // address is an object start, and a `debug_assert!` that fires inside a
        // GC pause manufactures a worse bug than it reports.
        //
        // The whole-range test stays regardless: it is what makes the screen
        // correct for a straddling object no matter which card was named, and
        // the block-offset table needs the START rule for a different purpose
        // (deciding where to RESUME a walk, not whether to scan an object).
        // See `docs/feature-designs/concurrent-gc-maturation.md` §2.2.
        //
        // The query is gated on `screen`: it used to run unconditionally on
        // both arms and its result was then thrown away by
        // `if screen && !obj_dirty`, so a run with `CRATONVM_G1_CARD_RSET=0` —
        // the A/B arm the feature is measured against — paid a card-table probe
        // per object for an answer nothing read.
        let obj_addr = obj_ptr as usize;
        let obj_dirty = if !screen {
            false
        } else if use_cursor {
            if next_dirty
                .is_some_and(|d| d.wrapping_add(crate::g1_cards::G1_CARD_BYTES) <= obj_addr)
            {
                next_dirty = snapshot.as_ref().and_then(|s| s.next_dirty_addr(obj_addr));
            }
            next_dirty.is_some_and(|d| d < obj_addr + obj_size)
        } else {
            match &snapshot {
                Some(s) => s.any_in_span(obj_addr, obj_size),
                // gci i15/rs: the young table when the young screen is on.
                None if young_cards => cards.any_young_dirty_in(obj_addr, obj_size),
                None => cards.any_dirty_in(obj_addr, obj_size),
            }
        };
        walked_upto = offset + obj_size;
        if screen && !obj_dirty {
            skipped_bytes += obj_size as u64;
            offset += obj_size;
            continue;
        }
        scanned_bytes += obj_size as u64;
        // Card cleaning — does this object STILL reference another region once
        // this pause has rewritten its slots? That is the question a card
        // answers, and the only thing that licenses leaving it dirty. Set by
        // the slot loops below, and by any slot they refuse to interpret (a
        // refusal is not evidence of absence).
        let mut holds_cross_region = false;
        // gci i15/rs -- did this walk rewrite one of this holder's slots? Read
        // only under `young_redirty`.
        let mut rewrote_a_slot = false;

        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                let declared = header.array_length() as usize;
                let len = if screens_armed {
                    gc.holder_walkable_slots_view(arm.view(), obj_ptr, declared, 8, holder_bound)
                } else {
                    declared
                };
                // A REFUSAL IS NOT EVIDENCE OF ABSENCE. The clamp above walks a
                // holder whose declared element count would leave its region
                // only as far as the region goes. The elements past the clamp
                // are then never examined — and with card cleaning on,
                // `holds_cross_region` stayed false, this object's card was
                // written CLEAN, and the next pause stepped over an object that
                // may hold a live reference into the collection set. That is
                // the use-after-free the whole structure exists to prevent,
                // reached through the guard that was added to prevent a
                // different one.
                //
                // Keep the card instead. The cost is that a corrupt holder
                // re-screens its own card forever, which is exactly the
                // over-retention this module prefers.
                if len < declared {
                    holds_cross_region = true;
                }
                for i in 0..len {
                    let slot_ptr = obj_ptr.add(HEADER_SIZE + i * 8);
                    let raw: u64 = std::ptr::read(slot_ptr as *const u64);
                    if raw == 0 {
                        continue;
                    }
                    // Item 4 — CSet membership from the ADDRESS first; the
                    // header screen runs only for a CSet resident.
                    let ref_ptr = raw as usize as *mut u8;
                    let Some(ridx) = gc.lookup_region_for_addr(raw as usize) else {
                        continue;
                    };
                    // Item 1 -- the span-liveness mark; see
                    // `note_humongous_reference`.
                    if humongous_marks {
                        arm.note_humongous_reference(ridx);
                    }
                    // Card cleaning — a cross-region reference keeps this
                    // object's card dirty. Read BEFORE evacuation, which is
                    // sound because the source region is never in the CSet: a
                    // referent sharing this region is therefore not evacuated,
                    // and one that is evacuated was already in another region
                    // and stays in another region.
                    if ridx != source_idx {
                        holds_cross_region = true;
                    }
                    if !cset.contains(&ridx) {
                        continue;
                    }
                    let (ok, proved) = arm.candidate_screen(sites.array, obj_ptr, i, raw as usize);
                    if !ok {
                        continue;
                    }
                    if let Some((new_ptr, fresh)) = arm.evacuate_screened(ref_ptr, proved) {
                        note_ref_write(
                            slot_ptr as usize,
                            new_ptr as u64,
                            obj_ptr as usize,
                            holder_extent(obj_ptr, header),
                            8,
                        );
                        std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
                        rewrote_a_slot = true;
                        arm.note_evacuated(ref_ptr, new_ptr, fresh);
                    }
                }
            }
        } else {
            // COMPACT-LAYOUT PARITY (G1-9) — `num_slots()` is the
            // hierarchy-wide FIELD COUNT, so a legacy 16-byte stride runs off
            // the end of a compact body; `for_each_flat_object_reference_capped`
            // is the one dispatch that reads the registered layout.
            //
            // The flat walk gets the SAME region clamp the reference-array
            // branch above has, with the 16-byte `SLOT_SIZE` stride.
            // `candidate_header_is_plausible` validated the holder's tag bytes
            // and its containment; it did not validate that
            // `HEADER_SIZE + num_slots * SLOT_SIZE` lands inside the region,
            // and a legacy slot leaves a region TWICE as fast as an array
            // element does. An unclamped walk here does not merely read past
            // the holder: it rewrites `Value` cells past the holder with
            // forwarded pointers, i.e. it corrupts whatever follows it in the
            // region.
            let declared_slots = header.num_slots() as usize;
            let walkable_slots = if screens_armed && !cratonvm_types::is_compact_object(header) {
                gc.holder_walkable_slots_view(
                    arm.view(),
                    obj_ptr,
                    declared_slots,
                    SLOT_SIZE,
                    holder_bound,
                )
            } else {
                declared_slots            };
            // See the twin in the array arm, and note this is the MORE
            // dangerous of the two: the 16-byte stride is the one a clamp fires
            // on first.
            //
            // gci i9/ng: not for a COMPACT holder. Its walk ignores the cap
            // (`for_each_flat_object_reference_capped` bounds it by the
            // layout's own body) and visits every reference field, so the
            // closure below sets `holds_cross_region` exactly; a cap computed
            // with the legacy stride over a field COUNT only kept such a
            // holder's card dirty forever (opt-in card cleaning only).
            if walkable_slots < declared_slots && !cratonvm_types::is_compact_object(header) {
                holds_cross_region = true;
            }
            for_each_flat_object_reference_capped(
                obj_ptr,
                header,
                0,
                walkable_slots,
                |slot_ptr, raw, compact| {
                    let ref_ptr = raw as *mut u8;
                    let Some(ridx) = gc.lookup_region_for_addr(raw) else {
                        return;
                    };
                    if humongous_marks {
                        arm.note_humongous_reference(ridx);
                    }
                    // Card cleaning — see the array arm.
                    if ridx != source_idx {
                        holds_cross_region = true;
                    }
                    if !cset.contains(&ridx) {
                        return;
                    }
                    let (ok, proved) = arm.candidate_screen(
                        sites.flat,
                        obj_ptr,
                        // The SLOT, not the candidate -- see the twin at
                        // `worklist-scan[object]`.
                        (slot_ptr as usize).wrapping_sub(obj_ptr as usize),
                        raw,
                    );
                    if !ok {
                        return;
                    }
                    if let Some((new_ptr, fresh)) = arm.evacuate_screened(ref_ptr, proved) {
                        write_flat_object_reference_watched(
                            slot_ptr,
                            new_ptr as usize,
                            compact,
                            obj_ptr,
                            holder_extent(obj_ptr, header),
                        );
                        rewrote_a_slot = true;
                        arm.note_evacuated(ref_ptr, new_ptr, fresh);
                    }
                },
            );
        }

        // gci i15/rs -- re-dirty a holder this walk rewrote (young-dirty: a
        // producer store), so the fix-up's young card screen walks it, derives
        // its new edges into the to-space regions and decides its card from
        // the rewritten slots. Under the premise every such holder is already
        // young-dirty; this makes the fix-up independent of the premise for
        // the holders Phase 2 itself touched (an unscreened wholesale source).
        if young_redirty && rewrote_a_slot {
            cards.dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(header));
        }

        // Card cleaning — the object's start card is what every producer
        // dirties (`post_write_barrier_rset` and the Phase-4 rebuild both name
        // the holder's address, not the slot's), so it is what this walk
        // re-dirties. Keeping the START card is enough even for an object that
        // straddles several: the screen above asks over `[obj, obj+size)`,
        // whose range begins at that card.
        //
        // `holds_cross_region` is also set by a CLAMPED slot walk, so "keep"
        // covers both "still points elsewhere" and "was not fully examined".
        if holds_cross_region {
            if let Some(k) = keep.as_mut() {
                k.insert_addr(obj_ptr as usize);
            }
        }

        offset += obj_size;
    }

    // Card cleaning — rewrite the table for the bytes this walk examined. Every
    // object overlapping them has been visited, and `keep` holds the start card
    // of each one that still references another region, so a card left clean
    // here is a fact about the heap rather than about its history.
    if let Some(k) = keep.as_ref() {
        let covered = cards.cards_in(base as usize, walked_upto);
        let kept = k.count();
        cards.clean_and_redirty(base as usize, walked_upto, k);
        gc.card_cards_kept.fetch_add(kept as u64, Ordering::Relaxed);
        gc.card_cards_cleaned
            .fetch_add(covered.saturating_sub(kept) as u64, Ordering::Relaxed);
    }

    // F-05 — publish this region's screen outcome. Both counters, always: a
    // `scanned` with no `skipped` beside it says the screen RAN and refused
    // everything, which is a different fact from the screen never having run,
    // and a single "cards scanned" number cannot tell them apart.
    gc.card_bytes_scanned
        .fetch_add(scanned_bytes, Ordering::Relaxed);
    gc.card_bytes_skipped
        .fetch_add(skipped_bytes, Ordering::Relaxed);

    // W4-A — the block-offset table's engagement, accumulated locally and
    // published once for the same reason the two above are: the jump sits in
    // the per-object loop this whole structure exists to make cheaper, and an
    // atomic RMW per jump would be a cost the feature charges itself.
    //
    // `block_bytes_jumped` is the number that says what the table did, and it
    // is deliberately a COUNT. The effect it represents is a few hundred
    // microseconds of a pause inside a multi-second run, which is below this
    // host's wall-clock noise band by construction — see
    // `orchestrator-wave-1-measurements.md` §8.1, where the same situation was
    // settled by counting the work rather than by differencing two clocks.
    if block_jumps_here != 0 {
        gc.block_jumps_taken
            .fetch_add(block_jumps_here, Ordering::Relaxed);
        gc.block_bytes_jumped
            .fetch_add(block_bytes_jumped_here, Ordering::Relaxed);
        BLOCK_JUMPS_TAKEN.fetch_add(block_jumps_here, Ordering::Relaxed);
        BLOCK_BYTES_JUMPED.fetch_add(block_bytes_jumped_here, Ordering::Relaxed);
    }
    // W5-B — the audit's own two, published even when both are zero would be
    // wasted adds, so the same "publish when there is something to publish"
    // rule applies. A zero here is read off `enabled=` on the census line
    // rather than off a counter that was never added to.
    if block_audit_spans_here != 0 {
        BLOCK_AUDIT_SPANS.fetch_add(block_audit_spans_here, Ordering::Relaxed);
        BLOCK_AUDIT_OBJECTS.fetch_add(block_audit_objects_here, Ordering::Relaxed);
        BLOCK_AUDIT_VIOLATIONS.fetch_add(block_audit_violations_here, Ordering::Relaxed);
    }
    // The end-of-region break is counted APART from the jumps, and not as a
    // rounding detail. It is the one skip here that does not consult the
    // block-offset table at all — it needs only the card cursor's "nothing from
    // here to the end of this region is dirty" — so folding it into `jumps`
    // would credit the table with bytes the table had no part in, and would
    // hide the diagnosis that matters most: `jumps=0` beside a non-zero
    // `entries_refused` says the table is being rejected wholesale, and a
    // combined counter cannot say it.
    if block_tail_break_here {
        gc.block_tail_breaks.fetch_add(1, Ordering::Relaxed);
        gc.block_tail_bytes
            .fetch_add(block_tail_bytes_here, Ordering::Relaxed);
        BLOCK_TAIL_BREAKS.fetch_add(1, Ordering::Relaxed);
    }
    if block_no_entry_here != 0 {
        BLOCK_NO_ENTRY.fetch_add(block_no_entry_here, Ordering::Relaxed);
    }
    if block_jumps_refused_here {
        gc.block_entries_refused.fetch_add(1, Ordering::Relaxed);
    }
}

/// The SERIAL arm of [`walk_source_region_for_cset_refs`]: evacuate into the
/// collector's `pointer_map` and push fresh to-space copies onto the pause's
/// `work_list`.
pub struct SerialSourceWalk<'a> {
    pub gc: &'a G1Collector,
    pub regions: &'a mut Vec<G1Region>,
    pub cset: &'a RegionSet,
    pub pointer_map: &'a mut cratonvm_types::PointerMap,
    pub objects_copied: &'a mut usize,
    pub bytes_copied: &'a mut usize,
    pub work_list: &'a mut Vec<*mut u8>,
}

impl SourceWalkArm for SerialSourceWalk<'_> {
    #[inline]
    fn view(&self) -> RegionView<'_> {
        RegionView::Slice(&self.regions[..])
    }

    /// Unconditionally armed. The serial arm has run the header screens since
    /// 2026-09-02 and `CRATONVM_G1_PARALLEL_EVAC_SCREEN` governs the other arm
    /// by name; standing these down here would make that switch a HALF A/B.
    #[inline]
    fn screens_armed(&self) -> bool {
        true
    }

    /// The exact bound: this arm places every to-space copy with
    /// `alloc_in_type_locked`, which publishes the destination cursor as it
    /// bumps, so a holder is always below its region's cursor.
    #[inline]
    fn holder_bound(&self) -> HolderBound {
        HolderBound::Cursor
    }

    #[inline]
    fn card_rset(&self) -> bool {
        true
    }

    #[inline]
    fn card_clean(&self) -> bool {
        gc_flags().g1_card_clean
    }

    #[inline]
    fn sites(&self) -> &'static SourceWalkSites {
        &SERIAL_SOURCE_WALK_SITES
    }

    unsafe fn evacuate(&mut self, referent: *mut u8) -> Option<(*mut u8, bool)> {
        self.gc.evacuate_object(
            self.regions,
            referent,
            self.pointer_map,
            self.objects_copied,
            self.bytes_copied,
            self.cset,
        )
    }

    /// Step 9: `fresh` ignored — this RSet-source scan keeps its existing
    /// UNCONDITIONAL push. It is behaviour-identical (the Cheney loop below
    /// dedups by forwarding state) and it is what this arm has always done; the
    /// parallel arm gates on `fresh` because its queue is shared and a
    /// duplicate there costs another worker a scan.
    #[inline]
    fn note_evacuated(&mut self, _old_ptr: *mut u8, new_ptr: *mut u8, _fresh: bool) {
        self.work_list.push(new_ptr);
    }

    #[inline]
    fn note_humongous_reference(&self, idx: usize) {
        lane_a_note_humongous_reference_view(self.gc, RegionView::Slice(&self.regions[..]), idx);
    }

    #[inline]
    fn candidate_ok(&self, site: &'static str, holder: *mut u8, slot: usize, raw: usize) -> bool {
        self.gc.evacuation_candidate_is_an_object_view(
            RegionView::Slice(&self.regions[..]),
            site,
            holder,
            slot,
            raw,
        )
    }
}

/// The PARALLEL arm of [`walk_source_region_for_cset_refs`]: evacuate into this
/// worker's TLAB and publish fresh to-space copies onto the shared gray queue.
pub struct ParallelSourceWalk<'a, 'e> {
    pub shared: &'a SharedEvac<'e>,
    pub tlab: &'a mut TlabSet,
    pub forwards: &'a mut Vec<(usize, usize)>,
    pub objs: &'a mut usize,
    pub bytes: &'a mut usize,
    pub deferred_self_forwarded: &'a mut Vec<usize>,
    /// Fresh children found by this region's walk.
    ///
    /// Accumulated locally and published ONCE, at the end of the walk, in one
    /// acquisition of the shared queue's mutex. A source region can discover
    /// thousands of children and the queue lock is the evacuator's scaling wall
    /// (see `SharedEvac::run_worker`), so pushing per object would put that
    /// lock inside the loop the card screen exists to make cheap.
    pub newly: Vec<usize>,
    pub layout_cache: crate::g1::value_from_unaligned_ptr::CompactLayoutCache,
}

impl ParallelSourceWalk<'_, '_> {
    /// Hand this region's discovered children to the shared queue.
    ///
    /// `outstanding` is bumped BEFORE the push, as the termination protocol
    /// requires: every address that is queued anywhere must already have
    /// contributed its `+1`, or a worker can observe `outstanding == 0` while
    /// work is in flight.
    pub fn publish(&mut self) {
        if self.newly.is_empty() {
            return;
        }
        self.shared
            .outstanding
            .fetch_add(self.newly.len(), Ordering::AcqRel);
        let mut q = self.shared.queue.lock();
        q.extend(self.newly.drain(..));
    }
}

impl SourceWalkArm for ParallelSourceWalk<'_, '_> {
    #[inline]
    fn view(&self) -> RegionView<'_> {
        self.shared.view()
    }

    /// `CRATONVM_G1_PARALLEL_EVAC_SCREEN=0` stands the header screens down,
    /// which is the same-binary A/B for the crash clusters they were added for.
    /// Read from the per-pause snapshot rather than `gc_flags()`; see
    /// `SharedEvac::screens`.
    #[inline]
    fn screens_armed(&self) -> bool {
        self.shared.screens_armed()
    }

    /// `HolderBound::Cursor` is not available on this arm: workers carve
    /// per-worker TLABs and write the cursor back only in `retire_tlab`, so for
    /// the whole dispatch a freshly-copied holder sits ABOVE its region's
    /// published cursor and the cursor bound would return 0 for every object
    /// this arm copies. See `holder_walkable_slots_view` for the full argument.
    #[inline]
    fn holder_bound(&self) -> HolderBound {
        HolderBound::RegionEnd
    }

    #[inline]
    fn card_rset(&self) -> bool {
        self.shared.card_rset
    }

    #[inline]
    fn card_clean(&self) -> bool {
        self.shared.card_clean
    }

    #[inline]
    fn sites(&self) -> &'static SourceWalkSites {
        &PARALLEL_SOURCE_WALK_SITES
    }

    #[inline]
    unsafe fn evacuate(&mut self, referent: *mut u8) -> Option<(*mut u8, bool)> {
        self.shared.evacuate_screened_cached(
            self.tlab,
            &mut self.layout_cache,
            referent,
            self.forwards,
            self.objs,
            self.bytes,
            false,
        )
    }

    #[inline]
    unsafe fn evacuate_screened(
        &mut self,
        referent: *mut u8,
        proved: bool,
    ) -> Option<(*mut u8, bool)> {
        self.shared.evacuate_screened_cached(
            self.tlab,
            &mut self.layout_cache,
            referent,
            self.forwards,
            self.objs,
            self.bytes,
            proved,
        )
    }

    /// Gated on `fresh`, unlike the serial arm: this queue is SHARED, so a
    /// duplicate costs another worker a whole ref scan rather than one
    /// already-forwarded lookup.
    #[inline]
    fn note_evacuated(&mut self, old_ptr: *mut u8, new_ptr: *mut u8, fresh: bool) {
        if fresh {
            // SAFETY: the same evacuation contract as `Self::evacuate` above;
            // `new_ptr` is the copy this walk just made, header included.
            unsafe {
                self.shared.record_fresh_child_screened(
                    old_ptr,
                    new_ptr,
                    true,
                    &mut self.newly,
                    self.deferred_self_forwarded,
                );
                self.shared.note_overlay_owner_copied(old_ptr, new_ptr);
            }
            if self.newly.len() >= 1024 {
                self.publish();
            }
        }
    }

    #[inline]
    fn note_humongous_reference(&self, idx: usize) {
        lane_a_note_humongous_reference_view(self.shared.collector, self.shared.view(), idx);
    }

    #[inline]
    fn candidate_ok(&self, site: &'static str, holder: *mut u8, slot: usize, raw: usize) -> bool {
        self.shared.candidate_ok(site, holder, slot, raw)
    }

    #[inline]
    fn candidate_screen(
        &self,
        site: &'static str,
        holder: *mut u8,
        slot: usize,
        raw: usize,
    ) -> (bool, bool) {
        self.shared.candidate_screen(site, holder, slot, raw)
    }
}

/// Chunking metadata for splitting a large `HumongousStart` reference array
/// across all Phase-2 workers instead of letting one worker walk 16M+ elements.
pub struct HumongousSeedMeta {
    pub source_idx: usize,
    pub obj_ptr: usize,
    pub obj_size: usize,
    pub len: usize,
    pub card_clean: bool,
    pub young_redirty: bool,
    pub humongous_marks: bool,
    pub holds_cross_region: std::sync::atomic::AtomicBool,
    pub rewrote_a_slot: std::sync::atomic::AtomicBool,
}

pub enum HumongousSeedPlan {
    NotApplicable,
    AlreadySkipped,
    Split(HumongousSeedMeta),
}

/// Check whether `source_idx` is a single large `HumongousStart` reference array
/// (`len >= 32_768`) eligible for chunked Phase-2 seeding. Handles card-screen
/// region skips directly so clean humongous arrays spawn zero chunks.
#[allow(clippy::too_many_arguments)]
pub unsafe fn try_prepare_humongous_array_seed(
    gc: &G1Collector,
    shared: &SharedEvac<'_>,
    source_idx: usize,
    card_screen: bool,
    cursor_limit: Option<usize>,
    precise_rset_liveness: Option<&MarkProvenDead>,
    is_wholesale: bool,
) -> HumongousSeedPlan {
    let view = shared.view();
    let Some(r) = view.get(source_idx) else {
        return HumongousSeedPlan::NotApplicable;
    };
    if r.region_type() != RegionType::HumongousStart || r.pinned {
        return HumongousSeedPlan::NotApplicable;
    }
    let live = r.cursor();
    let cursor = match cursor_limit {
        Some(limit) => live.min(limit),
        None => live,
    };
    let base = r.data.addr() as *mut u8;
    if cursor < HEADER_SIZE {
        return HumongousSeedPlan::NotApplicable;
    }
    if jit_tlab_skip_span_len(shared.jit_skips, base as usize).is_some()
        || gap_filler_len(base).is_some()
    {
        return HumongousSeedPlan::NotApplicable;
    }
    let screens_armed = shared.screens_armed();
    if screens_armed && !gc.candidate_header_is_plausible_view(view, base as usize) {
        return HumongousSeedPlan::NotApplicable;
    }
    let header = &*(base as *const ObjectHeader);
    if is_humongous_filler(header) {
        return HumongousSeedPlan::NotApplicable;
    }
    let obj_size = object_total_size(header);
    if obj_size < HEADER_SIZE || obj_size > cursor {
        return HumongousSeedPlan::NotApplicable;
    }
    if header.kind() != ObjectKind::Array || header.element_type() != ArrayElementType::Reference {
        return HumongousSeedPlan::NotApplicable;
    }
    let rset_liveness = precise_rset_liveness.filter(|_| !is_wholesale);
    if rset_liveness.is_some_and(|dead| dead.proves_dead(r, source_idx, base as usize)) {
        return HumongousSeedPlan::NotApplicable;
    }
    let declared = header.array_length() as usize;
    let len = if screens_armed {
        gc.holder_walkable_slots_view(view, base, declared, 8, HolderBound::RegionEnd)
    } else {
        declared
    };
    if len < 32_768 {
        return HumongousSeedPlan::NotApplicable;
    }

    let screen = card_screen && shared.card_rset;
    let young_redirty = young_card_screen_armed(
        young_card_screen_switch(),
        shared.card_rset,
        shared.card_clean,
    );
    let young_cards = screen
        && young_redirty
        && cset_is_young_only(shared.cset, |i| view.get(i).map(|reg| reg.region_type()));
    let cards = gc.cards();
    let has_cards = if young_cards {
        cards.any_young_dirty_in(base as usize, cursor)
    } else {
        cards.any_dirty_in(base as usize, cursor)
    };
    if screen && !has_cards {
        gc.card_regions_offered.fetch_add(1, Ordering::Relaxed);
        gc.card_regions_skipped.fetch_add(1, Ordering::Relaxed);
        gc.card_bytes_skipped
            .fetch_add(cursor as u64, Ordering::Relaxed);
        return HumongousSeedPlan::AlreadySkipped;
    }

    // Object start card check if screen is enabled: if this single object covers the
    // whole region, `has_cards` above already verified a dirty card in `[base, obj_size)`.
    gc.card_regions_offered.fetch_add(1, Ordering::Relaxed);
    HumongousSeedPlan::Split(HumongousSeedMeta {
        source_idx,
        obj_ptr: base as usize,
        obj_size,
        len,
        card_clean: screen && shared.card_clean,
        young_redirty,
        humongous_marks: gc_flags().g1_humongous_marks,
        holds_cross_region: std::sync::atomic::AtomicBool::new(len < declared),
        rewrote_a_slot: std::sync::atomic::AtomicBool::new(false),
    })
}

/// Walk slot slice `[start, end)` of a split `HumongousStart` reference array
/// during Phase 2, evacuating CSet targets into `tlab` and publishing children.
#[allow(clippy::too_many_arguments)]
pub unsafe fn seed_humongous_array_chunk(
    shared: &SharedEvac<'_>,
    tlab: &mut TlabSet,
    forwards: &mut Vec<(usize, usize)>,
    objs: &mut usize,
    bytes: &mut usize,
    deferred_self_forwarded: &mut Vec<usize>,
    meta: &HumongousSeedMeta,
    start: usize,
    end: usize,
) {
    let mut arm = ParallelSourceWalk {
        shared,
        tlab,
        forwards,
        objs,
        bytes,
        deferred_self_forwarded,
        newly: Vec::new(),
        layout_cache: crate::g1::value_from_unaligned_ptr::CompactLayoutCache::default(),
    };
    let obj_ptr = meta.obj_ptr as *mut u8;
    let header = &*(obj_ptr as *const ObjectHeader);
    let extent = holder_extent(obj_ptr, header);
    let gc = shared.collector;
    let cset = shared.cset;
    let mut holds_cross = false;
    let mut rewrote = false;
    if shared.prefetch {
        let pre_end = end.min(start + crate::g1::EVAC_PREFETCH_AHEAD);
        for p in start..pre_end {
            let raw = std::ptr::read(obj_ptr.add(HEADER_SIZE + p * 8) as *const u64) as usize;
            if raw != 0 {
                shared.prefetch_cset_referent(raw);
            }
        }
    }
    for i in start..end {
        if shared.prefetch && i + crate::g1::EVAC_PREFETCH_AHEAD < end {
            let ahead = std::ptr::read(
                obj_ptr.add(HEADER_SIZE + (i + crate::g1::EVAC_PREFETCH_AHEAD) * 8) as *const u64,
            ) as usize;
            if ahead != 0 {
                shared.prefetch_cset_referent(ahead);
            }
        }
        let slot_ptr = obj_ptr.add(HEADER_SIZE + i * 8);
        let raw: u64 = std::ptr::read(slot_ptr as *const u64);
        if raw == 0 {
            continue;
        }
        let ref_ptr = raw as usize as *mut u8;
        let Some(ridx) = gc.lookup_region_for_addr(raw as usize) else {
            continue;
        };
        if meta.humongous_marks {
            arm.note_humongous_reference(ridx);
        }
        if ridx != meta.source_idx {
            holds_cross = true;
        }
        if !cset.contains(&ridx) {
            continue;
        }
        let (ok, proved) = arm.candidate_screen(PARALLEL_SOURCE_WALK_SITES.array, obj_ptr, i, raw as usize);
        if !ok {
            continue;
        }
        if let Some((new_ptr, fresh)) = arm.evacuate_screened(ref_ptr, proved) {
            note_ref_write(
                slot_ptr as usize,
                new_ptr as u64,
                meta.obj_ptr,
                extent,
                8,
            );
            std::ptr::write(slot_ptr as *mut u64, new_ptr as u64);
            rewrote = true;
            arm.note_evacuated(ref_ptr, new_ptr, fresh);
        }
    }
    arm.publish();
    if holds_cross {
        meta.holds_cross_region.store(true, Ordering::Relaxed);
    }
    if rewrote {
        meta.rewrote_a_slot.store(true, Ordering::Relaxed);
    }
}

/// Apply single-object post-walk card updates after all chunks of a split
/// `HumongousStart` array finish in Phase 2.
pub unsafe fn finish_humongous_array_seed(gc: &G1Collector, meta: &HumongousSeedMeta) {
    let cards = gc.cards();
    if meta.young_redirty && meta.rewrote_a_slot.load(Ordering::Relaxed) {
        let header = &*(meta.obj_ptr as *const ObjectHeader);
        cards.dirty_addr_at_object_start(crate::g1_cards::ObjectStart::of_header(header));
    }
    if meta.card_clean {
        let mut keep = cards.empty_set(meta.obj_ptr, meta.obj_size);
        if meta.holds_cross_region.load(Ordering::Relaxed) {
            keep.insert_addr(meta.obj_ptr);
        }
        let covered = cards.cards_in(meta.obj_ptr, meta.obj_size);
        let kept = keep.count();
        cards.clean_and_redirty(meta.obj_ptr, meta.obj_size, &keep);
        gc.card_cards_kept.fetch_add(kept as u64, Ordering::Relaxed);
        gc.card_cards_cleaned
            .fetch_add(covered.saturating_sub(kept) as u64, Ordering::Relaxed);
    }
    gc.card_bytes_scanned
        .fetch_add(meta.obj_size as u64, Ordering::Relaxed);
}

/// How [`walk_source_region_for_cset_refs`] chooses between the monotone card
/// cursor and the per-object card query. See [`lane_a_card_cursor`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CardCursorMode {
    /// Always ask `CardSet::any_in_span` per object.
    Off,
    /// Always walk the snapshot with the cursor.
    Always,
    /// Cursor on a region whose dirty cards are sparse, query on a dense one.
    Adaptive,
}

/// `CRATONVM_G1_CARD_CURSOR` — how Phase 2's per-object card screen reads the
/// snapshot. Three values, and the default is today's behaviour:
///
/// | value | mode | what the walk does |
/// |---|---|---|
/// | unset, `0`, anything unrecognised | [`CardCursorMode::Off`] | `CardSet::any_in_span` per object |
/// | `1`, `always` | [`CardCursorMode::Always`] | the monotone cursor, every region |
/// | `2`, `adaptive` | [`CardCursorMode::Adaptive`] | the cursor on a SPARSE region, the query on a dense one |
///
/// The cursor form is exactly equivalent — see `CardSet::next_dirty_addr` for
/// the argument, `the_cursor_agrees_with_any_in_span_everywhere` and
/// `gc/tests/g1_w2a_cards.rs` for the cross-checks — so this is a throughput
/// choice between two identical answers and picking the wrong side can never
/// change what a pause scans. The lever exists because "exactly equivalent" is
/// a claim about a reindexing of a bitset in a walk that also mutates the heap,
/// and a screen that under-approximates drops a live cross-region edge, which
/// is a use-after-free rather than a wrong number.
///
/// # WAVE 1 FLIPPED THIS OFF ON A MEASUREMENT. WAVE 2 COULD NOT REPRODUCE IT.
///
/// The wave-1 table said the cursor cost 8-9% on two workloads and won 3% on a
/// third:
///
/// | workload | cursor ON | cursor OFF |
/// |---|---|---|
/// | `G1ChurnPauseProbe 32 400 -Xmx256m` (median of 7) | 1867 ms | **1723 ms** |
/// | `HumongousChurn 48 6000 512 -Xmx160m` (best of 3) | 4134 ms | **3778 ms** |
/// | `HumongousWide 64 400 -Xmx512m` (best of 3) | **3379 ms** | 3483 ms |
///
/// **THE LEVER WAS NOT ENGAGED ON THE PATH THOSE RUNS MEASURED.** Until wave 2
/// the cursor existed only in `scan_source_region_for_cset_refs`, the SERIAL
/// Phase-2 walk. `CRATONVM_G1_PARALLEL_EVAC` is default-ON, so a young pause
/// seeds through `SharedEvac::seed_source_region` — which had no cursor, and
/// did not even take the snapshot a cursor reads unless card CLEANING was on.
/// On these three probes the young path is nearly the whole of the pause time,
/// so `CRATONVM_G1_CARD_CURSOR=0` and `=1` were the same binary doing the same
/// thing.
///
/// Wave 2 re-measured it as a NULL EXPERIMENT — the same wave-1 release binary,
/// `-XX:+UseG1GC`, five arms fully interleaved, twelve reps, 180 runs, zero
/// failures, every checksum expected:
///
/// | workload | median `base` | median `CARD_CURSOR=1` | paired mean delta |
/// |---|---|---|---|
/// | `churn` | 2466 ms | 2562 ms | **+4.5%** |
/// | `hchurn` | 4304 ms | 4100 ms | **-9.0%** |
/// | `humwide` | 3639 ms | 3518 ms | **+4.8%** |
///
/// Those three numbers are produced by a lever that provably does nothing on
/// the arm under test. They are the HOST'S NOISE FLOOR, they bracket every
/// effect wave 1 reported, and a per-rep paired delta on this box ranges over
/// [-26%, +43%]. Nothing in the wave-1 table survives that.
///
/// The same 180 runs also measured the cursor where it DID engage, with
/// `CRATONVM_G1_PARALLEL_EVAC=0`, and found a SMALLER effect than the null
/// experiment: paired mean `-1.4%` / `+0.6%` / `+3.8%` on the three workloads.
///
/// # SO WHY IS IT STILL OFF
///
/// Because wave 2 could not measure the thing it changed. Unifying the two
/// walks makes the cursor reachable from the parallel arm for the FIRST TIME,
/// and `Adaptive` is new code; measuring either needs a release binary of this
/// wave, which lane W2-A could not build (the worktree is shared with four
/// other lanes and did not compile as a whole for most of the session). A
/// default flipped on an argument rather than a number is precisely the mistake
/// wave 1 made, twice.
///
/// **What the next person should run**, on a wave-2 release binary, interleaved,
/// at least twelve reps, all three probes: `base` (this off),
/// `CRATONVM_G1_CARD_CURSOR=1`, `CRATONVM_G1_CARD_CURSOR=2`, plus the `base`
/// vs `base` null arm that calibrates the host. Flip the default to `2` if and
/// only if adaptive beats both fixed arms by more than that null arm's spread.
///
/// Read once per Phase-2 walk (its only caller), not latched per process: t1
/// wave 3 (lane Y) removed the `OnceLock`, so two G1 heaps in one process (or
/// a per-thread flag override) can differ.
pub fn lane_a_card_cursor() -> CardCursorMode {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => match v.trim() {
            "1" | "always" => CardCursorMode::Always,
            "2" | "adaptive" => CardCursorMode::Adaptive,
            // Anything else, including "0" and a typo, is today's
            // behaviour. A lever whose unparseable value silently enables
            // something is not a kill switch.
            _ => CardCursorMode::Off,
        },
        Err(_) => CardCursorMode::Off,
    }
}

/// `CRATONVM_G1_RSET_PRUNE_ON_SCAN` — drop a stale remembered-set entry at the
/// moment Phase 2 notices it, instead of leaving it for the next `cleanup`.
/// `0` (the default) restores "notice and skip, prune only at cleanup".
///
/// # Why dropping here is not a new risk
///
/// `G1Collector::live_rset_sources` ALREADY refuses to scan a source for which
/// `rset_entry_is_stale` holds — that refusal is the shipped fix for defect
/// G1-8 and is on by default. The staleness test is
/// `recorded_generation < source.recycled_in_generation()`, and
/// `recycled_in_generation` is monotone nondecreasing (it is only ever assigned
/// the current generation by `G1Region::reset`). So an entry that is stale in
/// one pause is stale in every later pause: deleting it removes an entry that
/// no future pause would have acted on, which is a strictly weaker act than the
/// skip that already happens.
///
/// What it buys is the thing the additive remembered set costs: without it a
/// dead entry is re-examined, and its source re-hashed, on EVERY pause until
/// the next concurrent-mark `cleanup` — which on a workload that does not
/// allocate hard enough to trigger a mark cycle is never. `cleanup`'s
/// `retain_sources_in_generation` applies the identical criterion; this only
/// changes how often it is applied.
///
/// The lever is here because the safe direction for a remembered set is always
/// over-retention, and a bisection that needs to ask "is a missing rset entry
/// the reason this crashed?" should not need a rebuild to take the pruning out.
///
/// # WAVE 1 FLIPPED THIS OFF; WAVE 2 FIXED WHAT IT ACTUALLY COST
///
/// The wave-1 table read:
///
/// | workload | prune ON | prune OFF |
/// |---|---|---|
/// | `G1ChurnPauseProbe 32 400 -Xmx256m` (median of 7) | 1867 ms | **1746 ms** |
/// | `HumongousChurn 48 6000 512 -Xmx160m` (best of 3) | 4134 ms | **3601 ms** |
/// | `HumongousWide 64 400 -Xmx512m` (best of 3) | **3379 ms** | 3515 ms |
///
/// and its diagnosis was "the prune takes the WRITE side of the remembered-set
/// lock on a path that otherwise only reads it". **There is no read side.**
/// `RememberedSet::sources` is a plain `parking_lot::Mutex<FxHashMap>`;
/// `sources_with_generations` — the non-prune arm — takes it exclusively too,
/// and allocates a `Vec<(usize, u64)>` where the prune arm allocates a
/// `Vec<usize>`. The non-prune arm was never the cheaper one on the lock.
///
/// What the prune arm genuinely cost more was the WRITING: a `HashMap::retain`
/// that dirties the table's control bytes, plus a `shrink_if_slack` that can
/// reallocate and rehash — paid on every scanned source whether or not one
/// single entry was dead. Wave 2 makes that conditional:
/// `RememberedSet::retain_and_collect_sources` now decides every entry in one
/// pass, and returns having written NOTHING when nothing was stale. That is
/// "gate the prune on a stale-fraction threshold" with the threshold at its
/// only defensible value — any positive threshold has to count the stale
/// entries first, and counting them is the whole pass.
///
/// So the arm is now strictly cheaper than the arm it is compared against, and
/// the wave-1 argument for leaving it off is gone. What has NOT changed is the
/// evidence. The same 180-run interleaved battery that refuted the cursor table
/// (see `lane_a_card_cursor`) measured this one on the wave-1 binary — i.e.
/// before the rewrite — and found paired mean deltas of `+0.3%` / `-4.3%` /
/// `+16.8%` against a host whose NULL arm spread was `+4.5%` / `-9.0%` /
/// `+4.8%`. That is "indistinguishable", not "better".
///
/// # SO WHY IS IT STILL OFF
///
/// Because the rewrite has not been measured. It cannot be from this worktree:
/// a release binary of this wave needs a tree that compiles, and this one is
/// shared with four concurrently-editing lanes. A default flipped on an
/// argument rather than a number is exactly the mistake wave 1 made.
///
/// **What the next person should run**, on a wave-2 release binary,
/// interleaved, at least twelve reps, all three probes: `base`,
/// `CRATONVM_G1_RSET_PRUNE_ON_SCAN=1`, and a `base` vs `base` null arm to
/// calibrate the host. The prediction the rewrite makes is that the two arms
/// are now indistinguishable on a heap with no stale entries and that the prune
/// arm wins on one that accumulates them without ever running a mark cycle —
/// which is the case the lever exists for and which none of these three probes
/// reaches. A workload that allocates below the IHOP and recycles regions
/// steadily is the one that would show it.
pub fn lane_a_rset_prune_on_scan() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => v.trim() == "1",
        Err(_) => false,
    }
}

/// `CRATONVM_G1_BLOCK_OFFSETS` — let the remembered-set source walk ENTER a
/// dirty card at the block-offset table's recorded object start instead of
/// sizing its way to that card from the region's base. `1` arms it; unset,
/// `0`, and anything unparseable are today's behaviour.
///
/// # This is the last O(region bytes) term in Phase 2, and the riskiest lever
/// # in the lane
///
/// The whole-region screen removed regions with no dirty card at all; the
/// per-object screen removed the expensive half of an object (its slot reads,
/// its `evacuation_candidate_is_an_object` call per non-null slot, its
/// `region_for_ptr` per candidate); the cursor made the screen itself nearly
/// free. What none of them removes is the LINEAR STEP — a header read, a
/// `candidate_header_is_plausible` and an `object_total_size` for every object
/// in the region, dirty card nearby or not — because a linear walk is the only
/// thing that can tell where an object begins. At 32 bytes per object that is
/// ~32000 iterations per 1 MiB source region, per pause, per source.
///
/// **And it is the one lever in this lane whose failure mode is not
/// over-scanning.** A wrong screen makes a pause slower. A wrong block offset
/// makes the walk decode an object header at an address that does not hold
/// one: not a slow path, a wild read. That is why this was designed twice and
/// deferred twice, and it is why the arming is opt-in, the entry is validated
/// before it is used, and a refusal is counted and logged rather than asserted.
///
/// # What arming it changes, precisely
///
/// Three things, and nothing else:
///
/// * the snapshot is built on a source region that would not otherwise have
///   needed one (the jump reads the same dirty-card cursor card cleaning and
///   `lane_a_card_cursor` read);
/// * the walk may skip forward to a validated object start instead of stepping
///   to it, and may stop early when no card from its position to the region's
///   end is dirty;
/// * three counters move.
///
/// The per-object screen, the clamps, the header-plausibility guards, the
/// DESYNC diagnostic and the `jit_tlab_skip_span_len` spans are untouched and
/// still decide every object the walk actually visits. Arming this can only
/// change WHICH objects are visited, never what is concluded about one.
///
/// # What it rests on, stated as a debt rather than a claim
///
/// Skipping an object because the cards it lies under are clean is sound only
/// if an object holding a cross-region reference always has a DIRTY START
/// CARD. That is the producer rule W3-B verified across all five producers and
/// nailed down with
/// `a_store_into_a_straddling_objects_last_slot_dirties_its_start_card`. The
/// default-off state is the price of that rule being an enforced invariant
/// rather than a proved theorem. See
/// `docs/feature-designs/concurrent-gc-maturation.md` §"the correctness
/// argument" for what would settle it.
pub fn lane_a_block_offsets() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(
        || match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            Ok(v) => v.trim() == "1",
            Err(_) => false,
        },
    )
}

/// How many `[g1] block-offset ENTRY REFUSED` reports the walk has produced.
///
/// A mis-entry is the failure this table exists to be unable to cause, so it
/// has to be counted rather than trusted-away — and counted where a RELEASE
/// binary can read it, which rules out `debug!`/`trace!` (`release_max_level_info`
/// compiles those out and no `RUST_LOG` recovers them). The rate-limited
/// `warn!` beside this counter is the narrative; the counter on the
/// `[GC] g1 block-offsets:` census line is the number.
pub static BLOCK_ENTRY_REFUSED: AtomicU64 = AtomicU64::new(0);

/// Jumps taken and bytes jumped over, across EVERY collector in the process.
///
/// These duplicate `G1Collector::block_jumps_taken` / `block_bytes_jumped`, and
/// the duplication is deliberate rather than an oversight. The per-collector
/// fields are what the `[GC] g1 block-offsets:` census line must read, because
/// that line describes one collector's run. These exist for the tests in
/// `gc/tests/g1_w4a_*.rs` that reuse a shared fixture which owns its collector
/// and does not hand it back: without a process-wide reading, those binaries
/// can assert that the heap survived the jump but not that the jump ever
/// happened — and a test that passes identically with its lever inert is the
/// exact failure §7.3 of `orchestrator-wave-1-measurements.md` attributes this
/// round's three retracted measurements to.
///
/// They cost two relaxed adds PER SOURCE REGION, not per object: the walk
/// accumulates locally and publishes once, the same way the card screen's own
/// counters do.
pub static BLOCK_JUMPS_TAKEN: AtomicU64 = AtomicU64::new(0);
pub static BLOCK_BYTES_JUMPED: AtomicU64 = AtomicU64::new(0);

/// W5-B follow-up (2026-09-21) — WHY a walk took no jump, process-wide.
///
/// `jumps == 0` has three causes and they want different responses, so a test
/// or an operator that sees it must not have to guess which one happened:
///
/// * the walk never had a GAP to jump over — every position it stood on was
///   already on a dirty card, or the rest of the region was clean and it took
///   the end-of-region break. That is a fact about the WORKLOAD, not about the
///   table: there was nothing to skip;
/// * the walk had a gap and the TABLE could not name a target for it — no
///   entry for the next dirty card, or one that does not move the walk
///   forward, or one outside the clamp;
/// * the table named something that is not an object header — that is
///   [`BLOCK_ENTRY_REFUSED`], and it already had a counter, because it is the
///   one that says the lever must go back off.
///
/// The first two did not. `gc/tests/g1_w5b_producer_rule_is_enforced.rs`
/// asserted `jumps > 0` and could report only `bytes_jumped=0 refused=0`,
/// which is the same output for both — so a failing run could not say whether
/// the fixture had stopped producing clean card runs or the table had stopped
/// answering for them. These two close that.
///
/// Same accounting discipline as the pair above: accumulated in a local and
/// published once per source region, never an atomic RMW inside the walk's
/// own loop.
pub static BLOCK_TAIL_BREAKS: AtomicU64 = AtomicU64::new(0);
pub static BLOCK_NO_ENTRY: AtomicU64 = AtomicU64::new(0);

/// W4-A — `(jumps, bytes_jumped, entries_refused)` over every G1 collector this
/// process has run. See [`BLOCK_JUMPS_TAKEN`] for why this exists beside
/// [`G1Collector::block_offset_census`].
pub fn block_offset_process_census() -> (u64, u64, u64) {
    (
        BLOCK_JUMPS_TAKEN.load(Ordering::Relaxed),
        BLOCK_BYTES_JUMPED.load(Ordering::Relaxed),
        BLOCK_ENTRY_REFUSED.load(Ordering::Relaxed),
    )
}

/// W5-B follow-up — `(end_of_region_breaks, gaps_with_no_usable_entry)` over
/// every G1 collector this process has run.
///
/// Read beside [`block_offset_process_census`] when `jumps == 0`. See
/// [`BLOCK_TAIL_BREAKS`] for what each element distinguishes; in one line:
/// a high first element with a zero second says the walk never had a gap the
/// table was asked about, and a non-zero second says it did and the table had
/// no answer.
pub fn block_offset_no_jump_census() -> (u64, u64) {
    (
        BLOCK_TAIL_BREAKS.load(Ordering::Relaxed),
        BLOCK_NO_ENTRY.load(Ordering::Relaxed),
    )
}

/// W5-B — `CRATONVM_G1_BLOCK_OFFSET_AUDIT`: after every jump, WALK THE BYTES
/// IT SKIPPED and check the claim the jump made about them.
///
/// # The hole this is for, which is not the one W4-A's refusal counter covers
///
/// The jump is licensed by one sentence: *an object holding a cross-region
/// reference always has a dirty START card*. W5-B closed two of the three ways
/// that can break.
///
/// * A producer naming a non-object address: closed by construction — it now
///   takes an `ObjectStart`, which is minted from a header reference or an
///   `ObjectRef` and not from a `usize`.
/// * A producer naming an address whose bytes are not a published header:
///   closed at runtime — the entry is vouched against `GC_FLAG_HEADER` before
///   it is installed, and a failure disarms the jump process-wide.
/// * **A producer naming a real object start that belongs to the WRONG
///   object.** Neither of the above sees it. The address vouches, the entry
///   decodes, `entries_refused` stays 0, and the walk quietly steps over an
///   object that needed scanning. That is the residue, and this is the
///   instrument for it.
///
/// # What it actually checks, and why it is the exact question
///
/// For every object STARTING in the skipped span: does it hold a reference
/// into a region other than the source? If one does, the producer rule is
/// broken — because a cross-region reference is exactly what dirties a card,
/// so that object's own start card would have been dirty and the walk would
/// have stopped at it instead of jumping over it.
///
/// Note what it deliberately does NOT check: whether the linear walk would
/// have *visited* those objects. It would visit some of them — an object
/// straddling into the dirty card from the left has a dirty byte range and so
/// passes the per-object screen — and skipping those is sound for the reason
/// `walk_source_region_for_cset_refs` gives. Auditing against the linear
/// walk's behaviour would report those as violations; auditing against the
/// cross-region reference reports only real ones.
///
/// # It is a SAMPLING PERIOD, not a boolean, and that is what lets it ship
///
/// The value is `N`: audit one jump in every `N`. `0` or unset is off, `1` is
/// every jump.
///
/// A full audit costs exactly the walk the jump removes, so `=1` measures
/// correctness and says nothing about performance — the right trade for a soak
/// or a CI arm and the wrong one for production. But the fault it is looking
/// for is a PRODUCER, and a producer is systematic: a card producer that names
/// the wrong object does it on every edge it records, not once. So a sample is
/// not a weaker version of the audit, it is the same audit with a delay — at
/// `N = 1024` a systematic violation surfaces within a few thousand jumps, and
/// on the workload in §6 of
/// `docs/feature-designs/concurrent-gc-maturation.md` that is
/// inside a single pause.
///
/// And the cost is exactly `1/N` of the thing the jump saves, because the
/// audited work IS the skipped work: at `N = 1024` a 99.6% reduction in bytes
/// stepped over becomes a 99.5% one.
///
/// **A violation disarms the jump**, exactly as an unvouched producer address
/// does (`g1_cards::disarm_block_offsets`). That is what makes a sampled audit
/// a safety mechanism rather than a diagnostic: it is the only thing in a
/// shipping binary that can see a producer naming a real object start
/// belonging to the wrong object.
///
/// Inert unless `CRATONVM_G1_BLOCK_OFFSETS` is also on, because there are no
/// jumps to audit otherwise.
pub fn lane_a_block_offset_audit() -> u32 {
    use std::sync::OnceLock;
    static PERIOD: OnceLock<u32> = OnceLock::new();
    *PERIOD.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
            // An unparseable value is OFF and not 1: this lever costs a walk,
            // and a typo must not silently buy one.
            Ok(v) => v.trim().parse::<u32>().unwrap_or(0),
            Err(_) => 0,
        }
    })
}

/// W5-B — the audit's sampling tick.
///
/// THREAD-LOCAL and non-atomic, deliberately. A process-wide counter would put
/// a relaxed RMW on every jump, which is the one per-jump cost W4-A went out of
/// its way not to pay (it accumulates `block_jumps_here` locally and publishes
/// once per region for exactly that reason). A per-region counter would be
/// worse than free — it would restart at every region and so would sample only
/// the same ordinal jump of each one. A thread-local `Cell` samples uniformly
/// across every jump a walker takes and costs a load, an add and a store on
/// memory that thread already owns.
pub fn block_audit_due(period: u32) -> bool {
    if period <= 1 {
        return period == 1;
    }
    thread_local! {
        static TICK: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }
    TICK.with(|t| {
        let n = t.get() + 1;
        if n >= period {
            t.set(0);
            true
        } else {
            t.set(n);
            false
        }
    })
}

/// W5-B — objects the audit walked, and objects it found holding a
/// cross-region reference inside a span a jump had skipped.
///
/// `BLOCK_AUDIT_VIOLATIONS` is the number the block-offset table's default
/// rests on, and it is a number that only has meaning beside
/// `BLOCK_AUDIT_OBJECTS`: zero violations out of zero objects says the audit
/// never ran, which is a different fact from the audit running clean. Both are
/// printed, always, for that reason.
pub static BLOCK_AUDIT_OBJECTS: AtomicU64 = AtomicU64::new(0);
pub static BLOCK_AUDIT_VIOLATIONS: AtomicU64 = AtomicU64::new(0);

/// W5-B — jump spans the audit actually walked.
///
/// Beside `jumps` this is the sampling period, MEASURED rather than declared:
/// `jumps / spans` should be the `N` the lever was set to. A lever whose
/// engagement cannot be read off the output is the failure
/// `orchestrator-wave-1-measurements.md` §7.1 retracted a whole measurement
/// for, and a sampling period is exactly the kind of lever that can silently
/// resolve to "never".
pub static BLOCK_AUDIT_SPANS: AtomicU64 = AtomicU64::new(0);

/// W5-B — `(audited_spans, audited_objects, violations)` over every G1
/// collector this process has run.
pub fn block_offset_audit_census() -> (u64, u64, u64) {
    (
        BLOCK_AUDIT_SPANS.load(Ordering::Relaxed),
        BLOCK_AUDIT_OBJECTS.load(Ordering::Relaxed),
        BLOCK_AUDIT_VIOLATIONS.load(Ordering::Relaxed),
    )
}

/// W5-B — walk `[from, to)` — the bytes a block-offset jump is about to skip —
/// and count the objects in it, and the objects in it that hold a reference
/// into another region.
///
/// Returns `(objects_walked, violations)`. A non-zero second element is the
/// producer rule breaking; see [`lane_a_block_offset_audit`] for why this is
/// the exact predicate and for the two shapes of violation it is NOT the
/// instrument for.
///
/// It reads and reports; it writes nothing and evacuates nothing. It stops at
/// the first thing it cannot parse rather than guessing, because a walk that
/// cannot resynchronise must not keep decoding — the same rule the real walk
/// follows, and here it costs only a shorter audit.
///
/// SAFETY: the same conditions [`walk_source_region_for_cset_refs`] runs
/// under — a stop-the-world safepoint with a quiescent region table, and
/// `[from, to)` inside the source region's pre-dispatch cursor.
pub unsafe fn audit_block_jump_span(
    gc: &G1Collector,
    view: RegionView<'_>,
    jit_skips: &[(usize, usize)],
    source_idx: usize,
    from: usize,
    to: usize,
) -> (u64, u64) {
    let mut objects = 0u64;
    let mut violations = 0u64;
    let mut at = from;
    while at < to {
        if let Some(skip) = jit_tlab_skip_span_len(jit_skips, at) {
            at = at.saturating_add(skip.max(1));
            continue;
        }
        if let Some(gap) = gap_filler_len(at as *const u8) {
            at = at.saturating_add(gap.max(1));
            continue;
        }
        // The audit applies the header screen UNCONDITIONALLY, unlike the
        // linear walk, whose parallel arm takes it from
        // `CRATONVM_G1_PARALLEL_EVAC_SCREEN`. An audit that faulted would be a
        // worse bug than the one it reports.
        if !gc.candidate_header_is_plausible_view(view, at) {
            break;
        }
        let header = &*(at as *const ObjectHeader);
        if is_humongous_filler(header) {
            break;
        }
        let size = object_total_size(header);
        if size < HEADER_SIZE || at + size > to {
            break;
        }
        objects += 1;
        let mut cross = false;
        let mut note = |raw: usize| {
            if raw == 0 || cross {
                return;
            }
            if let Some(ridx) = gc.lookup_region_for_addr(raw) {
                if ridx != source_idx {
                    cross = true;
                }
            }
        };
        if header.kind() == ObjectKind::Array {
            if header.element_type() == ArrayElementType::Reference {
                // Clamped to the object's OWN sized extent, which the two
                // checks above have already placed inside `[at, to)` — a
                // declared length is a `u32` and is not implied to be in
                // bounds by a plausible header, which is the same clamp the
                // real walk applies. Clamping to the object rather than to the
                // audited span also keeps a corrupt length from making the
                // audit read the NEXT object's slots and report them as this
                // one's, i.e. from manufacturing the violation it is here to
                // detect.
                let declared = header.array_length() as usize;
                let room = (size - HEADER_SIZE) / 8;
                for i in 0..declared.min(room) {
                    note(std::ptr::read((at + HEADER_SIZE + i * 8) as *const u64) as usize);
                }
            }
        } else {
            let declared = header.num_slots() as usize;
            let room = size.saturating_sub(HEADER_SIZE) / SLOT_SIZE;
            for_each_flat_object_reference_capped(
                at as *mut u8,
                header,
                0,
                declared.min(room),
                |_slot, raw, _compact| note(raw),
            );
        }
        if cross {
            violations += 1;
            // DISARM, exactly as an unvouched producer address does. This is
            // the one thing in a shipping binary that can see a producer
            // naming a real object start belonging to the WRONG object, and
            // the fault is the same fault: the table is no longer
            // trustworthy. Every later walk goes back to the linear one.
            crate::g1_cards::disarm_block_offsets();
            let n = BLOCK_AUDIT_VIOLATIONS.load(Ordering::Relaxed) + violations;
            if n <= 8 || n.is_power_of_two() {
                tracing::warn!(
                    "[g1] block-offset AUDIT VIOLATION (#{n}): region={source_idx} \
                     obj=0x{at:x} size={size} inside a span the jump skipped \
                     [0x{from:x}, 0x{to:x}) — this object holds a reference into ANOTHER \
                     REGION, so its own start card should have been dirty and the walk \
                     should have stopped at it. The producer rule is broken: some card \
                     producer named an address other than the start of the object holding \
                     the edge. CRATONVM_G1_BLOCK_OFFSETS must go back to off until it is \
                     found."
                );
            }
        }
        at += size;
    }
    (objects, violations)
}

// --- lane B helpers ---
//
// The evacuation closure and the Phase-4 whole-heap reference fix-up. Same
// convention as lane A above: a lever whose only readers are this lane's own
// code reads its environment variable through a `OnceLock` rather than growing
// a field on the process-wide `GcFlags` that every collector shares.

/// What [`G1Collector::verify_no_dangling_into_cset_within`] concluded about one
/// reference slot whose target lands inside a collection-set region.
///
/// The two verdicts have different proofs, different severities and different
/// suspects, and collapsing them (which is what a `bool` did) is why the one
/// always-on tripwire guarding the Phase-4 narrowing could not see a narrowing
/// bug. See the classifier for the full argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CsetSlotVerdict {
    /// The target has NO forwarding entry: nothing copied it, and Phase 5 then
    /// freed the region it lives in. The referent was never reached, which
    /// means some remembered set did not name this holder's region — the
    /// classic incomplete-rset use-after-free. Aborts in debug builds.
    Dangling,
    /// The target HAS a forwarding entry naming a different address, and its
    /// region is now `Free`: the object was copied and this slot was not
    /// rewritten to follow it. The payload is where the object actually went.
    ///
    /// This is the failure mode of a wrong Phase-4 walk set, and of nothing
    /// else — the referent is demonstrably reachable (something evacuated it),
    /// so the remembered set was not the thing that failed. Report-only: the
    /// walk is linear over surviving regions and so inspects dead holders too,
    /// and a dead holder's un-rewritten slot is floating garbage rather than a
    /// defect.
    StaleForward(usize),
}

/// Slots found holding a from-space address whose object this pause moved and
/// freed — see [`CsetSlotVerdict::StaleForward`].
///
/// Expected to be ZERO. A non-zero count means the Phase-4 fix-up did not walk
/// a region that held a live slot into the collection set, which on a heap
/// where that holder is reachable is a use-after-free waiting for the next
/// dereference. The first suspect is `G1Collector::phase4_regions_to_walk`.
pub static PHASE4_STALE_FORWARD_SLOTS: AtomicUsize = AtomicUsize::new(0);

/// Forwarding marks [`G1Collector::decode_forwarding_target`] accepted on
/// alignment alone because this collector's arena bounds were degenerate.
///
/// Expected to be ZERO, and unreachable as long as `G1Collector::new` publishes
/// `[arena_base, arena_end)` — which it does. It exists because the alternative
/// coding of that case (fold it into "outside the arena" and refuse) turns a
/// missing bound into a dropped forward, and a dropped forward is a dangling
/// slot rather than a wrong number.
pub static FORWARD_TARGET_UNBOUNDED_ARENA: AtomicUsize = AtomicUsize::new(0);

/// `CRATONVM_G1_VERIFY_STALE_FORWARD` — should the post-pause verifier report
/// the [`CsetSlotVerdict::StaleForward`] class as well as the dangling one?
/// `0` restores the pre-2026-09-20 behaviour, in which a slot pointing at a
/// forwarded from-space address was silently accepted.
///
/// Default ON, because the SAFE side of a verifier is the side that looks. The
/// cost is one `PointerMap` probe on a slot that has already been established
/// to point into a collection-set region — i.e. on the rarest kind of slot this
/// walk sees, and only after the walk decided to look at it at all. The lever
/// exists for the same reason lane A's do: a report-only check that turns out
/// to be noisy on some workload must be switchable without a rebuild, and this
/// one deliberately reports a population (dead holders in surviving regions)
/// that it cannot prove is live.
pub fn verify_stale_forward_enabled() -> bool {
    true
}

/// `CRATONVM_G1_FREE_CENSUS_ON_RECLAIM` — should Phase 5
/// ([`G1Collector::free_or_keep_cset`]) republish the free/young region census
/// after it resets the collection set? `0` restores the pre-2026-09-20
/// behaviour, in which a pause driven directly through `young_collection` /
/// `mixed_collection` left `free_region_count` reading its PRE-pause value
/// until the `needs_gc` backstop recount fired.
///
/// Default ON. See the call site for the four consumers a stale-low count
/// pushes in the pause-storm direction, and for why the full one-pass census is
/// the right form rather than a `fetch_add`.
pub fn free_census_on_reclaim() -> bool {
    true
}

/// `CRATONVM_G1_NARROW_DRAIN_FIXUP` — should the evacuation-failure drain
/// NARROW its Phase-4 fix-up walk the way the four pause drivers already do,
/// instead of passing `None` (walk every non-CSet, non-Free region end to end)?
///
/// `1` enables it; the default is `0`, which is today's WIDE walk.
///
/// # The number this lever is for
///
/// `HumongousChurn 48 6000 512 -Xmx160m --nojit -XX:+UseG1GC`, wave-1 binary,
/// `--verbose:gc`. The run enters the drain exactly once, and that pause's
/// `[GC-STAT]` line reads:
///
/// ```text
/// pause_us=607810 closure_us=434647 fixup_us=108979 fixup_regions=60 fixup_bytes=61818624
/// ```
///
/// So the drain's single fix-up walk is **109 ms, 60 regions, 61.8 MB — 18% of
/// a 608 ms pause**, and the bytes it walks are the whole live heap rather than
/// anything proportional to what the drain moved (it copied 25.5 MB). That is
/// what a narrow set would cut into, and it is the only number on this path
/// worth cutting.
///
/// # Why it defaults OFF, and what has to be true before it is flipped
///
/// The drivers have EARNED the narrowing: Phase 2 of a young or mixed pause
/// SCANS every region in `live_rset_sources(cset)`, so the narrow set is the
/// same set the pause already trusted for reachability. The drain does no
/// Phase-2 source scan at all — by design, because "any whole-region source
/// walk resurrects dead objects' targets, a compounding *undead* population"
/// (see [`G1Collector::retry_after_evacuation_failure`]). Its correctness for
/// the fix-up therefore rests on the WIDE walk rather than on the remembered
/// set, and narrowing it asserts that the kept regions' remembered sets are
/// complete — a property nothing in the drain establishes.
///
/// The assertion is plausible: the FAILING pause's own Phase 4 ran immediately
/// before this one and rebuilt the rsets through
/// `collect_outgoing_cross_region_edges`, and every mutator store reaches
/// `post_write_barrier_rset` (G1-2 is closed; the chain is written out in
/// `young_collection_serial`). What it does NOT cover is a holder whose region
/// was outside the FAILING pause's own narrow set and whose rset entry has
/// since been aged out by `rset_generation`. That is why this is a lever and
/// not an edit.
///
/// The tripwire, not a review argument, is what says whether it holds:
/// `PHASE4_STALE_FORWARD_SLOTS` (see
/// [`G1Collector::verify_no_dangling_into_cset_within`]) fires precisely when a
/// fix-up walk misses a region that held a slot into the collection set. A
/// narrowed drain that is wrong shows up there, with the address it should have
/// rewritten, rather than as a crash three pauses later.
pub fn narrow_drain_fixup() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => v.trim() == "1",
        Err(_) => false,
    }
}

/// Whole-heap Phase-4 fix-up walks charged to the evacuation-failure drain,
/// summed across every pass of every drain in the process.
///
/// The drain's own `phases.fixup_us` / `fixup_regions` / `fixup_bytes` are
/// OVERWRITTEN by each pass's `record_collection_with_phases`, so a pause that
/// spent most of its time in eight whole-heap walks reported the cost of the
/// last one. These four are the denominator that was missing: they are the only
/// evidence in the process that the drain's fix-up is a repeated O(heap) cost
/// rather than a single one.
pub static DRAIN_FIXUP_WALKS: AtomicUsize = AtomicUsize::new(0);
/// Regions walked by [`DRAIN_FIXUP_WALKS`], summed. See it.
pub static DRAIN_FIXUP_REGIONS: AtomicU64 = AtomicU64::new(0);
/// Region bytes walked by [`DRAIN_FIXUP_WALKS`], summed. See it.
pub static DRAIN_FIXUP_BYTES: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent in [`DRAIN_FIXUP_WALKS`], summed. See it.
pub static DRAIN_FIXUP_US: AtomicU64 = AtomicU64::new(0);

/// The drain fix-up totals, for tests and diagnostics: `(walks, regions, bytes,
/// micros)`. See [`DRAIN_FIXUP_WALKS`].
pub fn drain_fixup_totals() -> (usize, u64, u64, u64) {
    (
        DRAIN_FIXUP_WALKS.load(Ordering::Relaxed),
        DRAIN_FIXUP_REGIONS.load(Ordering::Relaxed),
        DRAIN_FIXUP_BYTES.load(Ordering::Relaxed),
        DRAIN_FIXUP_US.load(Ordering::Relaxed),
    )
}

/// Zero the drain fix-up totals. Tests only — a process-wide counter is not
/// meaningful to a test that cannot establish where it started.
pub fn reset_drain_fixup_totals() {
    DRAIN_FIXUP_WALKS.store(0, Ordering::Relaxed);
    DRAIN_FIXUP_REGIONS.store(0, Ordering::Relaxed);
    DRAIN_FIXUP_BYTES.store(0, Ordering::Relaxed);
    DRAIN_FIXUP_US.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------
// --- lane C helpers ---
//
// Constants and small free functions belonging to the marking cycle's policy
// and cost: the IHOP back-off, and the per-batch class-loader side-table
// snapshot the concurrent marker's scan loop reads instead of three
// process-global registries per object. Same convention as lanes A/B/D above —
// a number that only this lane's code reads lives here, not on the
// process-wide `GcFlags` every collector shares. The two BEHAVIOURS these serve
// are both flagged (`CRATONVM_G1_IHOP_BACKOFF`, `CRATONVM_G1_MARK_SIDE_TABLES`)
// because they change a policy decision and a per-object cost respectively;
// the constants themselves are not levers.

// ---------------------------------------------------------------------------
// LANE W3-C — the mark cycle's own two stop-the-world pauses
// ---------------------------------------------------------------------------

/// How many `remark` pauses have run, and how long they took in total.
///
/// # Why this had to be added before anything about the cycle could be measured
///
/// `remark` and `cleanup` are stop-the-world pauses and **neither was timed**.
/// They do not go through `record_collection_with_phases`, so they are absent
/// from `[GC-STAT]`, absent from the `pause_summary` percentiles, and absent
/// from every census in the tree. The only trace either leaves is the
/// `record_g1_cycle(CONCURRENT_CLEANUP, …)` line, which carries a region count
/// and no duration.
///
/// That is why
/// `w2c-the-satb-registry-walk-is-still-o-threads-per-cycle.md`'s own
/// measurement recipe — "the cleanup pause read out of
/// `collector_decision_report()` against thread count" — could not be followed
/// as written: there is no cleanup pause time in that report, or anywhere
/// else. The registry walk it is about is a fraction of a pause nothing
/// measures, so the fraction could not be put in proportion.
///
/// Totals rather than percentiles: a mark cycle happens a handful of times in
/// a run (this round's probes take between one and six), so a p99 over that
/// population is a number about three samples. `count` beside `us` is what
/// lets a reader divide, and the count is also the engagement figure — a run
/// that reports `cleanup_pauses=0` did not run a mark cycle at all, which on
/// the default JIT build is the ordinary outcome (see
/// `CRATONVM_G1_JIT_MARK_DRIVER`) and is the reading that every marking-lane
/// A/B in this round has to check first.
pub static REMARK_PAUSES: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent in [`REMARK_PAUSES`], summed. See it.
pub static REMARK_PAUSE_US: AtomicU64 = AtomicU64::new(0);
/// How many `cleanup` pauses have run. See [`REMARK_PAUSES`].
pub static CLEANUP_PAUSES: AtomicU64 = AtomicU64::new(0);
/// Microseconds spent in [`CLEANUP_PAUSES`], summed. See [`REMARK_PAUSES`].
pub static CLEANUP_PAUSE_US: AtomicU64 = AtomicU64::new(0);

/// How many times `check_ihop` has been consulted, and how many mark cycles
/// were too short for `fold_mark_cycle_sample` to time.
///
/// # Why `backoff_suppressions` needed a denominator
///
/// `mark_backoff_suppressions`' own doc called it "how many times `check_ihop`
/// has declined a CYCLE", and `w2c-what-landed-and-what-to-measure.md`'s A/B
/// recipe reads it as "suppressions climb". It is not a count of cycles.
/// `check_ihop` is a POLL — the mark-cycle trigger consults it on the
/// allocation slow path and from the JIT's mark driver, so it runs thousands
/// of times per second while occupancy sits above the threshold, and ONE
/// deferred cycle produces one increment per consult for as long as it stays
/// deferred. Reading it as cycles overstates by whatever the poll rate
/// happened to be, which is the same shape as `work_units_done` being
/// `steps × 256` (`lane-c-small-residue.md` §5).
///
/// The number is not wrong, its NAME was: it is declined polls. So it is
/// reported as `backoff_declined_polls`, with `ihop_polls` beside it as the
/// denominator, and the ratio is the fraction of the run spent above the
/// threshold with the back-off holding the cycle off. Neither number lies and
/// neither can be read as the other.
pub static IHOP_POLLS: AtomicU64 = AtomicU64::new(0);

/// LANE W6-B — the exactness bound the poll gate trades for its speed, and
/// where the fast-decline count lives.
///
/// The count itself is `G1Collector::backoff_fast_declines`, a FIELD and not a
/// static, for a reason worth stating: [`IHOP_POLLS`] is process-wide because
/// its denominator role is process-wide, but a test binary runs its tests as
/// parallel threads in ONE process, so an assertion on a process-wide
/// fast-decline counter would be an assertion on what the rest of the suite
/// happened to be doing at the time. Per-collector, it is exact.
///
/// # The cost two batteries counted and neither could price
///
/// [`IHOP_POLLS`] explains why the back-off's decline counter is a poll
/// counter. This constant, and the flag it serves, are about what a poll
/// COSTS. With the back-off armed,
/// the declined path performs two read-modify-writes on two process-global
/// cache lines — [`IHOP_POLLS`] and `mark_backoff_suppressions` — and it is
/// taken at the ALLOCATION rate, because a suppressed cycle never sets
/// `g1_is_marking_active()` and so never short-circuits the JIT mark driver
/// that `jit_new_object`, `jit_newarray` and `jit_anewarray_object` each call
/// unconditionally. Wave 3 measured 1 616 077 polls; wave 5 measured
/// 41 641 213 against 130 mark cycles, i.e. 320 000 declined polls per mark
/// cycle saved. Both probes were SINGLE-THREADED, so both measured those two
/// lines uncontended and both said so.
///
/// Under the flag the declined path is two relaxed loads, a compare and a
/// thread-local increment, flushed to the three counters every
/// this batch. `backoff_fast_declines` is how many polls took that path, so
/// an arm on which it reads zero is an arm on which the fix could not engage
/// and whose wall clock says nothing about it — §7.1 of
/// `orchestrator-wave-1-measurements.md`.
///
/// # How many fast-path declines one thread accumulates before publishing them
///
/// The trade is exactness against shared writes, and it is bounded rather than
/// hand-waved: at any instant the three counters under-report by at most
/// `IHOP_POLL_FLUSH_BATCH - 1` per LIVE THREAD that is mid-streak. Against the
/// 41 641 213 polls wave 5 measured on one thread, 1023 is 0.002%; against 32
/// threads it is 32 736, or 0.08%. `flush_gated_polls` publishes a thread's
/// residue on demand for a test that needs the exact number.
///
/// A power of two so the comparison is a single test, and not larger because
/// the point is to remove a CONTENDED write, not to remove writes: one
/// uncontended `fetch_add` per 1024 allocations is already below anything this
/// round can measure.
pub const IHOP_POLL_FLUSH_BATCH: u64 = 1024;

thread_local! {
    /// `(collector identity, unflushed fast-path declines)` for this thread.
    ///
    /// The identity is carried because the counters it flushes into are not
    /// all process-wide: [`IHOP_POLLS`] is, but `mark_backoff_suppressions`
    /// and `backoff_fast_declines` belong to one `G1Collector`. A
    /// process with two live collectors (a test shape — a VM has one) would
    /// otherwise credit one collector's declines to the other. On a switch the
    /// residue still reaches the process-wide poll count, and only the
    /// departed collector's own two counters lose it, bounded by
    /// [`IHOP_POLL_FLUSH_BATCH`].
    pub static IHOP_POLL_BATCH: std::cell::Cell<(usize, u64)> =
        const { std::cell::Cell::new((0, 0)) };
}

/// Mark cycles whose measured duration rounded to zero milliseconds, so
/// `fold_mark_cycle_sample` returned before folding anything.
///
/// The disengagement `ihop_disengaged_cycles` counts is "the cycle folded a
/// rate of zero". A cycle too short to time folds NO rate at all — it returns
/// one line earlier — so it is at least as disengaged and was invisible to the
/// counter that exists to report disengagement. On a small heap with a fast
/// marker (this round's probes take 92-156 ms for a cycle, but a 4 MiB old
/// generation can finish in under a millisecond) that is not a hypothetical.
///
/// Counted separately rather than folded into `disengaged_cycles` because they
/// have different fixes: a zero RATE is what
/// `CRATONVM_G1_IHOP_GROSS_GROWTH` addresses, whereas a zero DURATION means
/// the clock's resolution is the problem and the answer is to carry the
/// duration in microseconds.
pub static IHOP_UNTIMED_CYCLES: AtomicU64 = AtomicU64::new(0);

/// `(ihop_polls, untimed_cycles)` — see [`IHOP_POLLS`] and
/// [`IHOP_UNTIMED_CYCLES`].
pub fn ihop_poll_census() -> (u64, u64) {
    (
        IHOP_POLLS.load(Ordering::Relaxed),
        IHOP_UNTIMED_CYCLES.load(Ordering::Relaxed),
    )
}

/// Mark cycles that started, and of those, how many started with at least one
/// of the three class-loader registries populated.
///
/// # The falsification test `CRATONVM_G1_MARK_SIDE_TABLES` needs
///
/// `w2c-what-landed-and-what-to-measure.md` says an A/B over that flag which
/// comes out identical "would mean the registries are empty in the workload
/// being measured and the probe is not exercising the path at all". Nothing in
/// the tree could tell those two apart, so the falsification could not be
/// applied — which is §7.2's mistake exactly (the humongous probes that
/// allocated one humongous object) waiting to happen again.
///
/// `cycles_with_pins == 0` says the registries were empty for every cycle in
/// the run and the flag had nothing to remove, whatever the wall clock said.
/// The only writer of `loader_pin::set_loader_pin` in the tree is
/// `native-builtins/src/classloader.rs`'s `defineClass`, so a program that
/// never defines a class through a user `ClassLoader` reads zero here by
/// construction — see `LoaderChurnProbe.java`, which is the arm
/// that flips it.
///
/// Sampled once per cycle in `start_concurrent_mark`, on BOTH arms of the
/// flag, because the question is about the WORKLOAD and not about the lever.
/// One snapshot per mark cycle is not a cost worth guarding.
pub static MARK_CYCLES_STARTED: AtomicU64 = AtomicU64::new(0);
/// Of [`MARK_CYCLES_STARTED`], those that began with a non-empty registry.
pub static MARK_CYCLES_WITH_LOADER_PINS: AtomicU64 = AtomicU64::new(0);
/// Objects for which the tail of `scan_object_refs` actually enqueued a
/// class-loader root. See [`MARK_CYCLES_STARTED`].
///
/// Counted inside the `if let Some(..)` arms, so it costs nothing on an object
/// with no loader edge — which is every object in a run with empty registries,
/// and the reason this is safe to leave on: instrumenting the marking hot path
/// with an unconditional atomic would confound the very comparison the flag is
/// for.
pub static MARK_LOADER_TAIL_HITS: AtomicU64 = AtomicU64::new(0);

/// `(cycles_started, cycles_with_loader_pins, loader_tail_hits)` — see
/// [`MARK_CYCLES_STARTED`].
pub fn mark_loader_pin_census() -> (u64, u64, u64) {
    (
        MARK_CYCLES_STARTED.load(Ordering::Relaxed),
        MARK_CYCLES_WITH_LOADER_PINS.load(Ordering::Relaxed),
        MARK_LOADER_TAIL_HITS.load(Ordering::Relaxed),
    )
}

/// `(remark_pauses, remark_us, cleanup_pauses, cleanup_us)` — see
/// [`REMARK_PAUSES`].
pub fn mark_pause_totals() -> (u64, u64, u64, u64) {
    (
        REMARK_PAUSES.load(Ordering::Relaxed),
        REMARK_PAUSE_US.load(Ordering::Relaxed),
        CLEANUP_PAUSES.load(Ordering::Relaxed),
        CLEANUP_PAUSE_US.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// LANE W7-D (2026-09-21) — the mark-cycle DOOR census
// ---------------------------------------------------------------------------

/// Which call site advanced (or declined to advance) the G1 mark-cycle
/// lifecycle. One variant per production driver of the finish/start pair.
///
/// # Why a door breakdown and not another total
///
/// [`REMARK_PAUSES`] and [`CLEANUP_PAUSES`] say how many cycles started and
/// how many finished. They do not say WHY a cycle did not finish, and the two
/// candidate explanations demand opposite fixes:
///
/// * **nothing asked.** No driver ran at an instant when the cycle was
///   finishable — the lifecycle has no door on the path this workload's pauses
///   take. Fix: put a door there.
/// * **something asked and the answer was no.** A driver ran, found
///   `g1_is_marking_active()` true and `g1_concurrent_mark_finished()` false,
///   and returned — over and over. Fix: find out why the background marker
///   never reaches a fixed point.
///
/// `remark_pauses=1, cleanup_pauses=0` — the reading
/// `w6m-a-workload-that-mixes.md` §4 reports on a DEFAULT build — is produced
/// identically by both, and no counter in the tree separated them. `waiting`
/// against `visits` does: a run whose every door reads `visits=0` took no
/// pause through any driver, and a run that reads `waiting=N` for large `N`
/// asked `N` times and was refused.
///
/// The same breakdown is the engagement reading `README.md` §5 rule 3 asks
/// for, at the granularity the decision needs: "how many cycles completed" is
/// the outcome, "which door completed them" is the thing a default can be
/// changed about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkDoor {
    /// `interpreter::maybe_concurrent_gc`, in `maybe_gc`'s epilogue — i.e. only
    /// on a pause `maybe_gc` itself took.
    MaybeGc = 0,
    /// `interpreter::maybe_gc_forced_at`'s epilogue — the allocation-failure
    /// pause, which is where G1 takes most of its young pauses and which had
    /// no lifecycle call at all before this lane. Gated by
    /// `CRATONVM_G1_ALLOC_MARK_DRIVE` (default ON, `=0` restores the old
    /// behaviour).
    AllocFail = 1,
    /// `jit::helpers::jit_drive_g1_concurrent_mark`, gated on the opt-in
    /// `CRATONVM_G1_JIT_MARK_DRIVER`.
    JitDriver = 2,
    /// `interpreter::g1_force_full_cycle` — `System.gc()` and the pre-OOM
    /// ladder.
    ForceFull = 3,
    /// `interpreter::force_gc_from_native`'s own `maybe_concurrent_gc` call.
    /// Counted apart from [`Self::MaybeGc`] because `System.gc()` is the
    /// flag-free route §4 of `w6m-a-workload-that-mixes.md` measured working,
    /// and a door census in which it is invisible cannot be used to check that
    /// account.
    SystemGc = 4,
}

impl MarkDoor {
    /// Stable report token. Used in `[GC] g1 mark-doors:` and nowhere else.
    pub fn token(self) -> &'static str {
        match self {
            MarkDoor::MaybeGc => "maybe_gc",
            MarkDoor::AllocFail => "alloc_fail",
            MarkDoor::JitDriver => "jit_driver",
            MarkDoor::ForceFull => "force_full",
            MarkDoor::SystemGc => "system_gc",
        }
    }
}

/// What one visit to a [`MarkDoor`] did.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MarkDoorOutcome {
    /// No cycle was active and the IHOP policy declined to start one. The
    /// door worked; the collector had nothing for it.
    Idle,
    /// No cycle was active and this visit opened one.
    Started,
    /// A cycle was active and the background marker had NOT drained. This is
    /// the reading that distinguishes "nobody asked" from "asked and refused".
    Waiting,
    /// A cycle was active, the marker had drained, and this visit ran the STW
    /// final remark + cleanup to completion.
    Finished,
    /// As [`Self::Finished`], but the brief STW was lost to another initiator,
    /// so nothing ran and the cycle stays open for the next visit.
    LostStw,
}

/// Per-door counters. Five `u64`s, written once per pause at most — this is
/// not on an allocation path, and the JIT driver (the one door that IS polled
/// at allocation rate) reaches `note_mark_door` only after its own
/// `g1_jit_mark_driver_enabled()` gate.
pub struct MarkDoorCounts {
    /// Total visits, i.e. `idle + started + waiting + finished + lost_stw`.
    pub visits: AtomicU64,
    /// [`MarkDoorOutcome::Idle`].
    pub idle: AtomicU64,
    /// [`MarkDoorOutcome::Started`].
    pub started: AtomicU64,
    /// [`MarkDoorOutcome::Waiting`].
    pub waiting: AtomicU64,
    /// [`MarkDoorOutcome::Finished`].
    pub finished: AtomicU64,
    /// [`MarkDoorOutcome::LostStw`].
    pub lost_stw: AtomicU64,
}

impl MarkDoorCounts {
    pub const fn new() -> Self {
        Self {
            visits: AtomicU64::new(0),
            idle: AtomicU64::new(0),
            started: AtomicU64::new(0),
            waiting: AtomicU64::new(0),
            finished: AtomicU64::new(0),
            lost_stw: AtomicU64::new(0),
        }
    }
}

/// The five doors, indexed by [`MarkDoor`]'s discriminant.
pub static MARK_DOORS: [MarkDoorCounts; 5] = [
    MarkDoorCounts::new(),
    MarkDoorCounts::new(),
    MarkDoorCounts::new(),
    MarkDoorCounts::new(),
    MarkDoorCounts::new(),
];

/// Record one visit to one mark-cycle driver. See [`MarkDoor`].
pub fn note_mark_door(door: MarkDoor, outcome: MarkDoorOutcome) {
    let c = &MARK_DOORS[door as usize];
    c.visits.fetch_add(1, Ordering::Relaxed);
    let field = match outcome {
        MarkDoorOutcome::Idle => &c.idle,
        MarkDoorOutcome::Started => &c.started,
        MarkDoorOutcome::Waiting => &c.waiting,
        MarkDoorOutcome::Finished => &c.finished,
        MarkDoorOutcome::LostStw => &c.lost_stw,
    };
    field.fetch_add(1, Ordering::Relaxed);
}

/// `(door, visits, idle, started, waiting, finished, lost_stw)` for every
/// door, in [`MarkDoor`] order. Zero rows are INCLUDED: a door that was never
/// reached is the finding, and dropping it would make "no driver ran" look
/// like "no report".
pub fn mark_door_census() -> Vec<(&'static str, u64, u64, u64, u64, u64, u64)> {
    [
        MarkDoor::MaybeGc,
        MarkDoor::AllocFail,
        MarkDoor::JitDriver,
        MarkDoor::ForceFull,
        MarkDoor::SystemGc,
    ]
    .into_iter()
    .map(|d| {
        let c = &MARK_DOORS[d as usize];
        (
            d.token(),
            c.visits.load(Ordering::Relaxed),
            c.idle.load(Ordering::Relaxed),
            c.started.load(Ordering::Relaxed),
            c.waiting.load(Ordering::Relaxed),
            c.finished.load(Ordering::Relaxed),
            c.lost_stw.load(Ordering::Relaxed),
        )
    })
    .collect()
}

/// One `[GC] g1 mark-doors:` line per door, newline-separated, no trailing
/// newline — the shape `gc_metrics`'s summary builder splices.
pub fn mark_door_report() -> String {
    let mut s = String::new();
    for (i, (tok, visits, idle, started, waiting, finished, lost)) in
        mark_door_census().into_iter().enumerate()
    {
        if i > 0 {
            s.push('\n');
        }
        s.push_str(&format!(
            "[GC] g1 mark-doors: door={tok} visits={visits} idle={idle} started={started} \
             waiting={waiting} finished={finished} lost_stw={lost}"
        ));
    }
    s
}

#[doc(hidden)]
/// Test-only reset, so a delta assertion in a multi-test binary is meaningful
/// (`README.md` §5 / house rule 8: process-global counters are asserted as
/// DELTAS). Not called from any production path.
pub fn reset_mark_door_census_for_tests() {
    for c in MARK_DOORS.iter() {
        c.visits.store(0, Ordering::Relaxed);
        c.idle.store(0, Ordering::Relaxed);
        c.started.store(0, Ordering::Relaxed);
        c.waiting.store(0, Ordering::Relaxed);
        c.finished.store(0, Ordering::Relaxed);
        c.lost_stw.store(0, Ordering::Relaxed);
    }
}

/// The widest the mark-cycle back-off may open: `check_ihop` will never demand
/// more than this many regions of old-generation growth before it re-arms.
///
/// A cap is what stops the back-off from becoming a ban. Each futile cycle
/// doubles the gate, so without one a long run of cycles that legitimately
/// reclaim little (a heap sized very close to its live set, where each cycle
/// frees a region or two) would widen the requirement past the free space that
/// remains and marking would stop for the life of the process — trading a
/// collector that spins for a collector that has quietly given up, which is a
/// strictly worse failure because it ends in OOM rather than in slowness.
///
/// 64 regions is 64 MiB at the default region size. Not measured: it is a
/// bound on a pathology, not a tuning knob, and the number that matters is the
/// one at the other end (1 region, the re-arm), which is what governs the
/// steady state.
pub const MARK_BACKOFF_MAX_REGIONS: usize = 64;

/// A cycle that got back less than this fraction of the old generation counts
/// as having reclaimed NOTHING for back-off purposes: 1/64th, i.e. ~1.5%.
///
/// A strict `reclaimed == 0` test is too sharp to be useful. The shape this
/// guards against is a cycle that marks the whole old generation and frees one
/// region out of two hundred — which is not reclamation, it is noise, and
/// treating it as progress re-arms the trigger immediately and restores exactly
/// the spin the back-off exists to stop. Expressed as a fraction of occupancy
/// rather than as an absolute so it means the same thing on a 64 MiB heap and
/// on a 64 GiB one.
pub const MARK_BACKOFF_PROGRESS_SHIFT: u32 = 6;

/// LANE W5-C — how many collections may complete with the back-off's growth
/// gate shut before `check_ihop` lets a cycle through anyway, under
/// `CRATONVM_G1_MARK_BACKOFF_DEADLINE`.
///
/// # Why there is a deadline at all
///
/// The back-off re-arms on old-generation GROWTH and on nothing else. Growth
/// detects promotion. It does not, and cannot, detect the other thing that
/// makes a new cycle worth running — old objects DYING — because that event
/// leaves the occupancy it tests exactly where it was. A program that drops a
/// large long-lived data set and then allocates only short-lived objects
/// therefore holds the gate shut across the one cycle that would have
/// reclaimed a large fraction of the heap. The evidence the gate wants is
/// evidence only the cycle can produce, so the bound has to be a deadline
/// rather than a sharper test.
///
/// # Why this number
///
/// It is `mixed_gc_count_target`, the number of mixed collections one mark
/// cycle's liveness data is budgeted to feed (`cleanup` stores exactly that
/// into `mixed_gc_remaining`). That is the collector's own statement of how
/// long a cycle's findings stay useful: once that many collections have gone
/// by, the data the last cycle produced is spent by construction, and the next
/// cycle is not "the same question again" even if occupancy has not moved.
///
/// Deliberately NOT scaled by the back-off width. The doubling already lives
/// in the growth gate, where it belongs — the wider that gate gets, the longer
/// the deadline is the thing actually letting cycles through, and making both
/// grow would compound two hysteresis terms into an interval nobody can
/// predict from either. Flat here means the worst case is stateable in one
/// sentence: with the deadline armed, marking runs at most once per
/// `MARK_BACKOFF_DEADLINE_COLLECTIONS` collections and at least that often
/// while occupancy sits above the threshold.
///
/// It bounds the back-off; it does not weaken it. The spin wave 2 set out to
/// stop is a cycle that starts with ZERO collections in between, and the
/// hardest case for this constant — `G1ChurnPauseProbe`, where every cycle is
/// futile by construction — is measured with it in
/// `docs/feature-designs/concurrent-gc-maturation.md`.
pub const MARK_BACKOFF_DEADLINE_COLLECTIONS: u64 = 8;

/// A marking worker's private snapshot of the three class-loader side tables,
/// so the scan loop stops asking three process-global registries about every
/// object it touches.
///
/// # What it replaces
///
/// The tail of [`G1Collector::scan_object_refs`] follows three edges the object
/// header cannot carry — instance → defining loader, loader → class mirrors,
/// loader → metadata oops — by calling `loader_pin::loader_pin_addr_where`,
/// `mirror_pin::mirrors_for_loader` and `metadata_pin::roots_for_loader`. Each
/// has a `NON_EMPTY` latch, so on a run with no user-defined `ClassLoader` that
/// is three relaxed atomic loads per scanned object: cheap, and deliberately
/// so.
///
/// The moment ONE custom loader exists the latch flips permanently and every
/// one of the three becomes, per scanned object and per marking worker, a
/// process-global `RwLock` acquisition — plus, for the two `*_for_loader`
/// calls, a heap-allocating `Vec` clone. `metadata_pin::roots_for_loader`'s own
/// doc names this cost and `metadata_pin::snapshot` exists precisely to replace
/// it; the generational young marker already uses the snapshot form and G1's
/// concurrent marker did not. It lands on the hottest loop the collector owns,
/// on N concurrent threads, for the whole duration of a cycle — and every
/// Spring / Tomcat / Hibernate workload has custom loaders, so the latch is
/// flipped in every application this collector is being evaluated on.
///
/// # Why a snapshot is sound here, and what bounds its staleness
///
/// The VALUES are heap addresses, and an evacuation pause both moves the
/// objects and rewrites the registries (`replace_loader_pins` /
/// `replace_mirror_pins` / `replace_metadata_pins`, all from the post-GC
/// reconciliation). A snapshot taken before a pause and used after it would
/// therefore be testing post-pause addresses against pre-pause keys, and would
/// MISS edges — an under-mark, the one direction that is not safe.
///
/// So the snapshot carries the `rset_cache_epoch` it was taken at, and the scan
/// loop rebuilds it whenever that epoch moves. Every collection path that can
/// reclassify or move a region bumps that epoch as its first act under the
/// regions WRITE guard, and a marking batch holds the regions READ guard for
/// its whole duration — so within one batch no pause can have interleaved, and
/// across batches the epoch check catches every one that did.
///
/// What remains is a registry mutation with no safepoint behind it:
/// `set_loader_pin` and `add_mirror_pin` on the class-definition path, which a
/// running mutator can execute mid-batch. Those add rows for a class being
/// defined RIGHT NOW, whose mirror was therefore allocated after the mark-start
/// snapshot — above TAMS, hence implicitly live for this cycle whether or not
/// the marker follows the edge. The loader itself is on the defining thread's
/// stack for the duration, so the remark root re-scan reaches it. The window is
/// one batch wide and the objects in it are live by another argument; that is
/// the whole of the freshness question, and it is why this is a snapshot rather
/// than a cache with an invalidation protocol.
///
/// # Why per-worker rather than shared
///
/// One clone per pause per marking worker, against one shared copy behind a
/// lock that every worker would contend for on the same cadence. A pause is
/// milliseconds; a clone of a registry holding a few thousand rows is
/// microseconds, and it happens on the order of once per pause rather than once
/// per object. The contended shared copy is the shape this type exists to
/// remove.
pub struct MarkSideTables {
    /// `rset_cache_epoch` when the three maps below were read. A batch that
    /// finds a different epoch rebuilds before scanning anything.
    pub epoch: u64,
    /// `class_id -> defining loader address`, or `None` when the registry is
    /// empty — which is the case this type must not make more expensive, so
    /// `None` costs exactly the relaxed latch load the old code paid.
    pub loaders: Option<FxHashMap<u32, usize>>,
    /// `loader address -> class mirror addresses`.
    pub mirrors: Option<FxHashMap<usize, Vec<usize>>>,
    /// `loader address -> metadata oop addresses`.
    pub metadata: Option<FxHashMap<usize, Vec<usize>>>,
    /// `metadata_pin::generation()`, read BEFORE the maps above (a change
    /// that races the capture leaves a newer generation behind, so the next
    /// batch recaptures). gcd d10/r: the initial mark files this cycle's
    /// `metadata_pin` rows AFTER the worker started, and the remark adds to
    /// them -- registry writes that do not move `epoch`. The loader and mirror
    /// registries are not keyed: their only writes outside a pause are the
    /// class-definition rows the freshness argument above covers, and keying
    /// them would recapture once per defined class.
    pub metadata_generation: u64,
}

impl MarkSideTables {
    /// Read all three registries once. Each returns `None` when its
    /// `NON_EMPTY` latch is clear, so the common case allocates nothing.
    ///
    /// `arena` is this collector's `[arena_base, arena_end)`: when two live
    /// VMs' user-loader classes share a class id, the loader row kept is the
    /// one inside this heap (`loader_pin::snapshot_where`), not whichever VM
    /// wrote last. Live heaps do not overlap, so that is this VM's row.
    pub fn capture(epoch: u64, arena: std::ops::Range<usize>) -> Self {
        let metadata_generation = cratonvm_types::metadata_pin::generation();
        Self {
            epoch,
            loaders: cratonvm_types::loader_pin::snapshot_where(|a| arena.contains(&a)),
            mirrors: cratonvm_types::mirror_pin::snapshot(),
            metadata: cratonvm_types::metadata_pin::snapshot(),
            metadata_generation,
        }
    }

    /// Still current at `epoch`? One more load per batch than the epoch test.
    #[inline]
    pub fn is_current(&self, epoch: u64) -> bool {
        self.epoch == epoch
            && self.metadata_generation == cratonvm_types::metadata_pin::generation()
    }

    /// True when every registry was empty at capture time, i.e. the scan loop
    /// has nothing to look up. Kept so the hot path can branch once rather
    /// than three times.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.loaders.is_none() && self.mirrors.is_none() && self.metadata.is_none()
    }
}

// ---------------------------------------------------------------------------
// --- lane D helpers ---
//
// Tuning constants and env levers for the parallel evacuator's worker loop and
// its two pause drivers. Kept together, and kept OUT of `cratonvm_types::GcFlags`
// on purpose: each of these is a knob on ONE loop in this file, and the typed
// config is the place for flags the rest of the VM has an opinion about. The
// `CRATONVM_G1_*` naming and the `OnceLock` latching match
// [`verify_budget`] above, so they behave like every other lever here.
// ---------------------------------------------------------------------------

/// Largest batch a worker lifts out of the shared gray queue in one
/// acquisition. See [`SharedEvac::run_worker`].
///
/// Sized against the two failure modes it sits between. Too small and the
/// lock acquisition per object is barely amortised at all; too large and one
/// worker drains a frontier its neighbours could have shared, turning a
/// parallel closure back into a serial one with extra steps. 64 objects is
/// roughly a microsecond of scanning — long enough to be worth a lock, short
/// enough that a worker re-enters the pool of sharers many times over any
/// realistic closure.
pub const EVAC_LOCAL_BATCH_MAX: usize = 64;

/// To-space pool regions per evacuation worker: a pause runs at most
/// `pool / this` workers (at least one; `CRATONVM_G1_EVAC_WORKERS_BY_POOL=0`
/// lifts the cap).
///
/// A worker's TLAB is a whole region (one Survivor and one Old at a time), so
/// every COPYING worker can end the pause holding two part-filled regions, and
/// those tails are unusable until the next pause. With eight copying workers
/// on a 64-region heap that was the free pool: `MonWaitGcChurn 4 20000` at G1
/// `-Xmx64m` hit "parallel evacuation EXHAUSTED ITS POOL" with 18 Survivor
/// regions, none with room, and then `OutOfMemoryError` with a few MB live,
/// once the idle-peer spill made all eight copy (2026-10-06). HotSpot carves
/// small PLABs out of a shared region instead, and scales its active GC
/// workers with the heap (`UseDynamicNumberOfGCThreads`,
/// `HeapSizePerGCThread`). Eight keeps the tails to about a quarter of the
/// pool; an 8 GiB heap of 4 MiB regions still gets every worker.
pub const EVAC_POOL_REGIONS_PER_WORKER: usize = 8;

/// JIT round 23 wave 10, lane L9: the [`EVAC_POOL_REGIONS_PER_WORKER`] quantum
/// when both destinations are shared PLAB regions (`CRATONVM_G1_SURVIVOR_PLAB`
/// and `CRATONVM_G1_OLD_PLAB`). The pool then feeds two regions at a time (the
/// current Survivor and Old ones) whatever the width; a worker's own regions
/// are only resumed Old regions, which are not Free-pool regions. Two keeps a
/// pause off a pool too small for its own two shared regions and leaves the
/// rest of the cap's job -- not running wide on a nearly-empty heap -- in
/// place: 16-19 Free regions give 8-9 workers instead of 2.
pub const EVAC_POOL_REGIONS_PER_WORKER_SHARED: usize = 2;

/// The to-space pool regions a pause budgets per evacuation worker: the
/// shared-destination quantum when both destinations are shared PLAB regions,
/// the whole-region one otherwise. Pure.
#[inline]
pub fn evac_pool_regions_per_worker(both_destinations_shared: bool) -> usize {
    if both_destinations_shared {
        EVAC_POOL_REGIONS_PER_WORKER_SHARED
    } else {
        EVAC_POOL_REGIONS_PER_WORKER
    }
}

/// JIT round 23 wave 13, lane L9: `CRATONVM_G1_KEEPALIVE_REGRAY_ONCE` (opt-in,
/// `=1`). See the serial drivers' keep-alive loops and
/// `G1Collector::marking_keepalive_roots_split`. Read once per pause.
pub fn g1_keepalive_regray_once_enabled() -> bool {
    false
}

/// Does keep-alive root `k` of a pause need an explicit re-gray of its copy?
/// Always with the switch off. With it on, only the first `satb_sourced`
/// roots (from the SATB log, which no gray entry names); the rest came from
/// the gray set, whose end-of-pause remap rewrites the entry to the copy.
/// Pure.
#[inline]
pub fn keepalive_needs_regray(regray_once: bool, k: usize, satb_sourced: usize) -> bool {
    !regray_once || k < satb_sourced
}

/// g1ep7: `CRATONVM_G1_PARALLEL_RESURRECT` (default ON, `=0` kills). See
/// [`G1Collector::resurrect_dead_finalizers_parallel`]. Read once per pause.
pub fn g1_parallel_resurrect_enabled() -> bool {
    true
}

/// g1ep7: `CRATONVM_G1_PARALLEL_REMARK_DRAIN` (default ON, `=0` kills). See
/// [`G1Collector::drain_marking_to_fixed_point`]. Read once per drain.
pub fn g1_parallel_remark_drain_enabled() -> bool {
    true
}

/// JIT round 23 wave 11, lane L9: `CRATONVM_G1_SATB_FILTER_ABOVE_TAMS` (opt-in
/// `=1`: neutral on n23-w11a KeepLong/R23W3Keep, and the R22FlipGcRows -Xmx64m
/// gate read 1/12 OOME twice with it on against 0/12 off). See
/// `G1Collector::marking_keepalive_roots`. Read once per pause.
pub fn g1_satb_filter_above_tams_enabled() -> bool {
    false
}

/// JIT round 23 wave 10, lane L9: how many evacuation workers keep a whole Old
/// region of their own when `CRATONVM_G1_OLD_PLAB` widens a pause: what the
/// pool affords at [`EVAC_POOL_REGIONS_PER_WORKER`] (at least the driver).
/// Workers past it promote through the shared Old region. Pure.
#[inline]
pub fn old_plab_worker_split(pool_len: usize) -> usize {
    (pool_len / EVAC_POOL_REGIONS_PER_WORKER).max(1)
}

