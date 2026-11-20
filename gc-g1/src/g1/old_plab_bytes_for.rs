#[allow(unused_imports)]
use super::*;

/// JIT round 23 wave 10, lane L9: the Old PLAB chunk for a pause of `workers`
/// workers on `region_size` regions -- about a region's share per worker,
/// never under the Survivor PLAB size, 8-aligned. HotSpot sizes its PLABs from
/// the last pauses' use (`PLABStats`); a share of the region is the static
/// form of the same goal: few refills, and one worker's run of promotions
/// (a chain the local stack keeps on that worker) contiguous in one chunk.
/// Pure.
#[inline]
pub fn old_plab_bytes_for(region_size: usize, workers: usize) -> usize {
    (region_size / workers.max(1)).max(SURVIVOR_PLAB_BYTES) & !7
}

/// The Old chunk a claim asks for: the pause's Old PLAB size, or the object
/// when it is larger, never past the region. Pure.
#[inline]
pub fn old_plab_chunk(need: usize, region_len: usize, plab_bytes: usize) -> usize {
    plab_bytes.max(need).min(region_len & !7)
}

/// JIT round 23 wave 10, lane L9: `CRATONVM_G1_OLD_PLAB` (OPT-IN, `=1`). See
/// `SharedEvac::claim_old_plab`. Read once per pause on the evacuation driver.
///
/// Opt-in on n23-w10a (`KeepLong 30 -Xmx1g`, `CRATONVM_GC_STATS=1`, 3 reps):
/// ON ops 14.4/12.0/12.0 M (`parallel=52/32/40 serial=594/463/475`,
/// `workers_last=9/8/9`) against 19.2/16.8/16.8 M at `=0` (`parallel=71/72/52
/// serial=667/694/631 workers_last=2`); keep-obj 82.2/81.4/74.0 against
/// 87.6/88.2/96.4 ns/op. The width went up and the row went down.
pub fn g1_old_plab_enabled() -> bool {
    false
}

/// Local-stack depth above which a worker spills its oldest half back to the
/// shared queue. See [`SharedEvac::run_worker`].
///
/// This is the load-balancing valve, and it has to be strictly greater than
/// [`EVAC_LOCAL_BATCH_MAX`] or a worker would spill the batch it just lifted
/// straight back and thrash the lock it was trying to avoid.
///
/// # What is compared against it, and what is not
///
/// Lane W7-W (2026-09-21). The compared quantity is `local.len()` AFTER a
/// parent's children are pushed — not the child list's own width. Because the
/// stack is popped one item at a time and extended a whole child list at a
/// time, its depth is exactly `discovered - processed`, which on a depth-first
/// walk is the number of not-yet-popped SIBLINGS AND UNCLES along the current
/// root path. Two different graphs therefore open this valve:
///
///   * one object with more than 256 CSet-bound references (`wide`), and
///   * a walk whose accumulated un-popped fan-out crosses 256 — which needs
///     no wide object at all, only depth times (branching − 1).
///
/// [`EvacShareCensus`] is the instrument that tells those two apart on a real
/// workload, and `g1_w7w_share_engagement.rs` is the falsifying fixture.
pub const EVAC_LOCAL_PUBLISH_HIGH: usize = 256;

/// Buckets in each [`EvacShareCensus`] histogram.
///
/// Bucket `k > 0` holds the values `2^(k-1) ..= 2^k - 1`; bucket 0 holds zero.
/// With twelve buckets the last is `>= 2048`, and — the reason for this exact
/// count — **bucket 9 is the first bucket entirely at or above
/// [`EVAC_LOCAL_PUBLISH_HIGH`]**: buckets 0..=8 are `0..=255`, bucket 9 is
/// `256..=511`. So "does this workload reach the line?" is
/// `hist[9..].iter().sum() > 0`, with no arithmetic and no boundary argument.
pub const EVAC_SHARE_BUCKETS: usize = 12;

/// The bucket [`EvacShareCensus`] files `v` in. See [`EVAC_SHARE_BUCKETS`].
#[inline]
pub fn evac_share_bucket(v: usize) -> usize {
    if v == 0 {
        0
    } else {
        ((usize::BITS - v.leading_zeros()) as usize).min(EVAC_SHARE_BUCKETS - 1)
    }
}

/// LANE W7-W (2026-09-21) — the distributions behind the load-balancing valve.
///
/// # The question this exists to answer
///
/// `w6p-work-sharing-engages-on-child-list-width-not-graph-width.md` §3 states
/// what it could not establish: *"Whether production Java graphs sit above or
/// below that line is unmeasured, and is the first thing a follow-up should
/// find out — it decides whether this is a latent property or a live cost."*
///
/// [`EvacWorkerCensus`] cannot answer it. It counts `spills`, i.e. how often
/// the valve OPENED, and a workload that never comes within 200 of the
/// threshold and a workload that reaches 255 on every single push both report
/// `spills=0`. Those are opposite findings — the first says the mechanism is
/// irrelevant to real graphs, the second says it is one field declaration away
/// from engaging — and no counter in the tree told them apart.
///
/// # Two histograms, because the two candidate mechanisms differ
///
/// W6-P's account is that the valve opens when ONE `process_object` pushes
/// more than [`EVAC_LOCAL_PUBLISH_HIGH`] children. That is sufficient but not
/// necessary: the local stack is popped ONE item at a time and extended a
/// whole child list at a time, so its depth is `discovered - processed`, which
/// a depth-first walk accumulates as `depth * (branching - 1)`. So:
///
/// * [`Self::child_hist`] is W6-P's quantity — children pushed per scan.
/// * [`Self::residue_hist`] is the quantity actually compared against the
///   constant — `local.len()` immediately after each push.
///
/// [`Self::spills_from_accumulation`] is the discriminator, and it is the
/// number to read first: a spill whose own child list was at or below the
/// threshold **cannot** be explained by child-list width. Non-zero on a real
/// workload falsifies the "child-list width" framing in production rather
/// than in a fixture.
///
/// # Reading it
///
/// Every field is a count taken on worker-local state and flushed once per
/// worker per pause; there is no clock in here, so the numbers are
/// load-independent in the sense W6-P §3 establishes for the census. All
/// counts are PROCESS-LIFETIME totals, so a test reads a DELTA.
///
/// `residue_max` is the single most informative number: it is the closest any
/// worker in the process ever came to the valve. `residue_max = 166` on a
/// workload says the valve is not marginal there, it is unreachable.
#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct EvacShareCensus {
    /// `process_object` calls observed (Phase-3 scans, all workers).
    pub scans: u64,
    /// Total children pushed by those scans.
    pub children_total: u64,
    /// Widest single child list any one scan pushed. **This is the quantity
    /// W6-P's title names.** Below [`EVAC_LOCAL_PUBLISH_HIGH`] it is
    /// impossible for child-list width alone to have opened the valve.
    pub children_max: u64,
    /// Children-per-scan, bucketed; see [`EVAC_SHARE_BUCKETS`].
    pub child_hist: [u64; EVAC_SHARE_BUCKETS],
    /// Deepest `local.len()` reached by any worker after a push. **This is the
    /// quantity compared against [`EVAC_LOCAL_PUBLISH_HIGH`].**
    pub residue_max: u64,
    /// `local.len()` after each push, bucketed; see [`EVAC_SHARE_BUCKETS`].
    pub residue_hist: [u64; EVAC_SHARE_BUCKETS],
    /// Times the valve opened (mirrors `EvacWorkerCensus::spills`; counted
    /// again here so this table is self-contained and can be diffed alone).
    pub spills: u64,
    /// Spills whose triggering child list was ITSELF over the threshold — the
    /// mechanism W6-P describes.
    pub spills_from_one_wide_object: u64,
    /// Spills whose triggering child list was at or below the threshold, so
    /// the stack crossed it by ACCUMULATION. Non-zero means the valve engaged
    /// on something other than child-list width.
    pub spills_from_accumulation: u64,
    /// Scans made with `CRATONVM_G1_EVAC_LOCAL_QUEUE=0`, where there is no
    /// local stack and `residue_*` is therefore structurally zero. Printed so
    /// an all-zero residue column cannot be misread as "the walk never
    /// accumulated" when it means "there was nothing to accumulate in".
    pub scans_without_local_stack: u64,
}

impl EvacShareCensus {
    /// Accumulate `other` into `self`, field for field.
    pub fn merge(&mut self, other: &Self) {
        self.scans += other.scans;
        self.children_total += other.children_total;
        self.children_max = self.children_max.max(other.children_max);
        self.residue_max = self.residue_max.max(other.residue_max);
        self.spills += other.spills;
        self.spills_from_one_wide_object += other.spills_from_one_wide_object;
        self.spills_from_accumulation += other.spills_from_accumulation;
        self.scans_without_local_stack += other.scans_without_local_stack;
        for i in 0..EVAC_SHARE_BUCKETS {
            self.child_hist[i] += other.child_hist[i];
            self.residue_hist[i] += other.residue_hist[i];
        }
    }

    /// `self - before`, for a test that has to assert a DELTA over a
    /// process-global table. `max` fields are taken from `self`, since a
    /// maximum does not subtract; a caller that needs a per-pause maximum
    /// reads it from a process that took one pause.
    pub fn since(&self, before: &Self) -> Self {
        let mut d = Self {
            scans: self.scans - before.scans,
            children_total: self.children_total - before.children_total,
            children_max: self.children_max,
            residue_max: self.residue_max,
            spills: self.spills - before.spills,
            spills_from_one_wide_object: self.spills_from_one_wide_object
                - before.spills_from_one_wide_object,
            spills_from_accumulation: self.spills_from_accumulation
                - before.spills_from_accumulation,
            scans_without_local_stack: self.scans_without_local_stack
                - before.scans_without_local_stack,
            ..Default::default()
        };
        for i in 0..EVAC_SHARE_BUCKETS {
            d.child_hist[i] = self.child_hist[i] - before.child_hist[i];
            d.residue_hist[i] = self.residue_hist[i] - before.residue_hist[i];
        }
        d
    }

    /// Pushes that reached a local depth at or above
    /// [`EVAC_LOCAL_PUBLISH_HIGH`] — the sum of the buckets that sit entirely
    /// on or above the line. See [`EVAC_SHARE_BUCKETS`].
    pub fn residue_at_or_over_high(&self) -> u64 {
        self.residue_hist[9..].iter().sum()
    }

    /// Scans whose child list reached [`EVAC_LOCAL_PUBLISH_HIGH`] on its own.
    pub fn children_at_or_over_high(&self) -> u64 {
        self.child_hist[9..].iter().sum()
    }
}

/// Process-lifetime [`EvacShareCensus`], merged once per worker per pause.
///
/// A `Mutex` rather than atomics for the same reason [`EVAC_WORKER_CENSUS`] is
/// one: nothing on a hot path touches it. The per-object counting happens on a
/// worker-local `EvacShareCensus` living in `run_worker`'s frame, and this
/// lock is taken once, by each worker, as it leaves the drain loop.
pub static EVAC_SHARE_CENSUS: Mutex<EvacShareCensus> = Mutex::new(EvacShareCensus {
    scans: 0,
    children_total: 0,
    children_max: 0,
    child_hist: [0; EVAC_SHARE_BUCKETS],
    residue_max: 0,
    residue_hist: [0; EVAC_SHARE_BUCKETS],
    spills: 0,
    spills_from_one_wide_object: 0,
    spills_from_accumulation: 0,
    scans_without_local_stack: 0,
});

/// Snapshot of the process-lifetime [`EvacShareCensus`].
pub fn g1_evac_share_census() -> EvacShareCensus {
    *EVAC_SHARE_CENSUS.lock()
}

/// Merge one worker's local tally into the process table.
pub fn publish_evac_share_census(local: &EvacShareCensus) {
    EVAC_SHARE_CENSUS.lock().merge(local);
}

/// The decision-report line for [`EvacShareCensus`].
///
/// Emitted on EVERY arm including the disarmed one, for the reason every other
/// guard line in `collector_decision_report` is: a line that appears only when
/// it has something to say cannot be told apart from a line whose report never
/// ran. The disarmed form says so in words rather than printing zeros that
/// would read as a measurement.
pub fn g1_evac_share_census_report() -> String {
    if !evac_share_census_enabled() {
        return "[GC] g1 evac-share: disarmed (set CRATONVM_G1_EVAC_SHARE_CENSUS=1 to \
                measure how close the local stack gets to EVAC_LOCAL_PUBLISH_HIGH=256)"
            .to_string();
    }
    let c = g1_evac_share_census();
    let hist = |h: &[u64; EVAC_SHARE_BUCKETS]| {
        h.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(",")
    };
    format!(
        "[GC] g1 evac-share: scans={} children={} children_max={} residue_max={} \
         high={EVAC_LOCAL_PUBLISH_HIGH} spills={} from_one_wide_object={} \
         from_accumulation={} children_at_or_over_high={} residue_at_or_over_high={} \
         no_local_stack_scans={}\n\
         [GC] g1 evac-share-hist: buckets=0,1,2-3,4-7,8-15,16-31,32-63,64-127,128-255,\
         256-511,512-2047,2048+ children=[{}] residue=[{}] \
         (residue is `local.len()` after a push — the quantity compared against \
         EVAC_LOCAL_PUBLISH_HIGH; children is the width of ONE object's child list. \
         Buckets 9.. are the ones at or above the line.)",
        c.scans,
        c.children_total,
        c.children_max,
        c.residue_max,
        c.spills,
        c.spills_from_one_wide_object,
        c.spills_from_accumulation,
        c.children_at_or_over_high(),
        c.residue_at_or_over_high(),
        c.scans_without_local_stack,
        hist(&c.child_hist),
        hist(&c.residue_hist),
    )
}

/// Is [`EvacShareCensus`] armed? `CRATONVM_G1_EVAC_SHARE_CENSUS=1`.
///
/// Read through the per-pause `gc_flags()` snapshot on the DRIVER thread and
/// carried into the workers on [`SharedEvac::share_census`], not re-read inside
/// them — `flags::with_thread_overrides` is thread-local, and a worker thread
/// spawned by the pause would not see a test's override.
pub fn evac_share_census_enabled() -> bool {
    gc_flags().g1_evac_share_census
}

/// Empty probes a worker makes with `spin_loop` before it starts calling
/// `yield_now`. See [`SharedEvac::run_worker`].
///
/// The state being waited on is "a neighbour is mid-`process_object` and about
/// to push", which resolves in well under a microsecond. Conceding the core
/// immediately turns that wait into a scheduler round trip — and on a pause
/// that is already holding every mutator, a scheduler round trip is pause
/// time. The count is small because the OTHER state that reaches this arm is
/// "the closure is genuinely finishing", where spinning is pure waste.
pub const EVAC_IDLE_SPINS_BEFORE_YIELD: u32 = 64;

/// `yield_now` probes an idle worker makes before it starts sleeping
/// [`EVAC_IDLE_SLEEP`] per probe (`CRATONVM_G1_EVAC_IDLE_SLEEP`).
pub const EVAC_IDLE_YIELDS_BEFORE_SLEEP: u32 = 32;

/// How long an idle evacuation worker sleeps per probe once spinning and
/// yielding have not found it work: HotSpot's task terminator likewise
/// spins, yields, then sleeps.
///
/// A yield on an oversubscribed host returns at once, so a worker that only
/// yields is a busy loop that takes CPU from the worker holding the work; a
/// local stack cannot be stolen, so the whole pause waits for that worker.
/// Measured 2026-10-06 (binary trees depth 18, eight-core host at load ~35):
/// three young pauses took 645 ms of wall time and 0.15 s of GC CPU with ONE
/// worker, and no less wall time with eight.
pub const EVAC_IDLE_SLEEP: std::time::Duration = std::time::Duration::from_micros(50);



/// Full passes of [`SharedEvac::shared_dest_alloc`] that found nothing and
/// therefore recorded a proof for the rest of the pause.
///
/// Read against `PARALLEL_TLAB_POOL_EXHAUSTED`: before the latch those two
/// were the SAME number, because every exhausted allocation paid its own
/// O(regions) pass. Measured on `HumongousChurn 48 6000 512 -Xmx160m
/// --nojit`, that was 742,926 passes over 160 regions with `shared_dest_allocs
/// = 0` — 119 million region probes, none of which could have succeeded. A
/// `proved_empty` count far below `tlab_pool_exhausted` is the latch working;
/// the two converging again means a pause is finding room and then losing it,
/// which would be a different defect.
pub static PARALLEL_SHARED_DEST_PROVED_EMPTY: AtomicU64 = AtomicU64::new(0);

/// Snapshot of [`PARALLEL_SHARED_DEST_PROVED_EMPTY`].
pub fn parallel_shared_dest_proved_empty() -> u64 {
    PARALLEL_SHARED_DEST_PROVED_EMPTY.load(Ordering::Relaxed)
}



/// Process-lifetime per-worker engagement table; see [`EvacWorkerCensus`].
///
/// A plain `Mutex<Vec<_>>` rather than an array of atomics, because unlike
/// `mark_worker_scans` this is written exactly ONCE PER WORKER PER PAUSE, by
/// the driver, after the closure's barrier — so it is not on any hot path and
/// there is nothing to contend. A `Vec` also means the table is not sized at
/// construction: `parallel_worker_count` reads an env override, and an array
/// sized from it would silently truncate the row of any worker beyond the
/// width the first pause happened to pick.
pub static EVAC_WORKER_CENSUS: Mutex<Vec<EvacWorkerCensus>> = Mutex::new(Vec::new());

