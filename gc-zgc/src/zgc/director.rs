// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! HotSpot-ZGC-shaped collection rules: allocation rate, proactive, high usage.
//!
//! zr z1/tr (2026-10-12). Stage 2 of
//! `docs/feature-designs/zgc-architecture.md`:
//! the projection that `CRATONVM_ZGC_TRIGGER_SHADOW` only printed, made able to
//! decide, behind `CRATONVM_ZGC_RATE_TRIGGER=1` (default OFF).
//!
//! # The rules (HotSpot ZGC's director, restated)
//!
//! HotSpot's ZGC decides when to collect from a director thread that samples
//! the allocation rate ten times a second. The rules this module carries are
//! the non-generational director's (JDK 17-22 `zDirector.cpp`), because every
//! collection of this collector's default mode is a whole-heap one; JDK 25's
//! generational director splits the same arithmetic into a minor rule and a
//! cost-weighted major rule, which has no counterpart here until the young
//! generation does.
//!
//! * **Allocation rate.** Estimate the worst plausible allocation rate as the
//!   sampled average times a spike tolerance (`ZAllocationSpikeTolerance`,
//!   default 2) plus ~3.3 standard deviations (one sample in a thousand
//!   outside). Divide the free space under the soft maximum by it: that is the
//!   time until the heap is exhausted. Estimate the cycle's duration the same
//!   way (decaying average plus ~3.3 decaying standard deviations). Collect
//!   when the time to exhaustion, less the cycle's duration and one sample
//!   interval, is not positive. Disabled until one cycle has been timed: an
//!   untimed duration is not trustable, and the warm-up rule covers the start.
//! * **Proactive.** Once warm (three cycles), collect when the heap has grown
//!   by a tenth of the soft maximum since the last cycle, or five minutes have
//!   passed, AND the time since the last cycle exceeds the interval at which a
//!   cycle would cost at most 1% of throughput (assuming a cycle halves
//!   throughput while it runs: `duration * (0.5 / 0.01 - 1)`).
//! * **High usage.** Collect when free space under the soft maximum is 5% or
//!   less: the backstop for an allocation rate too low for the rate rule to
//!   ever fire while the heap still creeps towards full.
//! * **Warm-up** stays where it already is (`needs_gc`'s `ZGC_WARMUP_CYCLES`
//!   clause, r16 zgcarray ZA2); this module only supplies its soft-max base.
//!
//! # How it is evaluated without a director thread
//!
//! This VM has no director thread; `needs_gc` is a predicate polled from the
//! allocation slow path and from native boundaries, many thousands of times a
//! second. So the decision is converted into a **byte threshold**: at each
//! sample the rules are evaluated once, and the outcome is published as "fire
//! when `allocated` reaches N". Between samples a poll is two relaxed loads
//! and a compare. Samples are gated on bytes (a poll takes the clock only once
//! `allocated` has moved by [`sample_step_bytes`]) and committed only when at
//! least [`SAMPLE_INTERVAL`] has passed, so a rate sample is always the average
//! over a window of at least a tenth of a second -- HotSpot's 10 Hz cadence,
//! stretched rather than skipped when allocation is slow.
//!
//! One deliberate difference from HotSpot's sampler: a sample window opens on
//! a byte step rather than on the clock, so a slow allocator samples less
//! often. The rate rule cannot miss exhaustion because of that -- a slow
//! allocator is far from exhaustion by definition -- and the high-usage rule
//! is re-evaluated on every byte step regardless of the clock.
//!
//! Pure arithmetic lives in [`evaluate`] so the rules are unit-tested without a
//! heap; [`ZDirector`] is the per-heap state (no process global).

use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// `ZAllocationSpikeTolerance`'s default.
pub const ALLOCATION_SPIKE_TOLERANCE: f64 = 2.0;
/// zt t7/x4: [`ZDirector`]'s "no `-XX:ZAllocationSpikeTolerance`" word, a NaN
/// bit pattern that no finite `f64` has (`0.0`'s bits are `0`).
const SPIKE_TOLERANCE_UNSET: u64 = u64::MAX;
/// The number of standard deviations that leaves one sample in a thousand
/// outside a normal distribution's confidence interval.
pub const ONE_IN_1000: f64 = 3.290527;
/// HotSpot's director samples ten times a second.
pub const SAMPLE_INTERVAL: Duration = Duration::from_millis(100);
/// The allocation-rate window: one second of samples (HotSpot's
/// `ZStatAllocRate` keeps `sample_hz` of them).
pub const RATE_WINDOW: usize = 10;
/// Cycles before the heap counts as warm (HotSpot's `ZStatCycle::is_warm`).
pub const WARM_CYCLES: u64 = 3;
/// High-usage rule: free space under the soft maximum at or below this share.
pub const HIGH_USAGE_FREE_PERCENT: f64 = 5.0;
/// Proactive rule: growth since the last cycle that makes one worth asking
/// about, as a share of the soft maximum.
pub const PROACTIVE_GROWTH_PERCENT: u64 = 10;
/// Proactive rule: time since the last cycle that makes one worth asking
/// about regardless of growth.
pub const PROACTIVE_MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Proactive rule: assumed throughput loss while a cycle runs.
pub const PROACTIVE_ASSUMED_DROP: f64 = 0.50;
/// zr zs s1/tp: the proactive rule's assumed throughput loss while a
/// STOP-THE-WORLD cycle runs ([`DirectorInputs::stw_model`]): all of it.
pub const PROACTIVE_STW_DROP: f64 = 1.0;
/// Proactive rule: throughput loss a proactive cycle may cost.
pub const PROACTIVE_ACCEPTABLE_DROP: f64 = 0.01;
/// The decaying average's weight on history (HotSpot's `AbsSeq` default).
pub const DECAY_ALPHA: f64 = 0.7;

/// Bytes of allocation between two looks at the clock: a 256th of the soft
/// maximum, clamped to `[256 KiB, 16 MiB]`. At 1 GB/s and the ceiling that is
/// ~60 clock reads a second, and only under the switch.
pub fn sample_step_bytes(soft_max: u64) -> u64 {
    (soft_max / 256).clamp(256 * 1024, 16 * 1024 * 1024)
}

/// Which rule asked for a collection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectorRule {
    /// Time to exhaustion is shorter than a cycle (HotSpot `Allocation Rate`).
    AllocationRate,
    /// A cheap cycle is due (HotSpot `Proactive`).
    Proactive,
    /// Free space is 5% of the soft maximum or less (HotSpot `High Usage`).
    HighUsage,
}

impl DirectorRule {
    /// HotSpot's `GCCause` spelling.
    pub fn hotspot_cause(self) -> &'static str {
        match self {
            DirectorRule::AllocationRate => "Allocation Rate",
            DirectorRule::Proactive => "Proactive",
            DirectorRule::HighUsage => "High Usage",
        }
    }

    fn code(self) -> u64 {
        match self {
            DirectorRule::AllocationRate => 1,
            DirectorRule::Proactive => 2,
            DirectorRule::HighUsage => 3,
        }
    }

    fn from_code(code: u64) -> Option<DirectorRule> {
        match code {
            1 => Some(DirectorRule::AllocationRate),
            2 => Some(DirectorRule::Proactive),
            3 => Some(DirectorRule::HighUsage),
            _ => None,
        }
    }

    /// Short name for the shadow line.
    pub fn key(self) -> &'static str {
        match self {
            DirectorRule::AllocationRate => "rate",
            DirectorRule::Proactive => "proactive",
            DirectorRule::HighUsage => "high_usage",
        }
    }
}

/// A sequence with a decaying average and a decaying variance (HotSpot's
/// `AbsSeq::davg` / `dsd` with the default alpha): each new value takes 30% of
/// the weight.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecayingSeq {
    num: u64,
    davg: f64,
    dvar: f64,
}

impl DecayingSeq {
    pub fn add(&mut self, v: f64) {
        if self.num == 0 {
            self.davg = v;
            self.dvar = 0.0;
        } else {
            self.davg = (1.0 - DECAY_ALPHA) * v + DECAY_ALPHA * self.davg;
            let diff = v - self.davg;
            self.dvar = (1.0 - DECAY_ALPHA) * diff * diff + DECAY_ALPHA * self.dvar;
        }
        self.num += 1;
    }
    pub fn num(&self) -> u64 {
        self.num
    }
    pub fn davg(&self) -> f64 {
        self.davg
    }
    pub fn dsd(&self) -> f64 {
        self.dvar.max(0.0).sqrt()
    }
}

