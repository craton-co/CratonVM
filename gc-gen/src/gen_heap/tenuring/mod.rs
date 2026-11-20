// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! gen r4w6/young6 (2026-09-24) — HotSpot-style ADAPTIVE TENURING for the
//! generational young collector.
//!
//! # What was there
//!
//! A constant: `gen_heap::PROMOTION_AGE = 3`. Every survivor was copied twice
//! and promoted on its third survival, whatever the survivor volume, and the
//! only adaptivity was the all-or-nothing promote-on-pressure arm (one cycle
//! with >75 % survival at >= half occupancy made the NEXT cycle tenure every
//! survivor, then the threshold snapped back to 3). See
//! `docs/feature-designs/generative-gc-architecture.md`.
//!
//! Two failure shapes follow from a fixed 3:
//!
//! * a MEDIUM-LIVED object (one that survives 3 to 5 young collections and
//!   then dies) is promoted on its third survival and dies in the OLD
//!   generation, where only a major reclaims it;
//! * a steady large live set is copied twice before promotion however large
//!   it is.
//!
//! # What HotSpot does, and what this module does
//!
//! HotSpot (Serial, Parallel and G1 alike) keeps an AGE TABLE per young
//! collection — bytes of survivors that stayed young, by their new age — and
//! recomputes the tenuring threshold after every collection
//! (`AgeTable::compute_tenuring_threshold`):
//!
//! ```text
//! desired = survivor_capacity * TargetSurvivorRatio / 100
//! age = 1; total = 0
//! while age < table_size { total += bytes[age]; if total > desired { break }; age += 1 }
//! threshold = min(age, MaxTenuringThreshold)
//! ```
//!
//! and tenures an object whose age at the start of a collection is `>=
//! threshold`. That is what [`compute_tenuring_threshold`] implements, in the
//! same HotSpot units (`T`). This collector's tenuring predicate
//! (`gen_heap::should_tenure`) asks `gc_age + 1 >= promotion_age`, so its
//! `promotion_age` is `T + 1`: HotSpot's `T = 0` ("always tenure") is
//! `promotion_age = 1`, HotSpot's default ceiling `T = 15` is `16`, and the
//! historical `PROMOTION_AGE = 3` is `T = 2`.
//!
//! # The survivor capacity, on a collector with no survivor space
//!
//! This young generation is a pair of semispaces: to-space is as large as
//! from-space, so the survivor "space" never overflows and its capacity is
//! the wrong denominator (it would put the target at half the young
//! generation, which no workload that is not already in the promote-on-
//! pressure regime ever reaches). The quantity HotSpot sizes its survivor
//! spaces against is EDEN — `SurvivorRatio = 8` makes each survivor space one
//! eighth of eden. Eden's equivalent here is the young TRIGGER AS CONFIGURED
//! (`gen_heap::tenuring_eden_bytes`: the from-space capacity times
//! `CRATONVM_GC_YOUNG_TRIGGER_PERCENT`, 50 % by default), or, under the
//! default-ON `CRATONVM_GEN_YOUNG_TRIGGER_SERIAL_EDEN` on a heap built from
//! `-Xmx`, HotSpot Serial's own eden for its `-Xmx` / `-Xmn` / `NewRatio`
//! (`GenerationalHeap::tenuring_eden_for`), so
//!
//! ```text
//! survivor_capacity = eden_bytes / SurvivorRatio   (SurvivorRatio = 8)
//! desired           = survivor_capacity * TargetSurvivorRatio / 100   (in words, as HotSpot)
//! ```
//!
//! which at the default 64 MiB semispace (a 32 MiB trigger) is a 2 MiB target
//! (about 4.3 MiB at `-Xmx256m` under the opt-in Serial eden, 68.3 MiB).
//!
//! gcf f2/y: the target used to follow the LIVE trigger (`young_gc_threshold`),
//! so when the pause-goal loop (`adapt_young_trigger_to_pause`) halved the
//! nursery the tenuring target halved with it and the threshold fell. HotSpot
//! Serial has no such coupling -- its target is `to()->capacity()`, moved only
//! by a heap resize -- and the brief for this module is HotSpot's rule
//! exactly, so the target now follows the configured trigger of the arena
//! the cycle collected. The rule itself (`compute_tenuring_threshold`), the
//! initial threshold (`MaxTenuringThreshold`, as Serial and G1) and the
//! word-rounded target ([`desired_survivor_bytes`]) are HotSpot's. Two
//! documented differences remain: `-XX:InitialTenuringThreshold` is honoured
//! (only Parallel reads it in HotSpot), and the threshold is recomputed after
//! every young cycle, where Serial skips the adjustment on a promotion
//! failure (this collector has none: a full old generation keeps survivors
//! young and they are counted).
//!
//! # The graded response to promotion pressure
//!
//! The age table IS the graded response: the more bytes survive at low ages,
//! the lower the threshold, one age at a time, and it recovers the same way
//! as the survivor volume falls. The severe arm (>75 % survival at >= half
//! occupancy) is kept as the one-cycle floor `T = 0` — the gap page's
//! requirement that `BinT 14`'s death-spiral break be preserved — and it is
//! still expressed through the existing one-shot `force_promote_all` arm, so
//! its census and its non-moving consumption are unchanged.
//!
//! # Default ON (gcf vf3)
//!
//! Adaptive tenuring is engaged by default since gcf vf3 (flip-gate items 4-7;
//! `CRATONVM_GC_ADAPTIVE_TENURING=0` restores the fixed age), and by
//! `CRATONVM_GC_ADAPTIVE_TENURING` (HotSpot's defaults: `MaxTenuringThreshold
//! = 15`, `TargetSurvivorRatio = 50`), or by any of `-XX:MaxTenuringThreshold`,
//! `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio` on the command line
//! (an operator who typed a HotSpot tenuring flag asked for HotSpot's
//! adaptive semantics). One heap can be pinned to the fixed age
//! (`GenerationalHeap::with_fixed_promotion_age`, gcg g1/y): that is how the
//! gc unit tests that assume the historical age ask for it. The probe that
//! decided the default flip is `GenR4W6TenuringProbe.java` (old-gen growth of a medium-lived
//! pattern), run beside `GenR4W4SurvivorOverflowProbe`,
//! `GenR4W4EvacThroughputProbe` and `BinT 14`.
//!
//! State is PER HEAP (a field of `GenerationalHeap`), never a process global.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};