/// Lane W7-C (2026-09-21) — the PAUSE-WEIGHTED copy span, latched here because
/// [`EVAC_WORKER_CENSUS`] cannot carry it.
///
/// # Why a lifetime table cannot answer this question
///
/// `EVAC_WORKER_CENSUS` sums each worker's columns over the whole process. A
/// span ratio computed from that sum — `Σ total_bytes / max row` — is not the
/// balance of any pause. If pause 1 has worker 3 copying every byte and pause 2
/// has worker 9 copying every byte, the lifetime table reads two equal rows and
/// a span ratio of 2.00, while EVERY PAUSE was 1.00. Rotation launders a solo
/// copier into a balanced-looking table, and the more pauses a run takes the
/// better the laundering gets.
///
/// So the span is accumulated per pause, at the one instant the per-pause split
/// exists, and reported as `Σ_pauses total_bytes / Σ_pauses max_worker_bytes` —
/// a pause-weighted ratio that rotation cannot move. Both numbers are printed
/// so the ratio has its denominator beside it, and the lifetime-table ratio is
/// printed next to it so the gap between the two is itself visible.
///
/// Pauses that copied nothing are excluded and counted separately: a pause with
/// `total_bytes == 0` has no span, and folding it in as `0/0` would silently
/// change the population the ratio is over.
pub static EVAC_SPAN_COPYING_PAUSES: AtomicU64 = AtomicU64::new(0);
/// Pauses in which the parallel evacuator copied zero bytes; see
/// [`EVAC_SPAN_COPYING_PAUSES`].
pub static EVAC_SPAN_EMPTY_PAUSES: AtomicU64 = AtomicU64::new(0);
/// `Σ_pauses total_bytes` — the numerator of the pause-weighted span.
pub static EVAC_SPAN_TOTAL_BYTES: AtomicU64 = AtomicU64::new(0);
/// `Σ_pauses max_worker_bytes` — the denominator of the pause-weighted span.
pub static EVAC_SPAN_MAX_BYTES: AtomicU64 = AtomicU64::new(0);
/// Copying pauses in which exactly ONE worker copied a byte. This is the
/// reading `scanned` cannot produce: 17 workers scanning and one copying
/// increments this and leaves `driver_scan_share`, `never_scanned_helpers` and
/// `idle_ms` all looking healthy.
pub static EVAC_SPAN_SOLO_PAUSES: AtomicU64 = AtomicU64::new(0);
/// `Σ_pauses (workers that copied a byte)`, so `solo_copier_pauses` has an
/// average to be read against rather than only a total.
pub static EVAC_SPAN_COPIERS: AtomicU64 = AtomicU64::new(0);

/// Fold one pause's rows into [`EVAC_WORKER_CENSUS`]. Row 0 is the driver.
pub fn publish_evac_worker_census(rows: &[EvacWorkerCensus]) {
    if !evac_census_enabled() {
        return;
    }
    // Lane W7-C — the per-pause copy span, taken BEFORE the fold, because the
    // fold is what destroys it. See [`EVAC_SPAN_COPYING_PAUSES`].
    let pause_total: u64 = rows.iter().map(EvacWorkerCensus::total_bytes).sum();
    if pause_total == 0 {
        EVAC_SPAN_EMPTY_PAUSES.fetch_add(1, Ordering::Relaxed);
    } else {
        let pause_max = rows
            .iter()
            .map(EvacWorkerCensus::total_bytes)
            .max()
            .unwrap_or(0);
        let copiers = rows.iter().filter(|r| r.total_bytes() > 0).count().max(1) as u64;
        EVAC_SPAN_COPYING_PAUSES.fetch_add(1, Ordering::Relaxed);
        EVAC_SPAN_TOTAL_BYTES.fetch_add(pause_total, Ordering::Relaxed);
        EVAC_SPAN_MAX_BYTES.fetch_add(pause_max, Ordering::Relaxed);
        EVAC_SPAN_COPIERS.fetch_add(copiers, Ordering::Relaxed);
        if copiers == 1 {
            EVAC_SPAN_SOLO_PAUSES.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut table = EVAC_WORKER_CENSUS.lock();
    if table.len() < rows.len() {
        table.resize(rows.len(), EvacWorkerCensus::default());
    }
    for (slot, row) in table.iter_mut().zip(rows.iter()) {
        slot.merge(row);
    }
}

// Snapshot of the pause-weighted copy span, as
// `(copying_pauses, empty_pauses, total_bytes, max_worker_bytes,
// solo_copier_pauses, copier_slots)`.
//
// Public for the same reason [`g1_evac_worker_census`] is: the only readings
// worth quoting are taken from outside the pause, and a test in another crate
// has to be able to assert a DELTA on these (they are process-lifetime).
//
// # Its production reader is in this file, and that is why the line below is
// here
//
// [`g1_evac_worker_census_report`] reads this and formats the
// `[GC] g1 evac-copy-span:` line; that report is appended to
// `gc_metrics::collector_decision_report`, which `vm-cli` emits on BOTH exit
// arms. So the chain does leave the crate — but the first hop does not leave
// the FILE, and `scripts/check-orphan-instruments.sh` treats a same-file
// reference as a non-caller by design (see its L8). It reported this function
// as an instrument with no reader on its first real run, and
// `docs/feature-designs/concurrent-gc-maturation.md`
// was filed on that reading. It was a false positive; the annotation below is
// the checked form of the correction — it names the reader, and the gate
// verifies both halves of the claim (that `g1_evac_worker_census_report`
// really calls this, and that IT is really read from another file).
//
// **The annotation is now dormant, and deliberately kept.** Since the
// instrument-census retirement this function also has a direct cross-file
// reader (`vm/src/runtime/instrument_census.rs`), so it is no longer an
// uncalled candidate and the gate's exemption pass never looks at it. Removing
// the line would delete the only in-tree example of the mechanism while the
// mechanism is still documented (gate header, `docs/ci/orphan-instrument-gate.md`)
// and still cited by the retired debt register. If the census reader ever goes,
// this is what stops the false positive coming back.
//
// The span cannot simply be formatted in `gc_metrics.rs` beside the other
// `[GC] g1 ...` lines, which would have made this moot: four of the fields on
// that line (`workers_that_copied`, `never_copied_helpers`,
// `lifetime_span_ratio`, `driver_copy_share`) are computed from the per-worker
// rows, so the two halves of the line have to be assembled together.
//
// orphan-gate: read-by g1_evac_worker_census_report
pub fn g1_evac_copy_span() -> (u64, u64, u64, u64, u64, u64) {
    (
        EVAC_SPAN_COPYING_PAUSES.load(Ordering::Relaxed),
        EVAC_SPAN_EMPTY_PAUSES.load(Ordering::Relaxed),
        EVAC_SPAN_TOTAL_BYTES.load(Ordering::Relaxed),
        EVAC_SPAN_MAX_BYTES.load(Ordering::Relaxed),
        EVAC_SPAN_SOLO_PAUSES.load(Ordering::Relaxed),
        EVAC_SPAN_COPIERS.load(Ordering::Relaxed),
    )
}

/// Snapshot of the per-worker engagement table, driver first.
///
/// Public because the instrument's whole purpose is to be read from outside
/// the pause — by an integration test in a separate crate, and by the
/// shutdown report. An empty Vec means no parallel evacuation has run (or the
/// census is off), which is itself a reading: it is the state in which a
/// wall-clock number about "the parallel evacuator" is about something else.
pub fn g1_evac_worker_census() -> Vec<EvacWorkerCensus> {
    EVAC_WORKER_CENSUS.lock().clone()
}

/// The engagement census as the shutdown report prints it.
///
/// # Why this is a report line and not a `debug!`
///
/// `debug!`/`tracing::debug` is compiled out of this tree's release builds, so
/// a census behind one cannot be recovered from a release binary with any
/// `RUST_LOG` setting — which means it cannot be read on the only builds whose
/// numbers anybody quotes. It also has to go through
/// `gc_metrics::collector_decision_report` rather than
/// `G1Collector::print_gc_summary`: that function is reached only from
/// `vm-cli`'s normal-return teardown, and every JUnit-shaped workload in the
/// suites ends in `System.exit`, which never unwinds Rust frames. The decision
/// report is the one census emitted on BOTH exit arms, and this wave's whole
/// premise is that the split must be readable on the workloads that actually
/// run.
///
/// The `driver_scan_share` percentage on the summary line is the number the
/// proposal page asks for in one glance: it is the fraction of all scanning
/// that landed on worker 0. At 100% with a worker count above 1, the pause
/// woke a pool and then did the work itself.
///
/// # Lane W7-C (2026-09-21) — `scanned` is the walk, not the cost
///
/// The field was called `driver_share` until this wave and it was a share of
/// `scanned`. That is the REFERENCE WALK, and for a copying collector the walk
/// is the cheap half. `SharedEvac::process_object` evacuates a gray object's
/// REFERENTS, so every byte a pause copies is charged to whichever worker
/// scanned the PARENT. Spreading the parents spreads the copying; spreading
/// the children spreads only the walk.
///
/// The case this report has to be able to show, measured on the shipped `rset`
/// fixture of `gc/tests/g1_w6p_mixed_parallel_equivalence.rs`:
///
/// ```text
/// per-worker bytes   = [2572416, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, ...]
/// per-worker scanned = [1586, 614, 612, 529, 478, 310, 239, 195, 141, 88, ...]
/// ```
///
/// Seventeen workers scanning, one worker copying 100% of the bytes, and
/// `idle_ms` **0** — they were busy, and busy with the half that is not the
/// cost. Read only through `scanned`, that pause is well balanced on every
/// number the previous report printed: `driver_share` moderate,
/// `never_scanned_helpers` low, `idle_ms` zero.
///
/// So the report now carries BOTH columns under names that cannot be swapped:
/// `driver_scan_share` / `never_scanned_helpers` on the `evac-workers` line for
/// the walk, and a separate `evac-copy-span` line for the copy. `scanned` is
/// kept — it answers the termination-protocol question, which the byte column
/// cannot — but nothing on either line is called just "share" any more.
///
/// `span_ratio` is `total_bytes / max_worker_bytes`: the ceiling on what the
/// worker pool can buy before any scheduling overhead is paid. **1.00 means one
/// thread did every byte.** It is accumulated per pause (see
/// [`EVAC_SPAN_COPYING_PAUSES`]) rather than over the lifetime table, because
/// the lifetime table launders a rotating solo copier into a balanced one;
/// `lifetime_span_ratio` is printed beside it so that gap is visible rather
/// than assumed absent.
pub fn g1_evac_worker_census_report() -> String {
    let rows = g1_evac_worker_census();
    let (span_pauses, span_empty, span_total, span_max, span_solo, span_copiers) =
        g1_evac_copy_span();
    // The pause-weighted ratio and its population. Computed once, used on both
    // the empty and the populated arm, because BOTH lines are emitted on every
    // arm: a line that appears only when it is non-empty cannot be told apart
    // from a line whose report never ran, which is the rule every other guard
    // counter in the decision report already follows.
    let span_ratio = if span_max == 0 {
        0.0
    } else {
        span_total as f64 / span_max as f64
    };
    let avg_copiers = if span_pauses == 0 {
        0.0
    } else {
        span_copiers as f64 / span_pauses as f64
    };
    // The trailing parenthetical is not decoration. Wave 6 found that a reader
    // takes `scanned` for the distribution because it is the column that is
    // there; this sentence is what makes the two impossible to confuse on a
    // line somebody pastes into a report.
    const SPAN_NOTE: &str = "(span_ratio=1.00 means ONE worker copied every byte; \
         `scanned` on the evac-workers line is the reference walk, and a parent's \
         scan charges its children's BYTES to whoever scanned the parent, so a \
         balanced scan column does not imply a balanced copy)";
    if rows.is_empty() {
        return format!(
            "[GC] g1 evac-workers: rows=0 (no parallel evacuation ran{})\n\
             [GC] g1 evac-copy-span: copying_pauses={span_pauses} \
             empty_pauses={span_empty} copied_bytes={span_total} \
             max_worker_bytes={span_max} span_ratio={span_ratio:.2} \
             solo_copier_pauses={span_solo} copier_slots={span_copiers} \
             avg_copiers_per_pause={avg_copiers:.2} workers_that_copied=0/0 \
             never_copied_helpers=0 lifetime_span_ratio=0.00 \
             driver_copy_share=0.0% {SPAN_NOTE}",
            if evac_census_enabled() {
                ""
            } else {
                ", census disabled by CRATONVM_G1_EVAC_CENSUS=0"
            },
        );
    }
    let mut total = EvacWorkerCensus::default();
    for r in &rows {
        total.merge(r);
    }
    let driver_scan_share = if total.scanned == 0 {
        0.0
    } else {
        rows[0].scanned as f64 * 100.0 / total.scanned as f64
    };
    let lifetime_bytes = total.total_bytes();
    let lifetime_max = rows
        .iter()
        .map(EvacWorkerCensus::total_bytes)
        .max()
        .unwrap_or(0);
    let lifetime_ratio = if lifetime_max == 0 {
        0.0
    } else {
        lifetime_bytes as f64 / lifetime_max as f64
    };
    let driver_copy_share = if lifetime_bytes == 0 {
        0.0
    } else {
        rows[0].total_bytes() as f64 * 100.0 / lifetime_bytes as f64
    };
    // A worker whose row is entirely zero was dispatched and found nothing to
    // do. Counting them separately keeps "the pool is too wide for this
    // workload" distinguishable from "the work never spread".
    let idle_rows = rows.iter().skip(1).filter(|r| r.scanned == 0).count();
    // The same question asked of the expensive column. `never_scanned_helpers`
    // and `never_copied_helpers` differ by exactly the helpers that walked
    // references without copying a byte — the population Finding B is about.
    let never_copied = rows.iter().skip(1).filter(|r| r.total_bytes() == 0).count();
    let copied_rows = rows.iter().filter(|r| r.total_bytes() > 0).count();
    // The denominator both `never_*_helpers` counts are read against. The table
    // is indexed by SLOT and merged over the process, so a tail row can be zero
    // because the slot was dispatched and found nothing, or because no pause
    // was ever wide enough to reach it — `parallel_worker_count` reads an env
    // override and a pause with `outstanding == 0` dispatches no helper at all.
    // Without this, `never_copied_helpers=22` cannot be told from
    // `helpers_dispatched=0`.
    let helpers_dispatched = rows.iter().skip(1).filter(|r| r.pauses > 0).count();
    let mut s = format!(
        "[GC] g1 evac-workers: rows={} scanned={} copied_objs={} copied_bytes={} \
         seed_regions={} lifts={} lifted={} spills={} idle_spills={} idle_ms={} idle_probes={} \
         driver_scan_share={driver_scan_share:.1}% never_scanned_helpers={idle_rows} \
         never_copied_helpers={never_copied} helpers_dispatched={helpers_dispatched}",
        rows.len(),
        total.scanned,
        total.seed_objs + total.objs_copied,
        lifetime_bytes,
        total.seed_regions,
        total.lifts,
        total.lifted,
        total.spills,
        total.idle_spills,
        total.idle_ns / 1_000_000,
        total.idle_probes,
    );
    s.push('\n');
    s.push_str(&format!(
        "[GC] g1 evac-copy-span: copying_pauses={span_pauses} empty_pauses={span_empty} \
         copied_bytes={span_total} max_worker_bytes={span_max} span_ratio={span_ratio:.2} \
         solo_copier_pauses={span_solo} copier_slots={span_copiers} \
         avg_copiers_per_pause={avg_copiers:.2} workers_that_copied={copied_rows}/{} \
         never_copied_helpers={never_copied} lifetime_span_ratio={lifetime_ratio:.2} \
         driver_copy_share={driver_copy_share:.1}% {SPAN_NOTE}",
        rows.len(),
    ));
    for (i, r) in rows.iter().enumerate() {
        s.push('\n');
        s.push_str(&format!(
            "[GC] g1 evac-worker[{i}]{}: pauses={} scanned={} seed_regions={} \
             seed_bytes={} closure_bytes={} total_bytes={} lifts={} lifted={} \
             spills={} idle_ms={}",
            if i == 0 { " (driver)" } else { "" },
            r.pauses,
            r.scanned,
            r.seed_regions,
            r.seed_bytes,
            r.bytes_copied,
            r.total_bytes(),
            r.lifts,
            r.lifted,
            r.spills,
            r.idle_ns / 1_000_000,
        ));
    }
    s
}

/// Is the per-worker engagement census armed? `CRATONVM_G1_EVAC_CENSUS=0`
/// disarms it.
///
/// # Why this one defaults ON when the wave's rule is "default to today's
/// behaviour"
///
/// Because the rule's purpose is that a lever must not change what a pause
/// DOES on an argument, and this lever changes nothing a pause does: every
/// counter is a `u64` add on a worker-local struct, and the only clock read is
/// one `Instant` pair per idle EPISODE (not per probe — see
/// [`SharedEvac::run_worker`]). The publish is one `Mutex` acquisition per
/// pause on the driver, after the barrier.
///
/// And it defaults on because of a MEASUREMENT, not that argument. Same
/// binary, `G1ChurnPauseProbe 32 400 -Xmx256m`, arms interleaved, 5 reps —
/// see `docs/feature-designs/concurrent-gc-maturation.md` for the raw
/// numbers. The two arms are inside each other's spread, which is the result
/// the cost model predicts and is the only thing that licenses leaving the
/// instrument armed in production.
///
/// `=0` remains a real bisection arm: it restores the pre-census worker loop
/// byte for byte, including the absent clock reads.
pub fn evac_census_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| true)}