/// The last [`RATE_WINDOW`] samples, with their mean and population standard
/// deviation (HotSpot's `TruncatedSeq::avg` / `sd`).
#[derive(Clone, Copy, Debug, Default)]
pub struct WindowSeq {
    ring: [f64; RATE_WINDOW],
    len: usize,
    next: usize,
}

impl WindowSeq {
    pub fn add(&mut self, v: f64) {
        self.ring[self.next] = v;
        self.next = (self.next + 1) % RATE_WINDOW;
        self.len = (self.len + 1).min(RATE_WINDOW);
    }
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    pub fn avg(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        self.ring[..self.len].iter().sum::<f64>() / self.len as f64
    }
    pub fn sd(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        let avg = self.avg();
        let var = self.ring[..self.len]
            .iter()
            .map(|v| (v - avg) * (v - avg))
            .sum::<f64>()
            / self.len as f64;
        var.max(0.0).sqrt()
    }
}

/// Everything one evaluation reads. Bytes and seconds.
#[derive(Clone, Copy, Debug, Default)]
pub struct DirectorInputs {
    /// `-XX:SoftMaxHeapSize`, defaulting to the heap's capacity.
    pub soft_max: u64,
    /// Bytes in use now (`ZgcRealHeap::allocated`).
    pub used: u64,
    /// Bytes in use right after the last cycle (the live set).
    pub used_after_last_cycle: u64,
    /// Allocation rate window mean and standard deviation, bytes/second.
    pub rate_avg: f64,
    pub rate_sd: f64,
    /// Whether at least one rate sample exists.
    pub rate_known: bool,
    /// Cycle duration decaying mean and standard deviation, seconds.
    pub cycle_avg: f64,
    pub cycle_sd: f64,
    /// Cycles whose duration was measured.
    pub cycles_timed: u64,
    /// Cycles completed.
    pub cycles: u64,
    /// Seconds since the last cycle ended (since construction before one).
    pub since_last_cycle: f64,
    /// zr zs s1/tp (`CRATONVM_ZGC_DIRECTOR_STW_MODEL=1`, opt-in): the rules
    /// translated to a stop-the-world collector that polls on BYTES, not on a
    /// 10 Hz clock. Drops the rate rule's `gc_duration` term (no mutator
    /// allocates while a stop-the-world cycle runs), and also (a) replaces
    /// the rate rule's `max_rate x sample_interval` term --
    /// HotSpot's allowance for the next director tick being 100 ms away -- by
    /// [`Self::poll_latency_bytes`], the bytes this VM can allocate between
    /// the published threshold being crossed and a poll site acting on it;
    /// (b) charges the proactive rule's throughput budget with a FULL drop
    /// while a cycle runs (a stop-the-world cycle stops the application; the
    /// 50% of [`PROACTIVE_ASSUMED_DROP`] is a concurrent cycle sharing CPU).
    /// See [`stw_model_rate_reserve`].
    pub stw_model: bool,
    /// zr zs s1/tp: bytes between the director's threshold being crossed and
    /// a poll acting on it ([`poll_latency_bytes`]). Read only under
    /// [`Self::stw_model`].
    pub poll_latency_bytes: u64,
    /// `CRATONVM_ZGC_DIRECTOR_BYTE_POLLED_INTERVAL` (default on): the
    /// concurrent (HotSpot-shaped) rate rule reserves [`Self::poll_latency_bytes`]
    /// for the reaction time instead of `max_rate x sample_interval`. HotSpot's
    /// 100 ms term allows for the next director tick being that far away; this
    /// director is polled on bytes, so its reaction time is the poll latency.
    /// At the allocation rates a small heap sees (GB/s) the 100 ms term alone
    /// reserved hundreds of MiB and started a cycle at ~30 percent occupancy,
    /// ~3x as many cycles as HotSpot ran on the same probe. The cycle-duration
    /// term stays (the mutators allocate during the concurrent phase). Read
    /// only outside [`Self::stw_model`].
    pub byte_polled_interval: bool,
    /// zt t1/tp (`CRATONVM_ZGC_DIRECTOR_HOTSPOT_WARMTH`, default on): count
    /// warmth as HotSpot's `ZStatCycle` does -- in WARM-UP cycles
    /// ([`Self::warmup_cycles`]), not in every cycle. `false` is the zr rule
    /// ([`Self::cycles`]).
    pub hotspot_warmth: bool,
    /// zt t1/tp: cycles whose cause was the warm-up rule. Read only under
    /// [`Self::hotspot_warmth`].
    pub warmup_cycles: u64,
    /// zt t6/tc (zt1tp item 4): `-XX:-ZProactive` -- the proactive rule is
    /// off. `false` (the default, HotSpot's `ZProactive=true`) keeps it.
    pub proactive_off: bool,
    /// zt t6/tc (zt1tp item 4): `-XX:ZAllocationSpikeTolerance=<f>`; `None`
    /// is HotSpot's default, [`ALLOCATION_SPIKE_TOLERANCE`].
    pub spike_tolerance: Option<f64>,
    /// zt t14/tg6 (`CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK`, default on): the
    /// proactive rule reads [`Self::since_last_major`] and
    /// [`Self::used_after_last_major`], as HotSpot 25's `rule_major_proactive`
    /// reads the OLD generation's cycle clock and its used-at-relocate-end, so
    /// the minor-capable cycles (`Allocation Rate`, `High Usage`, an
    /// allocation stall) do not restart it. `false`: every cycle's
    /// [`Self::since_last_cycle`] / [`Self::used_after_last_cycle`] (zr).
    pub proactive_major_clock: bool,
    /// zt t14/tg6: seconds since the last cycle HotSpot would run as a major
    /// ended ([`ZDirector::note_major_cycle_end`]; since construction before
    /// one).
    pub since_last_major: f64,
    /// zt t14/tg6: bytes in use right after that cycle.
    pub used_after_last_major: u64,
}

/// zt t1/tp: HotSpot's `ZStatCycle::is_warm()` and `is_time_trustable()` for
/// `i`, as `(warm, time_trustable)`.
///
/// HotSpot counts the cycles whose cause was `Warmup` (`_nwarmup_cycles`):
/// the heap is warm after three of them, and its cycle times are trustable
/// after one -- the allocation-rate rule is off until then
/// (`rule_allocation_rate`: `if (!ZStatCycle::is_time_trustable()) return
/// false`), and the proactive rule until the heap is warm. A `System.gc()` or
/// an allocation stall is a cycle but not a warm-up one, so a program that
/// calls `System.gc()` three times early is not warm there. The zr rule
/// counted every cycle (`cycles >= WARM_CYCLES`, `cycles_timed > 0`). Either
/// way the rate rule also needs a measured duration (`cycles_timed > 0`):
/// without one the duration terms are zero.
pub fn warmth(i: &DirectorInputs) -> (bool, bool) {
    if i.hotspot_warmth {
        (
            i.warmup_cycles >= WARM_CYCLES && i.cycles_timed > 0,
            i.warmup_cycles > 0 && i.cycles_timed > 0,
        )
    } else {
        (i.cycles >= WARM_CYCLES, i.cycles_timed > 0)
    }
}

/// zr zs s1/tp: the bytes a compiled allocation loop can allocate before a
/// poll site consults `needs_gc` again: the VM's compiled TLAB refill trigger
/// consults once per this many refilled bytes
/// (`NEEDSGC_MIN_REFILL_BYTES_BETWEEN_FIRES`,
/// `vm/src/runtime/interpreter/gc_and_alloc.rs`; the interpreter polls every
/// 16 KiB, `GC_OCCUPANCY_POLL_BYTES`). The gc crate cannot name the VM
/// constant, so the VM pins the two equal with a compile-time assert beside
/// it (zt t2/rv).
pub const POLL_REFILL_CONSULT_BYTES: u64 = 4 * 1024 * 1024;

/// zr zs s1/tp: the director's poll latency in bytes -- one sample step (the
/// rules are re-evaluated once per step, [`sample_step_bytes`]) plus one
/// refill-consult step ([`POLL_REFILL_CONSULT_BYTES`]). This is what HotSpot's
/// `sample_interval` term stands for (it is subtracted so the next tick does
/// not start the cycle too late), expressed in the unit this VM's polls
/// actually advance in.
pub fn poll_latency_bytes(soft_max: u64) -> u64 {
    sample_step_bytes(soft_max).saturating_add(POLL_REFILL_CONSULT_BYTES)
}

