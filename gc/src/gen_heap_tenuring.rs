// SPDX-License-Identifier: Apache-2.0
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
//! `docs/known-issues/gc/gengc-r4w4-young4-fixed-tenuring-threshold-and-all-or-nothing-promote-pressure-20260924.md`.
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
//! eighth of eden. Eden's equivalent here is the young TRIGGER (the from-space
//! occupancy that starts a young collection, `young_gc_threshold`), so
//!
//! ```text
//! survivor_capacity = young_trigger_bytes / SurvivorRatio   (SurvivorRatio = 8)
//! desired           = survivor_capacity * TargetSurvivorRatio / 100
//! ```
//!
//! which at the default 64 MiB semispace (a 32 MiB trigger) is a 2 MiB target.
//! Tying it to the trigger rather than the capacity has a second benefit: when
//! the pause-goal loop (`adapt_young_trigger_to_pause`) halves the trigger,
//! the tenuring target halves with it, which is the coupling the gap page
//! asked for between the two levers that act on survivor volume.
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
//! # Opt-in
//!
//! Default behaviour is unchanged. Adaptive tenuring is engaged by
//! `CRATONVM_GC_ADAPTIVE_TENURING` (HotSpot's defaults: `MaxTenuringThreshold
//! = 15`, `TargetSurvivorRatio = 50`), or by any of `-XX:MaxTenuringThreshold`,
//! `-XX:InitialTenuringThreshold`, `-XX:TargetSurvivorRatio` on the command line
//! (an operator who typed a HotSpot tenuring flag asked for HotSpot's
//! adaptive semantics). The probe that would justify flipping the default is
//! `tools/bench/GenR4W6TenuringProbe.java` (old-gen growth of a medium-lived
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
pub(crate) const AGE_TABLE_LEN: usize = MAX_GC_AGE as usize + 1;

/// HotSpot's default `-XX:MaxTenuringThreshold`.
pub const DEFAULT_MAX_TENURING_THRESHOLD: u8 = 15;

/// HotSpot's default `-XX:TargetSurvivorRatio`, in percent.
pub const DEFAULT_TARGET_SURVIVOR_RATIO: u8 = 50;

/// HotSpot's default `-XX:SurvivorRatio`: eden is this many times one survivor
/// space. See the module doc for why eden's equivalent is the young trigger.
pub(crate) const SURVIVOR_RATIO: u64 = 8;

/// The HotSpot tenuring flags as the launcher parsed them, carried on the VM's
/// config to the heap (`VmHeap::set_tenuring_config`). `None` = not given.
///
/// Values are in HotSpot's units: a threshold `T` tenures an object whose age
/// at the start of a young collection is `>= T`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TenuringConfig {
    /// `-XX:MaxTenuringThreshold=<0..=15>`.
    pub max_tenuring_threshold: Option<u8>,
    /// `-XX:InitialTenuringThreshold=<0..=15>`. HotSpot's Serial and G1 start
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
pub(crate) struct TenuringDecision {
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
/// (and so at `MAX_GC_AGE`, since the loop cannot pass `AGE_TABLE_LEN`).
///
/// Never 0: a table whose age-1 bucket alone exceeds the target yields 1, as
/// in HotSpot. `T = 0` ("tenure everything") is reachable only through
/// `max_threshold = 0` or the severe promote-on-pressure arm.
pub(crate) fn compute_tenuring_threshold(
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
    (age as u8).min(max_threshold).min(MAX_GC_AGE)
}

/// HotSpot's desired survivor size, from the young trigger (eden's
/// equivalent; see the module doc) and `TargetSurvivorRatio`.
pub(crate) fn desired_survivor_bytes(young_trigger_bytes: usize, target_survivor_ratio: u8) -> u64 {
    (young_trigger_bytes as u64 / SURVIVOR_RATIO).saturating_mul(u64::from(target_survivor_ratio))
        / 100
}

/// The age table as HotSpot prints it under `-XX:+PrintTenuringDistribution`
/// (`-Xlog:gc+age=trace`), one line per non-empty age, each prefixed
/// `[GC] tenuring:` so a log grep finds the block.
pub(crate) fn distribution_lines(d: &TenuringDecision, ages: &[u64; AGE_TABLE_LEN]) -> Vec<String> {
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
        out.push(format!("[GC] tenuring: - age {age:>3}: {bytes:>10} bytes, {total:>10} total"));
    }
    out
}