use cratonvm_types::MAX_GC_AGE;

/// Buckets in the age table: one per representable age, `0..=MAX_GC_AGE`.
/// HotSpot's `AgeTable::table_size` is `markWord::max_age + 1` for the same
/// reason. Bucket 0 is never written (a survivor's age is incremented before
/// it is counted), exactly as in HotSpot.
pub const AGE_TABLE_LEN: usize = MAX_GC_AGE as usize + 1;

/// HotSpot's default `-XX:MaxTenuringThreshold`.
pub const DEFAULT_MAX_TENURING_THRESHOLD: u8 = 15;

/// HotSpot's default `-XX:TargetSurvivorRatio`, in percent.
pub const DEFAULT_TARGET_SURVIVOR_RATIO: u8 = 50;

/// HotSpot's default `-XX:SurvivorRatio`: eden is this many times one survivor
/// space. See the module doc for why eden's equivalent is the young trigger.
pub const SURVIVOR_RATIO: u64 = 8;

/// The HotSpot tenuring flags as the launcher parsed them, carried on the VM's
/// config to the heap (`VmHeap::set_tenuring_config`). `None` = not given.
///
/// Values are in HotSpot's units: a threshold `T` tenures an object whose age
/// at the start of a young collection is `>= T`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TenuringConfig {
    /// `-XX:MaxTenuringThreshold=<0..=16>` (16 = `NeverTenure`; gci i23/tn,
    /// doc fixed gci i27/gs).
    pub max_tenuring_threshold: Option<u8>,
    /// `-XX:InitialTenuringThreshold=<0..=16>`. HotSpot's Serial and G1 start
    /// at `MaxTenuringThreshold`; only Parallel reads this one. Honoured here
    /// as the first cycle's threshold when given (clamped to the maximum).
    pub initial_tenuring_threshold: Option<u8>,
    /// `-XX:TargetSurvivorRatio=<0..=100>`.
    pub target_survivor_ratio: Option<u8>,
    /// `-XX:+PrintTenuringDistribution`: print the age table after every
    /// moving young collection, whether or not adaptive tenuring is engaged.
    pub print_tenuring_distribution: bool,
}

impl TenuringConfig {
    /// Nothing given: the heap behaves exactly as before.
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    /// Did the operator type one of the three HotSpot tenuring VALUES? Any of
    /// them engages adaptive tenuring on this heap (see the module doc).
    pub fn requests_adaptive(&self) -> bool {
        self.max_tenuring_threshold.is_some()
            || self.initial_tenuring_threshold.is_some()
            || self.target_survivor_ratio.is_some()
    }

    /// The HotSpot spellings of the flags that were given, for the launcher's
    /// "honoured by the Generational collector only" note.
    pub fn given_flag_names(&self) -> Vec<&'static str> {
        let mut v = Vec::new();
        if self.max_tenuring_threshold.is_some() {
            v.push("-XX:MaxTenuringThreshold");
        }
        if self.initial_tenuring_threshold.is_some() {
            v.push("-XX:InitialTenuringThreshold");
        }
        if self.target_survivor_ratio.is_some() {
            v.push("-XX:TargetSurvivorRatio");
        }
        if self.print_tenuring_distribution {
            v.push("-XX:+PrintTenuringDistribution");
        }
        v
    }

    /// Parse one HotSpot tenuring VALUE flag's text, with HotSpot's range.
    /// `Err` carries the one-line warning the launcher prints; the flag is
    /// then ignored (HotSpot refuses to start instead — a warning is the
    /// CratonVM convention for a malformed tuning flag, see `-XX:NewRatio`).
    pub fn parse_value(spelling: &str, raw: &str, max: u8) -> Result<u8, String> {
        match raw.trim().parse::<u64>() {
            Ok(v) if v <= u64::from(max) => Ok(v as u8),
            Ok(v) => Err(format!(
                "ignoring {spelling}={v} (must be between 0 and {max}, as in HotSpot)"
            )),
            Err(_) => Err(format!(
                "ignoring {spelling}={raw} (expected an integer between 0 and {max})"
            )),
        }
    }
}

/// One young collection's tenuring decision, for the census line and the
/// distribution print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenuringDecision {
    /// HotSpot's "desired survivor size".
    pub desired_bytes: u64,
    /// The threshold this collection used (HotSpot units).
    pub previous: u8,
    /// The threshold the NEXT collection will use (HotSpot units).
    pub next: u8,
    /// The ceiling (`MaxTenuringThreshold`).
    pub max: u8,
    /// Whether the threshold is adaptive (else it is the fixed historical 2).
    pub adaptive: bool,
}

/// HotSpot's `AgeTable::compute_tenuring_threshold`, in HotSpot units.
///
/// `ages[a]` is the bytes of survivors that stayed YOUNG this collection and
/// now carry age `a`. The result is the smallest age at which the cumulative
/// bytes of ages `1..=age` exceed `desired_bytes`, capped at `max_threshold`
/// (and so at `AGE_TABLE_LEN` = `MAX_GC_AGE + 1`, where the loop stops).
///
/// Never 0: a table whose age-1 bucket alone exceeds the target yields 1, as
/// in HotSpot. `T = 0` ("tenure everything") is reachable only through
/// `max_threshold = 0` or the severe promote-on-pressure arm.
pub fn compute_tenuring_threshold(
    ages: &[u64; AGE_TABLE_LEN],
    desired_bytes: u64,
    max_threshold: u8,
) -> u8 {
    let mut total: u64 = 0;
    let mut age = 1usize;
    while age < AGE_TABLE_LEN {
        total = total.saturating_add(ages[age]);
        if total > desired_bytes {
            break;
        }
        age += 1;
    }
    // `age` is at most `AGE_TABLE_LEN` (16), so the cast is exact.
    // gci i23/tn TN-H1: the cap is `MAX_GC_AGE + 1` (HotSpot's `table_size`,
    // `NeverTenure` at `MaxTenuringThreshold=16`), not `MAX_GC_AGE`: with the
    // old cap a typed 16 fell back to 15 after the first young collection. The
    // result indexes no array (it only feeds `promotion_age`, a `u8` compare).
    (age as u8).min(max_threshold).min(MAX_GC_AGE + 1)
}