/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_FUTILE_YOUNG_ESCALATION`
/// (OPT-IN, `=1`). See `G1Collector::note_futile_young_pause`.
///
/// Opt-in on its A/B (n23-w8g, pinned, 3 reps, `-XX:+UseG1GC -Xmx1g`): with
/// it ON, `R23W3Keep obj` ran 58.6/75.5/83.8 ns/op and `KeepLong 30`
/// 19.2/19.2/19.2 M ops; with `=0`, 63.1/104.9/64.5 ns/op and 19.2/19.2/16.8 M.
/// It engages (`KeepLong 12` census: `mark-doors alloc_fail waiting=220
/// finished=2 started=3`, `mark_ms=3244`, against `waiting=281 finished=0
/// started=2`, `mark_ms=2684` at `=0`), but the ops do not move. The G1 OOM
/// gate (`R22FlipGcRows -Xmx64m`) passed with it ON: 0/5 OOME, 5/5 SAME,
/// `--nojit` and JIT.
pub fn g1_futile_young_escalation_enabled() -> bool {
    false
}









/// Starved young pauses in a row before the first starved full; restored by a
/// productive one (`G1Collector::note_starved_full_outcome`).
pub const STARVED_FULL_STREAK: usize = 2;

/// A young pause is starved (for the starved full) when it leaves fewer than
/// this percentage of the heap's regions Free: HotSpot's `G1ReservePercent`,
/// under which its young pauses meet an evacuation failure and a `Pause Full`
/// (`R23W6KeepLong -Xmx1g`: HotSpot's last young pause of a round leaves about
/// 5% free; here, at the 3% futile test, eden shrank to 7-16 MB pauses first).
pub const STARVED_FULL_FREE_PCT: usize = 10;

/// Did a young pause's survivors dominate (for the starved full)? Copied at
/// least half of what it freed: `bytes_freed` counts the collection set's
/// whole regions, so an all-live Eden copies a little less than it frees
/// (`R23W6KeepLong`: 47 MB of 49), while a collecting one copies KB per MB
/// (`Zs3Nd3UpcallMints proxy -Xmx64m`).
#[inline]
pub fn survivors_dominated(bytes_copied: usize, bytes_freed: usize) -> bool {
    bytes_copied.saturating_mul(2) >= bytes_freed
}

/// The pure starved test: a cycle is open and `free` is under
/// [`STARVED_FULL_FREE_PCT`] percent of `regions`.
#[inline]
pub fn starved_young_pause(marking: bool, free: usize, regions: usize) -> bool {
    marking && free.saturating_mul(100) < regions.saturating_mul(STARVED_FULL_FREE_PCT)
}

/// The starved full's back-off cap (doubling from [`STARVED_FULL_STREAK`]).
pub const STARVED_FULL_STREAK_MAX: usize = 256;

/// Consecutive futile young pauses before the mark driver finishes the open
/// cycle early. Conservative: a healthy cycle that merely overlaps a few
/// tight pauses never reaches it.
pub const FUTILE_YOUNG_STREAK: usize = 8;

/// A young pause is futile (for the escalation) when it leaves fewer than
/// this percentage of the heap's regions Free. HotSpot's young sizing is at
/// its floor well before this (`G1ReservePercent` is 10), so a pause under
/// it is one evacuation failure away from a Full collection there.
pub const FUTILE_YOUNG_FREE_PCT: usize = 3;

/// The pure futile test: a cycle is open and `free` is under
/// [`FUTILE_YOUNG_FREE_PCT`] percent of `regions`.
#[inline]
pub fn futile_young_pause(marking: bool, free: usize, regions: usize) -> bool {
    marking && free.saturating_mul(100) < regions.saturating_mul(FUTILE_YOUNG_FREE_PCT)
}

/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_EVAC_LEAF_NO_PUSH` (OPT-IN,
/// `=1`). The parallel evacuator does not push a fresh primitive-array copy
/// onto the gray stack; see `SharedEvac::record_fresh_child_screened`. Read on
/// the driver once per pause.
///
/// Opt-in on its A/B (n23-w8b, 3 reps, loaded host, `-XX:+UseG1GC -Xmx1g`):
/// default ON `KeepLong 30` 12.0/19.2/16.8 M ops vs `=0` 16.8/16.8/16.8 M,
/// `R23W3Keep obj` 87-107 vs 76-108 ns/op -- no measurable win. The census
/// says why: about 90% of `KeepLong`'s young pauses run the SERIAL evacuator
/// and the parallel ones run at 2 workers, so this per-worker cost is not
/// where that workload's time goes.
pub fn evac_leaf_no_push_enabled() -> bool {
    false
}

/// g1sz: `CRATONVM_G1_EVAC_PREFETCH` (OPT-IN, `=1`): the parallel closure
/// prefetches a holder's collection-set referents before it copies them
/// (`SharedEvac::prefetch_cset_referent`). Read once per pause.
///
/// Opt-in on its A/B (2026-10-27, `G1epKeepRounds 10 -Xmx1g`, 3 interleaved
/// reps on a loaded 16-core host): with it on, median wall 2.66x HotSpot and
/// 39.1 s CPU; with it off, 2.42x and 36.9 s. The second walk over each
/// holder's references costs more than the overlapped misses save.
pub fn g1_evac_prefetch_enabled() -> bool {
    false
}

/// g1sz: how many reference-array elements ahead of the evacuate pass the
/// closure prefetches (`CRATONVM_G1_EVAC_PREFETCH`).
pub const EVAC_PREFETCH_AHEAD: usize = 8;

/// JIT round 23 wave 8, lane L9: `CRATONVM_G1_EVAC_IDLE_TRY_LOCK` (OPT-IN,
/// `=1`). An idle evacuation worker's repeat probes `try_lock` the shared
/// queue instead of blocking on it; see `SharedEvac::run_worker`. Read on the
/// driver once per pause.
///
/// Opt-in on its A/B (n23-w8b, 3 reps, loaded host): default ON `KeepLong 30`
/// 12.0/19.2/16.8 M ops vs `=0` 12.0/14.4/19.2 M, `R23W3Keep obj` 87-107 vs
/// 84-145 ns/op -- no measurable win (same census note as the leaf switch).
pub fn evac_idle_try_lock_enabled() -> bool {
    false
}

/// Does `header` describe an array whose elements are primitives (no reference
/// to scan)? The parallel evacuator's leaf test
/// (`CRATONVM_G1_EVAC_LEAF_NO_PUSH`).
#[inline]
pub fn evac_header_is_primitive_array(header: &ObjectHeader) -> bool {
    header.kind() == ObjectKind::Array && header.element_type() != ArrayElementType::Reference
}





pub fn parallel_seed_enabled() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => !matches!(v.trim(), "0" | "false" | "FALSE" | "no" | "off"),
        Err(_) => true,
    }
}

/// Should `mixed_collection` be allowed to dispatch to the PARALLEL evacuator?
///
/// Default ON since 2026-10-27 (below); `CRATONVM_G1_PARALLEL_MIXED=0` makes
/// every mixed pause serial no matter what `CRATONVM_G1_PARALLEL_EVAC` says,
/// which is what the flag being unset used to mean.
///
/// # Why this is opt-in when the young path is opt-OUT
///
/// It is not a considered asymmetry; it is an inherited one, and naming it is
/// half the point of this lever. `mixed_collection` has refused the parallel
/// evacuator unconditionally since a 2026 note citing "a RARE, NON-DETERMINISTIC
/// race in the *young* parallel evacuator ... Because it lives in the shared
/// `parallel_evacuate` closure that `mixed_collection_parallel` also drives,
/// mixed must not be routed to it until the race is fixed". Meanwhile
/// `CRATONVM_G1_PARALLEL_EVAC` was flipped to default-ON, so the young path
/// takes that same shared closure on EVERY production pause. Both cannot be
/// right: either the race is real, in which case the refusal protects the
/// infrequent pause and exposes the frequent one, or it is not, in which case
/// mixed is serial for no reason.
///
/// This lever exists so the question is ANSWERABLE in one binary: before it,
/// `mixed_collection_parallel` was live code with no route to it outside unit
/// tests, so no gauntlet arm could measure it and no soak could reproduce
/// anything on it. See
/// `docs/feature-designs/concurrent-gc-maturation.md`.
///
/// # Why it stayed opt-in after the race question was answered
///
/// Not the race. That did not reproduce in three independent passes: 120/120
/// clean at `CRATONVM_G1_WORKERS ∈ {1,2,8,23}` (W2-D), the last UB-class
/// candidate closed by W3-A, eight serial-vs-parallel pairs agreeing object
/// for object over one fixture in one process (W6-P), and 48/48 clean on
/// `SteadyChurnRecreation @16m --nojit` across three arms (2026-09-21).
///
/// The reason is that the lever cannot buy anything: it parallelises PHASE 3,
/// and on the shape a mixed pause actually has — an old live set reached
/// through the REMEMBERED SET — 100.0% of the bytes the pause copies are
/// copied in Phase 2, by the driver thread alone. The copy-span ratio is
/// exactly 1.00 there and 3.71 on a shape built to spread, so the 1.00 is a
/// reading and not a blind spot. That is inert BY CONSTRUCTION, not by
/// workload, and no probe can show otherwise while it holds. The three links,
/// and what to fix instead (Phase-2 seed parallelism, remembered-set source
/// precision, the evacuator's child-list-width work-sharing trigger), are in
/// `docs/feature-designs/concurrent-gc-maturation.md`.
///
/// [`g1_mixed_route_counts`] is how a run says which arm it took.
///
/// # Why it is default-ON since 2026-10-27 (g1ep5)
///
/// "Inert" was measured on mixed pauses whose copying was the OLD half. A
/// mixed pause also collects the whole young generation, and when the young
/// half dominates, the serial body copies all of it on the driver thread.
/// `G1epKeepRounds 10 -Xmx1g` has a mode (about 1 run in 5 on the 16-core
/// host) where a starved full reclaims nothing, every later cleanup reopens
/// a mixed phase, and every pause is Mixed with about 270 young regions in
/// its set. Serial, one such pause copied 279 MB in 1.66 s of closure (a
/// young pause copies 322 MB in 64 ms); those runs spent 36-37 s of 44 s in
/// pauses. Interleaved against dev on one binary (10 pairs), the parallel
/// route's run in the mode spent 3.5 s in pauses and 9.5 s of wall, and the
/// runs outside it matched dev. A `System.gc()` or allocation-failure
/// compaction still takes the serial body (`requested_compaction`).
/// `CRATONVM_G1_PARALLEL_MIXED=0` is the serial route back.
pub fn parallel_mixed_enabled() -> bool {
    true
}

/// LANE W6-P — mixed pauses routed to the SERIAL body.
pub static MIXED_ROUTE_SERIAL: AtomicU64 = AtomicU64::new(0);
/// LANE W6-P — mixed pauses routed to [`G1Collector::mixed_collection_parallel`].
pub static MIXED_ROUTE_PARALLEL: AtomicU64 = AtomicU64::new(0);

/// Record which arm a mixed pause took. Called once per `mixed_collection`,
/// from the dispatcher, before either body runs.
pub fn note_mixed_route(parallel: bool) {
    let slot = if parallel {
        &MIXED_ROUTE_PARALLEL
    } else {
        &MIXED_ROUTE_SERIAL
    };
    slot.fetch_add(1, Ordering::Relaxed);
}

/// `(serial, parallel)` — how many mixed pauses each arm of the
/// `CRATONVM_G1_PARALLEL_MIXED` dispatcher has actually run.
///
/// # Why this number and not a wall clock
///
/// This is the DENOMINATOR under every claim made about this lever. A mixed
/// pause is a once-per-run event at best on every workload in this tree and
/// usually never happens at all, so `CRATONVM_G1_PARALLEL_MIXED=0` vs `=1`
/// over a whole run is, on the available battery, a comparison of two runs
/// that took the same code path zero times. Both arms reading `(0, 0)` here is
/// the reading that says so — and it is the reading a wall-clock A/B cannot
/// produce, because it reports a difference either way.
///
/// `serial + parallel` is every mixed pause the process took; `parallel`
/// alone is the engagement count for the lever. Process-global and
/// monotonic, so a test must assert a DELTA around its own pause rather than
/// an absolute (`libtest`'s other threads share this process).
pub fn g1_mixed_route_counts() -> (u64, u64) {
    (
        MIXED_ROUTE_SERIAL.load(Ordering::Relaxed),
        MIXED_ROUTE_PARALLEL.load(Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// --- lane E helpers ---
//
// Allocation / region-management / sizing / observability helpers added by the
// 2026-09-20 G1 design-and-performance wave. Everything below is reachable only
// from the lane's own owned functions.
// ---------------------------------------------------------------------------

/// Refills whose requested chunk was clamped to `region_size / 2` instead of
/// being refused outright. See [`G1Collector::refill_tlab`].
pub static TLAB_OVERSIZE_CLAMPED: AtomicU64 = AtomicU64::new(0);

/// The key of `post_write_barrier_rset`'s per-thread last-edge memo:
/// `(collector instance, holder address, destination region index,
/// rset_cache_epoch, card clean_epoch)`.
pub type G1LastEdgeKey = (u64, usize, usize, u64, u64);

/// Does the remembered last edge vouch for `key`? Exact equality: a different
/// collector, holder, destination region, or ANY epoch movement misses.
#[inline]
pub fn g1_last_edge_memo_hit(memo: Option<G1LastEdgeKey>, key: G1LastEdgeKey) -> bool {
    memo == Some(key)
}



/// JIT round 23 wave 3, lane L4: `CRATONVM_G1_YOUNG_SOURCE_FILTER` (perf,
/// default ON, `0` kill; effective only with [`g1_young_source_prereq_enabled`]),
/// read once per process: the lean compiled-store barrier
/// (`G1Collector::young_holder_skips_post_barrier`, asked by the VM's
/// `jit_g1_post_write_barrier`) returns at once for a holder in an Eden /
/// Survivor region (HotSpot's young-card filter), and `G1Collector::new`
/// publishes [`cratonvm_gc_core::jit_bounds::JIT_G1_YOUNG`] so compiled code can skip the
/// call for such a holder inline. `post_write_barrier_rset` itself is
/// unchanged (interpreter, native and GC stores keep recording). Also the
/// gate of the prerequisite's pause work: with no filter, no young store was
/// skipped and nothing needs re-walking. Per heap it is further gated on the
/// cleanup scrub (`G1Collector::young_source_filter_on`).
pub fn g1_young_source_filter_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
            true
        })
}

/// The byte the inline young-holder filter reads: region `i`'s type is at
/// `regions_ptr + i * G1_REGION_STRIDE + G1_REGION_TYPE_OFFSET`
/// ([`cratonvm_gc_core::jit_bounds::JIT_G1_YOUNG`]); a value `<= G1_REGION_YOUNG_MAX`
/// (`Eden` = 0, `Survivor` = 1) is young. Compile-time constants of this build.
pub const G1_REGION_STRIDE: usize = std::mem::size_of::<G1Region>();
/// See [`G1_REGION_STRIDE`].
pub const G1_REGION_TYPE_OFFSET: usize = std::mem::offset_of!(G1Region, region_type);
/// See [`G1_REGION_STRIDE`].
pub const G1_REGION_YOUNG_MAX: u8 = RegionType::Survivor.to_u8();
const _: () = assert!(
    RegionType::Eden.to_u8() == 0 && RegionType::Survivor.to_u8() == 1,
    "the inline young test is `type <= 1`"
);





/// gci i13/oa finding 1: `CRATONVM_G1_HUMONGOUS_RESERVE_ONE` (default OFF,
/// `runtime_flag_on`). A FALLIBLE humongous request at the TLAB reserve
/// ([`G1Collector::alloc_fallible_initialized`]) may take every reserve region
/// but one ([`humongous_may_take_reserve`]). Read per call, only on that
/// at-reserve humongous arm (the path to an `OutOfMemoryError`), so no
/// process `OnceLock`.
pub fn g1_humongous_reserve_one() -> bool {
    false
}

/// gci i29/cv2: `CRATONVM_G1_DBG_FLAGLESS_SWEEP` (default OFF,
/// `runtime_flag_on`). Diagnostic only: whole-heap header sweeps at pause
/// start, pause end and cleanup end that name the phase in which a live
/// object's header lost `GC_FLAG_HEADER`
/// ([`G1Collector::flagless_header_sweep`]). Read per sweep (three per cycle at
/// most), so no process `OnceLock`.
pub fn g1_dbg_flagless_sweep() -> bool {
    false}

/// gci i29/cv2: what one [`G1Collector::flagless_headers_in`] walk covered.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct FlaglessSweepTally {
    /// Non-Free, non-continuation regions walked.
    pub regions: usize,
    /// Object starts the walks stepped over.
    pub objects: usize,
    /// Flagless sixteen-byte pairs that were all zero: zeroed space, not a
    /// header (an unused TLAB tail), never reported.
    pub zero_pairs: usize,
    /// Regions whose walk met a size that left the cursor (the walk stops
    /// there, so anything flagless past it is not seen).
    pub desynced_regions: usize,
}