/// zr zs s1/tp: the rate rule's reserve under [`DirectorInputs::stw_model`]:
/// the poll latency alone. No mutator allocates while a stop-the-world cycle
/// runs (so no `gc_duration` term), and the reaction time is bytes, not a
/// tick (so no `max_rate x sample_interval` term).
///
/// # Why this is the term that mattered (the z1 +25 %)
///
/// HotSpot's reserve is `max_rate x (gc_duration + 0.1 s)` with
/// `max_rate = 2 x avg + 3.29 sd`. On `Zr1TrRateTriggerProbe 64 10` at
/// `-Xmx1g` a compiled 8 KiB-array loop allocates on the order of GB/s, so
/// the 0.1 s interval term alone reserves hundreds of MiB -- the rule fires
/// with a third or more of the heap free, every cycle -- while a
/// stop-the-world pause of a 64 MiB live set is tens of milliseconds, a
/// fraction of the interval. z3/cs's STW reserve (dropping only that
/// smaller term) was refuted and its switch retired in zt t12/rt.
pub fn stw_model_rate_reserve(poll_latency_bytes: u64) -> u64 {
    poll_latency_bytes
}

/// One evaluation's outcome.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DirectorDecision {
    /// A rule that already says yes, the first in rule order.
    pub fire_now: Option<DirectorRule>,
    /// `used` at which the earliest byte-expressible rule (rate, high usage)
    /// will say yes if nothing else changes; `u64::MAX` for never.
    pub fire_at_used: u64,
    /// The rule `fire_at_used` belongs to.
    pub fire_at_rule: Option<DirectorRule>,
    /// The allocation-rate rule's `time_until_gc`, seconds (`f64::INFINITY`
    /// when the rule is disabled). Negative means "late".
    pub time_until_gc: f64,
}

/// The rules, as arithmetic. Order: allocation rate, proactive, high usage
/// (high usage last, as in HotSpot's minor rule list where it is the
/// backstop).
pub fn evaluate(i: &DirectorInputs) -> DirectorDecision {
    let soft_max = i.soft_max.max(1);
    let free = soft_max.saturating_sub(i.used.min(soft_max)) as f64;

    // ---- allocation rate ------------------------------------------------
    let gc_duration = i.cycle_avg + i.cycle_sd * ONE_IN_1000;
    // zr zs s1/tp: a stop-the-world cycle reserves nothing for its own
    // duration (no mutator allocates during it). The proactive rule below
    // keeps the measured duration: its interval is about throughput, not
    // exhaustion.
    let reserve_duration = if i.stw_model {
        0.0
    } else {
        gc_duration
    };
    let tolerance = i.spike_tolerance.unwrap_or(ALLOCATION_SPIKE_TOLERANCE);
    let max_rate = i.rate_avg * tolerance + i.rate_sd * ONE_IN_1000;
    let interval = SAMPLE_INTERVAL.as_secs_f64();
    // zt t1/tp: warmth and trustable times as HotSpot counts them (`warmth`).
    let (warm, time_trustable) = warmth(i);
    let rate_enabled = time_trustable && i.rate_known;
    let (time_until_gc, rate_fire_at) = if rate_enabled && i.stw_model {
        // zr zs s1/tp: the stop-the-world, byte-polled translation (see
        // `stw_model_rate_reserve`): time to exhaustion less the poll latency.
        let reserve = stw_model_rate_reserve(i.poll_latency_bytes).min(soft_max);
        let t = (free - reserve as f64) / (max_rate + 1.0);
        (t, soft_max.saturating_sub(reserve))
    } else if rate_enabled {
        // +1 B/s so a zero rate is "never" rather than a division by zero.
        let time_until_oom = free / (max_rate + 1.0);
        // The reaction time: HotSpot's sample interval, or (byte-polled) the
        // bytes the polls can lag by, at the worst-case rate.
        let reaction = if i.byte_polled_interval {
            i.poll_latency_bytes as f64 / (max_rate + 1.0)
        } else {
            interval
        };
        let t = time_until_oom - reserve_duration - reaction;
        // The same inequality solved for `used`: fire once the free space
        // falls to what the worst-case rate eats during one cycle plus the
        // reaction time.
        let reserve = (max_rate + 1.0) * (reserve_duration + reaction);
        let fire_free = reserve.min(soft_max as f64).max(0.0) as u64;
        (t, soft_max.saturating_sub(fire_free))
    } else {
        (f64::INFINITY, u64::MAX)
    };

    // ---- high usage -----------------------------------------------------
    let high_usage_used =
        soft_max.saturating_sub((soft_max as f64 * HIGH_USAGE_FREE_PERCENT / 100.0) as u64);

    // ---- proactive ------------------------------------------------------
    // zt t14/tg6: the old generation's clock (`DirectorInputs::proactive_major_clock`).
    let (used_after_last, since_last) = if i.proactive_major_clock {
        (i.used_after_last_major, i.since_last_major)
    } else {
        (i.used_after_last_cycle, i.since_last_cycle)
    };
    let proactive = !i.proactive_off && warm && time_trustable && {
        let grown =
            i.used >= used_after_last.saturating_add(soft_max / 100 * PROACTIVE_GROWTH_PERCENT);
        let stale = since_last >= PROACTIVE_MAX_INTERVAL.as_secs_f64();
        (grown || stale) && {
            // zr zs s1/tp: a stop-the-world cycle drops throughput to zero
            // while it runs, not to half (`DirectorInputs::stw_model`).
            let drop = if i.stw_model {
                PROACTIVE_STW_DROP
            } else {
                PROACTIVE_ASSUMED_DROP
            };
            let acceptable_interval = gc_duration * (drop / PROACTIVE_ACCEPTABLE_DROP - 1.0);
            since_last >= acceptable_interval
        }
    };

    // zt t20/tg9 (Defect D8 of `t16-row-verdicts.md`): under the major clock
    // the proactive rule is HotSpot 25's MAJOR rule, and `make_gc_decision`
    // asks every major rule (`rule_major_proactive` among them) before the
    // minor ones (`rule_minor_allocation_rate`, `rule_minor_high_usage`), so
    // a due proactive cycle wins over an allocation-rate yes. Rate-first, a
    // heap whose rate rule said yes at every sample (`Zr1TrRateTriggerProbe 64
    // 10`, `-Xmx1g`: the reserve exceeded the soft maximum and the anti-storm
    // floor alone paced 183 `Allocation Rate` cycles) never published the
    // proactive yes: 0 `Proactive` against HotSpot's 11 (row `tg6_p1`). The
    // `=0` kill of `CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK` keeps the zr order.
    let proactive_first = i.proactive_major_clock && proactive;
    let fire_now = if proactive_first {
        Some(DirectorRule::Proactive)
    } else if rate_enabled && time_until_gc <= 0.0 {
        Some(DirectorRule::AllocationRate)
    } else if proactive {
        Some(DirectorRule::Proactive)
    } else if i.used >= high_usage_used {
        Some(DirectorRule::HighUsage)
    } else {
        None
    };
    let (fire_at_used, fire_at_rule) = if rate_fire_at <= high_usage_used {
        (
            rate_fire_at,
            rate_enabled.then_some(DirectorRule::AllocationRate),
        )
    } else {
        (high_usage_used, Some(DirectorRule::HighUsage))
    };
    DirectorDecision {
        fire_now,
        fire_at_used,
        fire_at_rule,
        time_until_gc,
    }
}

// ---------------------------------------------------------------------------
// zr z3/cs: every trigger opens a concurrent cycle (`CRATONVM_ZGC_CONC_EVERY_TRIGGER`)
// ---------------------------------------------------------------------------
//
// HotSpot ZGC has ONE mechanism: a rule (warm-up, allocation rate, proactive,
// high usage) starts a GC, the GC is a concurrent mark, and relocation follows
// the mark. It falls back to stalling only when an allocation cannot be
// served (`Allocation Stall`). This collector's opt-in concurrent cycle was an
// occupancy window running BESIDE `needs_gc`'s clauses, and lost whenever a
// clause it could not predict (headroom, the director) collected first
// (`zgc-zr2cc-conc-start-misses-headroom-and-director-collections-FIXED-20261012`).
// Under the switch every non-urgent `needs_gc` yes becomes a REQUEST to open
// a concurrent cycle (when the heap has the headroom to finish one), and the
// collection is taken when the mark reaches its fixed point. The pure policy
// lives here so it is unit-tested without a heap.