/// The per-heap tenuring state. Every field is written only by a young
/// collection (stop-the-world, one driver) or by the one-time configuration
/// call before the first collection, so `Relaxed` is enough throughout: the
/// readers are the same collector thread, or diagnostics.
pub(crate) struct TenuringState {
    /// `-XX:` tenuring values were given for this heap (see
    /// [`TenuringConfig::requests_adaptive`]).
    configured_adaptive: AtomicBool,
    /// `-XX:+PrintTenuringDistribution`.
    print: AtomicBool,
    /// `MaxTenuringThreshold`, HotSpot units.
    max_threshold: AtomicU8,
    /// `TargetSurvivorRatio`, percent.
    target_survivor_ratio: AtomicU8,
    /// The threshold the NEXT young collection uses when adaptive tenuring is
    /// engaged, HotSpot units.
    threshold: AtomicU8,
    /// `[adapted, raised, lowered, unchanged]` over the heap's life.
    census: [AtomicU64; 4],
}

impl TenuringState {
    /// HotSpot's defaults; adaptive tenuring not configured.
    pub(crate) const fn new() -> Self {
        Self {
            configured_adaptive: AtomicBool::new(false),
            print: AtomicBool::new(false),
            max_threshold: AtomicU8::new(DEFAULT_MAX_TENURING_THRESHOLD),
            target_survivor_ratio: AtomicU8::new(DEFAULT_TARGET_SURVIVOR_RATIO),
            // HotSpot's Serial and G1 start at `MaxTenuringThreshold`.
            threshold: AtomicU8::new(DEFAULT_MAX_TENURING_THRESHOLD),
            census: [
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
                AtomicU64::new(0),
            ],
        }
    }