/// gci i29/cv2: split this sweep's flagless addresses into those NEW since
/// the previous sweep and those CARRIED by a pause (a to-space address whose
/// `forwards` source was flagless at the previous sweep). Returns
/// `(new, carried)` with `new` sorted and `carried` as `(to, from)` pairs
/// sorted by `to`. Pure.
pub fn flagless_new_and_carried(
    prev: &FxHashSet<usize>,
    current: &FxHashSet<usize>,
    forwards: Option<&cratonvm_types::PointerMap>,
) -> (Vec<usize>, Vec<(usize, usize)>) {
    let mut fresh: Vec<usize> = current.iter().copied().filter(|a| !prev.contains(a)).collect();
    fresh.sort_unstable();
    let mut carried: Vec<(usize, usize)> = Vec::new();
    if let Some(map) = forwards {
        if !fresh.is_empty() {
            let want: FxHashSet<usize> = fresh.iter().copied().collect();
            for (&from, &to) in map.iter() {
                if want.contains(&to) && prev.contains(&from) {
                    carried.push((to, from));
                }
            }
        }
    }
    carried.sort_unstable();
    carried.dedup_by_key(|p| p.0);
    let carried_to: FxHashSet<usize> = carried.iter().map(|p| p.0).collect();
    fresh.retain(|a| !carried_to.contains(a));
    (fresh, carried)
}

/// gci i13/oa finding 1: may a fallible humongous claim of `claims` regions be
/// served from `free` Free regions when the TLAB reserve is `reserve`? Only if
/// it leaves at least one Free region (`reserve.min(1)`); never when there is
/// no reserve (the caller's ordinary arm already served that case). Pure.
pub fn humongous_may_take_reserve(free: usize, claims: usize, reserve: usize) -> bool {
    reserve > 0 && free >= claims.saturating_add(reserve.min(1))
}

/// gci nd48: [`G1Collector::reserve_credit_used`] while no refusal at the TLAB
/// reserve is on record.
pub const RESERVE_CREDIT_UNARMED: usize = usize::MAX;



/// gci nd48: may a small fallible request at the TLAB reserve claim one more
/// Free region ([`G1Collector::alloc_from_reclaim_credit`])? Only when the
/// claim leaves at least one Free region (`free >= 2`), and collections have
/// given back `reclaimed` bytes since the last refusal worth half a region
/// for each of the `spent` claims already made plus this one. Pure.
pub fn reserve_credit_admits(free: usize, reclaimed: usize, spent: usize, region_size: usize) -> bool {
    reserve_credit_admits_at(free, reclaimed, spent, (region_size / 2).max(1))
}

/// [`reserve_credit_admits`] with the bytes one claim costs given. Pure.
pub fn reserve_credit_admits_at(free: usize, reclaimed: usize, spent: usize, per_claim: usize) -> bool {
    free >= 2 && reclaimed >= spent.saturating_add(1).saturating_mul(per_claim.max(1))
}













/// The reclaimed bytes one reserve claim costs: half a region, or a
/// sixteenth under [`g1_reserve_credit_fine`]. Pure.
pub fn reserve_credit_per_claim(region_size: usize, fine: bool) -> usize {
    if fine {
        (region_size / 16).max(1)
    } else {
        (region_size / 2).max(1)
    }
}

/// gci i13/oa item 2: region types whose objects a young pause can reclaim --
/// its collection set (Eden, Survivor) and eagerly reclaimed humongous spans.
/// Pure.
pub fn region_type_young_pause_reclaims(ty: RegionType) -> bool {
    matches!(
        ty,
        RegionType::Eden
            | RegionType::Survivor
            | RegionType::HumongousStart
            | RegionType::HumongousContinuation
    )
}

/// gci i13/oa item 5: `CRATONVM_G1_HUMONGOUS_COMPACTION_NEEDS_RESERVE`
/// (default OFF, `runtime_flag_on`): the allocation-failure compaction is
/// armed for a fragmented humongous request only when the Free pool also holds
/// what the retry's reserve check needs
/// ([`humongous_compaction_has_room_for_retry`]). Read once per arming, on the
/// path to an `OutOfMemoryError`.
pub fn g1_humongous_compaction_needs_reserve() -> bool {
    false
}

/// gci i13/oa item 5: can a fallible retry of a `requested`-region humongous
/// span succeed with `free` Free regions, whatever compaction does to their
/// shape? [`G1Collector::alloc_fallible_initialized`] serves it only from
/// `requested + reserve` Free regions (`requested + 1` under
/// `CRATONVM_G1_HUMONGOUS_RESERVE_ONE`, `reserve_one`). Pure.
pub fn humongous_compaction_has_room_for_retry(
    free: usize,
    requested: usize,
    reserve: usize,
    reserve_one: bool,
) -> bool {
    if reserve == 0 {
        return free >= requested;
    }
    let held_back = if reserve_one { reserve.min(1) } else { reserve };
    free >= requested.saturating_add(held_back)
}



/// The best-fit arm of [`find_contiguous_free`]: the LOWEST-indexed run of at
/// least `count` adjacent Free regions among the SHORTEST such runs.
///
/// Ties break toward the lower index, which keeps humongous spans packed at the
/// bottom of the heap — the half of the item-8 placement split that young
/// claims deliberately grow away from.
pub fn find_contiguous_free_best_fit(regions: &[G1Region], count: usize) -> Option<usize> {
    let (found, probed) = best_fit_run(
        regions.iter().map(|r| r.region_type() == RegionType::Free),
        count,
    );
    note_contig_scan(probed);
    found
}

/// The pure core of [`find_contiguous_free_best_fit`], over a stream of "is
/// this slot free" bits rather than over the region table.
///
/// Split out so the placement policy can be tested against an explicit free-map
/// without building a collector for every shape — a region table can only be
/// driven into a given fragmentation pattern through the allocator, which is
/// the thing under test.
///
/// Returns `(start_of_chosen_run, slots_probed)`. The probe count is returned
/// rather than recorded here so the caller owns the instrument (this function
/// is also called from tests, which must not move the process counters).
pub fn best_fit_run<I: Iterator<Item = bool>>(free: I, count: usize) -> (Option<usize>, usize) {
    let mut best: Option<(usize, usize)> = None; // (run_len, run_start)
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut probed = 0usize;

    // `<=` on the incumbent length, so the FIRST run of the winning length wins
    // and ties break toward the lower index.
    fn close(len: usize, start: usize, count: usize, best: &mut Option<(usize, usize)>) {
        if len < count {
            return;
        }
        match best {
            Some((blen, _)) if *blen <= len => {}
            _ => *best = Some((len, start)),
        }
    }

    // One pass, closing each run as it ends. The trailing run is closed after
    // the loop rather than inside it, because a run that reaches the end of the
    // table has no occupied slot to terminate it.
    for (i, is_free) in free.enumerate() {
        probed += 1;
        if is_free {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
        } else {
            close(run_len, run_start, count, &mut best);
            run_len = 0;
        }
    }
    close(run_len, run_start, count, &mut best);
    (best.map(|(_, start)| start), probed)
}

/// Test hook for [`best_fit_run`] — `free[i] == true` means "region `i` is
/// Free". Returns the start index of the run the humongous allocator would
/// choose, or `None` when no run of `count` adjacent free regions exists.
pub fn g1_dbg_best_fit_run(free: &[bool], count: usize) -> Option<usize> {
    best_fit_run(free.iter().copied(), count).0
}

/// Test hook for the first-fit arm that `CRATONVM_G1_HUMONGOUS_BEST_FIT=0`
/// restores: the LOWEST run of `count` adjacent free regions.
///
/// Present beside [`g1_dbg_best_fit_run`] so the property that matters — the
/// two arms agree on whether a run EXISTS, and differ only in which one they
/// name — can be asserted rather than assumed.
pub fn g1_dbg_first_fit_run(free: &[bool], count: usize) -> Option<usize> {
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    for (i, &is_free) in free.iter().enumerate() {
        if is_free {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            if run_len >= count {
                return Some(run_start);
            }
        } else {
            run_len = 0;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// LANE E, wave 2 — the adaptive heap-sizing policy
// ---------------------------------------------------------------------------

/// Is the adaptive heap-sizing policy armed (`CRATONVM_G1_HEAP_RESIZE`)?
///
/// **Default ON since 2026-10-26 (g1sz), `=0` kills.** The soak the paragraphs
/// below ask for ran twice (see `docs/feature-designs/concurrent-gc-maturation.md`). The first refused the flip on
/// `BinTreesClassic 18` alone (1.155x CPU: its three young pauses were 3-4x
/// HotSpot's). The second, after the g1sz young-pause work, passed: that row
/// at 1.041x with the RSS of every short allocation run cut toward HotSpot's.
///
/// With it off, [`G1Collector::heap_target_regions`] stays pinned
/// at the region-table length for the life of the process, `needs_gc` measures
/// the free fraction against exactly the same denominator it always has, and
/// the trailing-free-run shrink keeps exactly the `-Xms` floor it always had.
/// The flag-off build is therefore byte-for-byte the pre-wave-2 collector, not
/// approximately it.
///
/// It shipped opt-in for the reason wave 1 wrote down after two of its three
/// default-ON levers turned out to cost time: a sizing policy is a bet about a
/// workload's future, and it had not been run against a soak. The soak's
/// criteria and numbers are on the retired page named above.
pub fn g1_heap_resize_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| true)}

/// Does the mixed collection set price the fix-up walk it CAUSES and the young
/// half it is already committed to (`CRATONVM_G1_PAUSE_COST_MODEL=1`), or only
/// the old-half copying it is choosing (the default, and the pre-wave-2
/// behaviour)?
///
/// See [`G1Collector::old_cset_copy_budget_ns`] for the model and
/// `docs/feature-designs/concurrent-gc-maturation.md` for why
/// the fixed `fixup_ns_ema` term under-estimates in the overrun direction.
///
/// **default on.** Both terms the model adds are SUBTRACTIONS from the budget, so
/// arming it can only make a mixed collection set smaller — which is the safe
/// direction for a pause goal and the unsafe direction for throughput, since a
/// mixed phase that takes fewer old regions per pause needs more pauses to
/// retire the same candidate set. That trade is a measurement, and it has not
/// been taken on a soak, so today's behaviour stays the default.
pub fn g1_pause_cost_model() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => matches!(v.trim(), "1" | "true" | "TRUE" | "yes"),
        Err(_) => false,
    }
}

/// LANE W4-B — does a YOUNG pause refresh the `evac_ns_per_byte` copy-cost
/// calibration (`CRATONVM_G1_EVAC_COST_FROM_YOUNG=1`), or do only mixed
/// collections (the default, and the pre-wave-4 behaviour)?
///
/// # The finding this exists for
///
/// `lane-e-proposals.md` P1 — allocation-rate-driven young sizing, the
/// proposal that most directly serves the pause goal — builds its predictor on
/// two inputs, and names `evac_ns_per_byte` as one of them:
///
/// > `predicted_pause(n_regions) ~= n * survivor_bytes_per_region *
/// > evac_ns_per_byte + fixup_ns_ema`
///
/// and it ends by saying the right first step is to compute offline, from an
/// existing run's `[GC-STAT]` log, what that predictor WOULD have chosen.
///
/// That step cannot be taken, because `update_evac_cost` has exactly two
/// callers — `mixed_collection` and `mixed_collection_parallel` — and nothing
/// else in the collector touches the estimate. **On any run without a mixed
/// collection, `evac_ns_per_byte` holds its constructor value of 4 for the
/// life of the process.** It is printed on every `[GC-STAT]` line, so the
/// evidence has been in every log this round produced: `evac_ns_per_byte=4` on
/// all five pauses of `HumongousChurn 48 6000 512 -Xmx160m --nojit`, and the
/// `[GC] g1 young-sizing:` line reports the same 4 at exit.
///
/// A predictor whose per-byte term is a compile-time constant is not a
/// predictor of anything; it is `n * survivor_bytes_per_region` with a scale
/// factor. So P1's offline step would have produced a clean, confident,
/// meaningless answer. This is `w3c-instrumentation-audit.md` §1 in the
/// sizing family rather than the marking one — a lever whose input is never
/// written on the path the workload takes — and it was found the same way, by
/// asking who calls the thing rather than by running the experiment.
///
/// # Why a young pause is a legitimate sample, and why it is still opt-in
///
/// The estimate's definition is "whole pause over bytes copied", deliberately
/// including the fixed scan overhead so it slightly OVER-estimates and
/// therefore produces a smaller, safer collection set. That reasoning is not
/// specific to mixed pauses; it holds for any evacuating pause. A young pause
/// copies the young half of what a mixed pause copies and pays the same roots,
/// remembered-set and fix-up overheads, so its ratio is if anything the more
/// conservative of the two.
///
/// It is opt-in anyway, because arming it changes what `select_old_regions_for_mixed_gc`
/// budgets with on a workload that has BOTH young and mixed pauses, and the
/// direction depends on the workload rather than on an argument: a young pause
/// with a small live set reads cheap per byte (shrinking the estimate, growing
/// the mixed CSet) and one that barely copies reads expensive. That is a
/// throughput-versus-pause trade and it needs a soak, which this lane did not
/// run. What the flag buys today is the ability to take P1's offline step at
/// all.
///
/// Drain passes are excluded by `teaches_controllers` at the call site, before
/// this flag is consulted — see [`g1_drain_not_a_pause`]. Today the drain
/// cannot reach `update_evac_cost` because only the mixed drivers call it;
/// this flag would give it a route, and the gate is written in from the start
/// rather than discovered later.
///
/// **Superseded by [`g1_evac_cost_real`] (LANE W5-A), and kept rather than
/// retired.** Wave 5 found that the quantity this flag measures is not the
/// quantity the budget spends — a whole pause over its copied bytes charges the
/// roots scan, the rset walk and the fix-up a second time, on top of a budget
/// that has already subtracted them — so the 27 ns/byte this probe settled on
/// is not an estimate of the copy cost, it is a pause divided by a small
/// number. The flag stays because wave 4's measurements were taken against it
/// and a retired flag makes them unreproducible; with
/// `CRATONVM_G1_EVAC_COST_REAL` on it is inert by construction.
pub fn g1_evac_cost_from_young() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => matches!(v.trim(), "1" | "true" | "TRUE" | "yes"),
        Err(_) => false,
    }
}

/// LANE W5-A — is the mixed copy budget's per-byte term a MEASUREMENT
/// (`CRATONVM_G1_EVAC_COST_REAL=1`), or the constructor constant it has been
/// since Step 7 (the default, and the pre-wave-5 behaviour)?
///
/// # The defect, in one paragraph
///
/// `old_cset_copy_budget_ns` prices a candidate old region at
/// `live_bytes × evac_ns_per_byte`; that product is the only thing standing
/// between `max_gc_pause_ms` and a mixed collection set of any size.
/// `update_evac_cost` — the sole writer of that term — is called from
/// `mixed_collection` and `mixed_collection_parallel` and from nowhere else.
/// A mixed collection needs a completed mark cycle, so on a young-only run the
/// term holds `4` for the life of the process; and on a run that DOES go mixed,
/// it still holds `4` for the first mixed pause, which is the pause with the
/// largest candidate set and therefore the one the goal was written for. W4-B's
/// probe put the true figure at least 7x higher. A pause goal enforced with a
/// term 7x low is not a bound, it is a decoration.
///
/// # The three things this flag changes, and why each is a separate defect
///
/// 1. **A young pause takes a sample** — into its OWN term
///    ([`G1Collector::young_evac_ns_per_byte`]), because young copying and old
///    copying are different populations, not two draws from one. The old
///    budget uses the young term only until a mixed pause has measured a real
///    one ([`G1Collector::old_evac_ns_per_byte`]).
/// 2. **The numerator is the closure, not the pause.** The budget has already
///    subtracted the fixed phases before it prices a byte, so a whole-pause
///    numerator double-charges them by an arbitrary, workload-set multiplier.
///    See [`G1Collector::fold_evac_cost`].
/// 3. **The first real sample is adopted whole.** A 1/8 EMA walking up from a
///    guess needs ~20 samples; W4-B's probe was still climbing when the run
///    ended. Tracking drift and replacing a guess are different operations and
///    the sample count is what tells them apart.
///
/// # Why it ships OFF, and it is NOT the reason that was expected
///
/// The argument written here first was "arming it can only shrink a mixed
/// collection set, which is safe for the pause goal and unsafe for throughput".
/// **The measurement falsified that**, and the correction is the more useful
/// half of this doc: the measured closure cost per live byte is 1 ns/byte on
/// `G1ChurnPauseProbe`, 3 on `HumongousRealChurn`, 4 on `G1OldBurstProbe` and
/// 145 on `G1RsetWideProbe` — a 145x range straddling the shipped constant in
/// BOTH directions. So arming this does not move the collection set one way; it
/// moves it whichever way that workload's real copy cost lies from 4, and on at
/// least one probe that is toward a LARGER set.
///
/// It ships off because of the engagement count, which is the number that
/// actually decides it: across the probe battery, **no run took a mixed pause
/// at all**, so nothing in the battery reads this term and all three arms
/// produce identical work (same checksum, same pause counts, no goal overruns
/// anywhere). A change that is provably inert on every workload available to
/// measure it has no number saying to default it on. Where it WOULD bind — a
/// large heap with a pointer-dense live set, where the measured term is 36x the
/// constant — the consequence is a mixed collection set cut from ~40 regions to
/// ~2, which is a large throughput change resting on a projection rather than
/// on an observed mixed pause.
///
/// See `docs/feature-designs/concurrent-gc-maturation.md`. The
/// question asked there is whether `max_gc_pause_ms` BINDS, not whether the run
/// is faster: a term that stops a collection set from overrunning its goal is
/// doing its job even when the wall clock is unchanged.
pub fn g1_evac_cost_real() -> bool {
    match Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        Ok(v) => matches!(v.trim(), "1" | "true" | "TRUE" | "yes"),
        Err(_) => false,
    }
}