/// Free bytes at or below which a collection may not wait for a concurrent
/// mark: 5% of the capacity (HotSpot's high-usage share), at least the
/// collector's headroom margin, at most a quarter of the capacity.
pub fn conc_urgent_reserve(capacity: usize, headroom_margin: usize) -> usize {
    (capacity / 20).max(headroom_margin).min(capacity / 4)
}

/// Bytes the mutators are expected to allocate while a concurrent mark runs:
/// the smoothed rate (bytes per nanosecond, scaled by 2^20) times the smoothed
/// concurrent-phase duration, times 3/2 -- the same safety factor
/// `ZgcRealHeap::refresh_adaptive_conc_start` applies. `None` until both have
/// been measured.
pub fn conc_expected_mark_allocation(rate_scaled: u64, mark_nanos: u64) -> Option<u64> {
    if rate_scaled == 0 || mark_nanos == 0 {
        return None;
    }
    let bytes = ((rate_scaled as u128 * mark_nanos as u128) >> 20).min(u64::MAX as u128) as u64;
    Some(bytes.saturating_mul(3) / 2)
}

/// Has the heap the headroom to FINISH a concurrent mark opened now at `used`?
/// Free space must exceed the urgent reserve plus the expected allocation
/// during the mark; before a mark has been timed, twice the reserve.
pub fn conc_cycle_has_headroom(
    capacity: usize,
    used: usize,
    reserve: usize,
    rate_scaled: u64,
    mark_nanos: u64,
) -> bool {
    let free = capacity.saturating_sub(used) as u64;
    let need = match conc_expected_mark_allocation(rate_scaled, mark_nanos) {
        Some(e) => (reserve as u64).saturating_add(e),
        None => (reserve as u64).saturating_mul(2),
    };
    free > need
}

/// Allocation a requested cycle may wait for its opener before the request
/// lapses into a stop-the-world collection: half the urgent reserve. Bounds a
/// request no poll site serves (a door that asks `needs_gc` but cannot open a
/// cycle), so the mode can never defer a collection without limit.
pub fn conc_request_grace(reserve: usize) -> usize {
    (reserve / 2).max(1)
}

/// What one `needs_gc` poll sees under the switch.
#[derive(Clone, Copy, Debug, Default)]
pub struct ConcTriggerInputs {
    /// A concurrent cycle is open.
    pub active: bool,
    /// Its marker reached a fixed point (only meaningful when `active`).
    pub caught_up: bool,
    /// `used` when the pending request was made; `0` = no request.
    pub requested_at: usize,
    /// The VM reported that the requested mark start did not open.
    pub refused: bool,
    pub used: usize,
    pub capacity: usize,
    pub reserve: usize,
}

/// The poll's answer under the switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConcTriggerGate {
    /// No cycle open and none requested: the ordinary clauses decide (and an
    /// early yes from them becomes a request).
    Clauses,
    /// Hold the collection: the open cycle is still tracing, or the requested
    /// one has not opened yet.
    Hold,
    /// Collect: the mark reached its fixed point (HotSpot's mark end).
    MarkFinished,
    /// Collect: free space fell to the urgent reserve with a cycle open; the
    /// collection finishes the mark stop-the-world.
    UrgentWhileOpen,
    /// Collect stop-the-world: the requested cycle did not open (refused,
    /// urgent, or its grace was spent).
    RequestLapsed,
}

impl ConcTriggerGate {
    /// `Some(answer)` for `needs_gc`, `None` to fall through to the clauses.
    pub fn answer(self) -> Option<bool> {
        match self {
            ConcTriggerGate::Clauses => None,
            ConcTriggerGate::Hold => Some(false),
            _ => Some(true),
        }
    }
}

/// The gate, as arithmetic.
pub fn conc_trigger_gate(i: &ConcTriggerInputs) -> ConcTriggerGate {
    let free = i.capacity.saturating_sub(i.used);
    if i.active {
        if i.caught_up {
            return ConcTriggerGate::MarkFinished;
        }
        if free <= i.reserve {
            return ConcTriggerGate::UrgentWhileOpen;
        }
        return ConcTriggerGate::Hold;
    }
    if i.requested_at != 0 {
        if i.refused
            || free <= i.reserve
            || i.used >= i.requested_at.saturating_add(conc_request_grace(i.reserve))
        {
            return ConcTriggerGate::RequestLapsed;
        }
        return ConcTriggerGate::Hold;
    }
    ConcTriggerGate::Clauses
}

/// zt t1/tp: bits of [`ZDirector`]'s published word that hold the `used`
/// threshold; the top two hold the rule code (1..=3, [`DirectorRule::code`]).
pub const FIRE_USED_BITS: u32 = 62;
/// The threshold field's all-ones value: "never" ([`u64::MAX`] on unpack).
/// No heap has 2^62 bytes, so no real threshold reaches it.
pub const FIRE_USED_NEVER: u64 = (1u64 << FIRE_USED_BITS) - 1;

/// zt t1/tp: pack a published decision -- `used` (clamped into the
/// threshold field; [`u64::MAX`] and anything at or above
/// [`FIRE_USED_NEVER`] is "never") and a rule code -- into one word, so a
/// poll reads both with one load.
pub fn pack_fire(used: u64, rule_code: u64) -> u64 {
    ((rule_code & 0b11) << FIRE_USED_BITS) | used.min(FIRE_USED_NEVER)
}

/// zt t1/tp: the inverse of [`pack_fire`]: `(used, rule_code)`, with the
/// "never" field answered as [`u64::MAX`].
pub fn unpack_fire(word: u64) -> (u64, u64) {
    let used = word & FIRE_USED_NEVER;
    let used = if used == FIRE_USED_NEVER {
        u64::MAX
    } else {
        used
    };
    (used, word >> FIRE_USED_BITS)
}

/// The estimators and the sample window, behind one lock that only a
/// sampling poll takes (`try_lock`: a poll that loses the race skips).
#[derive(Debug)]
struct DirectorState {
    rate: WindowSeq,
    cycle: DecayingSeq,
    window_start: Instant,
    window_start_used: u64,
    /// Bytes allocated in this window before the last collection(s) rewrote
    /// `allocated`: the sample is a cumulative count, as HotSpot's is. A window
    /// that RESTARTED at every cycle end starved the sampler once cycles came
    /// less than [`SAMPLE_INTERVAL`] apart (the estimate froze on its warm-up
    /// samples, and the rate rule fired a cycle every ~95 ms).
    window_carry: u64,
    /// Stop-the-world pause time elapsed inside the current sample window.
    window_pause: Duration,
    /// The `used` the last sample saw.
    last_used: u64,
    last_cycle_end: Instant,
    used_after_last_cycle: u64,
    cycles: u64,
    /// zt t1/tp: cycles the heap reported as warm-up cycles
    /// ([`ZDirector::note_warmup_cycle`]).
    warmup_cycles: u64,
    /// zt t14/tg6: the end of the last cycle HotSpot would run as a major,
    /// and the bytes in use after it ([`ZDirector::note_major_cycle_end`]).
    last_major_end: Instant,
    used_after_last_major: u64,
    last: Option<DirectorDecision>,
}

