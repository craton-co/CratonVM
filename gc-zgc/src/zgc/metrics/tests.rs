use super::*;

/// No test in this module asserts an elapsed duration. Fixed wall-clock
/// bounds in a test are a latent CI flake on a loaded shared host (the
/// repo has a standing note about exactly this), and every value this
/// module produces from a real clock is unbounded from below *and* above
/// on a contended box. Assertions are on counts, ordering, classification
/// and column arithmetic only; durations are injected via
/// [`ZgcMetrics::record_phase`] / [`ZgcMetrics::record_allocation_stall`],
/// which take an explicit `nanos`.
fn metrics() -> ZgcMetrics {
    ZgcMetrics::new()
}

// -- phase table ------------------------------------------------------

#[test]
fn every_phase_has_a_distinct_index_label_and_key() {
    assert_eq!(ZgcPhase::ALL.len(), ZGC_PHASE_COUNT);
    let mut seen_keys: Vec<&str> = Vec::new();
    let mut seen_labels: Vec<&str> = Vec::new();
    for (i, phase) in ZgcPhase::ALL.iter().enumerate() {
        assert_eq!(phase.index(), i, "{phase:?} index must match ALL position");
        let key = phase.key();
        let label = phase.label();
        assert!(!key.is_empty());
        assert!(!label.is_empty());
        assert!(!seen_keys.contains(&key), "duplicate key {key:?}");
        assert!(!seen_labels.contains(&label), "duplicate label {label:?}");
        // The TSV column names are built from `key`; a tab or a space in
        // one would corrupt the header.
        assert!(
            !key.contains('\t') && !key.contains(' '),
            "TSV key {key:?} must be a bare snake_case token",
        );
        seen_keys.push(key);
        seen_labels.push(label);
        assert_eq!(ZgcPhase::from_key(key), Some(*phase));
    }
    assert_eq!(ZgcPhase::from_key("not-a-phase"), None);
}

#[test]
fn stw_classification_matches_the_pause_prefix() {
    // Every OpenJDK "Pause ..." phase is STW; every "Concurrent ..." phase
    // is not. `Sweep` is ours and is STW. This pins the label and the
    // verdict together so they cannot drift, the same way
    // `gc_metrics::decision_reason` pins `label` to `is_moving`.
    for phase in ZgcPhase::ALL {
        let by_label = phase.label().starts_with("Pause ");
        let expected = by_label || phase == ZgcPhase::Sweep;
        assert_eq!(
            phase.is_stw(),
            expected,
            "{phase:?}: label {:?} disagrees with is_stw()",
            phase.label(),
        );
    }
    assert!(ZgcPhase::Sweep.is_stw(), "the mark-sweep reclaim is STW");
    assert!(!ZgcPhase::ConcurrentRelocate.is_stw());
}

// FINDING 8's first half is gone with the simulation it cross-checked
// (2026-09-02).
//
// `every_simulation_phase_except_idle_has_a_metrics_counter` matched
// exhaustively over `zgc::ZgcPhase` -- the SIMULATION's phase state
// variable -- so that adding a variant there without deciding where its
// nanoseconds landed was a compile error here. That was a real gate and it
// caught a real gap (`ConcurrentRemap` had a phase and no counter).
//
// Its subject no longer exists: the simulation was deleted, and the
// collector that runs (`ZgcRealHeap`) drives this module directly. The
// obligation the test encoded now belongs to whichever phase machine a
// future concurrent collector uses; `ZgcPhase::ALL` and the
// label/`is_stw` pinning above are what remain, and they are about THIS
// module's own consistency rather than about a second enum's.