/// Drain-pass records refused by the adaptive controllers under
/// [`g1_drain_not_a_pause`], process-wide.
///
/// The denominator §7.3 of `orchestrator-wave-1-measurements.md` asks for: a
/// run whose `[GC] g1 young-sizing:` line says `drain_samples_refused=0` did
/// not exercise this change at all, and its young target is the same number
/// both arms would have produced. Without it, the flag-on and flag-off arms of
/// a workload that never reaches an evacuation failure are indistinguishable
/// for the right reason and look indistinguishable for the wrong one.
pub static DRAIN_CONTROLLER_SAMPLES_REFUSED: AtomicU64 = AtomicU64::new(0);

/// [`DRAIN_CONTROLLER_SAMPLES_REFUSED`], for `print_gc_summary` and tests.
pub fn drain_controller_samples_refused() -> u64 {
    DRAIN_CONTROLLER_SAMPLES_REFUSED.load(Ordering::Relaxed)
}

/// Zero [`DRAIN_CONTROLLER_SAMPLES_REFUSED`]. Tests only — the counter is
/// process-wide, so a test that asserts on a delta has to establish its own
/// zero the way `reset_drain_fixup_totals` lets the drain-fixup tests do.
pub fn reset_drain_controller_samples_refused() {
    DRAIN_CONTROLLER_SAMPLES_REFUSED.store(0, Ordering::Relaxed);
}



/// How many of a run's first pauses form the card-screen trend's left
/// endpoint (`G1Collector::card_first_window_scanned`).
///
/// 64 is chosen to be long enough that a single warm-up pause cannot define the
/// baseline and short enough that it closes within the first second or two of
/// any workload that collects at all — the trend this exists to measure runs
/// over an hour, so the baseline wants to be an early sample, not a long one.
pub const G1_CARD_TREND_WINDOW: u64 = 64;

/// GC overhead at or above which the soft capacity grows.
///
/// gci i13/ef finding 7: G1's `GCTimeRatio` is 12 (`G1Arguments::initialize`
/// sets it when the flag is defaulted), so the threshold is 1/(1+12) = 7.69 %
/// of wall clock. This constant used to be 5 % and cite `GCTimeRatio=19`,
/// which is the Serial / Parallel default, not G1's.
pub const G1_RESIZE_OVERHEAD_HIGH_PPM: u64 = 1_000_000 / 13;
/// GC overhead below which a shrink is even considered: 1 %. Deliberately far
/// below the growth threshold rather than just under it — the gap IS the
/// hysteresis on the overhead axis, and a heap that is collecting 3 % of the
/// time is doing its job, not wasting memory.
pub const G1_RESIZE_OVERHEAD_LOW_PPM: u64 = 10_000;
/// gci i13/ef findings 5-6: HotSpot's `-XX:MinHeapFreeRatio` default. The
/// capacity grows when less than this percent of it is free (occupancy above
/// `100 - 40 = 60 %`), to the capacity that leaves exactly this much free.
pub const G1_DEFAULT_MIN_HEAP_FREE_RATIO: u8 = 40;
/// gci i13/ef findings 5-6: HotSpot's `-XX:MaxHeapFreeRatio` default. The
/// capacity shrinks when more than this percent of it is free (occupancy below
/// `100 - 70 = 30 %`), no lower than the capacity that leaves this much free.
///
/// These two replace the occupancy band 70 / 40 with a settle point of 60 this
/// policy shipped with: those were the two ratios transcribed as OCCUPANCY,
/// which grew at 70 % (HotSpot: above 60 %) and shrank below 40 % (HotSpot:
/// below 30 %). The hysteresis the old settle point provided is now the gap
/// between the two ratios: a grow lands at `100 - min_free` occupancy, which
/// is above the shrink edge `100 - max_free`, and a shrink lands at the shrink
/// edge, which is below the grow edge.
pub const G1_DEFAULT_MAX_HEAP_FREE_RATIO: u8 = 70;
/// Step sizes, as a percent of the current target. Growth is the larger step
/// on purpose: a heap that is collecting too often is costing the program time
/// NOW, and under-shooting the grow costs another full GC interval to discover.
pub const G1_RESIZE_GROW_STEP_PCT: usize = 20;
pub const G1_RESIZE_SHRINK_STEP_PCT: usize = 10;
/// Consecutive qualifying pauses before a shrink is allowed. The hysteresis on
/// the TIME axis: a burst that has just ended looks exactly like a program that
/// never needed the memory, and the only thing that tells them apart is how
/// long the quiet lasts.
pub const G1_RESIZE_SHRINK_STREAK: u32 = 4;

/// What one pause's worth of evidence says the soft heap capacity should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapResize {
    /// Inside the band, or blocked by the floor, the ceiling or the streak.
    Hold,
    /// Raise the soft capacity to this many regions.
    Grow(usize),
    /// Lower the soft capacity to this many regions.
    Shrink(usize),
}

/// The heap-sizing policy, as a pure function of the five numbers it decides
/// on. `target`, `used`, `floor` and `ceiling` are region counts.
///
/// Free-standing and total (no clock, no atomics, no `&self`) so the property
/// that actually matters can be asserted rather than argued: apply it to its
/// own output and it must reach a fixed point, which is the formal statement of
/// "it does not oscillate". `g1_w2e_heap_resize.rs` drives exactly that.
///
/// # The three hysteresis devices, and why one is not enough
///
/// * **A dead band on occupancy** (`LOW..HIGH`). Necessary but not sufficient:
///   a shrink that stops the moment it leaves the low band lands ON the edge,
///   and the next allocation crosses back.
/// * **A settle point strictly inside the band** (`SETTLE`). This is what makes
///   a shrink land somewhere it can stay. It is also why a shrink is capped
///   BELOW by `used * 100 / SETTLE` and not by `used * 100 / HIGH`.
/// * **A streak on the time axis** (`STREAK` consecutive qualifying pauses).
///   Occupancy sampled at one pause boundary says nothing about the burst that
///   ended a millisecond earlier.
///
/// Growth has no streak requirement and that asymmetry is deliberate: holding
/// memory a program does not need is a cost, and collecting a program's heap
/// twice as often as it needs is a worse one.
///
/// gci i13/ef findings 5 and 7: the occupancy band is now HotSpot's free
/// ratios (`min_free` / `max_free`, `-XX:MinHeapFreeRatio` /
/// `-XX:MaxHeapFreeRatio`, 40 / 70 by default: grow above 60 % occupancy to
/// 60 %, shrink below 30 % to 30 %) and the overhead threshold is G1's
/// `GCTimeRatio = 12`. The "settle point" above is the capacity each ratio
/// desires; the dead band is the gap between them.
#[allow(clippy::too_many_arguments)]
pub fn heap_resize_decision(
    target: usize,
    used: usize,
    floor: usize,
    ceiling: usize,
    overhead_ppm: u64,
    shrink_streak: u32,
    min_free: u8,
    max_free: u8,
) -> HeapResize {
    let ceiling = ceiling.max(1);
    let floor = floor.clamp(1, ceiling);
    let target = target.clamp(floor, ceiling);

    // ---- grow -------------------------------------------------------------
    // gci i13/ef finding 5: the occupancy arm is HotSpot's `MinHeapFreeRatio`
    // rule (less than `min_free` % of the capacity free), not "occupancy >=
    // 70 %".
    if target < ceiling
        && (overhead_ppm >= G1_RESIZE_OVERHEAD_HIGH_PPM
            || free_ratio_wants_grow(target, used, min_free))
    {
        let pct = if overhead_ppm >= G1_RESIZE_OVERHEAD_HIGH_PPM {
            let mult = (overhead_ppm / G1_RESIZE_OVERHEAD_HIGH_PPM).clamp(1, 5) as usize;
            (G1_RESIZE_GROW_STEP_PCT * mult).min(100)
        } else {
            G1_RESIZE_GROW_STEP_PCT
        };
        let step = (target * pct / 100).max(1);
        // AT LEAST enough to put the CURRENT live set at the settle point, not
        // just one step.
        //
        // A fixed percentage step is fine when the trigger is the overhead
        // axis, and it is a pause storm when the trigger is occupancy. Suppose
        // a program's live set jumps from 4 regions to 20 while the soft
        // capacity sits at 6. `needs_gc` now fires on every allocation — its
        // free fraction is measured against the capacity — and a 20 % step
        // needs ELEVEN pauses to climb from 6 to the ~33 that would put 20
        // regions back inside the band. Every one of those pauses is a real
        // collection doing real work, so it is not the "trigger the collector
        // cannot satisfy" shape `young_target_regions` documents, but it is ten
        // pauses nobody asked for.
        //
        // Jumping straight to the settle point is also the more honest reading
        // of the rule: growth exists to put occupancy back inside the band, and
        // the band's centre is where a shrink aims too, so both directions
        // target the same resting state and the policy has one fixed point
        // rather than two.
        //
        // gci i13/ef finding 5: the settle point is HotSpot's
        // `minimum_desired_capacity` (`used / (1 - MinHeapFreeRatio)`), i.e.
        // occupancy `100 - min_free` (60 %), no longer a fixed 60 % that
        // happened to equal it at the defaults.
        let settle_target = free_ratio_grow_target(used, min_free, ceiling);
        let next = target.saturating_add(step).max(settle_target).min(ceiling);
        if next > target {
            return HeapResize::Grow(next);
        }
    }

    // ---- shrink -----------------------------------------------------------
    if target > floor
        && shrink_streak >= G1_RESIZE_SHRINK_STREAK
        && free_ratio_allows_shrink(target, used, max_free)
        && overhead_ppm < G1_RESIZE_OVERHEAD_LOW_PPM
    {
        let step = (target * G1_RESIZE_SHRINK_STEP_PCT / 100).max(1);
        // Three lower bounds, and the tightest of them wins: never below
        // `-Xms`, never below one step (so the policy cannot free-fall), and
        // never below the capacity at which the CURRENT live set leaves
        // exactly `max_free` % free (HotSpot's `maximum_desired_capacity`,
        // gci i13/ef finding 5) — at which the shrink rule no longer fires and
        // the grow rule does not either (`100 - max_free < 100 - min_free`).
        let settle_floor = free_ratio_shrink_floor(used, max_free, ceiling);
        let next = target
            .saturating_sub(step)
            .max(settle_floor)
            .max(floor)
            .min(ceiling);
        // gci i13/ef: never shrink to a capacity the grow rule would undo.
        // With region granularity and ratios close together (`-XX:` values
        // one apart) a rounded shrink floor can sit just above the grow edge.
        if next < target && !free_ratio_wants_grow(next, used, min_free) {
            return HeapResize::Shrink(next);
        }
    }

    HeapResize::Hold
}

/// Test hook for [`heap_resize_decision`]. Returns `(direction, new_target)`
/// where direction is `-1` shrink, `0` hold, `1` grow — a shape a test can
/// iterate to a fixed point without naming a private enum.
pub fn g1_dbg_heap_resize_decision(
    target: usize,
    used: usize,
    floor: usize,
    ceiling: usize,
    overhead_ppm: u64,
    shrink_streak: u32,
) -> (i32, usize) {
    g1_dbg_heap_resize_decision_with_ratios(
        target,
        used,
        floor,
        ceiling,
        overhead_ppm,
        shrink_streak,
        G1_DEFAULT_MIN_HEAP_FREE_RATIO,
        G1_DEFAULT_MAX_HEAP_FREE_RATIO,
    )
}

/// [`g1_dbg_heap_resize_decision`] with `-XX:MinHeapFreeRatio` /
/// `-XX:MaxHeapFreeRatio` as parameters (gci i13/ef findings 5-6).
#[allow(clippy::too_many_arguments)]
pub fn g1_dbg_heap_resize_decision_with_ratios(
    target: usize,
    used: usize,
    floor: usize,
    ceiling: usize,
    overhead_ppm: u64,
    shrink_streak: u32,
    min_free: u8,
    max_free: u8,
) -> (i32, usize) {
    heap_resize_tuple(
        heap_resize_decision(
            target,
            used,
            floor,
            ceiling,
            overhead_ppm,
            shrink_streak,
            min_free,
            max_free,
        ),
        target,
        floor,
        ceiling,
    )
}

/// `(direction, target)` for a [`HeapResize`]: `-1` shrink, `0` hold (the
/// clamped current target), `1` grow.
pub fn heap_resize_tuple(d: HeapResize, target: usize, floor: usize, ceiling: usize) -> (i32, usize) {
    match d {
        HeapResize::Hold => (
            0,
            target.clamp(floor.clamp(1, ceiling.max(1)), ceiling.max(1)),
        ),
        HeapResize::Grow(n) => (1, n),
        HeapResize::Shrink(n) => (-1, n),
    }
}

/// gci i13/ef finding 5: HotSpot's expansion test -- less than `min_free` % of
/// the capacity is free, i.e. `target < used / (1 - min_free/100)`, in exact
/// integer arithmetic (`target * (100 - min_free) < used * 100`).
///
/// `min_free = 100` asks for an infinitely free heap: grow whenever anything
/// is used.
pub fn free_ratio_wants_grow(target: usize, used: usize, min_free: u8) -> bool {
    let occ = 100 - usize::from(min_free.min(100));
    target.saturating_mul(occ) < used.saturating_mul(100)
}

/// gci i13/ef finding 5: HotSpot's `minimum_desired_capacity` in regions --
/// the smallest capacity that leaves at least `min_free` % free, rounded UP so
/// it satisfies the rule ([`free_ratio_wants_grow`] is false at it). The
/// ceiling when `min_free = 100`.
pub fn free_ratio_grow_target(used: usize, min_free: u8, ceiling: usize) -> usize {
    let occ = 100 - usize::from(min_free.min(100));
    if occ == 0 {
        return ceiling;
    }
    used.saturating_mul(100).div_ceil(occ)
}

/// gci i13/ef finding 5: HotSpot's shrink test -- more than `max_free` % of the
/// capacity is free, i.e. `target > used / (1 - max_free/100)`
/// (`target * (100 - max_free) > used * 100`). Never at `max_free = 100`.
pub fn free_ratio_allows_shrink(target: usize, used: usize, max_free: u8) -> bool {
    let occ = 100 - usize::from(max_free.min(100));
    occ > 0 && target.saturating_mul(occ) > used.saturating_mul(100)
}

/// gci i13/ef finding 5: HotSpot's `maximum_desired_capacity` in regions --
/// the largest capacity that leaves at most `max_free` % free, rounded DOWN so
/// [`free_ratio_allows_shrink`] is false at it. The ceiling at `max_free = 100`.
pub fn free_ratio_shrink_floor(used: usize, max_free: u8, ceiling: usize) -> usize {
    let occ = 100 - usize::from(max_free.min(100));
    if occ == 0 {
        return ceiling;
    }
    used.saturating_mul(100) / occ
}

/// gci i13/ef finding 8: the sizing decision at a Remark / cleanup, HotSpot's
/// `G1HeapSizingPolicy::full_collection_resize_amount` (also run after
/// Remark): from this one census, grow to `minimum_desired_capacity` when the
/// capacity is below it, else shrink to `maximum_desired_capacity` when the
/// capacity is above it, both clamped to `[floor, ceiling]`. No streak and no
/// overhead term -- those belong to the evacuation-pause arm
/// ([`heap_resize_decision`]).
pub fn heap_resize_at_remark(
    target: usize,
    used: usize,
    floor: usize,
    ceiling: usize,
    min_free: u8,
    max_free: u8,
) -> HeapResize {
    let ceiling = ceiling.max(1);
    let floor = floor.clamp(1, ceiling);
    let target = target.clamp(floor, ceiling);
    if free_ratio_wants_grow(target, used, min_free) {
        let next = free_ratio_grow_target(used, min_free, ceiling).clamp(floor, ceiling);
        if next > target {
            return HeapResize::Grow(next);
        }
    }
    if free_ratio_allows_shrink(target, used, max_free) {
        let next = free_ratio_shrink_floor(used, max_free, ceiling).clamp(floor, ceiling);
        if next < target && !free_ratio_wants_grow(next, used, min_free) {
            return HeapResize::Shrink(next);
        }
    }
    HeapResize::Hold
}

/// Test hook for [`heap_resize_at_remark`], in the shape of
/// [`g1_dbg_heap_resize_decision`].
pub fn g1_dbg_heap_resize_at_remark(
    target: usize,
    used: usize,
    floor: usize,
    ceiling: usize,
    min_free: u8,
    max_free: u8,
) -> (i32, usize) {
    heap_resize_tuple(
        heap_resize_at_remark(target, used, floor, ceiling, min_free, max_free),
        target,
        floor,
        ceiling,
    )
}