/// Per-heap director state. Every field is per heap; nothing here is a
/// process global.
#[derive(Debug)]
pub struct ZDirector {
    /// `CRATONVM_ZGC_RATE_TRIGGER=1`: the rules decide (stage 2).
    deciding: bool,
    /// The rules are evaluated at all: deciding, or the shadow line is on.
    sampling: bool,
    /// zr z3/cs: this heap's cycles can have a concurrent phase (concurrent
    /// marking is configured). Set by the heap at construction.
    concurrent: AtomicBool,
    /// zr zs s1/tp: `CRATONVM_ZGC_DIRECTOR_STW_MODEL=1` (opt-in), read once
    /// per heap by the caller. Acts only while [`Self::concurrent`] is false;
    /// see [`DirectorInputs::stw_model`].
    stw_model: AtomicBool,
    /// `CRATONVM_ZGC_DIRECTOR_BYTE_POLLED_INTERVAL` (default on); see
    /// [`DirectorInputs::byte_polled_interval`].
    byte_polled_interval: AtomicBool,
    /// zt t1/tp: `CRATONVM_ZGC_DIRECTOR_HOTSPOT_WARMTH` (default on), set by
    /// the heap at construction; see [`warmth`].
    hotspot_warmth: AtomicBool,
    /// zt t14/tg6: `CRATONVM_ZGC_PROACTIVE_MAJOR_CLOCK` (default on), set by
    /// the heap at construction; see [`DirectorInputs::proactive_major_clock`].
    proactive_major_clock: AtomicBool,
    /// `-XX:SoftMaxHeapSize` (capacity when unset).
    soft_max: AtomicU64,
    /// zt t6/tc (zt1tp item 4): `-XX:-ZProactive`, per VM.
    proactive_off: AtomicBool,
    /// zt t6/tc (zt1tp item 4): `-XX:ZAllocationSpikeTolerance` as `f64`
    /// bits; [`SPIKE_TOLERANCE_UNSET`] = unset (HotSpot's default,
    /// [`ALLOCATION_SPIKE_TOLERANCE`]). zt t7/x4: the sentinel was `0`, which is
    /// also `0.0`'s bit pattern, so a `0` tolerance (HotSpot accepts it) read
    /// as unset.
    spike_tolerance_bits: AtomicU64,
    /// `used` at which the next sampling poll takes the clock.
    next_sample_used: AtomicU64,
    /// The published decision, ONE word ([`pack_fire`]): the `used` at which
    /// it fires in the low [`FIRE_USED_BITS`] bits ([`FIRE_USED_NEVER`]:
    /// never; `0`: a rule already says yes) and the [`DirectorRule::code`] of
    /// its rule in the top two.
    ///
    /// zt t1/tp (`zgc-zs1tp-trigger-review-residuals-CLOSED-20261019.md` item 3):
    /// these were two relaxed stores, `fire_rule` then `fire_at_used`, so a
    /// poll on another core could pair the new threshold with the previous
    /// rule and charge a high-usage yes as `Allocation Rate` (or the reverse).
    /// The decision (`used >= threshold`) was never affected, only the label;
    /// one word makes the pair atomic for the cost of a mask.
    fire: AtomicU64,
    /// Shadow: the `used` at which the rules first said yes in this interval
    /// (`0`: they have not), and which rule.
    shadow_first_used: AtomicU64,
    shadow_first_rule: AtomicU64,
    state: Mutex<DirectorState>,
}

impl ZDirector {
    /// `deciding` and `sampling` as read once per heap by the caller.
    pub fn new(soft_max: u64, deciding: bool, sampling: bool) -> ZDirector {
        let now = Instant::now();
        ZDirector {
            deciding,
            sampling: sampling || deciding,
            concurrent: AtomicBool::new(false),
            stw_model: AtomicBool::new(false),
            byte_polled_interval: AtomicBool::new(false),
            hotspot_warmth: AtomicBool::new(false),
            proactive_major_clock: AtomicBool::new(false),
            soft_max: AtomicU64::new(soft_max),
            proactive_off: AtomicBool::new(false),
            spike_tolerance_bits: AtomicU64::new(SPIKE_TOLERANCE_UNSET),
            next_sample_used: AtomicU64::new(0),
            fire: AtomicU64::new(pack_fire(u64::MAX, 0)),
            shadow_first_used: AtomicU64::new(0),
            shadow_first_rule: AtomicU64::new(0),
            state: Mutex::new(DirectorState {
                rate: WindowSeq::default(),
                cycle: DecayingSeq::default(),
                window_start: now,
                window_start_used: 0,
                window_carry: 0,
                window_pause: Duration::ZERO,
                last_used: 0,
                last_cycle_end: now,
                used_after_last_cycle: 0,
                cycles: 0,
                warmup_cycles: 0,
                last_major_end: now,
                used_after_last_major: 0,
                last: None,
            }),
        }
    }

    pub fn is_deciding(&self) -> bool {
        self.deciding
    }

    pub fn is_sampling(&self) -> bool {
        self.sampling
    }

    pub fn soft_max(&self) -> u64 {
        self.soft_max.load(Ordering::Relaxed)
    }

    /// zr z3/cs: the `used` at which the published decision fires
    /// ([`u64::MAX`]: never, or not evaluated since the last cycle; `0`: a
    /// rule already says yes). The concurrent-start window ends here when the
    /// director decides (`ZgcRealHeap::conc_start_window`).
    pub fn fire_at_used(&self) -> u64 {
        unpack_fire(self.fire.load(Ordering::Relaxed)).0
    }

    /// Set the published point, keeping the published rule. **Test seam**
    /// (zr z11/rv9).
    #[cfg(test)]
    pub fn dbg_set_fire_at_used(&self, used: u64) {
        let (_, code) = unpack_fire(self.fire.load(Ordering::Relaxed));
        self.fire.store(pack_fire(used, code), Ordering::Relaxed);
    }

    /// zr z3/cs: `concurrent` says this heap's cycles can have a concurrent
    /// phase, in which case HotSpot's reserve is kept whatever the
    /// stop-the-world model's switch says. (zt t12/rt: the z3/cs STW reserve
    /// switch this also took was retired.)
    pub fn set_cycle_shape(&self, concurrent: bool) {
        self.concurrent.store(concurrent, Ordering::Relaxed);
    }

    /// zr zs s1/tp: configure the stop-the-world director model
    /// ([`DirectorInputs::stw_model`]). It stands down
    /// whenever this heap's cycles can have a concurrent phase.
    pub fn set_stw_model(&self, on: bool) {
        self.stw_model.store(on, Ordering::Relaxed);
    }

    /// Configure the byte-polled reaction time of the HotSpot-shaped rate rule
    /// ([`DirectorInputs::byte_polled_interval`]).
    pub fn set_byte_polled_interval(&self, on: bool) {
        self.byte_polled_interval.store(on, Ordering::Relaxed);
    }

    /// Does the stop-the-world director model act on this heap?
    pub fn stw_model_active(&self) -> bool {
        self.stw_model.load(Ordering::Relaxed) && !self.concurrent.load(Ordering::Relaxed)
    }

    /// zt t1/tp: count warmth in warm-up cycles, as HotSpot does ([`warmth`]).
    pub fn set_hotspot_warmth(&self, on: bool) {
        self.hotspot_warmth.store(on, Ordering::Relaxed);
    }

    /// zt t14/tg6: the proactive rule reads the major clock
    /// ([`DirectorInputs::proactive_major_clock`]).
    pub fn set_proactive_major_clock(&self, on: bool) {
        self.proactive_major_clock.store(on, Ordering::Relaxed);
    }

    /// zt t1/tp: the cycle that is ending was a warm-up cycle (its cause was
    /// the warm-up rule). Call before [`Self::note_cycle_end`]. A no-op unless
    /// the director samples.
    pub fn note_warmup_cycle(&self) {
        if !self.sampling {
            return;
        }
        self.state.lock().warmup_cycles += 1;
    }

    /// zt t6/tc (zt1tp item 4): `-XX:ZProactive` (`false` turns the
    /// proactive rule off, as HotSpot's `rule_proactive` returns at once).
    pub fn set_proactive(&self, on: bool) {
        self.proactive_off.store(!on, Ordering::Relaxed);
    }

    /// zt t6/tc (zt1tp item 4): `-XX:ZAllocationSpikeTolerance=<f>`. A value
    /// that is not finite keeps the default. zt t7/x4: any finite value is
    /// kept, zero and negative included, as HotSpot 25.0.3 keeps them (the flag
    /// has no range: `-XX:ZAllocationSpikeTolerance=0` and `=-1` both print as
    /// `{command line}` under `-XX:+PrintFlagsFinal`); the rate rule's
    /// arithmetic is HotSpot's own on them.
    pub fn set_allocation_spike_tolerance(&self, tolerance: f64) {
        let bits = if tolerance.is_finite() {
            tolerance.to_bits()
        } else {
            SPIKE_TOLERANCE_UNSET
        };
        self.spike_tolerance_bits.store(bits, Ordering::Relaxed);
    }

    /// The spike tolerance the rate rule uses, `None` = the default.
    fn spike_tolerance(&self) -> Option<f64> {
        match self.spike_tolerance_bits.load(Ordering::Relaxed) {
            SPIKE_TOLERANCE_UNSET => None,
            bits => Some(f64::from_bits(bits)),
        }
    }