/// HotSpot's desired survivor size, from eden's equivalent (the configured
/// young trigger; see the module doc) and `TargetSurvivorRatio`.
///
/// gcf f2/y: in HotSpot's UNITS. `DefNewGeneration::adjust_desired_tenuring_threshold`
/// computes `desired = (size_t)(survivor_capacity_in_WORDS * TargetSurvivorRatio
/// / 100.0)` and `AgeTable::compute_tenuring_threshold` compares the age
/// table's word counts against it. Every survivor size is a whole number of
/// words, so `total_words > desired_words` is `total_bytes > desired_words *
/// 8`: the target is rounded down to a word here, and the comparison in
/// [`compute_tenuring_threshold`] stays in bytes. (A byte-granular target
/// could differ from HotSpot's by up to seven bytes, which flips the
/// threshold of a table that lands in that window.) The value is also what
/// HotSpot prints as "Desired survivor size N bytes" (words times `oopSize`).
#[cfg_attr(not(test), allow(dead_code))] // gci i45/sc: default-ratio reference (tests); production passes the heap's ratio
pub fn desired_survivor_bytes(eden_bytes: usize, target_survivor_ratio: u8) -> u64 {
    desired_survivor_bytes_at(eden_bytes, target_survivor_ratio, SURVIVOR_RATIO)
}

/// gci i45/sc (`gci-i42ta-launcher-drops-survivor-ratio-and-tlab-flags-RETIRED-UNFIXED-20261018`,
/// the consumer half): [`desired_survivor_bytes`] at an explicit
/// `-XX:SurvivorRatio` -- HotSpot Serial's survivor capacity is `eden /
/// SurvivorRatio` for an eden of `SurvivorRatio` survivors. `survivor_ratio`
/// is read as at least 1 (the launcher refuses 0). Pure.
pub fn desired_survivor_bytes_at(
    eden_bytes: usize,
    target_survivor_ratio: u8,
    survivor_ratio: u64,
) -> u64 {
    const WORD: u64 = 8;
    let survivor_words = eden_bytes as u64 / survivor_ratio.max(1) / WORD;
    let desired_words = survivor_words.saturating_mul(u64::from(target_survivor_ratio)) / 100;
    desired_words.saturating_mul(WORD)
}

/// gci i27/gs (`gci-i14mp-...` item 8, the desired-size half):
/// `CRATONVM_GEN_SURVIVOR_SPACE_ALIGNED` (default ON since 2026-10-17, `=0`
/// kills) -- the survivor capacity the tenuring target is taken from is a
/// whole number of HotSpot's
/// `SpaceAlignment` (64 KiB), as HotSpot Serial's to-space is
/// (`DefNewGeneration::compute_space_boundaries`: `survivor_size =
/// align_down(young / (SurvivorRatio + 2), alignment)`, at least one unit).
/// Read once per young collection's decision. It moves the threshold's
/// target (by up to 64 KiB, about 17 KiB of target at `-Xmx256m`); its
/// deciding row (`vi27-gs.list` DS) passed and the user flipped it default ON
/// on 2026-10-17.
pub fn survivor_space_aligned_enabled() -> bool {
    true
}

/// gci i27/gs: [`desired_survivor_bytes`] with HotSpot's aligned survivor
/// capacity: `eden / SurvivorRatio` (HotSpot Serial's survivor for an eden of
/// `SurvivorRatio` survivors) aligned down to 64 KiB, or one 64 KiB unit when
/// that is 0 (and eden is not), then `TargetSurvivorRatio` percent of it in
/// whole words. At `-Xmx256m` (Serial eden 0.8 x 256m/3) this is HotSpot's
/// `4456448` where the unaligned target is `4473920`. Pure.
#[cfg_attr(not(test), allow(dead_code))] // gci i45/sc: default-ratio reference (tests); production passes the heap's ratio
pub fn desired_survivor_bytes_space_aligned(eden_bytes: usize, target_survivor_ratio: u8) -> u64 {
    desired_survivor_bytes_space_aligned_at(eden_bytes, target_survivor_ratio, SURVIVOR_RATIO)
}

/// gci i45/sc: [`desired_survivor_bytes_space_aligned`] at an explicit
/// `-XX:SurvivorRatio` (read as at least 1). Pure.
pub fn desired_survivor_bytes_space_aligned_at(
    eden_bytes: usize,
    target_survivor_ratio: u8,
    survivor_ratio: u64,
) -> u64 {
    const WORD: u64 = 8;
    const ALIGN: u64 = 64 * 1024;
    let raw = eden_bytes as u64 / survivor_ratio.max(1);
    let capacity = if raw == 0 {
        0
    } else if raw < ALIGN {
        ALIGN
    } else {
        raw & !(ALIGN - 1)
    };
    let desired_words = (capacity / WORD).saturating_mul(u64::from(target_survivor_ratio)) / 100;
    desired_words.saturating_mul(WORD)
}

/// The target one young collection's decision uses: [`desired_survivor_bytes`],
/// or its space-aligned form under [`survivor_space_aligned_enabled`], at the
/// heap's `SurvivorRatio` (gci i45/sc; [`SURVIVOR_RATIO`] unless typed).
fn decision_desired_bytes(eden_bytes: usize, target_survivor_ratio: u8, survivor_ratio: u64) -> u64 {
    if survivor_space_aligned_enabled() {
        desired_survivor_bytes_space_aligned_at(eden_bytes, target_survivor_ratio, survivor_ratio)
    } else {
        desired_survivor_bytes_at(eden_bytes, target_survivor_ratio, survivor_ratio)
    }
}