/// gci i13/ef finding 8: the soft capacity a new collector starts at -- the
/// `-Xms` floor (clamped into `[1, ceiling]`) under `CRATONVM_G1_HEAP_RESIZE`,
/// HotSpot's capacity starting at `InitialHeapSize`; the ceiling without it,
/// exactly as before.
pub fn initial_heap_target_regions(enabled: bool, floor: usize, ceiling: usize) -> usize {
    let ceiling = ceiling.max(1);
    if enabled {
        floor.clamp(1, ceiling)
    } else {
        ceiling
    }
}

impl G1Collector {
    /// The soft heap capacity in regions. Diagnostics, tests and `needs_gc`.
    ///
    /// Always the region-table length when `CRATONVM_G1_HEAP_RESIZE` is off, so
    /// every consumer can read it unconditionally and the flag lives in exactly
    /// one place.
    pub fn heap_target_regions(&self) -> usize {
        self.heap_target_regions_within(g1_heap_resize_enabled())
    }

    /// [`Self::heap_target_regions`] with the opt-in as a PARAMETER.
    ///
    /// Split out for the reason `uncommit_trailing_free_regions_within` is: the
    /// flag is latched once per process, so a test that went through the
    /// wrapper could only ever exercise whichever arm the ambient environment
    /// selected — and for an opt-in flag that is always the arm that does
    /// nothing.
    pub fn heap_target_regions_within(&self, enabled: bool) -> usize {
        let ceiling = self.region_table_len().max(1);
        if !enabled {
            return ceiling;
        }
        self.heap_target_regions
            .load(Ordering::Relaxed)
            .clamp(self.heap_floor_regions(), ceiling)
    }

    /// The soft heap capacity in bytes — the number `heap_capacity()` reports
    /// and `Runtime.totalMemory()` is built from.
    pub fn heap_target_bytes(&self) -> usize {
        self.heap_target_bytes_within(g1_heap_resize_enabled())
    }

    /// [`Self::heap_target_bytes`] with the opt-in as a parameter.
    pub fn heap_target_bytes_within(&self, enabled: bool) -> usize {
        self.heap_target_regions_within(enabled)
            .saturating_mul(self.config.region_size)
    }

    /// `-Xms` (or the startup ergonomic) expressed in regions: the floor the
    /// policy may never size below, because an operator who asked for an
    /// initial heap asked not to pay for growing back into it.
    pub fn heap_floor_regions(&self) -> usize {
        let region_size = self.config.region_size.max(1);
        let ceiling = (self.config.heap_size / region_size).max(1);
        (self.initial_commit_bytes.div_ceil(region_size)).clamp(1, ceiling)
    }

    /// Fold one pause into the rolling GC-overhead estimate and return it.
    ///
    /// The quantity is `pause / (pause + mutator)` over the interval that ENDS
    /// with this pause, smoothed 7/8. The first pause of a process has no
    /// preceding interval to measure against, so it contributes nothing and
    /// only starts the clock — which is the conservative direction: an
    /// unmeasured overhead reads as low, and a low overhead never grows the
    /// heap.
    ///
    /// LANE W4-B — `fold` says whether this record contributes a SAMPLE, or
    /// only closes the interval.
    ///
    /// `fold == false` advances the clock and folds nothing: the interval that
    /// ends here leaves both the numerator and the denominator, exactly as the
    /// first pause of a process does. That is the only sound treatment of a
    /// drain pass, whose interval contains no mutator time by construction and
    /// would therefore read as 100 % overhead on every workload. Dropping the
    /// interval rather than attributing it either way leaves the ratio over
    /// the remaining intervals undisturbed.
    pub fn note_pause_wall_time(&self, pause_us: u64, fold: bool) -> u64 {
        let now = std::time::Instant::now();
        let previous = {
            let mut slot = self.last_pause_end.lock();
            slot.replace(now)
        };
        let Some(previous) = previous else {
            return self.gc_overhead_ppm.load(Ordering::Relaxed);
        };
        if !fold {
            return self.gc_overhead_ppm.load(Ordering::Relaxed);
        }
        let interval_us = now.saturating_duration_since(previous).as_micros() as u64;
        // `interval_us` spans the mutator time AND this pause (it is measured
        // between pause ENDS), so it is the denominator directly. A zero or
        // sub-microsecond interval carries no information and must not be
        // allowed to divide.
        if interval_us == 0 {
            return self.gc_overhead_ppm.load(Ordering::Relaxed);
        }
        let observed = (pause_us.saturating_mul(1_000_000) / interval_us).min(1_000_000);
        let prev = self.gc_overhead_ppm.load(Ordering::Relaxed);
        let next = if prev == 0 {
            observed
        } else {
            prev.saturating_mul(7).saturating_add(observed) / 8        };
        self.gc_overhead_ppm.store(next, Ordering::Relaxed);
        next
    }

    /// May this pause hand pages back to the OS?
    ///
    /// The gate `lane-e-heap-never-shrinks-and-never-resizes-to-occupancy.md`
    /// names, minus its evacuation-failure term. A self-forwarded object keeps
    /// its CSet region, and a kept region is NOT Free, so it is below the
    /// trailing run by construction and cannot be inside what this releases.
    ///
    /// What is left is the SATB argument, and it is the one that matters: a
    /// gray address or a queued snapshot entry names a raw heap address the
    /// marker will dereference later. Freeing the region under it is already
    /// wrong; DECOMMITTING it turns a stale read into a fault, which is the
    /// difference between a latent bug and a crash. So the shrink runs only
    /// with the phase Idle, the SATB queue inactive and the gray set empty.
    pub fn heap_shrink_is_safe(&self) -> bool {
        self.gc_state.phase() == ConcurrentGcPhase::Idle
            && !self.satb_queue.is_active()
            && self.mark_worklist.lock().is_empty()
    }

    /// The core of [`Self::uncommit_trailing_free_regions_within`], taking the
    /// number of regions to KEEP rather than the region table.
    ///
    /// Split out so the pause-driven caller can reach it: every evacuation
    /// driver funnels through `record_collection_with_phases` while holding the
    /// regions guard as a WRITER, so it cannot borrow the table again — but the
    /// per-pause census it already carries has counted the highest non-Free
    /// region for it (`G1PausePhases::highest_used_region`).
    pub fn uncommit_above_region_within(&self, keep_regions: usize, enabled: bool) -> usize {
        if !enabled {
            return 0;
        }
        let region_size = self.config.region_size;
        if region_size == 0 {
            return 0;
        }
        self.uncommit_to_bytes(
            keep_regions
                .saturating_mul(region_size)
                .max(self.shrink_floor_bytes()),
        )
    }

    /// Narrow the published JIT read bounds to `want` and decommit down to it.
    /// Returns the bytes released.
    ///
    /// The one place that unmaps heap pages. `want` must already be `>=` every
    /// floor that applies; this function does not re-derive them, because the
    /// two callers derive them differently (the F-16 shrink from `-Xms`, the
    /// sizing policy from the soft capacity) and a third opinion here would be
    /// a third source of truth for how small the heap may get.
    pub fn uncommit_to_bytes(&self, want: usize) -> usize {
        if !self.arena.is_reserved() {
            return 0;
        }
        let committed = self.arena.committed_len();
        if want >= committed {
            return 0;
        }

        // Order is the whole safety argument: publish the narrower bound, THEN
        // unmap. Doing it the other way leaves a window in which compiled code
        // is told it may load from pages that are gone.
        cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(0, self.arena_base, self.arena_base + want);
        if self.arena.decommit_to(want).is_none() {
            // The OS refused. The bound is already narrow, which is the
            // fail-safe direction (more helper calls, no faults); restore it to
            // what is actually committed rather than leaving it pessimistic.
            cratonvm_gc_core::jit_bounds::publish_jit_read_bounds(
                0,
                self.arena_base,
                self.arena_base + self.arena.committed_len(),
            );
            return 0;
        }
        let released = committed - self.arena.committed_len();
        self.heap_uncommitted_bytes
            .fetch_add(released as u64, Ordering::Relaxed);
        released
    }

    /// The lowest committed prefix the shrink may leave behind.
    ///
    /// `-Xms` always, and — once the sizing policy is armed — the soft capacity
    /// as well, because that is what makes a lowered target actually give
    /// memory back rather than merely promise to collect sooner. With the
    /// policy off this is exactly the `initial_commit_bytes` floor the F-16
    /// shrink has always used.
    pub fn shrink_floor_bytes(&self) -> usize {
        let region_size = self.config.region_size.max(1);
        let reserved = self.arena.reserved_len();
        let base = self.initial_commit_bytes.max(region_size).min(reserved);
        if !g1_heap_resize_enabled() {
            return base;
        }
        base.max(self.heap_target_bytes()).min(reserved)
    }

    /// The shrink floor with the sizing policy forced on, for
    /// [`Self::dbg_apply_heap_resize`]. Same three clamps.
    pub fn shrink_floor_bytes_within(&self, enabled: bool) -> usize {
        let region_size = self.config.region_size.max(1);
        let reserved = self.arena.reserved_len();
        let base = self.initial_commit_bytes.max(region_size).min(reserved);
        if !enabled {
            return base;
        }
        base.max(self.heap_target_bytes_within(true)).min(reserved)
    }

    /// Run the sizing policy for one completed pause, from the census that
    /// pause already took. Returns `(direction, new_target_regions,
    /// bytes_released)` when the capacity moved.
    ///
    /// Called from `record_collection_with_phases`, which is the ONE function
    /// every evacuation driver funnels through — young serial, young parallel,
    /// both mixed twins and the kept-region drain — so a driver added later
    /// cannot forget to size the heap, the same argument
    /// `update_tenuring_threshold` is placed there on.
    pub fn apply_heap_resize(&self, phases: &G1PausePhases) -> Option<(&'static str, usize, usize)> {
        self.apply_heap_resize_within(phases, g1_heap_resize_enabled())
    }

    /// [`Self::apply_heap_resize`] with the opt-in as a parameter.
    pub fn apply_heap_resize_within(
        &self,
        phases: &G1PausePhases,
        enabled: bool,
    ) -> Option<(&'static str, usize, usize)> {
        if !enabled {
            return None;
        }
        let ceiling = phases.total_regions as usize;
        if ceiling == 0 {
            // An uninstrumented pause (`record_collection`, i.e. a test): no
            // census, so no evidence. Sizing on a zero would read the heap as
            // empty and shrink it, which is the one direction a missing
            // measurement must never take.
            return None;
        }
        let used = ceiling.saturating_sub(phases.free_regions as usize);
        let floor = self.heap_floor_regions();
        let target = self.heap_target_regions_within(true);
        let overhead = self.gc_overhead_ppm.load(Ordering::Relaxed);

        // The streak is maintained HERE rather than inside the pure decision so
        // the decision stays a function of its arguments. The band it counts is
        // the same one the decision tests, so the two cannot drift: a pause
        // that qualifies extends the streak, and any pause that does not
        // resets it to zero.
        let (min_free, max_free) = self.heap_free_ratios();
        let in_band = free_ratio_allows_shrink(target, used, max_free)
            && overhead < G1_RESIZE_OVERHEAD_LOW_PPM;
        let streak = if in_band {
            self.heap_shrink_streak
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1)
        } else {
            self.heap_shrink_streak.store(0, Ordering::Relaxed);
            0
        };