/// FINDING 8, second half: the remap phase is a *concurrent* counter with
/// its own four TSV columns, not an alias of anything.
///
/// If a future collector genuinely folds remap into the next cycle's mark
/// (OpenJDK's behaviour), this phase reports `count=0` and the report says
/// so — which is the reading an absent column cannot give.
#[test]
fn the_remap_phase_is_concurrent_by_design_and_has_its_own_tsv_columns() {
    assert_eq!(
        ZgcPhase::from_key("concurrent_remap"),
        Some(ZgcPhase::ConcurrentRemap)
    );
    assert!(
        !ZgcPhase::ConcurrentRemap.is_stw(),
        "remap is driven by the load barrier alongside mutators; charging it \
         STW by design would overstate the pause",
    );

    let header = ZgcMetrics::tsv_header();
    for suffix in ["count", "total_ns", "max_ns", "p99_ns"] {
        let col = format!("concurrent_remap_{suffix}");
        assert!(
            header.split('\t').any(|c| c == col),
            "TSV header is missing {col}; remap time would vanish from every \
             aggregation: {header}",
        );
    }

    // With real concurrency the time is concurrent; without it, STW —
    // exactly like every other concurrent-by-design phase.
    let m = metrics();
    m.set_phases_run_concurrently(true);
    m.record_phase(ZgcPhase::ConcurrentRemap, 7_000);
    assert_eq!(m.total_concurrent_ns(), 7_000);
    assert_eq!(m.total_stw_ns(), 0);
    m.set_phases_run_concurrently(false);
    assert_eq!(m.total_stw_ns(), 7_000);
    assert_eq!(m.total_concurrent_ns(), 0);

    // A phase that never ran reports zero rather than being absent.
    let quiet = metrics();
    assert_eq!(quiet.phase_stats(ZgcPhase::ConcurrentRemap).count, 0);
    assert!(
        quiet.format_summary().contains("Concurrent Remap"),
        "a folded (never-run) remap must still get a summary row saying so",
    );
}

// -- the guard --------------------------------------------------------

#[test]
fn guard_records_exactly_one_sample_on_drop() {
    let m = metrics();
    assert_eq!(m.phase_stats(ZgcPhase::ConcurrentMark).count, 0);
    {
        let guard = m.scoped(ZgcPhase::ConcurrentMark);
        assert_eq!(guard.phase(), ZgcPhase::ConcurrentMark);
        // Still open: nothing recorded yet. This is the ordering the whole
        // RAII contract rests on.
        assert_eq!(
            m.phase_stats(ZgcPhase::ConcurrentMark).count,
            0,
            "an OPEN guard must not have recorded",
        );
    }
    assert_eq!(
        m.phase_stats(ZgcPhase::ConcurrentMark).count,
        1,
        "dropping the guard must record exactly one sample",
    );
    // And only its own phase.
    assert_eq!(m.phase_stats(ZgcPhase::Sweep).count, 0);
}

#[test]
fn explicit_end_records_once_not_twice() {
    let m = metrics();
    m.scoped(ZgcPhase::PauseMarkStart).end();
    assert_eq!(m.phase_stats(ZgcPhase::PauseMarkStart).count, 1);
}

#[test]
fn stall_guard_records_on_drop() {
    let m = metrics();
    assert_eq!(m.allocation_stalls().0, 0);
    {
        let _g = m.scoped_allocation_stall();
        assert_eq!(m.allocation_stalls().0, 0, "an OPEN stall must not count");
    }
    assert_eq!(m.allocation_stalls().0, 1);
}

// -- STW vs concurrent ------------------------------------------------

#[test]
fn stw_and_concurrent_totals_split_by_classification() {
    let m = metrics();
    m.set_phases_run_concurrently(true); // pretend the real collector

    m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
    m.record_phase(ZgcPhase::PauseMarkEnd, 2_000);
    m.record_phase(ZgcPhase::PauseRelocateStart, 3_000);
    m.record_phase(ZgcPhase::Sweep, 4_000);
    m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
    m.record_phase(ZgcPhase::ConcurrentRelocate, 200_000);
    m.record_phase(ZgcPhase::ConcurrentSelectRelocationSet, 50_000);

    assert_eq!(m.total_stw_ns(), 1_000 + 2_000 + 3_000 + 4_000);
    assert_eq!(m.total_concurrent_ns(), 100_000 + 200_000 + 50_000);
    // 10_000 STW out of 360_000 total.
    assert_eq!(m.stw_share(), 10_000.0 / 360_000.0);
}

#[test]
fn without_real_concurrency_every_phase_is_charged_as_stw() {
    // The default. This is the reading the ZGC-real regression
    // investigation must get: `ZgcRealHeap` holds one StopTheWorldToken
    // across the whole cycle, so a concurrent total above zero would be a
    // fabrication.
    let m = metrics();
    assert!(!m.phases_run_concurrently());
    m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
    m.record_phase(ZgcPhase::PauseMarkStart, 1_000);

    assert!(m.phase_was_stw(ZgcPhase::ConcurrentMark));
    assert_eq!(m.total_concurrent_ns(), 0);
    assert_eq!(m.total_stw_ns(), 101_000);
    assert_eq!(m.stw_share(), 1.0);

    // The design classification is untouched — only the accounting moved.
    assert!(!ZgcPhase::ConcurrentMark.is_stw());

    let summary = m.format_summary();
    assert!(summary.contains("concurrency=none"), "{summary}");
    assert!(
        summary.contains("BY CONSTRUCTION"),
        "a zero concurrent column must say why: {summary}",
    );
}