/// HotSpot's `AgeTable::add(age, size)`: `bytes` of one survivor that stayed
/// young, at its NEW age (already incremented; clamped to the table).
#[inline]
pub fn add_to_age_table(ages: &mut [u64; AGE_TABLE_LEN], new_age: u8, bytes: usize) {
    let i = usize::from(new_age).min(AGE_TABLE_LEN - 1);
    ages[i] = ages[i].saturating_add(bytes as u64);
}

/// The age table as HotSpot prints it under `-XX:+PrintTenuringDistribution`
/// (`-Xlog:gc+age=trace`), one line per non-empty age, each prefixed
/// `[GC] tenuring:` so a log grep finds the block.
pub fn distribution_lines(d: &TenuringDecision, ages: &[u64; AGE_TABLE_LEN]) -> Vec<String> {
    let mut out = Vec::with_capacity(AGE_TABLE_LEN + 1);
    out.push(format!(
        "[GC] tenuring: Desired survivor size {} bytes, new threshold {} (max threshold {}) \
         previous_threshold={} mode={}",
        d.desired_bytes,
        d.next,
        d.max,
        d.previous,
        if d.adaptive { "adaptive" } else { "fixed" },
    ));
    let mut total: u64 = 0;
    for (age, &bytes) in ages.iter().enumerate().skip(1) {
        if bytes == 0 {
            continue;
        }
        total = total.saturating_add(bytes);
        out.push(format!(
            "[GC] tenuring: - age {age:>3}: {bytes:>10} bytes, {total:>10} total"
        ));
    }
    out
}

/// The per-heap tenuring state. Every field is written only by a young
/// collection (stop-the-world, one driver) or by the one-time configuration
/// call before the first collection, so `Relaxed` is enough throughout: the
/// readers are the same collector thread, or diagnostics.
pub struct TenuringState {
    /// `-XX:` tenuring values were given for this heap (see
    /// [`TenuringConfig::requests_adaptive`]).
    configured_adaptive: AtomicBool,
    /// gcg g1/y: this heap ignores `CRATONVM_GC_ADAPTIVE_TENURING` (reads it
    /// as `=0`) -- the fixed-age arm, pinned per heap
    /// ([`Self::pin_fixed`]). A typed `-XX:` tenuring value still engages
    /// adaptive tenuring, exactly as it does under the variable's `=0`.
    pinned_fixed: AtomicBool,
    /// `-XX:+PrintTenuringDistribution`.
    print: AtomicBool,
    /// `MaxTenuringThreshold`, HotSpot units.
    max_threshold: AtomicU8,
    /// `TargetSurvivorRatio`, percent.
    target_survivor_ratio: AtomicU8,
    /// gci i45/sc: `-XX:SurvivorRatio` ([`SURVIVOR_RATIO`] unless typed), the
    /// divisor of the survivor capacity the target is taken from. Written
    /// once, before the first collection ([`Self::set_survivor_ratio`]).
    survivor_ratio: AtomicU64,
    /// The threshold the NEXT young collection uses when adaptive tenuring is
    /// engaged, HotSpot units.
    threshold: AtomicU8,
    /// `[adapted, raised, lowered, unchanged]` over the heap's life.
    census: [AtomicU64; 4],
    /// gcf f4/r: `-Xlog:gc+age` is on for the collection in progress (set by
    /// the VM before each one): fill the age table and stash the decision.
    xlog: AtomicBool,
    /// gcf f4/r: the last young collection's `(desired_bytes, next, max,
    /// ages)` for the `gc,age` lines, taken by the VM's `-Xlog` emitter.
    xlog_last: std::sync::Mutex<Option<(u64, u8, u8, [u64; AGE_TABLE_LEN])>>,
}

impl TenuringState {
    /// HotSpot's defaults; adaptive tenuring not configured.
    pub const fn new() -> Self {
        Self {
            configured_adaptive: AtomicBool::new(false),
            pinned_fixed: AtomicBool::new(false),
            print: AtomicBool::new(false),
            max_threshold: AtomicU8::new(DEFAULT_MAX_TENURING_THRESHOLD),
            target_survivor_ratio: AtomicU8::new(DEFAULT_TARGET_SURVIVOR_RATIO),
            survivor_ratio: AtomicU64::new(SURVIVOR_RATIO),
            // HotSpot's Serial and G1 start at `MaxTenuringThreshold`.
            threshold: AtomicU8::new(DEFAULT_MAX_TENURING_THRESHOLD),
            census: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
            xlog: AtomicBool::new(false),
            xlog_last: std::sync::Mutex::new(None),
        }
    }

    /// gcf f4/r: arm (or disarm) the `gc,age` census for the next collection.
    pub fn set_xlog(&self, on: bool) {
        self.xlog.store(on, Ordering::Relaxed);
    }

    /// gcf f4/r: is `-Xlog:gc+age` asking for this collection's age table?
    #[inline]
    pub fn xlog_requested(&self) -> bool {
        self.xlog.load(Ordering::Relaxed)
    }

    /// gcf f4/r: keep this collection's decision and age table for the
    /// `gc,age` lines.
    pub fn stash_xlog(&self, d: &TenuringDecision, ages: &[u64; AGE_TABLE_LEN]) {
        if let Ok(mut last) = self.xlog_last.lock() {
            *last = Some((d.desired_bytes, d.next, d.max, *ages));
        }
    }

    /// gcf f4/r: the stashed `(desired_bytes, next, max, ages)`, once.
    pub fn take_xlog(&self) -> Option<(u64, u8, u8, [u64; AGE_TABLE_LEN])> {
        self.xlog_last.lock().ok().and_then(|mut last| last.take())
    }