        let decision = heap_resize_decision(
            target, used, floor, ceiling, overhead, streak, min_free, max_free,
        );
        self.commit_heap_resize(decision, phases)
    }

    /// gci i13/ef findings 5-6: `(MinHeapFreeRatio, MaxHeapFreeRatio)` the
    /// sizing policy uses -- HotSpot's 40 / 70 unless the VM handed this
    /// collector the `-XX:` flags ([`Self::set_heap_free_ratios`]).
    pub fn heap_free_ratios(&self) -> (u8, u8) {
        (
            self.heap_min_free_ratio.load(Ordering::Relaxed),
            self.heap_max_free_ratio.load(Ordering::Relaxed),
        )
    }

    /// gci i13/ef finding 6: `-XX:MinHeapFreeRatio` / `-XX:MaxHeapFreeRatio`
    /// for G1's sizing policy (read only under `CRATONVM_G1_HEAP_RESIZE`).
    /// `None` keeps the current value. The launcher has already validated the
    /// pair as HotSpot does (`heap_free_ratios_from_flags`); this clamps to
    /// `0..=100` and keeps `max >= min` regardless, so a caller that skipped
    /// the validation cannot make the policy oscillate.
    pub fn set_heap_free_ratios(&self, min: Option<u8>, max: Option<u8>) {
        let (cur_min, cur_max) = self.heap_free_ratios();
        let min = min.unwrap_or(cur_min).min(100);
        let max = max.unwrap_or(cur_max).min(100).max(min);
        self.heap_min_free_ratio.store(min, Ordering::Relaxed);
        self.heap_max_free_ratio.store(max, Ordering::Relaxed);
    }

    /// Apply one sizing decision: store the new soft capacity, count it, and
    /// on a shrink hand the trailing Free run back when that is safe. Shared by
    /// the evacuation-pause arm ([`Self::apply_heap_resize_within`]) and the
    /// Remark arm ([`Self::consider_heap_resize_at_cleanup`], gci i13/ef
    /// finding 8). Returns `(direction, new_target_regions, bytes_released)`.
    pub fn commit_heap_resize(
        &self,
        decision: HeapResize,
        phases: &G1PausePhases,
    ) -> Option<(&'static str, usize, usize)> {
        // gci i45/gy: the capacity before this decision, for the young
        // sizer's heap-size-change hook (`young_bounds_heap_size_changed`).
        // Every caller reaches here only with the opt-in armed.
        let old_regions = self.heap_target_regions_within(true);
        match decision {
            HeapResize::Hold => None,
            HeapResize::Grow(next) => {
                // gci i45/gy: the young target first (see the hook's doc).
                self.young_bounds_heap_size_changed(old_regions, next);
                self.heap_target_regions.store(next, Ordering::Relaxed);
                self.heap_grow_events.fetch_add(1, Ordering::Relaxed);
                // Nothing is committed here. Growth in this collector means
                // "allowed to fill more before collecting"; the pages arrive
                // when `claim_free_region*` asks for them, which is the lazy
                // commit F-16 built and the only shape `publish_jit_read_bounds`
                // can describe.
                Some(("grow", next, 0))
            }
            HeapResize::Shrink(next) => {
                self.heap_target_regions.store(next, Ordering::Relaxed);
                // gci i45/gy: the young target after the capacity (see the hook's doc).
                self.young_bounds_heap_size_changed(old_regions, next);
                self.heap_shrink_events.fetch_add(1, Ordering::Relaxed);
                // A lowered target is only half of giving memory back; the
                // other half is the trailing Free run above the highest region
                // still in use, and this is the point in a pause at which that
                // run is largest — Phase 5 has just reset the collection set.
                let released = if self.heap_shrink_is_safe() {
                    // The sizing policy IS the opt-in for its own shrink:
                    // `CRATONVM_G1_UNCOMMIT` remains the switch for the
                    // cleanup-driven F-16 shrink, and arming the policy without
                    // also arming the release would leave it able to promise
                    // memory back and unable to give it.
                    let want = phases.highest_used_region as usize;
                    let floor = self.shrink_floor_bytes_within(true);
                    self.uncommit_to_bytes(want.saturating_mul(self.config.region_size).max(floor))
                } else {
                    0
                };
                Some(("shrink", next, released))
            }
        }
    }

    /// The sizing policy's state, for the `[GC-STAT]` line.
    ///
    /// The finding this closes asked for the decision to be "visible in the
    /// pause log rather than silent". A resize that only moves an atomic is
    /// indistinguishable, from outside, from a resize that never happened — and
    /// the failure mode worth catching is OSCILLATION, which needs the grow and
    /// shrink counts side by side rather than one event at a time.
    pub fn heap_resize_note(&self) -> String {
        if !g1_heap_resize_enabled() {
            return String::new();
        }
        format!(
            " heap_target_regions={} heap_target_bytes={} heap_floor_regions={} gc_overhead_ppm={} shrink_streak={} heap_grows={} heap_shrinks={} uncommitted_bytes={} committed_bytes={}",
            self.heap_target_regions(),
            self.heap_target_bytes(),
            self.heap_floor_regions(),
            self.gc_overhead_ppm.load(Ordering::Relaxed),
            self.heap_shrink_streak.load(Ordering::Relaxed),
            self.heap_grow_events.load(Ordering::Relaxed),
            self.heap_shrink_events.load(Ordering::Relaxed),
            self.heap_uncommitted_bytes.load(Ordering::Relaxed),
            self.committed_bytes(),
        )
    }

    /// `(grows, shrinks, uncommitted_bytes, overhead_ppm)` — the sizing
    /// policy's run totals, for `print_gc_summary` and for tests.
    pub fn heap_resize_counts(&self) -> (u64, u64, u64, u64) {
        (
            self.heap_grow_events.load(Ordering::Relaxed),
            self.heap_shrink_events.load(Ordering::Relaxed),
            self.heap_uncommitted_bytes.load(Ordering::Relaxed),
            self.gc_overhead_ppm.load(Ordering::Relaxed),
        )
    }

    /// LANE E resize hook — the cleanup-time arm of the same policy.
    ///
    /// A concurrent-mark cleanup is the other moment at which a large amount of
    /// memory can come back at once (the in-place Old frees and the dead
    /// humongous reclaim), and unlike an evacuation pause it has a FRESH
    /// whole-heap census in hand. It is also, per the finding, the place the
    /// shrink has always been called from — so running the policy here as well
    /// costs one pass over a table cleanup has already walked, and keeps the
    /// soft capacity honest for the workloads that do cross IHOP.
    ///
    /// Returns the bytes released, if any.
    pub fn consider_heap_resize_at_cleanup(&self, regions: &[G1Region]) -> usize {
        if !g1_heap_resize_enabled() {
            return 0;
        }
        let mut census = G1PausePhases::default();
        census.record_region_census(regions);
        match self.remark_heap_resize_within(&census, true) {
            Some((direction, target, released)) => {
                tracing::debug!(
                    "g1 cleanup: heap {direction} to {target} regions ({released} bytes returned)"
                );
                released
            }
            None => 0,
        }
    }

    /// gci i13/ef finding 8: the Remark / cleanup arm of the sizing policy,
    /// HotSpot's resize after Remark ([`heap_resize_at_remark`]): decided from
    /// this one census by the two free ratios, with no shrink streak and no
    /// overhead term (it used to run the evacuation-pause policy, so a Remark
    /// shrink waited for four qualifying pauses). The opt-in is a parameter for
    /// the reason `apply_heap_resize_within`'s is.
    pub fn remark_heap_resize_within(
        &self,
        census: &G1PausePhases,
        enabled: bool,
    ) -> Option<(&'static str, usize, usize)> {
        if !enabled {
            return None;
        }
        let ceiling = census.total_regions as usize;
        if ceiling == 0 {
            return None;
        }
        let used = ceiling.saturating_sub(census.free_regions as usize);
        let (min_free, max_free) = self.heap_free_ratios();
        let decision = heap_resize_at_remark(
            self.heap_target_regions_within(true),
            used,
            self.heap_floor_regions(),
            ceiling,
            min_free,
            max_free,
        );
        self.commit_heap_resize(decision, census)
    }

    /// Count one humongous allocation request (lane E proposal P6).
    pub fn note_humongous_request(&self) {
        self.humongous_requests.fetch_add(1, Ordering::Relaxed);
    }

    /// Count one humongous allocation FAILURE, with the two numbers that say
    /// whether it was exhaustion or fragmentation.
    ///
    /// `free_regions` and `longest_run` are taken from the table the caller is
    /// already holding. The loop runs only on the failure path, which by
    /// definition is about to throw `OutOfMemoryError` — so an O(regions) pass
    /// there costs nothing anyone will ever measure, and it is the difference
    /// between "G1 OOMed" and "G1 OOMed with 103 free regions and a longest run
    /// of 2".
    pub fn note_humongous_failure(
        &self,
        free_regions: usize,
        longest_run: usize,
        requested_regions: usize,
    ) {
        self.humongous_failures.fetch_add(1, Ordering::Relaxed);
        self.humongous_fail_free_regions
            .store(free_regions as u64, Ordering::Relaxed);
        self.humongous_fail_longest_run
            .store(longest_run as u64, Ordering::Relaxed);
        // Record this request, including exhaustion. The fresh mark can free
        // enough regions in separate holes, and the recovery rung must then
        // compact for THIS request, rather than an earlier, smaller failure.
        self.last_failed_humongous_span
            .store(requested_regions, Ordering::Release);
        tracing::warn!(
            requested_regions,
            "g1: humongous allocation failed with {free_regions} free region(s) and a longest \
             contiguous free run of {longest_run} — this is fragmentation, not exhaustion, \
             whenever the run is shorter than the span requested"
        );
    }

    /// gcd d10/o — arm one FULL-COMPACTION pause for the next collection.
    ///
    /// The caller has just completed a marking cycle (`System.gc()`, and the
    /// last-ditch and overhead-limit rungs before `OutOfMemoryError`). Cleanup
    /// frees only Old regions with NO live byte, so a region holding a few
    /// hundred live bytes beside a megabyte of garbage survives every cycle
    /// until the mixed phase reaches it -- and none of those callers runs one.
    /// Measured on `Gcd10oG1DropRetentionProbe list -Xmx64m`: 45 of 64 regions
    /// were dropped `long[]` data each kept by 240-248 live bytes in its tail,
    /// so three `System.gc()` calls in a row left 52 MB used where HotSpot's
    /// full collection leaves 1.2 MB.
    ///
    /// The armed pause is a mixed pause whose old half is
    /// [`Self::full_compaction_candidates`]: every movable Old region with mark
    /// data and garbage worth taking, cheapest first, as far as the Free pool
    /// can hold the live bytes -- no percentage cap, no pause-time budget. That
    /// is HotSpot's trade for an explicit or last-ditch full collection.
    ///
    /// Refused (left unarmed) when the last cleanup did not trust its closure:
    /// `live_bytes` is then not a liveness verdict. Returns whether it armed.
    /// `CRATONVM_G1_FULL_COMPACTION=0` disables it.
    pub fn arm_full_compaction(&self) -> bool {
        if !self.last_mark_trusted.load(Ordering::Acquire) {
            return false;
        }
        self.full_compaction_armed.store(true, Ordering::Release);
        true
    }

    /// JIT round 22 wave 11 (lane memory; the OOME path's last resort,
    /// `vm` `gc_and_alloc::g1_staged_full_compaction` under
    /// `CRATONVM_G1_OOME_MAXIMAL_COMPACTION`): [`Self::arm_full_compaction`],
    /// and the pause's candidates are every movable Old region with ANY
    /// garbage ([`full_compaction_min_garbage`]). HotSpot runs its maximal
    /// compaction (`G1FullCollector`, every region slid in place) before an
    /// `OutOfMemoryError`; the ordinary full compaction here skips a region
    /// with less than an eighth of a region of garbage, so a heap of regions
    /// each about 90 % live kept a tenth of the heap as garbage no stage took.
    /// Still bounded by the Free pool (a copying pass). Returns whether it armed.
    pub fn arm_full_compaction_maximal(&self) -> bool {
        if !self.arm_full_compaction() {
            return false;
        }
        self.full_compaction_maximal.store(true, Ordering::Release);
        true
    }

    /// The G1 near-full proxy-define page: after an armed full-compaction
    /// pause, slide the movable Old regions in place when the Free pool is
    /// still starved (`in_place_compaction_starved`), composing that pass's
    /// forwards onto `result`'s. `CRATONVM_G1_FULL_COMPACTION_IN_PLACE=0`
    /// returns `result` unchanged.
    pub fn full_compaction_in_place_after(
        &self,
        mut result: GcResult,
        roots: &mut [ObjectRef],
        monitors: &dyn MonitorCleanup,
    ) -> GcResult {
        if !compact_in_place::g1_full_compaction_in_place_enabled() {
            return result;
        }
        let total = self.config.heap_size / self.config.region_size.max(1);
        // r23 w6: a starved full slides whatever the pool holds -- its young
        // half has just emptied Eden into a pool that was starved before it.
        let starved_full = self.starved_full_this_pause.swap(false, Ordering::Relaxed);
        // An in-place-only arm: the same unconditional pass, no back-off.
        let in_place_only = self.in_place_only_this_pause.swap(false, Ordering::Relaxed);
        let fills_free = self
            .in_place_fills_free_this_pause
            .swap(false, Ordering::Relaxed);
        if !starved_full
            && !in_place_only
            && !compact_in_place::in_place_compaction_starved(self.free_region_count(), total)
        {
            self.note_in_place_compaction_declined("pool-not-starved");
            return result;
        }
        let free_before = self.free_region_count();
        let outcome = self.full_compaction_in_place_with(roots, monitors, fills_free);
        if gc_flags().g1_dbg_pins {
            eprintln!(
                "[g1][IN-PLACE] starved_full={starved_full} in_place_only={in_place_only} fills_free={fills_free} free {free_before}->{} outcome={} left_young=[{}]",
                self.free_region_count(),
                match &outcome {
                    Ok(r) => format!("ok moved={} freed={}", r.stats.bytes_copied, r.stats.bytes_freed),
                    Err(why) => format!("declined:{why}"),
                },
                compact_in_place::take_in_place_dbg_left(),
            );
        }
        if starved_full {
            let freed = self.free_region_count().saturating_sub(free_before);
            self.note_starved_full_outcome(if outcome.is_ok() { freed } else { 0 }, total);
        }
        match outcome {
            Ok(next) => {
                // The G1 near-full proxy row (`CRATONVM_G1_IN_PLACE_CLEARS_KEPT_UNRESOLVED`):
                // the pass marked from the roots and compacted, held or freed
                // every region a wedged drain left kept, so their recorded
                // self-forwards no longer describe what is live there.
                {
                    self.clear_kept_unresolved();
                }
                self.compose_forward_maps(&mut result.pointer_map, &next.pointer_map);
                result.stats.objects_copied += next.stats.objects_copied;
                result.stats.bytes_copied += next.stats.bytes_copied;
                result.stats.bytes_freed += next.stats.bytes_freed;
                result
            }
            Err(why) => {
                self.note_in_place_compaction_declined(why);
                result
            }
        }
    }

    /// The `[base, end)` ranges the last pause's in-place compaction slid
    /// (see the `last_pause_slid` field); empty after any other pause.
    pub fn last_pause_slid_ranges(&self) -> Vec<(usize, usize)> {
        self.last_pause_slid.lock().clone()
    }

    /// gcd d10/o — full-compaction pauses run so far (see
    /// [`Self::arm_full_compaction`]).
    pub fn full_compaction_pause_count(&self) -> u64 {
        self.full_compaction_pauses.load(Ordering::Relaxed)
    }

    /// Wave 9 lane G (`gch-h4g1-g1-young-only-pauses-stamp-the-old-pools-collection-usage`):
    /// pauses that collected old regions so far -- every mixed pause, the
    /// full-compaction and humongous-compaction pauses included (each enters
    /// through `collect_garbage`'s mixed arms, which bump this counter). A
    /// collection whose pause left it unchanged was young-only, which HotSpot
    /// 25 does not count as affecting `G1 Old Gen` (`G1MonitoringSupport`).
    pub fn old_collecting_pause_count(&self) -> u64 {
        self.mixed_pauses_entered.load(Ordering::Relaxed)
    }

    /// gcd d10/o — the old half of a full-compaction pause (see
    /// [`Self::arm_full_compaction`]), sorted cheapest first.
    ///
    /// A candidate is an unpinned Old region outside `jit_pinned` with mark data
    /// (`live_bytes > 0`; a region filled after cleanup has none and must not
    /// move on stale liveness) holding at least an eighth of a region of
    /// garbage. The live bytes taken are bounded by the Free regions left after
    /// the young CSet's worst case (`young_bytes`) and a one-region margin, so
    /// the pause does not run its own to-space dry.
    pub fn full_compaction_candidates(
        &self,
        regions: &[G1Region],
        jit_pinned: &RegionSet,
        young_bytes: usize,
    ) -> Vec<(usize, f64)> {
        // r23 w6: a starved full's cycle was aborted, so no liveness here is
        // this cycle's; its in-place pass compacts Old instead.
        if self.starved_full_this_pause.load(Ordering::Relaxed)
            || self.in_place_only_this_pause.load(Ordering::Relaxed)
        {
            return Vec::new();
        }
        let region_size = self.config.region_size;
        // JIT round 22 wave 11: the maximal pass takes any garbage-bearing region.
        let min_garbage = full_compaction_min_garbage(
            region_size,
            self.full_compaction_maximal.load(Ordering::Acquire),
        );
        let free = regions
            .iter()
            .filter(|r| r.region_type() == RegionType::Free)
            .count();
        let mut room = free
            .saturating_sub(1)
            .saturating_mul(region_size)
            .saturating_sub(young_bytes);
        let mut candidates: Vec<(usize, f64)> = regions
            .iter()
            .enumerate()
            .filter(|(i, r)| {
                r.region_type() == RegionType::Old
                    && !r.pinned
                    && !jit_pinned.contains(i)
                    && r.live_bytes > 0
                    && r.mixed_reclaimable(self.mixed_hotspot_choice) >= min_garbage
            })
            .map(|(i, r)| (i, r.mixed_rank(self.mixed_hotspot_choice, self.config.region_size)))
            .collect();
        candidates.sort_by(|a, b| {
            a.1.partial_cmp(&b.1)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.0.cmp(&b.0))
        });
        // JIT round 23 wave 8, lane L9: the cheapest candidate, kept for the
        // minimum-progress rule below.
        let cheapest = candidates.first().copied();
        // Live bytes are copied in object-sized pieces into bump regions, so a
        // destination region rarely fills to the byte; charge each an eighth
        // extra.
        candidates.retain(|&(i, _)| {
            let live = regions[i].mixed_live(self.mixed_hotspot_choice);
            let need = live + live / 8;
            if need > room {
                return false;
            }
            room -= need;
            true
        });
        // JIT round 23 wave 8, lane L9 (`CRATONVM_G1_FULL_COMPACTION_MIN_PROGRESS`,
        // default ON, `=0` kills): when the young CSet's WORST case (its
        // regions' allocated bytes, as if every byte survives) leaves no room
        // at all, still take the cheapest candidate if its live bytes fit the
        // Free regions less the one-region margin. Measured
        // (`R22FlipGcRows -Xmx64m`, JIT, n23-w8m, a failing run): Free 2,
        // young CSet 2 regions (~2 MB allocated, 1.2 MB actually copied), so
        // `room` was 0 and all four last-ditch compaction pauses took no Old
        // region and freed nothing; the OOME followed. The ordinary mixed
        // pauses right after it took 7-10 Old regions each at the same Free 2
        // (1.2 MB copied in all) and freed 8.5 MB apiece. Over-committing the
        // to-space by at most one cheap region is safe: a copy that finds no
        // room self-forwards (the evacuation-failure arm keeps the object and
        // its region), so nothing is lost; at worst the pause frees less. Each
        // staged compaction pause then frees at least that one region, which
        // is the progress the ladder needs before it may throw.
        if candidates.is_empty() {
            if let Some((i, rank)) = cheapest {
                let live = regions[i].mixed_live(self.mixed_hotspot_choice);
                let need = live + live / 8;
                if need <= free.saturating_sub(1).saturating_mul(region_size) {
                    candidates.push((i, rank));
                }
            }
        }
        candidates
    }

    /// Arm exactly one allocation-failure compaction pause, after the caller
    /// has forced a completed mark cycle.  The failure-side record is consumed
    /// here rather than at allocation time: mixing before fresh mark data would
    /// either retain garbage or move a post-TAMS object on stale liveness.
    pub fn arm_fragmented_humongous_compaction(&self) {
        let requested = self.last_failed_humongous_span.swap(0, Ordering::AcqRel);
        self.fragmented_slide_span
            .store(requested, Ordering::Release);
        self.compaction_stages.store(0, Ordering::Release);
        self.compaction_staged_old.lock().clear();
        tracing::debug!(
            collector = self as *const Self as usize,
            requested,
            "g1 allocation-failure compaction: consumed fragmented request"
        );
        let regions = self.regions.read();
        let (free, longest) = free_region_shape(&regions);
        if requested > 1 && longest >= requested && self.marking_complete.load(Ordering::Acquire) {
            // Cleanup has already recovered a sufficient run. The recovery
            // pause still collects young garbage, but must not use this run
            // as to-space and fragment it before the allocation retries.
            let start = find_contiguous_free(&regions, requested).unwrap();
            self.humongous_compaction_avoid
                .lock()
                .replace(HumongousCompactionReservation {
                    requested_regions: requested,
                    start,
                    end: start + requested,
                });
            return;
        }
        // gci i13/oa item 5: `fragmented_free_suffices` (opt-in) also asks for
        // the Free regions the retry's reserve check needs.
        if requested > 1
            && self.fragmented_free_suffices(free, requested)
            && longest < requested
            && self.marking_complete.load(Ordering::Acquire)
        {
            // `try_alloc_array_full` preserves this many regions for
            // infallible allocation/TLAB progress.  A recovery window exactly
            // the size of the failed request is therefore not retryable by the
            // native allocation that asked for it.  Coalesce the request plus
            // that same reserve, keeping the ordinary allocator's safety
            // policy intact rather than bypassing it on an OOME path.
            let total = self.config.heap_size / self.config.region_size.max(1);
            let target_regions = requested.saturating_add(self.tlab_reserve_regions(total));
            tracing::warn!(
                requested,
                target_regions,
                "g1 allocation-failure compaction: armed after completed mark"
            );
            self.pending_humongous_compaction
                .lock()
                .replace(HumongousCompactionPlan {
                    requested_regions: requested,
                    target_start: 0,
                    target_end: target_regions,
                    sources: Vec::new(),
                    young_sources: 0,
                    humongous_moves: Vec::new(),
                    staged_for: None,
                });
        } else if requested > 1 {
            tracing::warn!(
                requested,
                free,
                longest,
                marking_complete = self.marking_complete.load(Ordering::Acquire),
                "g1 allocation-failure compaction: no completed-mark fragmentation to compact"
            );
        } else {
            tracing::debug!(
                requested,
                "g1 allocation-failure compaction: no fragmented request survived to the full-cycle rung"
            );
        }
    }

    /// Whether an allocation has proved that total Free capacity exists but
    /// its shape cannot satisfy a humongous request.  This is deliberately a
    /// narrow failure-side predicate: callers use it to avoid letting the
    /// generic GC-overhead fast-fail skip the one full-compaction rung that
    /// can make such a request possible.
    #[inline]
    pub fn has_fragmented_humongous_request(&self) -> bool {
        let requested = self.last_failed_humongous_span.load(Ordering::Acquire);
        if requested <= 1 {
            return false;
        }
        let regions = self.regions.read();
        let (free, longest) = free_region_shape(&regions);
        self.fragmented_free_suffices(free, requested) && longest < requested
    }

    /// gci i13/oa item 5: is `free` enough Free regions for the fragmentation
    /// arms above ([`Self::arm_fragmented_humongous_compaction`],
    /// [`Self::has_fragmented_humongous_request`])? Default: `free >=
    /// requested`, as before. Under `CRATONVM_G1_HUMONGOUS_COMPACTION_NEEDS_RESERVE`:
    /// also the reserve the fallible retry keeps back
    /// ([`humongous_compaction_has_room_for_retry`]), so a request the retry
    /// would refuse whatever the region shape no longer arms one to four
    /// compaction pauses.
    pub fn fragmented_free_suffices(&self, free: usize, requested: usize) -> bool {
        if !g1_humongous_compaction_needs_reserve() {
            return free >= requested;
        }
        let total = self.config.heap_size / self.config.region_size.max(1);
        humongous_compaction_has_room_for_retry(
            free,
            requested,
            self.tlab_reserve_regions(total),
            g1_humongous_reserve_one(),
        )
    }

    // ---- test hooks -------------------------------------------------------
    //
    // Every one of these exists because the behaviour below it is gated on a
    // OnceLock-latched process flag or on a private field, and an integration
    // test in `gc/tests/` can reach neither. They take the arm as a PARAMETER
    // rather than reading the flag, which is the same reason
    // `uncommit_trailing_free_regions_within` was split: an opt-in flag read
    // from the ambient environment means a test can only ever exercise the arm
    // that does nothing.

    /// Drive the sizing policy from a synthetic census.
    ///
    /// `(free_regions, total_regions, highest_used_region)` is the three-number
    /// summary an evacuation pause hands `apply_heap_resize`. Returns
    /// `(direction, new_target_regions, bytes_released)`.
    pub fn dbg_apply_heap_resize(
        &self,
        free_regions: u32,
        total_regions: u32,
        highest_used_region: u32,
        enabled: bool,
    ) -> Option<(&'static str, usize, usize)> {
        let phases = G1PausePhases {
            free_regions,
            total_regions,
            highest_used_region,
            ..Default::default()
        };
        self.apply_heap_resize_within(&phases, enabled)    }

    /// gci i13/ef finding 8: [`Self::dbg_apply_heap_resize`] for the Remark
    /// arm (`remark_heap_resize_within`).
    pub fn dbg_apply_remark_heap_resize(
        &self,
        free_regions: u32,
        total_regions: u32,
        highest_used_region: u32,
        enabled: bool,
    ) -> Option<(&'static str, usize, usize)> {
        let census = G1PausePhases {
            free_regions,
            total_regions,
            highest_used_region,
            ..Default::default()
        };
        self.remark_heap_resize_within(&census, enabled)    }

    /// The RAW stored soft capacity, without the flag gate `heap_target_regions`
    /// applies. `(target, floor, ceiling, shrink_streak)`.
    pub fn dbg_heap_target_state(&self) -> (usize, usize, usize, u32) {
        (
            self.heap_target_regions.load(Ordering::Relaxed),
            self.heap_floor_regions(),
            self.region_table_len().max(1),
            self.heap_shrink_streak.load(Ordering::Relaxed),
        )
    }

    /// Force the rolling GC-overhead estimate. A test cannot make wall clock
    /// pass at a chosen rate.
    pub fn dbg_set_gc_overhead_ppm(&self, ppm: u64) {
        self.gc_overhead_ppm.store(ppm, Ordering::Relaxed);
    }

    /// Force the three pause-cost estimators. A test cannot make a pause take a
    /// chosen number of microseconds.
    pub fn dbg_set_cost_estimates(
        &self,
        fixup_ns: u64,
        fixup_regions: u64,
        young_copy_ns: u64,
        evac_ns_per_byte: u64,
    ) {
        self.fixup_ns_ema.store(fixup_ns, Ordering::Relaxed);
        self.fixup_regions_ema
            .store(fixup_regions, Ordering::Relaxed);
        self.young_copy_ns_ema
            .store(young_copy_ns, Ordering::Relaxed);
        self.evac_ns_per_byte
            .store(evac_ns_per_byte.max(1), Ordering::Relaxed);
    }

    /// LANE W4-B — record one synthetic collection, with the drain-pass rule
    /// as a parameter. Tests only.
    ///
    /// The production door to the controllers is private and is reached only
    /// from inside a pause, so the only way to ask "what does THIS record
    /// teach?" from outside the crate was to build a heap that exhausts
    /// to-space and hope. That reproduces the drain but not the comparison:
    /// the flag is latched per process, so a fixture can see one arm per test
    /// binary and never the difference between them. This hands both arms the
    /// same record. See `gc/tests/g1_w4b_drain_controllers.rs`.
    pub fn dbg_record_collection(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: G1PausePhases,
        drain_not_a_pause: bool,
    ) {
        self.record_collection_with_phases_within(
            collection_type,
            pause_us,
            stats,
            phases,
            drain_not_a_pause,
            g1_evac_cost_real(),
        );
    }

    /// LANE W5-A — [`Self::dbg_record_collection`] with the wave-5 copy-cost
    /// model as a second parameter.
    ///
    /// Same reason W4-B added the first one: `CRATONVM_G1_EVAC_COST_REAL` is
    /// latched in a `OnceLock` for the life of the process, so a fixture that
    /// set an environment variable could only ever observe the arm its test
    /// binary happened to start in — which pins today's default rather than
    /// the rule. Every assertion that matters here is a DIFFERENCE between the
    /// two arms given an identical record, and a difference needs both arms.
    pub fn dbg_record_collection_costed(
        &self,
        collection_type: G1CollectionType,
        pause_us: u64,
        stats: &GcStats,
        phases: G1PausePhases,
        drain_not_a_pause: bool,
        evac_cost_real: bool,
    ) {
        self.record_collection_with_phases_within(
            collection_type,
            pause_us,
            stats,
            phases,
            drain_not_a_pause,
            evac_cost_real,
        );
    }

    /// The four controller states LANE W4-B's rule governs, as one tuple:
    /// `(fixup_ns_ema, fixup_regions_ema, young_copy_ns_ema, gc_overhead_ppm)`.
    ///
    /// Read beside [`Self::young_target_regions`], which is the fifth and is
    /// already public. Tests, and the `[GC] g1 young-sizing:` line reports the
    /// same numbers on a shipped binary.
    pub fn dbg_controller_estimates(&self) -> (u64, u64, u64, u64) {
        (
            self.fixup_ns_ema.load(Ordering::Relaxed),
            self.fixup_regions_ema.load(Ordering::Relaxed),
            self.young_copy_ns_ema.load(Ordering::Relaxed),
            self.gc_overhead_ppm.load(Ordering::Relaxed),
        )
    }

    /// LANE W5-A — fold one per-byte copy-cost observation, with the
    /// population and the clock as parameters. Tests only.
    ///
    /// The production sample is taken inside a pause from `phases.closure_us`,
    /// which a test cannot make take a chosen number of microseconds; and the
    /// two arms it has to compare are separated by a `OnceLock`-latched flag,
    /// which a test binary sees exactly one value of. So the arm is a
    /// parameter, as everywhere else in this file.
    ///
    /// `young` picks which of the two terms receives the sample.
    pub fn dbg_fold_evac_cost(&self, young: bool, copy_ns: u64, bytes: usize) {
        if young {
            self.fold_evac_cost(
                &self.young_evac_ns_per_byte,
                &self.young_evac_cost_samples,
                copy_ns,
                bytes,
            );
        } else {
            self.fold_evac_cost(
                &self.evac_ns_per_byte,
                &self.evac_cost_samples,
                copy_ns,
                bytes,
            );
        }
    }

    /// LANE W5-A — the price a mixed collection set would charge a candidate
    /// old region per live byte, with the wave-5 model as a PARAMETER.
    ///
    /// `real == false` is the pre-wave-5 read: the raw field, bootstrap and
    /// all bypassed. `real == true` applies the young-derived bootstrap when no
    /// mixed pause has ever taken a sample.
    pub fn dbg_old_evac_ns_per_byte(&self, real: bool) -> u64 {
        let old = self.evac_ns_per_byte.load(Ordering::Relaxed);
        if !real || self.evac_cost_samples.load(Ordering::Relaxed) > 0 {
            return old.max(1);
        }
        let young = self.young_evac_ns_per_byte.load(Ordering::Relaxed);
        if young > 0 {
            young.max(1)
        } else {
            old.max(1)
        }
    }

    /// LANE W5-A — the pre-wave-5 writer, reachable from a test so that the
    /// convergence claim ("a 1/8 EMA cannot walk from the constructor value to
    /// the truth inside a run") is asserted against the real code rather than
    /// re-derived in the test.
    pub fn dbg_update_evac_cost_legacy(&self, pause_ns: u64, bytes_copied: usize) {
        self.update_evac_cost(pause_ns, bytes_copied);
    }

    /// `(budget_without_the_model, budget_with_it, ns_per_walked_region)`.
    pub fn dbg_old_cset_budget(&self) -> (u64, u64, u64) {
        let goal = self.config.max_gc_pause_ms.saturating_mul(1_000_000);
        let after_fixup = goal.saturating_sub(self.fixup_ns_ema.load(Ordering::Relaxed));
        (
            after_fixup,
            after_fixup.saturating_sub(self.young_copy_ns_ema.load(Ordering::Relaxed)),
            self.fixup_ns_per_region(),
        )
    }

    /// `(requests, failures, free_regions_at_last_failure,
    /// longest_run_at_last_failure)`. Diagnostics, `print_gc_summary`, tests.
    pub fn humongous_alloc_counts(&self) -> (u64, u64, u64, u64) {
        (
            self.humongous_requests.load(Ordering::Relaxed),
            self.humongous_failures.load(Ordering::Relaxed),
            self.humongous_fail_free_regions.load(Ordering::Relaxed),
            self.humongous_fail_longest_run.load(Ordering::Relaxed),
        )
    }
}