#[test]
fn stw_share_is_zero_not_nan_before_anything_is_collected() {
    let m = metrics();
    assert_eq!(m.stw_share(), 0.0);
    assert!(m.stw_share().is_finite());
    assert_eq!(m.allocation_rate_bytes_per_sec(), 0.0);
    assert!(m.allocation_rate_bytes_per_sec().is_finite());
    assert!(m.last_allocation_rate_bytes_per_sec().is_finite());
}

// -- min / max / last / avg -------------------------------------------

#[test]
fn min_max_last_and_avg_track_the_recorded_samples() {
    let m = metrics();
    let p = ZgcPhase::ConcurrentRelocate;

    // Before anything: min reports 0, not the u64::MAX sentinel.
    let empty = m.phase_stats(p);
    assert_eq!(empty.count, 0);
    assert_eq!(empty.min_ns, 0, "an unset min must report 0, not u64::MAX");
    assert_eq!(empty.max_ns, 0);
    assert_eq!(empty.last_ns, 0);
    assert_eq!(empty.avg_ns(), 0);

    m.record_phase(p, 50);
    m.record_phase(p, 5);
    m.record_phase(p, 100);
    m.record_phase(p, 45); // last

    let s = m.phase_stats(p);
    assert_eq!(s.count, 4);
    assert_eq!(s.total_ns, 200);
    assert_eq!(s.min_ns, 5);
    assert_eq!(s.max_ns, 100);
    assert_eq!(s.last_ns, 45, "last is the most recent, not the largest");
    assert_eq!(s.avg_ns(), 50);
}

#[test]
fn percentiles_use_nearest_rank_like_g1() {
    let m = metrics();
    let p = ZgcPhase::ConcurrentMark;
    for i in 1..=10u64 {
        m.record_phase(p, i * 10);
    }
    let s = m.phase_stats(p);
    // nearest-rank over [10..100]: p50 -> ceil(0.5*10)=5th -> 50;
    // p99 -> ceil(0.99*10)=10th -> 100. Same arithmetic as g1.rs.
    assert_eq!(s.p50_ns, 50);
    assert_eq!(s.p99_ns, 100);
    assert_eq!(s.recent_samples, 10);
}

// -- the ring ---------------------------------------------------------

#[test]
fn recent_ring_wraps_without_growing() {
    let m = metrics();
    let p = ZgcPhase::Sweep;
    let n = RECENT_SAMPLE_CAP * 3 + 7;
    for i in 0..n {
        m.record_phase(p, (i as u64) + 1);
    }
    let s = m.phase_stats(p);
    assert_eq!(
        s.count, n as u64,
        "the whole-run count is NOT bounded by the ring",
    );
    assert_eq!(
        s.recent_samples, RECENT_SAMPLE_CAP as u64,
        "the ring must saturate at its capacity rather than grow",
    );
    // Whole-run min/max survive eviction from the ring; only the
    // percentiles are windowed. Losing that distinction would make a long
    // soak's summary silently under-report its worst pause.
    assert_eq!(s.min_ns, 1);
    assert_eq!(s.max_ns, n as u64);
    assert_eq!(s.last_ns, n as u64);
    // The window holds only the most recent CAP samples, so its p50 is far
    // above the run's median.
    assert!(
        s.p50_ns > (n as u64 - RECENT_SAMPLE_CAP as u64),
        "the percentile window must be the RECENT samples: p50={} n={}",
        s.p50_ns,
        n,
    );
}

#[test]
fn ring_len_never_exceeds_capacity_directly() {
    let mut ring = RecentRing::new();
    for i in 0..(RECENT_SAMPLE_CAP * 5) {
        ring.push(i as u64);
        assert!(ring.len <= RECENT_SAMPLE_CAP);
        assert!(ring.next < RECENT_SAMPLE_CAP);
        assert_eq!(ring.samples.len(), RECENT_SAMPLE_CAP, "fixed array");
    }
    assert_eq!(ring.len, RECENT_SAMPLE_CAP);
    assert_eq!(ring.snapshot().len(), RECENT_SAMPLE_CAP);
}