    /// Apply the launcher's `-XX:` tenuring flags. Called once, before the
    /// first collection; values are clamped to HotSpot's ranges (the launcher
    /// already refused out-of-range text).
    pub(crate) fn configure(&self, cfg: &TenuringConfig) {
        let max = cfg
            .max_tenuring_threshold
            .unwrap_or(DEFAULT_MAX_TENURING_THRESHOLD)
            .min(MAX_GC_AGE);
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

    /// Is adaptive tenuring in force on this heap? `env_adaptive` is
    /// `CRATONVM_GC_ADAPTIVE_TENURING`, read by the caller.
    #[inline]
    pub(crate) fn engaged(&self, env_adaptive: bool) -> bool {
        env_adaptive || self.configured_adaptive.load(Ordering::Relaxed)
    }

    /// `-XX:+PrintTenuringDistribution`.
    #[inline]
    pub(crate) fn print_requested(&self) -> bool {
        self.print.load(Ordering::Relaxed)
    }

    /// The adaptive threshold, HotSpot units.
    #[inline]
    pub(crate) fn threshold(&self) -> u8 {
        self.threshold.load(Ordering::Relaxed)
    }

    /// `should_tenure`'s `promotion_age` for the next young collection:
    /// `threshold + 1` when adaptive (see the module doc for the unit
    /// conversion), else `fixed` (the historical `PROMOTION_AGE`).
    #[inline]
    pub(crate) fn promotion_age(&self, env_adaptive: bool, fixed: u8) -> u8 {
        if self.engaged(env_adaptive) {
            self.threshold().saturating_add(1)
        } else {
            fixed
        }
    }

    /// Recompute the threshold from this collection's age table. Returns the
    /// decision for the census / distribution print.
    pub(crate) fn adapt(
        &self,
        ages: &[u64; AGE_TABLE_LEN],
        young_trigger_bytes: usize,
    ) -> TenuringDecision {
        let max = self.max_threshold.load(Ordering::Relaxed);
        let desired = desired_survivor_bytes(
            young_trigger_bytes,
            self.target_survivor_ratio.load(Ordering::Relaxed),
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
    pub(crate) fn fixed_decision(&self, young_trigger_bytes: usize, fixed_threshold: u8) -> TenuringDecision {
        TenuringDecision {
            desired_bytes: desired_survivor_bytes(
                young_trigger_bytes,
                self.target_survivor_ratio.load(Ordering::Relaxed),
            ),
            previous: fixed_threshold,
            next: fixed_threshold,
            max: fixed_threshold,
            adaptive: false,
        }
    }

    /// `[adapted, raised, lowered, unchanged]`.
    pub(crate) fn census(&self) -> [u64; 4] {
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
// (`CRATONVM_GEN_PRECISE_ROOT_PROMOTE`, opt-in)
// ---------------------------------------------------------------------------
//
// `gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927.md`.
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
    cratonvm_types::flags::runtime_flag_on(PRECISE_ROOT_PROMOTE_FLAG)
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
static PRECISE_ROOT_CENSUS: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

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
/// `gengc-r5w6-conc10-selective-promotion-pins-precise-root-values-forever-20260927.md`):
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
pub fn take_sealed_precise_root_values(roots_len: usize) -> Option<rustc_hash::FxHashMap<usize, u32>> {
    let sealed = PRECISE_ROOT_SEALED_LEN.try_with(|c| c.take()).ok().flatten();
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
pub(crate) fn precise_only_root_values(
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
        "[GC] precise_root_promote: prp_sweeps={} prp_exempt={} prp_kept_pinned={} prp_stale_refused={}",
        PRECISE_ROOT_CENSUS[0].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[1].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[2].load(Ordering::Relaxed),
        PRECISE_ROOT_CENSUS[3].load(Ordering::Relaxed),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(usize, u64)]) -> [u64; AGE_TABLE_LEN] {
        let mut t = [0u64; AGE_TABLE_LEN];
        for &(age, bytes) in entries {
            t[age] = bytes;
        }
        t
    }

    /// HotSpot's rule, case by case: the first age whose CUMULATIVE bytes
    /// exceed the target; the ceiling when nothing does; never below 1.
    #[test]
    fn the_threshold_is_hotspots_cumulative_rule() {
        // Nothing survives: the ceiling.
        assert_eq!(compute_tenuring_threshold(&table(&[]), 1000, 15), 15);
        // Everything fits: the ceiling.
        assert_eq!(
            compute_tenuring_threshold(&table(&[(1, 100), (2, 100), (3, 100)]), 1000, 15),
            15
        );
        // Age 1 alone overflows: 1, not 0.
        assert_eq!(compute_tenuring_threshold(&table(&[(1, 5000)]), 1000, 15), 1);
        // Cumulative crosses at age 3 (400 + 400 + 400 > 1000).
        assert_eq!(
            compute_tenuring_threshold(&table(&[(1, 400), (2, 400), (3, 400)]), 1000, 15),
            3
        );
        // Exactly AT the target does not cross (HotSpot's `>`).
        assert_eq!(
            compute_tenuring_threshold(&table(&[(1, 500), (2, 500)]), 1000, 15),
            15
        );
        // The ceiling caps a late crossing.
        assert_eq!(
            compute_tenuring_threshold(&table(&[(1, 400), (2, 400), (3, 400)]), 1000, 2),
            2
        );
        // `MaxTenuringThreshold=0` is "always tenure".
        assert_eq!(compute_tenuring_threshold(&table(&[(1, 1)]), 1000, 0), 0);
        // Bucket 0 is ignored (never written by a collection).
        assert_eq!(compute_tenuring_threshold(&table(&[(0, u64::MAX)]), 0, 15), 15);
    }

    /// A saturated table cannot overflow the running sum or the cast.
    #[test]
    fn a_saturated_age_table_is_total() {
        let t = [u64::MAX; AGE_TABLE_LEN];
        assert_eq!(compute_tenuring_threshold(&t, u64::MAX - 1, 15), 1);
        assert_eq!(compute_tenuring_threshold(&t, u64::MAX, 15), 15);
    }

    /// The target is one eighth of the trigger, times the ratio.
    #[test]
    fn the_desired_survivor_size_follows_the_young_trigger() {
        let mib = 1024 * 1024;
        assert_eq!(desired_survivor_bytes(32 * mib, 50), 2 * mib as u64);
        assert_eq!(desired_survivor_bytes(32 * mib, 100), 4 * mib as u64);
        assert_eq!(desired_survivor_bytes(32 * mib, 0), 0);
        // Halving the trigger (the pause-goal loop) halves the target.
        assert_eq!(desired_survivor_bytes(16 * mib, 50), mib as u64);
    }

    /// Default state: not engaged, historical promotion age; the env lever or
    /// any `-XX:` tenuring value engages it; the unit conversion is `T + 1`.
    #[test]
    fn engagement_and_the_unit_conversion() {
        let s = TenuringState::new();
        assert!(!s.engaged(false));
        assert_eq!(s.promotion_age(false, 3), 3, "default: the fixed age");
        assert!(s.engaged(true));
        assert_eq!(
            s.promotion_age(true, 3),
            DEFAULT_MAX_TENURING_THRESHOLD + 1,
            "env lever alone: HotSpot's initial threshold (= Max)"
        );

        s.configure(&TenuringConfig {
            max_tenuring_threshold: Some(6),
            ..TenuringConfig::default()
        });
        assert!(s.engaged(false), "a typed -XX value engages adaptive tenuring");
        assert_eq!(s.threshold(), 6, "initial defaults to the max");
        assert_eq!(s.promotion_age(false, 3), 7);

        s.configure(&TenuringConfig {
            max_tenuring_threshold: Some(4),
            initial_tenuring_threshold: Some(9),
            ..TenuringConfig::default()
        });
        assert_eq!(s.threshold(), 4, "initial is clamped to the max");

        s.configure(&TenuringConfig {
            print_tenuring_distribution: true,
            ..TenuringConfig::default()
        });
        assert!(!s.engaged(false), "the print flag alone does not engage");
        assert!(s.print_requested());
    }

    /// `adapt` stores the new threshold, reports it, and counts the move.
    #[test]
    fn adapt_moves_the_threshold_both_ways_and_counts_it() {
        let s = TenuringState::new();
        s.configure(&TenuringConfig {
            max_tenuring_threshold: Some(15),
            ..TenuringConfig::default()
        });
        let mib = 1024 * 1024;
        // 32 MiB trigger -> 2 MiB target. 3 MiB at age 1 -> threshold 1.
        let d = s.adapt(&table(&[(1, 3 * mib)]), 32 * mib as usize);
        assert_eq!((d.previous, d.next, d.max), (15, 1, 15));
        assert_eq!(d.desired_bytes, 2 * mib);
        assert_eq!(s.promotion_age(false, 3), 2);
        // Survivors fall to a trickle -> back to the ceiling.
        let d = s.adapt(&table(&[(1, 1024), (2, 1024)]), 32 * mib as usize);
        assert_eq!((d.previous, d.next), (1, 15));
        // Unchanged.
        let _ = s.adapt(&table(&[]), 32 * mib as usize);
        assert_eq!(s.census(), [3, 1, 1, 1]);
    }

    /// The distribution print carries HotSpot's wording, skips empty ages and
    /// keeps a running total.
    #[test]
    fn the_distribution_lines_read_like_hotspots() {
        let d = TenuringDecision {
            desired_bytes: 2048,
            previous: 15,
            next: 2,
            max: 15,
            adaptive: true,
        };
        let lines = distribution_lines(&d, &table(&[(1, 1500), (3, 700)]));
        assert_eq!(lines.len(), 3);
        assert!(lines[0].starts_with(
            "[GC] tenuring: Desired survivor size 2048 bytes, new threshold 2 (max threshold 15)"
        ));
        assert!(lines[0].contains("mode=adaptive"));
        assert_eq!(
            lines[1],
            "[GC] tenuring: - age   1:       1500 bytes,       1500 total"
        );
        assert_eq!(
            lines[2],
            "[GC] tenuring: - age   3:        700 bytes,       2200 total"
        );
    }

    /// HotSpot's ranges for the value flags.
    #[test]
    fn the_value_flags_take_hotspots_ranges() {
        assert_eq!(TenuringConfig::parse_value("-XX:MaxTenuringThreshold", "15", 15), Ok(15));
        assert_eq!(TenuringConfig::parse_value("-XX:MaxTenuringThreshold", " 0 ", 15), Ok(0));
        assert!(TenuringConfig::parse_value("-XX:MaxTenuringThreshold", "16", 15).is_err());
        assert!(TenuringConfig::parse_value("-XX:TargetSurvivorRatio", "abc", 100).is_err());
        assert_eq!(TenuringConfig::parse_value("-XX:TargetSurvivorRatio", "100", 100), Ok(100));
        let c = TenuringConfig {
            target_survivor_ratio: Some(90),
            print_tenuring_distribution: true,
            ..TenuringConfig::default()
        };
        assert!(c.requests_adaptive());
        assert!(!c.is_default());
        assert_eq!(
            c.given_flag_names(),
            vec!["-XX:TargetSurvivorRatio", "-XX:+PrintTenuringDistribution"]
        );
        assert!(TenuringConfig::default().is_default());
    }

    /// gcd d2/g: a young value is exempt from the pin set only when every
    /// occurrence in the root list is a precise one the scan counted, it is
    /// not vetoed, and it is young.
    #[test]
    fn gcd_d2g_only_values_named_by_precise_tables_alone_are_exempt() {
        let at = |a: usize| {
            // SAFETY: never dereferenced; only the address is compared.
            unsafe { cratonvm_types::ObjectRef::from_raw(a as *mut u8) }
        };
        let (only, twice, shared, vetoed, old) =
            (0x10_0000usize, 0x10_0040usize, 0x10_0080usize, 0x10_00c0usize, 0x90_0000usize);
        // `twice`: two static slots name it (both counted); `shared`: one
        // static and one other root (a conservative word, say).
        let roots = vec![
            at(only),
            at(twice),
            at(twice),
            at(shared),
            at(shared),
            at(vetoed),
            at(old),
        ];
        let precise: rustc_hash::FxHashMap<usize, u32> =
            [(only, 1), (twice, 2), (shared, 1), (vetoed, 1), (old, 1)]
                .into_iter()
                .collect();
        let exempt = precise_only_root_values(
            &roots,
            &precise,
            |a| a < 0x80_0000,
            |a| a == vetoed,
        );
        let mut got: Vec<usize> = exempt.into_iter().collect();
        got.sort_unstable();
        assert_eq!(got, vec![only, twice]);

        // A count the list no longer matches (fewer occurrences) exempts
        // nothing for that address.
        let short = vec![at(twice)];
        assert!(precise_only_root_values(&short, &precise, |_| true, |_| false)
            .iter()
            .all(|&a| a != twice));
    }

    /// gcd d2/g: the published table is per thread and taken once.
    #[test]
    fn gcd_d2g_precise_root_values_are_taken_once() {
        clear_precise_root_values();
        assert!(take_precise_root_values().is_none());
        publish_precise_root_values([(0x10_0000usize, 1u32)].into_iter().collect());
        let taken = take_precise_root_values().expect("published");
        assert_eq!(taken.get(&0x10_0000), Some(&1));
        assert!(take_precise_root_values().is_none(), "take-once");
        publish_precise_root_values([(0x10_0000usize, 1u32)].into_iter().collect());
        clear_precise_root_values();
        assert!(take_precise_root_values().is_none(), "the scan's clear");
    }

    /// gcd d5/r: the sealed take hands the table only to a sweep whose root
    /// list is at least as long as the scan that published it returned, and
    /// only once that scan finished (sealed). Every refusal still consumes
    /// the table.
    #[test]
    fn gcd_d5r_the_sealed_take_refuses_a_table_that_does_not_describe_the_list() {
        let counts = || -> rustc_hash::FxHashMap<usize, u32> {
            [(0x10_0000usize, 1u32)].into_iter().collect()
        };
        clear_precise_root_values();
        // Nothing published: nothing taken, nothing sealed.
        seal_precise_root_values(4);
        assert!(take_sealed_precise_root_values(4).is_none());
        // Sealed at 4: a list of 4 or more takes it (the gatherer appends).
        publish_precise_root_values(counts());
        seal_precise_root_values(4);
        assert!(take_sealed_precise_root_values(6).is_some());
        assert!(take_sealed_precise_root_values(6).is_none(), "take-once");
        // Unsealed (the scan never finished): refused, and consumed.
        publish_precise_root_values(counts());
        assert!(take_sealed_precise_root_values(6).is_none());
        assert!(take_precise_root_values().is_none(), "the refusal consumed it");
        // Sealed longer than the list: refused, and consumed.
        publish_precise_root_values(counts());
        seal_precise_root_values(8);
        assert!(take_sealed_precise_root_values(3).is_none());
        assert!(take_precise_root_values().is_none(), "the refusal consumed it");
        // A later publish starts unsealed again.
        publish_precise_root_values(counts());
        seal_precise_root_values(2);
        publish_precise_root_values(counts());
        assert!(take_sealed_precise_root_values(9).is_none(), "the re-publish unsealed it");
        clear_precise_root_values();
    }
}