/// The longest run of adjacent Free regions in `regions`, and how many are
/// Free in total. One pass; called only on a humongous failure path.
pub fn free_region_shape(regions: &[G1Region]) -> (usize, usize) {
    let mut free = 0usize;
    let mut run = 0usize;
    let mut longest = 0usize;
    for r in regions.iter() {
        if r.region_type() == RegionType::Free {
            free += 1;
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    (free, longest)
}

/// The run-preserving arm of [`G1Collector::claim_free_region`]
/// (the default; `CRATONVM_G1_HUMONGOUS_RUN_GUARD=0` opts out): the lowest Free
/// region that is not in the interior of the heap's longest Free run.
///
/// Returns `(chosen, probed)`, with the probe count handed back rather than
/// recorded so the caller owns the instrument — the same split, and for the
/// same reason, as [`best_fit_run`].
///
/// # The rule, in one sentence
///
/// Prefer a Free region outside the longest run; failing that, take the LAST
/// region of the longest run, which shortens it by one instead of splitting it
/// into two shorter ones.
///
/// A single-region claim out of the middle of a run of length `n` leaves two
/// runs of lengths that sum to `n - 1`, and the humongous allocator can only
/// ever use the longer of them — so the heap's capacity to satisfy a large span
/// falls by far more than the one region the claim actually took. Taking the
/// END costs exactly one region of that capacity. Neither arm changes WHICH
/// regions are free, only which of them a claim consumes.
pub fn run_preserving_free_region(regions: &[G1Region], hint: usize) -> (Option<usize>, usize) {
    let choice = run_preserving_choice(
        |i| regions[i].region_type() == RegionType::Free,
        regions.len(),
        hint,
    );
    (choice, regions.len())
}

/// The pure core of [`run_preserving_free_region`], over a "is slot `i` free"
/// predicate rather than over the region table.
///
/// Split out for the reason [`best_fit_run`] is: a fragmentation pattern can
/// only be driven into a real region table through the allocator, which is the
/// thing under test.
pub fn run_preserving_choice<F: Fn(usize) -> bool>(
    is_free: F,
    len: usize,
    hint: usize,
) -> Option<usize> {
    if len == 0 {
        return None;
    }
    // One pass to find the longest run and where it starts. Ties break toward
    // the LOWER index, matching `best_fit_run`, so the two placement policies
    // cannot disagree about which run is "the" longest.
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut best: Option<(usize, usize)> = None; // (len, start)
    for i in 0..len {
        if is_free(i) {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            match best {
                Some((blen, _)) if blen >= run_len => {}
                _ => best = Some((run_len, run_start)),
            }
        } else {
            run_len = 0;
        }
    }
    let (best_len, best_start) = best?;
    let start = hint.min(len - 1);
    // A run of one or two cannot be SPLIT by a single-region claim — taking
    // either end of a two-run leaves one run of one, not two runs — so there is
    // nothing to protect and the ordinary hinted scan is the better answer.
    if best_len <= 2 {
        for step in 0..len {
            let i = (start + step) % len;
            if is_free(i) {
                return Some(i);
            }
        }
        return None;
    }
    // Everything but the run's LAST region is protected: claiming the last one
    // shortens the run, claiming any other splits it.
    let protected = best_start..(best_start + best_len - 1);
    // Rotate by the caller's hint exactly as `find_free_region_from` does, so
    // the guard does not quietly reintroduce the restart-from-zero that
    // `free_scan_hint` exists to remove.
    for step in 0..len {
        let i = (start + step) % len;
        if is_free(i) && !protected.contains(&i) {
            return Some(i);
        }
    }
    // Every Free region in the heap is inside the protected prefix: take the
    // run's last region, which shortens it rather than splitting it.
    Some(best_start + best_len - 1)
}

/// Test hook for [`run_preserving_choice`] — `free[i] == true` means "region
/// `i` is Free". Returns the index a single-region Old claim would take under
/// the default run-preserving policy.
pub fn g1_dbg_run_preserving_choice(free: &[bool], hint: usize) -> Option<usize> {
    run_preserving_choice(|i| free[i], free.len(), hint)
}

/// The INTERIOR of the longest Free run (both ends excluded), or `None` when
/// the longest run is too short to split (`<= 2`, the same cut-off
/// [`run_preserving_choice`] protects at) or nothing is Free. Ties break toward
/// the lower index, as in [`run_preserving_choice`], so the two agree on which
/// run is "the" longest.
pub fn longest_free_run_interior<F: Fn(usize) -> bool>(
    is_free: &F,
    len: usize,
) -> Option<std::ops::Range<usize>> {
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut best: Option<(usize, usize)> = None; // (len, start)
    for i in 0..len {
        if is_free(i) {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            match best {
                Some((blen, _)) if blen >= run_len => {}
                _ => best = Some((run_len, run_start)),
            }
        } else {
            run_len = 0;
        }
    }
    let (best_len, best_start) = best?;
    if best_len <= 2 {
        return None;
    }
    Some((best_start + 1)..(best_start + best_len - 1))
}

/// gci i13/ef finding 1: keep a single-region Old claim inside the committed
/// prefix when it can be, and grow the prefix by the least it can otherwise.
///
/// `choice` is what [`G1Collector::claim_free_region`]'s placement arm picked
/// (the run guard when `guarded`, else the hinted first-Free scan). A choice
/// inside the prefix (`< committed`) is returned unchanged, so placement
/// inside committed memory is exactly the arm's. A choice in the uncommitted
/// tail is replaced by the LOWEST Free region that is not in the interior of
/// the longest Free run (with `guarded`; any Free region without it): that is
/// a committed region whenever the prefix holds an eligible one, and otherwise
/// the first uncommitted index (the tail is Free by construction), so the
/// claim commits one region instead of everything up to `choice`. Either end
/// of the longest run is eligible, and taking an end shortens the run without
/// splitting it, which is the guard's whole property; the guard itself only
/// ever offered the HIGH end, which on a lazily committed heap is the top of
/// the reservation. Never fails where the arm succeeded: `choice` is Free and
/// is itself eligible (outside the interior), so the search finds at least it.
pub fn commit_aware_claim<F: Fn(usize) -> bool>(
    is_free: F,
    len: usize,
    committed: usize,
    choice: usize,
    guarded: bool,
) -> usize {
    if choice < committed {
        return choice;
    }
    let interior = if guarded {
        longest_free_run_interior(&is_free, len)
    } else {
        None
    };
    (0..len)
        .find(|&i| is_free(i) && !interior.as_ref().is_some_and(|r| r.contains(&i)))
        .unwrap_or(choice)
}

/// Test hook for [`commit_aware_claim`]: `free[i] == true` means region `i`
/// is Free; `committed` is the committed prefix in regions.
pub fn g1_dbg_commit_aware_claim(
    free: &[bool],
    committed: usize,
    choice: usize,
    guarded: bool,
) -> usize {
    commit_aware_claim(|i| free[i], free.len(), committed, choice, guarded)
}