    /// Apply the launcher's `-XX:` tenuring flags. Called once, before the
    /// first collection; values are clamped to HotSpot's ranges (the launcher
    /// already refused out-of-range text).
    pub fn configure(&self, cfg: &TenuringConfig) {
        let max = cfg
            .max_tenuring_threshold
            .unwrap_or(DEFAULT_MAX_TENURING_THRESHOLD)
            // gci i23/tn TN-H1: HotSpot's range is 0..=16 (`markWord::max_age + 1`);
            // 16 never tenures by age, as on HotSpot Serial.
            .min(MAX_GC_AGE + 1);
        let ratio = cfg
            .target_survivor_ratio
            .unwrap_or(DEFAULT_TARGET_SURVIVOR_RATIO)
            .min(100);
        let initial = cfg.initial_tenuring_threshold.unwrap_or(max).min(max);
        self.max_threshold.store(max, Ordering::Relaxed);
        self.target_survivor_ratio.store(ratio, Ordering::Relaxed);
        self.threshold.store(initial, Ordering::Relaxed);
        self.configured_adaptive
            .store(cfg.requests_adaptive(), Ordering::Relaxed);
        self.print
            .store(cfg.print_tenuring_distribution, Ordering::Relaxed);
    }

    /// gci i45/sc: the heap's `-XX:SurvivorRatio` (at least 1). Called once,
    /// before the first collection, by `GenerationalHeap::set_survivor_ratio`.
    pub fn set_survivor_ratio(&self, survivor_ratio: u64) {
        self.survivor_ratio
            .store(survivor_ratio.max(1), Ordering::Relaxed);
    }

    /// gci i45/sc: the `SurvivorRatio` the survivor target divides by.
    #[inline]
    pub fn survivor_ratio(&self) -> u64 {
        self.survivor_ratio.load(Ordering::Relaxed)
    }

    /// Is adaptive tenuring in force on this heap? `env_adaptive` is
    /// `CRATONVM_GC_ADAPTIVE_TENURING`, read by the caller; a heap pinned to
    /// the fixed age ([`Self::pin_fixed`]) reads it as `=0`.
    #[inline]
    pub fn engaged(&self, env_adaptive: bool) -> bool {
        (env_adaptive && !self.pinned_fixed.load(Ordering::Relaxed))
            || self.configured_adaptive.load(Ordering::Relaxed)
    }