    /// `-XX:SoftMaxHeapSize` in bytes. zt t7/x4b: stored as given, `0`
    /// included (HotSpot 25.0.3 keeps a zero soft maximum: `-Xlog:gc+init`
    /// prints `Soft Max Capacity: 0M`); every reader here guards it
    /// ([`evaluate`] takes `max(1)`, [`sample_step_bytes`] clamps). It was
    /// raised to 1, so the heap's `soft_max_capacity()` could never read 0.
    pub fn set_soft_max(&self, bytes: u64) {
        self.soft_max.store(bytes, Ordering::Relaxed);
    }

    /// The poll. `used` is the heap's `allocated`. Returns the rule that says
    /// collect, when deciding; records the first yes, when shadowing. Two
    /// relaxed loads and a compare unless a byte step was crossed.
    pub fn poll(&self, used: u64) -> Option<DirectorRule> {
        if !self.sampling {
            return None;
        }
        if used >= self.next_sample_used.load(Ordering::Relaxed) {
            self.sample(used, Instant::now());
        }
        // zt t1/tp: one load yields the threshold and its rule together.
        let (fire_at, code) = unpack_fire(self.fire.load(Ordering::Relaxed));
        if used < fire_at {
            return None;
        }
        let rule = DirectorRule::from_code(code)?;
        if self.shadow_first_used.load(Ordering::Relaxed) == 0 {
            self.shadow_first_rule.store(rule.code(), Ordering::Relaxed);
            self.shadow_first_used.store(used.max(1), Ordering::Relaxed);
        }
        self.deciding.then_some(rule)
    }

    /// zt t7/tg: the director tick's question on an idle heap
    /// (`CRATONVM_ZGC_DIRECTOR_IDLE_TICK`, default ON since zt t20). HotSpot's
    /// director evaluates `rule_major_proactive` on its own clock, so a warm heap that stops
    /// allocating still gets a `Proactive` cycle once five minutes passed
    /// since the last one ([`PROACTIVE_MAX_INTERVAL`]); this director is
    /// polled by allocation, which an idle program never does. Samples at
    /// `now` (as HotSpot's tick samples the allocation rate) and answers
    /// whether the published decision is a proactive yes. `false` unless the
    /// director decides.
    pub fn idle_proactive_due(&self, used: u64, now: Instant) -> bool {
        if !self.deciding {
            return false;
        }
        self.sample(used, now);
        let (fire_at, code) = unpack_fire(self.fire.load(Ordering::Relaxed));
        used >= fire_at && DirectorRule::from_code(code) == Some(DirectorRule::Proactive)
    }

    /// Take one sample at `now` if a window has elapsed, re-evaluate, and
    /// publish. Public for tests (which pass their own clock).
    pub fn sample(&self, used: u64, now: Instant) {
        let Some(mut st) = self.state.try_lock() else {
            return;
        };
        let soft_max = self.soft_max();
        self.next_sample_used.store(
            used.saturating_add(sample_step_bytes(soft_max)),
            Ordering::Relaxed,
        );
        let wall_elapsed = now.saturating_duration_since(st.window_start);
        let active_elapsed = wall_elapsed.saturating_sub(st.window_pause);
        if active_elapsed >= SAMPLE_INTERVAL {
            let bytes = st
                .window_carry
                .saturating_add(used.saturating_sub(st.window_start_used));
            st.rate.add(bytes as f64 / active_elapsed.as_secs_f64());
            st.window_start = now;
            st.window_start_used = used;
            st.window_carry = 0;
            st.window_pause = Duration::ZERO;
        }
        st.last_used = used;
        let inputs = DirectorInputs {
            soft_max,
            used,
            used_after_last_cycle: st.used_after_last_cycle,
            rate_avg: st.rate.avg(),
            rate_sd: st.rate.sd(),
            rate_known: !st.rate.is_empty(),
            cycle_avg: st.cycle.davg(),
            cycle_sd: st.cycle.dsd(),
            cycles_timed: st.cycle.num(),
            cycles: st.cycles,
            since_last_cycle: now
                .saturating_duration_since(st.last_cycle_end)
                .as_secs_f64(),
            stw_model: self.stw_model_active(),
            poll_latency_bytes: poll_latency_bytes(soft_max),
            byte_polled_interval: self.byte_polled_interval.load(Ordering::Relaxed),
            hotspot_warmth: self.hotspot_warmth.load(Ordering::Relaxed),
            warmup_cycles: st.warmup_cycles,
            proactive_off: self.proactive_off.load(Ordering::Relaxed),
            spike_tolerance: self.spike_tolerance(),
            proactive_major_clock: self.proactive_major_clock.load(Ordering::Relaxed),
            since_last_major: now
                .saturating_duration_since(st.last_major_end)
                .as_secs_f64(),
            used_after_last_major: st.used_after_last_major,
        };
        let d = evaluate(&inputs);
        st.last = Some(d);
        let word = match d.fire_now {
            Some(rule) => pack_fire(0, rule.code()),
            None => {
                let code = d.fire_at_rule.map_or(0, DirectorRule::code);
                pack_fire(d.fire_at_used, code)
            }
        };
        self.fire.store(word, Ordering::Relaxed);
    }

    /// A collection ended at `now`, took `duration` (when it was timed), and
    /// left `used_after` bytes in use. Re-arms everything for the next
    /// interval. Returns the shadow record of the interval that just closed:
    /// `(first used the rules said yes at, the rule)`.
    /// Exclude a stop-the-world pause from the current allocation-rate window
    /// so mutator-stopped intervals do not deflate the sample rate or inflate
    /// `rate_sd`.
    pub fn note_stw_pause(&self, pause: Duration) {
        if !self.sampling || pause.is_zero() {
            return;
        }
        let mut st = self.state.lock();
        st.window_pause = st.window_pause.saturating_add(pause);
    }

    pub fn note_cycle_end(
        &self,
        now: Instant,
        duration: Option<Duration>,
        used_after: u64,
    ) -> Option<(u64, DirectorRule)> {
        let first_used = self.shadow_first_used.swap(0, Ordering::Relaxed);
        let first_rule = DirectorRule::from_code(self.shadow_first_rule.swap(0, Ordering::Relaxed));
        if !self.sampling {
            return None;
        }
        {
            let mut st = self.state.lock();
            if let Some(d) = duration {
                st.cycle.add(d.as_secs_f64());
            }
            st.cycles += 1;
            st.last_cycle_end = now;
            st.used_after_last_cycle = used_after;
            // `allocated` was rewritten to the live set, and the bytes the
            // cycle freed are not allocation: bank what the window saw before
            // the rewrite and count on from the post-cycle occupancy. The
            // window itself keeps its clock (see `window_carry`).
            st.window_carry = st
                .window_carry
                .saturating_add(st.last_used.saturating_sub(st.window_start_used));
            st.window_start_used = used_after;
            st.last_used = used_after;
        }
        // Force a fresh evaluation at the next poll, and fire on nothing until
        // then: the decision published before the cycle was about a heap that
        // no longer exists.
        self.fire.store(pack_fire(u64::MAX, 0), Ordering::Relaxed);
        self.next_sample_used.store(0, Ordering::Relaxed);
        if first_used == 0 {
            None
        } else {
            first_rule.map(|r| (first_used, r))
        }
    }

    /// zt t14/tg6: the cycle that just ended ([`Self::note_cycle_end`]) is
    /// one HotSpot 25 would run as a MAJOR (any cause but `Allocation Rate`,
    /// `High Usage` and an allocation stall, which its director runs as
    /// minors): restart the proactive rule's clock at `now` and take
    /// `used_after` as its base. A no-op unless the director samples.
    pub fn note_major_cycle_end(&self, now: Instant, used_after: u64) {
        if !self.sampling {
            return;
        }
        let mut st = self.state.lock();
        st.last_major_end = now;
        st.used_after_last_major = used_after;
    }

    /// `(rate_avg, rate_sd, cycle_avg_secs, cycle_sd_secs, last decision)` for
    /// the shadow line.
    pub fn snapshot(&self) -> (f64, f64, f64, f64, Option<DirectorDecision>) {
        let st = self.state.lock();
        (
            st.rate.avg(),
            st.rate.sd(),
            st.cycle.davg(),
            st.cycle.dsd(),
            st.last,
        )
    }
}