// -- cycles -----------------------------------------------------------

#[test]
fn record_cycle_derives_reclaimed_and_live_bytes() {
    let m = metrics();
    assert_eq!(m.cycles(), 0);

    m.record_cycle(1_000_000, 250_000, 4_000_000);
    assert_eq!(m.cycles(), 1);
    assert_eq!(m.last_bytes_reclaimed(), 750_000);
    assert_eq!(m.live_bytes(), 250_000);
    assert_eq!(m.total_bytes_reclaimed(), 750_000);

    m.record_cycle(900_000, 300_000, 4_000_000);
    assert_eq!(m.cycles(), 2);
    assert_eq!(m.last_bytes_reclaimed(), 600_000);
    assert_eq!(m.live_bytes(), 300_000);
    assert_eq!(m.total_bytes_reclaimed(), 1_350_000);
}

/// zt t1/tp (zs1tp residuals item 2): the pause is not mutator time. The
/// same bytes over the same wall interval give a higher rate once the
/// pause is subtracted; a pause at or above the interval leaves it whole;
/// `record_cycle` is the `0` form.
#[test]
fn zt1tp_the_allocation_rate_interval_excludes_the_pause() {
    let whole = metrics();
    let net = metrics();
    let huge = metrics();
    std::thread::sleep(std::time::Duration::from_millis(20));
    whole.record_cycle(1_000_000, 100_000, 4_000_000);
    net.record_cycle_excluding_pause(1_000_000, 100_000, 4_000_000, 10_000_000);
    huge.record_cycle_excluding_pause(1_000_000, 100_000, 4_000_000, u64::MAX);
    let w = whole.last_interval_ns.load(Ordering::Relaxed);
    let n = net.last_interval_ns.load(Ordering::Relaxed);
    let h = huge.last_interval_ns.load(Ordering::Relaxed);
    assert!(w >= 20_000_000, "{w}");
    assert!(n >= 10_000_000 && n < w, "net {n} whole {w}");
    assert!(
        h >= 20_000_000,
        "an impossible pause leaves the interval whole: {h}"
    );
    assert!(
        net.last_allocation_rate_bytes_per_sec() > whole.last_allocation_rate_bytes_per_sec()
    );
    assert_eq!(
        net.total_interval_ns.load(Ordering::Relaxed),
        n,
        "the run-wide denominator takes the same net interval"
    );
}

#[test]
fn per_cycle_stw_split_is_a_subtraction_of_the_cumulative_totals() {
    let m = metrics();
    m.set_phases_run_concurrently(true);

    m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
    m.record_phase(ZgcPhase::ConcurrentMark, 10_000);
    m.record_cycle(1_000, 500, 8_000);
    let line1 = m.format_cycle_line();
    assert!(line1.contains("GC(0)"), "{line1}");

    m.record_phase(ZgcPhase::PauseMarkStart, 3_000);
    m.record_phase(ZgcPhase::ConcurrentMark, 40_000);
    m.record_cycle(2_000, 700, 8_000);

    // Cumulative totals are the sum; the SECOND cycle's own split is the
    // difference, not the total.
    assert_eq!(m.total_stw_ns(), 4_000);
    assert_eq!(m.total_concurrent_ns(), 50_000);
    assert_eq!(m.last_cycle_stw_ns.load(Ordering::Relaxed), 3_000);
    assert_eq!(m.last_cycle_concurrent_ns.load(Ordering::Relaxed), 40_000);

    let line2 = m.format_cycle_line();
    assert!(line2.contains("GC(1)"), "{line2}");
}

#[test]
fn cycle_line_says_so_before_the_first_collection() {
    let m = metrics();
    let line = m.format_cycle_line();
    assert!(
        line.contains("no cycle recorded"),
        "a report before the first cycle must not look like a completed \
         cycle of zeroes: {line}",
    );
}

#[test]
fn cycle_line_has_the_xlog_gc_shape() {
    let m = metrics();
    m.record_cycle(64 * 1024 * 1024, 8 * 1024 * 1024, 128 * 1024 * 1024);
    let line = m.format_cycle_line();
    assert!(line.starts_with("[GC] "), "{line}");
    assert!(
        line.contains("Garbage Collection (Allocation Rate)"),
        "{line}"
    );
    // 64M of 128M = 50%, 8M of 128M = 6.25% -> 6%.
    assert!(line.contains("64M(50%)->8M(6%)"), "{line}");
    assert!(line.contains("stalls="), "{line}");
}