    /// gcg g1/y: pin this heap to the fixed promotion age: the
    /// `CRATONVM_GC_ADAPTIVE_TENURING=0` arm for one heap, whatever the
    /// variable says. Replaces the `cfg(test)`-only "unset reads OFF" default
    /// the gc unit tests used to lean on: a test that assumes the historical
    /// age pins it here, explicitly. Per heap, never a process global.
    pub fn pin_fixed(&self) {
        self.pinned_fixed.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::pin_fixed`] pinned this heap (gc wave 9, lane gcC: a
    /// pinned heap also keeps the historical one-age-per-cycle rule, outside
    /// `CRATONVM_GEN_AGE_BY_SERIAL_EDEN`'s epoch).
    pub fn is_pinned_fixed(&self) -> bool {
        self.pinned_fixed.load(Ordering::Relaxed)
    }

    /// `-XX:+PrintTenuringDistribution`.
    #[inline]
    pub fn print_requested(&self) -> bool {
        self.print.load(Ordering::Relaxed)
    }

    /// The adaptive threshold, HotSpot units.
    #[inline]
    pub fn threshold(&self) -> u8 {
        self.threshold.load(Ordering::Relaxed)
    }

    /// `should_tenure`'s `promotion_age` for the next young collection:
    /// `threshold + 1` when adaptive (see the module doc for the unit
    /// conversion), else `fixed` (the historical `PROMOTION_AGE`).
    #[inline]
    pub fn promotion_age(&self, env_adaptive: bool, fixed: u8) -> u8 {
        if self.engaged(env_adaptive) {
            self.threshold().saturating_add(1)
        } else {
            fixed
        }
    }

    /// Recompute the threshold from this collection's age table. Returns the
    /// decision for the census / distribution print.
    pub fn adapt(&self, ages: &[u64; AGE_TABLE_LEN], eden_bytes: usize) -> TenuringDecision {
        let max = self.max_threshold.load(Ordering::Relaxed);
        let desired = decision_desired_bytes(
            eden_bytes,
            self.target_survivor_ratio.load(Ordering::Relaxed),
            self.survivor_ratio(),
        );
        let previous = self.threshold();
        let next = compute_tenuring_threshold(ages, desired, max);
        self.threshold.store(next, Ordering::Relaxed);
        self.census[0].fetch_add(1, Ordering::Relaxed);
        let slot = match next.cmp(&previous) {
            std::cmp::Ordering::Greater => 1,
            std::cmp::Ordering::Less => 2,
            std::cmp::Ordering::Equal => 3,
        };
        self.census[slot].fetch_add(1, Ordering::Relaxed);
        TenuringDecision {
            desired_bytes: desired,
            previous,
            next,
            max,
            adaptive: true,
        }
    }

    /// The decision record for a heap whose threshold is FIXED (adaptive
    /// tenuring off, `-XX:+PrintTenuringDistribution` on): nothing changes,
    /// the print shows what the table would have asked for.
    pub fn fixed_decision(
        &self,
        eden_bytes: usize,
        fixed_threshold: u8,
    ) -> TenuringDecision {
        TenuringDecision {
            desired_bytes: decision_desired_bytes(
                eden_bytes,
                self.target_survivor_ratio.load(Ordering::Relaxed),
                self.survivor_ratio(),
            ),
            previous: fixed_threshold,
            next: fixed_threshold,
            max: fixed_threshold,
            adaptive: false,
        }
    }

    /// `[adapted, raised, lowered, unchanged]`.
    pub fn census(&self) -> [u64; 4] {
        [
            self.census[0].load(Ordering::Relaxed),
            self.census[1].load(Ordering::Relaxed),
            self.census[2].load(Ordering::Relaxed),
            self.census[3].load(Ordering::Relaxed),
        ]
    }
}

// ---------------------------------------------------------------------------
// gcd d2/g (2026-09-27) — precise-root promotion
// (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, default ON since 2026-10-13, `=0` kills it)
// ---------------------------------------------------------------------------
//
// `gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-FIXED-20261013.md`.
//
// The non-moving young sweep's selective promotion pins EVERY root value that
// lands in young (`sweep_young_non_moving`, pin set (1)): a root word may be a
// conservative one that cannot be rewritten, and `collect_roots` returns one
// flat list, so the sweep cannot tell a static's value from a stack word. So
// an object whose DIRECT holder is a static field, the interned-string pool,
// the class-mirror table or the JNI global table never leaves young for as
// long as the JIT keeps young collections on the non-moving sweep.
//
// Those four tables are PRECISE and are rewritten after every collection that
// moves (`memory::gc::update_all_roots`, through the pointer map the sweep
// returns — the same map every moving young cycle and every evacuation of an
// interior object already feed). The VM's root scan (`memory::roots`) counts,
// per address, how many of its root entries came from those four tables and
// publishes the counts here, on the scanning thread. The sweep then leaves a
// young root value out of the pin set only when EVERY occurrence of it in the
// root list it was handed is one of those precise occurrences
// ([`precise_only_root_values`]). Anything else that also names the address
// — a conservative frame word, a peer's deposited snapshot, a frozen peer's
// register image, a movable JIT slot, a JNI local — adds an occurrence the
// scan did not count, and the value stays pinned exactly as before. Pins that
// never enter the root list (finalizer values, card-scan gap words, the late
// conservative bases, interior words resolved through the anchor oracle) are
// inserted by the sweep independently and still win, and a band word no
// channel can rewrite vetoes the exemption (`is_unrewritable_jit_root`).
//
// Per THREAD and TAKE-ONCE: the scan clears it first and publishes it after
// its step 9; the sweep takes it. A sweep with no published table (the flag
// off, a moving cycle in between, another gatherer) exempts nothing, which is
// the legacy behaviour.

/// The switch (declared in `cratonvm_types::flag_groups`, token
/// `gen-precise-root-promote`).
pub const PRECISE_ROOT_PROMOTE_FLAG: &str = "CRATONVM_GEN_PRECISE_ROOT_PROMOTE";

/// Is precise-root promotion engaged? Read by the VM's root scan, which is
/// the only publisher; the sweep consumes whatever was published.
pub fn precise_root_promote_enabled() -> bool {
    // Default ON since the gci orchestrator, 2026-10-13 (vi6-pn C10 PASS: sound, engaged, 0 findings
    // attributed to PRP; javac round-3 cost unchanged). `=0` kills it.
    cratonvm_types::flags::runtime_flag_default_on(PRECISE_ROOT_PROMOTE_FLAG)
}

thread_local! {
    /// `address -> number of root entries` the last root scan on this thread
    /// took from a precise, post-GC-rewritten table. See the section comment.
    static PRECISE_ROOT_VALUES: std::cell::RefCell<Option<rustc_hash::FxHashMap<usize, u32>>> =
        const { std::cell::RefCell::new(None) };
    /// gcd d5/r: the FINAL length of the root list the scan that published
    /// [`PRECISE_ROOT_VALUES`] returned ([`seal_precise_root_values`]), or
    /// `None` while that scan has not finished. See
    /// [`take_sealed_precise_root_values`].
    static PRECISE_ROOT_SEALED_LEN: std::cell::Cell<Option<usize>> =
        const { std::cell::Cell::new(None) };
}

/// Sweeps that consulted a published table, young values they exempted from
/// the pin set, young precise values they kept pinned because something
/// else named them too, and (gcd d5/r) published tables a sweep REFUSED
/// because they did not describe its root list (unsealed, or sealed longer
/// than the list). Diagnostics only (`[GC] precise_root_promote:`).
/// gci i6/pn: slot 4 counts values the count rule would have exempted that a
/// published conservative JIT pin vetoed instead
/// ([`precise_only_root_values_vetoing_conservative`]); expected 0 (the
/// count rule already sees every conservative word that entered `roots`).
static PRECISE_ROOT_CENSUS: [AtomicU64; 5] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// gci i13/cm item 6 (`gci-i13cm-generational-concurrent-cycle-under-the-user-flips-review-findings-FIXED-20261016`)
/// — `CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE` (default on, `runtime_flag_default_on`;
/// token `gen-conc-pause-no-precise-root-table`): a root scan taken inside
/// [`with_no_young_sweep_following`] (the Generational concurrent cycle's
/// initial-mark and remark pauses) builds no precise-root table. No young
/// sweep follows those pauses, so the table was never consumed (the driver
/// clears it right after the scan) and its counting -- one hash insert per
/// static, interned string, class mirror and JNI global -- was pure pause cost,
/// twice per cycle. Off: the scan counts and publishes as before.
pub const CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG: &str =
    "CRATONVM_GEN_CONC_PAUSE_NO_PRECISE_ROOT_TABLE";

thread_local! {
    /// gci i13/cm item 6: inside [`with_no_young_sweep_following`] on this
    /// thread. Scoped and restored, never left set.
    static NO_YOUNG_SWEEP_FOLLOWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// gci i13/cm item 6: run `f` (a root scan) as one that NO young sweep will
/// consume -- a concurrent-cycle pause. Nests; the previous state is restored
/// on every exit, an unwind included.
pub fn with_no_young_sweep_following<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            let _ = NO_YOUNG_SWEEP_FOLLOWS.try_with(|c| c.set(self.0));
        }
    }
    let previous = NO_YOUNG_SWEEP_FOLLOWS
        .try_with(|c| c.replace(true))
        .unwrap_or(false);
    let _reset = Reset(previous);
    f()
}

/// gci i13/cm item 6: may the root scan running on this thread skip the
/// precise-root table? Only inside [`with_no_young_sweep_following`] AND under
/// [`CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG`]; one thread-local load otherwise.
/// Read by the VM's `precise_root_promote_licensed`.
pub fn no_young_sweep_follows() -> bool {
    NO_YOUNG_SWEEP_FOLLOWS.try_with(|c| c.get()).unwrap_or(false)
        && cratonvm_types::flags::runtime_flag_on(CONC_PAUSE_NO_PRECISE_ROOT_TABLE_FLAG)
}

/// gcj j3/rf3 — is the root scan running on this thread inside
/// [`with_no_young_sweep_following`] (a Generational concurrent cycle's
/// initial-mark or remark pause), whatever the switch above reads? Read by the
/// String table's `pool_left_out_now` under
/// `CRATONVM_GEN_WEAK_STRING_TABLE_YOUNG_IN_CYCLE`: those two scans keep the
/// pool strong. One thread-local load.
pub fn in_no_young_sweep_scope() -> bool {
    NO_YOUNG_SWEEP_FOLLOWS.try_with(|c| c.get()).unwrap_or(false)
}

fn set_precise_root_sealed_len(len: Option<usize>) {
    let _ = PRECISE_ROOT_SEALED_LEN.try_with(|c| c.set(len));
}

/// Forget whatever the previous root scan on this thread published. Called at
/// the top of every `collect_roots`.
pub fn clear_precise_root_values() {
    let _ = PRECISE_ROOT_VALUES.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = None;
        }
    });
    set_precise_root_sealed_len(None);
}

/// Publish this scan's precise-table occurrence counts for the young sweep
/// that follows on this thread. Unsealed until the scan ends
/// ([`seal_precise_root_values`]).
pub fn publish_precise_root_values(counts: rustc_hash::FxHashMap<usize, u32>) {
    let _ = PRECISE_ROOT_VALUES.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = Some(counts);
        }
    });
    set_precise_root_sealed_len(None);
}

/// gcd d5/r (flip-gate item 5 of
/// `gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-FIXED-20261013.md`):
/// the scan that published this thread's table has returned a root list of
/// `final_len` entries. Called once, at the end of `collect_roots`. A no-op
/// when nothing was published.
pub fn seal_precise_root_values(final_len: usize) {
    let published = PRECISE_ROOT_VALUES
        .try_with(|c| c.try_borrow().map(|v| v.is_some()).unwrap_or(false))
        .unwrap_or(false);
    if published {
        set_precise_root_sealed_len(Some(final_len));
    }
}

/// Take (and clear) this thread's published counts. The young sweep is the
/// consumer; public so the VM's publisher can be tested against it.
pub fn take_precise_root_values() -> Option<rustc_hash::FxHashMap<usize, u32>> {
    set_precise_root_sealed_len(None);
    PRECISE_ROOT_VALUES
        .try_with(|c| c.try_borrow_mut().ok().and_then(|mut v| v.take()))
        .ok()
        .flatten()
}

/// gcd d5/r: [`take_precise_root_values`] for a sweep handed a root list of
/// `roots_len` entries, which takes the table only when it describes THAT
/// list: the scan that published it finished (sealed) and the list is at
/// least as long as the scan returned (the gatherer appends its frozen peers'
/// words after `collect_roots`; it never drops any). A table published by a
/// scan no sweep consumed -- a heap dump, a concurrent pause, a lost STW race
/// -- and then met by a sweep whose roots came from another gatherer is
/// refused (counted, `prp_stale_refused`), so nothing is exempt and every
/// root value stays pinned: the legacy behaviour. Taken either way.
pub fn take_sealed_precise_root_values(
    roots_len: usize,
) -> Option<rustc_hash::FxHashMap<usize, u32>> {
    let sealed = PRECISE_ROOT_SEALED_LEN
        .try_with(|c| c.take())
        .ok()
        .flatten();
    let table = take_precise_root_values()?;
    match sealed {
        Some(n) if roots_len >= n => Some(table),
        _ => {
            let n = PRECISE_ROOT_CENSUS[3].fetch_add(1, Ordering::Relaxed);
            // A SEALED table longer than the list means a gatherer dropped
            // entries after the scan: a finding (the counting rule assumes
            // nothing is ever removed). Said, rate-limited; never asserted,
            // because refusing is already the safe answer.
            if sealed.is_some() && n < 4 {
                tracing::warn!(
                    target: "cratonvm::gc",
                    sealed_len = ?sealed,
                    roots_len,
                    "precise-root promotion: the published table is longer than the root list \
                     the young sweep was handed; nothing is exempt this cycle",
                );
            }
            None
        }
    }
}

/// The young addresses of `precise` ALL of whose occurrences in `roots` are
/// the precise ones the scan counted, and that `vetoed` does not refuse. An
/// address with more occurrences in `roots` than `precise` counts (another
/// source also named it) — or fewer (the list was edited after the scan, so
/// the counts no longer describe it) — is not returned.
pub fn precise_only_root_values(
    roots: &[cratonvm_types::ObjectRef],
    precise: &rustc_hash::FxHashMap<usize, u32>,
    is_young: impl Fn(usize) -> bool,
    vetoed: impl Fn(usize) -> bool,
) -> rustc_hash::FxHashSet<usize> {
    let mut seen: rustc_hash::FxHashMap<usize, u32> = rustc_hash::FxHashMap::default();
    for r in roots {
        let a = r.as_ptr() as usize;
        if precise.contains_key(&a) {
            *seen.entry(a).or_insert(0) += 1;
        }
    }
    let mut exempt = rustc_hash::FxHashSet::default();
    let mut shared = 0u64;
    for (&a, &n) in precise {
        if !is_young(a) {
            continue;
        }
        if seen.get(&a) == Some(&n) && !vetoed(a) {
            exempt.insert(a);
        } else {
            shared += 1;
        }
    }
    PRECISE_ROOT_CENSUS[0].fetch_add(1, Ordering::Relaxed);
    PRECISE_ROOT_CENSUS[1].fetch_add(exempt.len() as u64, Ordering::Relaxed);
    PRECISE_ROOT_CENSUS[2].fetch_add(shared, Ordering::Relaxed);
    exempt
}

/// `[GC] precise_root_promote:` — `None` unless the flag is on.
pub fn precise_root_promote_census_line() -> Option<String> {
    if !precise_root_promote_enabled() {
        return None;
    }
    Some(format!(
        "[GC] precise_root_promote: prp_sweeps={} prp_exempt={} prp_kept_pinned={} prp_stale_refused={} prp_conservative_vetoed={}",
        PRECISE_ROOT_CENSUS[0].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[1].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[2].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[3].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[4].load(Ordering::Relaxed),
    ))
}

/// gci i6/pn (conc10): [`precise_only_root_values`] with a second veto -- a
/// value any CONSERVATIVE JIT pin names (`conservative`, the young sweep's
/// `gc_quiescence::pinned_jit_roots_snapshot()`: every thread's published
/// spill-region / band / register-image words, its deposits' and this pause's
/// helper-window pins) is never exempt, whatever the precise count says.
///
/// The count rule already refuses a value a conservative word names, as long
/// as that word reached the sweep's `roots` slice (one more occurrence than the
/// scan counted). This makes the property structural rather than a corollary
/// of the slice's composition: a pin published WITHOUT entering `roots` (a
/// helper-window pin, a peer entry, a future channel) vetoes too. A veto that
/// fires counts `prp_conservative_vetoed` -- non-zero names exactly such a
/// pin, and is a finding about the slice, not about this rule. The value then
/// stays pinned (the legacy behaviour), so the veto can only over-retain.
pub fn precise_only_root_values_vetoing_conservative(
    roots: &[cratonvm_types::ObjectRef],
    precise: &rustc_hash::FxHashMap<usize, u32>,
    is_young: impl Fn(usize) -> bool,
    unrewritable: impl Fn(usize) -> bool,
    conservative: &rustc_hash::FxHashSet<usize>,
) -> rustc_hash::FxHashSet<usize> {
    let vetoed_by_pin = std::cell::Cell::new(0u64);
    let exempt = precise_only_root_values(roots, precise, is_young, |a| {
        if unrewritable(a) {
            return true;
        }
        let pinned = conservative.contains(&a);
        if pinned {
            vetoed_by_pin.set(vetoed_by_pin.get() + 1);
        }
        pinned
    });
    let n = vetoed_by_pin.get();
    if n > 0 {
        PRECISE_ROOT_CENSUS[4].fetch_add(n, Ordering::Relaxed);
        // The vetoed values were counted `prp_kept_pinned` by the rule above,
        // which is what they are.
    }
    exempt
}

// ---------------------------------------------------------------------------
// gci i6/pn (conc10): WHY a young root value moved, for the root-remap audit
// ---------------------------------------------------------------------------
//
// `CRATONVM_DBG_ROOT_REMAP_AUDIT=1` re-runs the root scan after a collection
// and reports every scanned root still naming an address the collection
// vacated. On the non-moving sweep a ROOT value moves only through one of two
// exemptions from the pin set (`sweep_young_non_moving`, pin set (1)): the
// movable-JIT-root claim (`honour_movable`) or precise-root promotion. The
// audit line could not say which, so vi4-pn C10's 14 findings on
// `pn4_jt1_prp_1` were charged to precise-root promotion by elimination --
// and the same finding (`scan_section=14`, `<not-attributed>`) is in
// `gci-runs/i3/i2-fr/fr2_pr_aud_one` WITHOUT the switch.
//
// The sweep records the two exempted sets here, per thread, only while the
// audit is armed; the audit takes them (once) BEFORE its re-scan and labels
// each finding. A finding on a cycle with no record is a moving (Cheney)
// cycle or a non-generational path: `unrecorded`.

/// The two ways a young ROOT value left the non-moving sweep's pin set.
#[derive(Debug, Default)]
pub struct YoungRootExemptions {
    /// Exempt by precise-root promotion (`precise_only_root_values`).
    pub prp: rustc_hash::FxHashSet<usize>,
    /// Exempt by a movable-JIT-root claim on a proven cycle.
    pub movable: rustc_hash::FxHashSet<usize>,
}

impl YoungRootExemptions {
    /// `prp`, `movable`, or `pinned-or-not-a-root` for the sweep that
    /// recorded this.
    pub fn label(&self, addr: usize) -> &'static str {
        if self.prp.contains(&addr) {
            "prp"
        } else if self.movable.contains(&addr) {
            "movable"
        } else {
            "pinned-or-not-a-root"
        }
    }
}

thread_local! {
    static YOUNG_ROOT_EXEMPTIONS: std::cell::RefCell<Option<YoungRootExemptions>> =
        const { std::cell::RefCell::new(None) };
}

/// Should the young sweep record [`YoungRootExemptions`]? Only while the
/// root-remap audit, their one consumer, is armed.
pub fn young_root_exemptions_wanted() -> bool {
    false}

/// Record this sweep's exemptions on the collecting thread (replacing any
/// record no audit took).
pub fn record_young_root_exemptions(record: YoungRootExemptions) {
    let _ = YOUNG_ROOT_EXEMPTIONS.try_with(|c| {
        if let Ok(mut v) = c.try_borrow_mut() {
            *v = Some(record);
        }
    });
}

/// Take (and clear) the calling thread's last recorded exemptions. `None`:
/// no non-moving sweep recorded since the last take (a moving cycle, or the
/// audit unarmed).
pub fn take_young_root_exemptions() -> Option<YoungRootExemptions> {
    YOUNG_ROOT_EXEMPTIONS
        .try_with(|c| c.try_borrow_mut().ok().and_then(|mut v| v.take()))
        .ok()
        .flatten()
}

/// The audit's label for `addr` from a taken record: `unrecorded` when the
/// cycle left none.
pub fn young_root_exemption_label(record: Option<&YoungRootExemptions>, addr: usize) -> &'static str {
    match record {
        Some(r) => r.label(addr),
        None => "unrecorded",
    }
}

#[cfg(test)]
mod tests;