// ---- zr z6/tp: MINIMUM ALLOCATION PROGRESS NEAR A FULL HEAP -------------
//
// `needs_gc`'s live-bytes clause (`allocated >= gc_threshold`, 75% of
// `-Xmx`) and the headroom clause beside it are both behind ONE anti-storm
// floor, `gc_rearm = live + max(free / 4, 64 KiB)`, recomputed at the end of
// every collection from the post-sweep live figure (`free` = capacity - live).
// Once the live set itself is above the threshold, the threshold clause carries
// no information -- it is true on every poll -- so the floor alone paces the
// collector, and a quarter of the free space is a pacing that decays
// geometrically: each cycle lets `free / 4` be allocated, keeps the retained
// share of it, and the next cycle's floor is a quarter of what is left.
// `ProxyBoxingOomeProbe` (retains ~21% of what it allocates) then collects
// every ~5% of free-space shrinkage: ~20 whole-heap STW cycles per e-fold of
// the free space, 150-160 collections over the fill against HotSpot ZGC's 38
// (HotSpot's director polls on the clock and never starts a cycle while one
// runs, so its re-trigger needs mutator progress).
//
// The near-full floor: while the post-collection live set is at or above the
// live-bytes threshold, require `max(1 MiB, pct% of free)` of new allocation
// before an occupancy-triggered collection. Below the threshold nothing
// changes (warm-up pacing, the cause lines of `Zr1TrCauseProbe`). When the
// floor exceeds what is free, no occupancy clause fires at all and the next
// collection is the allocation failure's (the refusal ladder: collect, retry,
// clear soft references, retry, OOME), which is HotSpot's allocation stall --
// so an OutOfMemoryError cannot come earlier than it did; only the collections
// before it are spaced further apart.

/// zr zs s1/tp: the warm-up clause's progress floor
/// (`CRATONVM_ZGC_WARMUP_HOTSPOT_STEPS`, default on): at most HALF a warm-up
/// `step` (a step is a tenth of the soft maximum) of new allocation past the
/// last collection's live set (`watermark`), or the anti-storm floor `rearm`
/// when that is nearer. `hotspot_steps == false` is `rearm`, the pre-zs rule.
///
/// HotSpot's `rule_warmup` fires at `soft_max x 10% x (cycles + 1)` of USED
/// bytes, so on a small live set the second and third warm-up cycles come at
/// 20% and 30%. `rearm` is `live + max(free / 4, 64 KiB)`: with a 1% live
/// set that is ~26% of the heap, and the second cycle waited for it. The
/// warm-up clause runs at most three times per run, so its storm guard only
/// has to stop a back-to-back cycle with no progress. Half a step does that,
/// and leaves HotSpot's points exact whenever the live set is more than half
/// a step below the next point -- including the first warm-up after a
/// `System.gc()` (a 1% live set, the 10% point), which a whole step of
/// progress would have moved to 11%.
pub fn warmup_floor(watermark: usize, rearm: usize, step: usize, hotspot_steps: bool) -> usize {
    if !hotspot_steps {
        return rearm;
    }
    rearm.min(watermark.saturating_add(step / 2))
}

/// The pre-z6 anti-storm floor's minimum: 64 KiB.
pub const REARM_LEGACY_MIN: usize = 64 * 1024;
/// The near-full floor's minimum progress: 1 MiB of new allocation.
pub const REARM_NEAR_FULL_MIN: usize = 1024 * 1024;
/// Default share of the free space after the last collection that must be
/// allocated before an occupancy-triggered collection on a heap whose live set
/// is above the threshold (`CRATONVM_ZGC_NEAR_FULL_PROGRESS`).
pub const REARM_NEAR_FULL_DEFAULT_PERCENT: usize = 75;

/// The legacy floor: `max(free / 4, 64 KiB)` of new allocation.
pub fn rearm_floor_legacy(capacity: usize, live: usize) -> usize {
    (capacity.saturating_sub(live) / 4).max(REARM_LEGACY_MIN)
}

/// The re-arm floor: bytes of new allocation after the last collection before
/// the occupancy clauses may fire again. `near_full_percent == 0` is the kill
/// switch (the legacy floor everywhere); otherwise the legacy floor applies
/// while `live < threshold` and `max(1 MiB, near_full_percent% of free)` at or
/// above it -- never below the legacy floor.
pub fn rearm_floor(
    capacity: usize,
    live: usize,
    threshold: usize,
    near_full_percent: usize,
) -> usize {
    let legacy = rearm_floor_legacy(capacity, live);
    if near_full_percent == 0 || live < threshold {
        return legacy;
    }
    let free = capacity.saturating_sub(live);
    let share = (free as u128 * near_full_percent.min(100) as u128 / 100) as usize;
    share.max(REARM_NEAR_FULL_MIN).max(legacy)
}

/// Parse `CRATONVM_ZGC_NEAR_FULL_PROGRESS`: unset or empty -> the default
/// percent; a number -> that percent (clamped to 100; `0` is the kill switch);
/// an off word -> `0`; any other word -> the default.
pub fn near_full_progress_percent_from(value: Option<&str>) -> usize {
    let Some(v) = value else {
        return REARM_NEAR_FULL_DEFAULT_PERCENT;
    };
    let v = v.trim();
    if v.is_empty() {
        return REARM_NEAR_FULL_DEFAULT_PERCENT;
    }
    if let Ok(n) = v.parse::<usize>() {
        return n.min(100);
    }
    if cratonvm_types::flags::value_is_off(v) {
        0
    } else {
        REARM_NEAR_FULL_DEFAULT_PERCENT
    }
}

/// zs s6/no: HotSpot ZGC's small-page size (`ZGranuleSize`, 2 MiB) in KiB, the
/// value `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR=on` selects. HotSpot allocates
/// small objects only in fresh 2 MiB pages, so an allocation stall FAILS (the
/// `OutOfMemoryError`) after a collection that left no whole page free, with up
/// to a page of the heap unused; see [`exhausted_verdict`].
pub const EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB: usize = 2048;

/// Parse `CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR` into a floor in BYTES: unset,
/// empty or an off word -> `0` (off, the default); a number -> that many KiB
/// (`0` is off); any other on word (`on`, `true`, `yes`) -> HotSpot's page,
/// [`EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB`].
pub fn exhausted_free_floor_from(value: Option<&str>) -> usize {
    let Some(v) = value else {
        return 0;
    };
    let v = v.trim();
    if v.is_empty() || cratonvm_types::flags::value_is_off(v) {
        return 0;
    }
    if let Ok(kib) = v.parse::<usize>() {
        return kib.saturating_mul(1024);
    }
    EXHAUSTED_FREE_FLOOR_HOTSPOT_KIB * 1024
}

/// zs s6/no: the exhausted verdict (`CRATONVM_ZGC_EXHAUSTED_FREE_FLOOR`,
/// opt-in): after an allocation REFUSAL's collection, is the heap out of
/// allocatable space in HotSpot's page terms -- under `floor` bytes free?
/// `floor == 0` is off.
///
/// Why it is a separate rule from the near-full floor. Below ~1.33 MiB free no
/// occupancy clause fires, so every collection there is an allocation
/// failure's, and each frees only what was allocated since the previous one:
/// a fill that retains a share `r` of its allocation shrinks the free space by
/// `r` per collection, down to the last object (~30 whole-heap collections on
/// the `ProxyBoxingOomeProbe` / `Zr2CpCapacity grow` shape, ~95 ms of CPU each
/// at 229 MB live). HotSpot never enters that tail: its stall fails after the
/// first collection that cannot free a page. Under the verdict the refusing
/// thread's next ladder attempt is refused (`ZgcRealHeap::try_alloc_*`), so its
/// ladder goes on to the clear-soft rung and then throws, as HotSpot's stall
/// does after its (soft-clearing) collection. The OOME point moves earlier by
/// at most `floor` bytes of free space, toward HotSpot's.
pub fn exhausted_verdict(capacity: usize, live_after: usize, floor: usize) -> bool {
    floor != 0 && capacity.saturating_sub(live_after) < floor
}

/// The collection kinds the pacing census splits a collection into, from the
/// trigger mask `collect_garbage` snapshots (`trigger_seen`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaceKind {
    /// The live-bytes threshold clause (with or without `headroom_low`).
    Threshold,
    /// The headroom clause alone.
    Headroom,
    /// The warm-up clause.
    Warmup,
    /// The allocation-budget clause (`CRATONVM_ZGC_ALLOC_TRIGGER`).
    AllocBudget,
    /// A director rule (rate, proactive, high usage; opt-in).
    Director,
    /// The nursery clause (generational mode) or the stress lever.
    Other,
    /// No occupancy rule asked: an allocation failure (the refusal ladder),
    /// `System.gc()`, another forced door.
    Unasked,
}