/// zr zs s1/tp: the cause is the caller's, occupancy is HotSpot ZGC's
/// whole-MiB `ZSIZE_FMT` with a rounded percent (a 1026 MiB figure is
/// `1026M`, not `1G`; 7.8% is `8%`, not `7%`).
#[test]
fn zs1tp_cycle_line_names_the_cause_and_prints_zsize() {
    let m = metrics();
    let mib = 1024 * 1024;
    m.record_cycle(1026 * mib, 160 * mib, 2048 * mib);
    let line = m.format_cycle_line_with_cause("Warmup");
    assert!(line.contains("Garbage Collection (Warmup) "), "{line}");
    assert!(line.contains(" 1026M(50%)->160M(8%) "), "{line}");
    assert!(m.format_cycle_line().contains("(Allocation Rate)"));
    assert_eq!(zsize(0, 0), "0M(0%)");
}

#[test]
fn human_bytes_matches_the_hotspot_shorthand() {
    assert_eq!(human_bytes(0), "0B");
    assert_eq!(human_bytes(96), "96B");
    assert_eq!(human_bytes(1024), "1K");
    assert_eq!(human_bytes(1024 * 1024), "1M");
    assert_eq!(human_bytes(1026 * 1024 * 1024), "1G");
    assert_eq!(human_bytes(140 * 1024 * 1024), "140M");
}

/// zr zs s1/tp: `zsize` replaced the truncating `percent_of` (its only
/// caller); the zero-capacity and huge-heap cases keep their test.
#[test]
fn zsize_is_zero_rather_than_a_division_by_zero() {
    assert_eq!(zsize(10, 0), "0M(0%)");
    assert_eq!(zsize(0, 100), "0M(0%)");
    assert_eq!(
        zsize(u64::MAX, u64::MAX),
        format!("{}M(100%)", u64::MAX >> 20)
    );
}

// -- allocation stalls -------------------------------------------------

#[test]
fn stall_accounting_tracks_count_total_and_max() {
    let m = metrics();
    assert_eq!(m.allocation_stalls(), (0, 0, 0));

    m.record_allocation_stall(1_000);
    m.record_allocation_stall(9_000);
    m.record_allocation_stall(500);

    let (count, total, max) = m.allocation_stalls();
    assert_eq!(count, 3);
    assert_eq!(total, 10_500);
    assert_eq!(max, 9_000, "max is not the last sample");

    let summary = m.format_summary();
    assert!(summary.contains("allocation stalls: count=3"), "{summary}");
    // The averaged line must be present so a run with many small stalls
    // reads differently from a run with one enormous one.
    assert!(summary.contains("max_ms="), "{summary}");
}

#[test]
fn stalls_are_independent_of_phases() {
    // A stall is mutator time, not collector time: it must not leak into
    // the phase totals, or the STW/concurrent split stops meaning what the
    // summary says it means.
    let m = metrics();
    m.record_allocation_stall(1_000_000);
    assert_eq!(m.total_stw_ns(), 0);
    assert_eq!(m.total_concurrent_ns(), 0);
    for phase in ZgcPhase::ALL {
        assert_eq!(m.phase_stats(phase).count, 0);
    }
}

// -- TSV ---------------------------------------------------------------

#[test]
fn tsv_header_and_row_have_the_same_column_count() {
    // A header/row mismatch shifts every value one column left or right
    // and silently corrupts every downstream aggregation without erroring
    // anywhere. This is the assertion that stops it.
    let m = metrics();
    let header = ZgcMetrics::tsv_header();
    let row = m.to_tsv_row();
    let header_cols: Vec<&str> = header.split('\t').collect();
    let row_cols: Vec<&str> = row.split('\t').collect();
    assert_eq!(
        header_cols.len(),
        row_cols.len(),
        "TSV header has {} columns but the row has {}",
        header_cols.len(),
        row_cols.len(),
    );
    // 14 fixed columns + 4 per phase.
    assert_eq!(header_cols.len(), 14 + 4 * ZGC_PHASE_COUNT);
    assert_eq!(header_cols[0], "run");

    // The same still holds once every counter is populated, which is the
    // case a length-only check on an empty recorder would miss.
    for (i, phase) in ZgcPhase::ALL.iter().enumerate() {
        m.record_phase(*phase, (i as u64 + 1) * 1_000);
    }
    m.record_allocation_stall(42);
    m.record_cycle(4_096, 1_024, 65_536);
    let row = m.to_tsv_row();
    assert_eq!(row.split('\t').count(), header_cols.len());
    assert!(!row.contains('\n'), "a TSV row must be one line");
}

#[test]
fn tsv_header_names_every_phase_column() {
    let header = ZgcMetrics::tsv_header();
    for phase in ZgcPhase::ALL {
        for suffix in ["count", "total_ns", "max_ns", "p99_ns"] {
            let col = format!("{}_{}", phase.key(), suffix);
            assert!(
                header.split('\t').any(|c| c == col),
                "TSV header is missing column {col}: {header}",
            );
        }
    }
    for col in [
        "stall_count",
        "stall_total_ns",
        "stall_max_ns",
        "stw_ns",
        "concurrent_ns",
        "phases_run_concurrently",
    ] {
        assert!(
            header.split('\t').any(|c| c == col),
            "TSV header is missing column {col}",
        );
    }
}

#[test]
fn run_label_is_settable_and_cannot_break_the_row() {
    let m = metrics();
    assert_eq!(m.run_label(), "zgc-real");
    assert!(m.to_tsv_row().starts_with("zgc-real\t"));

    m.set_run_label("generational");
    assert!(m.to_tsv_row().starts_with("generational\t"));

    // A label carrying a tab would inject a column.
    m.set_run_label("bad\tlabel\nhere");
    let row = m.to_tsv_row();
    assert!(!row.contains("bad\tlabel"), "{row}");
    assert!(!row.contains('\n'), "{row}");
    assert_eq!(
        row.split('\t').count(),
        ZgcMetrics::tsv_header().split('\t').count(),
    );
}

// -- summary -----------------------------------------------------------

#[test]
fn summary_lists_every_phase_with_its_stw_verdict() {
    let m = metrics();
    m.set_phases_run_concurrently(true);
    m.record_phase(ZgcPhase::PauseMarkStart, 1_000);
    m.record_phase(ZgcPhase::ConcurrentMark, 100_000);
    m.record_cycle(2_048, 512, 8_192);

    let summary = m.format_summary();
    for phase in ZgcPhase::ALL {
        assert!(
            summary.contains(phase.label()),
            "summary omits {:?}: {summary}",
            phase,
        );
    }
    assert!(summary.contains("[GC-SUMMARY] zgc cycles=1"), "{summary}");
    assert!(summary.contains("concurrency=real"), "{summary}");
    assert!(summary.contains("stw_share="), "{summary}");
    // Every line carries the scrapeable prefix.
    for line in summary.lines() {
        assert!(line.starts_with("[GC-SUMMARY] zgc"), "stray line: {line}");
    }
}

#[test]
fn summary_states_that_percentiles_are_windowed() {
    // A p99 over the last 64 cycles of a 10 000-cycle run is a different
    // number from a p99 over the run, and a reader who does not know which
    // one they have will draw the wrong conclusion.
    let summary = metrics().format_summary();
    assert!(summary.contains("most recent"), "{summary}");
    assert!(
        summary.contains("whole-run"),
        "the summary must say which columns are whole-run: {summary}",
    );
}

// -- reset -------------------------------------------------------------

#[test]
fn reset_clears_every_counter() {
    let m = metrics();
    m.record_phase(ZgcPhase::Sweep, 1_234);
    m.record_allocation_stall(99);
    m.record_cycle(100, 10, 1_000);

    m.reset_for_test();

    assert_eq!(m.cycles(), 0);
    assert_eq!(m.total_stw_ns(), 0);
    assert_eq!(m.total_concurrent_ns(), 0);
    assert_eq!(m.allocation_stalls(), (0, 0, 0));
    assert_eq!(m.total_bytes_reclaimed(), 0);
    for phase in ZgcPhase::ALL {
        let s = m.phase_stats(phase);
        assert_eq!(s.count, 0);
        assert_eq!(s.total_ns, 0);
        assert_eq!(s.min_ns, 0);
        assert_eq!(s.max_ns, 0);
        assert_eq!(s.recent_samples, 0);
    }
}

#[test]
fn metrics_is_send_and_sync() {
    // The collector shares this by `&` across mutator and GC threads; if
    // it stops being Sync the heap stops compiling, and this test names
    // the reason rather than leaving a confusing error at the use site.
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ZgcMetrics>();
}