impl PaceKind {
    const ALL: [PaceKind; 7] = [
        PaceKind::Threshold,
        PaceKind::Headroom,
        PaceKind::Warmup,
        PaceKind::AllocBudget,
        PaceKind::Director,
        PaceKind::Other,
        PaceKind::Unasked,
    ];
    fn index(self) -> usize {
        self as usize
    }
    fn key(self) -> &'static str {
        match self {
            PaceKind::Threshold => "threshold",
            PaceKind::Headroom => "headroom",
            PaceKind::Warmup => "warmup",
            PaceKind::AllocBudget => "budget",
            PaceKind::Director => "director",
            PaceKind::Other => "other",
            PaceKind::Unasked => "unasked",
        }
    }
}

/// Window buckets (bytes allocated since the previous collection), upper
/// bounds exclusive.
const PACE_BUCKETS: [(usize, &str); 5] = [
    (64 * 1024, "lt64k"),
    (1024 * 1024, "lt1m"),
    (8 * 1024 * 1024, "lt8m"),
    (64 * 1024 * 1024, "lt64m"),
    (usize::MAX, "ge64m"),
];

/// zr z6/tp: per-heap pacing census for `[GC] zgc-pacing:` -- for every
/// collection, which rule asked for it and how much was allocated since the
/// previous one, split out for collections taken NEAR FULL (the previous
/// cycle's post-collection live set at or above the live-bytes threshold).
#[derive(Default)]
pub struct ZPacingCensus {
    collections: AtomicUsize,
    near_full: AtomicUsize,
    near_full_kind: [AtomicUsize; 7],
    near_full_bucket: [AtomicUsize; 5],
    near_full_window_bytes: AtomicUsize,
    near_full_window_min: AtomicUsize,
    /// Occupancy-trigger intervals the near-full floor held back (the legacy
    /// floor would already have admitted the collection): the engagement gauge,
    /// charged at most once per interval.
    pub deferred: AtomicUsize,
    /// zt t3/tp: whether the collection [`Self::record`] saw last was near
    /// full, and its kind ([`PaceKind::index`]), for [`Self::note_cost`],
    /// which the same collection calls later in its pause.
    last_near_full: AtomicBool,
    last_kind: AtomicUsize,
    /// zt t3/tp: timed collections and the sum of their timed laps (every
    /// collection, near full or not), WALL microseconds. zt t4/x3: the laps
    /// are the cycle's mark (start, concurrent part when there is one, end),
    /// reference, sweep and slide laps -- not only its pauses -- and wall
    /// time, so `pace_us` against process CPU understates a phase run by a
    /// worker pool (its CPU is in `[GC] zgc-cpu:`).
    timed: AtomicUsize,
    pause_us: AtomicU64,
    /// zt t3/tp: the near-full collections' pause, by kind and by phase
    /// (mark incl. its start / end laps, reference processing, sweep, slide),
    /// microseconds. What a near-full collection costs, and which phase and
    /// which rule pay for it: `pace_nf_us / pace_near_full` is the cost per
    /// collection, `pace_us` against the process CPU is the collector's share.
    near_full_kind_us: [AtomicU64; 7],
    near_full_phase_us: [AtomicU64; 4],
}

impl ZPacingCensus {
    /// Record one collection.
    pub fn record(&self, near_full: bool, kind: PaceKind, window: usize) {
        self.collections.fetch_add(1, Ordering::Relaxed);
        self.last_near_full.store(near_full, Ordering::Relaxed);
        self.last_kind.store(kind.index(), Ordering::Relaxed);
        if !near_full {
            return;
        }
        let n = self.near_full.fetch_add(1, Ordering::Relaxed);
        self.near_full_kind[kind.index()].fetch_add(1, Ordering::Relaxed);
        let b = PACE_BUCKETS
            .iter()
            .position(|(lim, _)| window < *lim)
            .unwrap_or(PACE_BUCKETS.len() - 1);
        self.near_full_bucket[b].fetch_add(1, Ordering::Relaxed);
        self.near_full_window_bytes
            .fetch_add(window, Ordering::Relaxed);
        if n == 0 {
            self.near_full_window_min.store(window, Ordering::Relaxed);
        } else {
            self.near_full_window_min
                .fetch_min(window, Ordering::Relaxed);
        }
    }

    /// zt t3/tp: the cost of the collection [`Self::record`] saw last, from
    /// its timed laps (microseconds): mark (incl. the mark-start and mark-end
    /// laps), reference processing, sweep, slide. Call once per timed
    /// collection, after `record`, in the same pause.
    pub fn note_cost(&self, mark_us: u64, refs_us: u64, sweep_us: u64, reloc_us: u64) {
        let total = mark_us
            .saturating_add(refs_us)
            .saturating_add(sweep_us)
            .saturating_add(reloc_us);
        self.timed.fetch_add(1, Ordering::Relaxed);
        self.pause_us.fetch_add(total, Ordering::Relaxed);
        if !self.last_near_full.load(Ordering::Relaxed) {
            return;
        }
        let kind = self.last_kind.load(Ordering::Relaxed).min(6);
        self.near_full_kind_us[kind].fetch_add(total, Ordering::Relaxed);
        let phases = [mark_us, refs_us, sweep_us, reloc_us];
        for (i, us) in phases.into_iter().enumerate() {
            self.near_full_phase_us[i].fetch_add(us, Ordering::Relaxed);
        }
    }

    /// zt t3/tp: near-full pause microseconds charged to `kind`.
    pub fn near_full_us_of(&self, kind: PaceKind) -> u64 {
        self.near_full_kind_us[kind.index()].load(Ordering::Relaxed)
    }

    /// Collections near full whose rule was `kind`.
    pub fn near_full_of(&self, kind: PaceKind) -> usize {
        self.near_full_kind[kind.index()].load(Ordering::Relaxed)
    }

    /// Collections recorded near full.
    pub fn near_full_count(&self) -> usize {
        self.near_full.load(Ordering::Relaxed)
    }

    /// The `[GC] zgc-pacing:` line. Every key is prefixed `pace_`, so none is
    /// shared with another `[GC]` line.
    pub fn line(&self, near_full_percent: usize) -> String {
        use std::fmt::Write;
        let nf = self.near_full.load(Ordering::Relaxed);
        let mut s = format!(
            "[GC] zgc-pacing: pace_collections={} pace_near_full={nf}",
            self.collections.load(Ordering::Relaxed)
        );
        for k in PaceKind::ALL {
            let _ = write!(s, " pace_nf_{}={}", k.key(), self.near_full_of(k));
        }
        for (i, (_, key)) in PACE_BUCKETS.iter().enumerate() {
            let _ = write!(
                s,
                " pace_nf_win_{key}={}",
                self.near_full_bucket[i].load(Ordering::Relaxed)
            );
        }
        let bytes = self.near_full_window_bytes.load(Ordering::Relaxed);
        let (avg, min) = if nf == 0 {
            (0, 0)
        } else {
            (
                bytes / nf,
                self.near_full_window_min.load(Ordering::Relaxed),
            )
        };
        let _ = write!(
            s,
            " pace_nf_win_bytes={bytes} pace_nf_win_avg={avg} pace_nf_win_min={min} \
             pace_floor_deferred={} pace_floor_pct={near_full_percent}",
            self.deferred.load(Ordering::Relaxed),
        );
        // zt t3/tp: what the collections cost (timed pauses, microseconds).
        let mut nf_us: u64 = 0;
        for k in PaceKind::ALL {
            nf_us = nf_us.saturating_add(self.near_full_us_of(k));
        }
        let _ = write!(
            s,
            " pace_timed={} pace_us={} pace_nf_us={nf_us}",
            self.timed.load(Ordering::Relaxed),
            self.pause_us.load(Ordering::Relaxed),
        );
        for k in PaceKind::ALL {
            let _ = write!(s, " pace_nf_us_{}={}", k.key(), self.near_full_us_of(k));
        }
        for (i, key) in ["mark", "refs", "sweep", "reloc"].iter().enumerate() {
            let _ = write!(
                s,
                " pace_nf_{key}_us={}",
                self.near_full_phase_us[i].load(Ordering::Relaxed)
            );
        }
        s
    }
}

#[cfg(test)]
mod tests;
