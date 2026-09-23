// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Lane-R (wave 4): read `proposal-a-concurrent-mark-by-default.md`'s step-3
//! bar off a real run, instead of arguing about it.
//!
//! # What this is and why it is a test file
//!
//! The proposal gates flipping `CRATONVM_ZGC_CONC_START`'s default on four
//! criteria. One of them is arithmetic on numbers the collector already prints:
//!
//! > `scanned_at_safepoint / scanned_concurrently` is below **0.1** on the
//! > **median cycle** — i.e. the window really is finishing the mark.
//!
//! Wave 3 put both halves on `[GC] zgc-markend:` (per cycle) and added the
//! cumulative pair to `[GC] zgc-concurrent:` (per run). Neither is the number
//! the bar names, and the difference is not pedantic — see
//! [`the_bar_is_a_median_and_the_shutdown_line_is_a_ratio_of_sums`] below,
//! where a run that **passes** the ratio-of-sums **fails** the median by a
//! factor of six. A single enormous well-absorbed cycle can carry an
//! arbitrarily bad body of ordinary ones.
//!
//! So this file is the missing half-page of arithmetic, as something runnable:
//!
//! ```text
//!   # 1. take the run. ANY workload; the bar is about the median cycle.
//!   CRATONVM_ZGC_CONC_START=auto \
//!     cratonvm -XX:+UseZGC --verbose:gc <workload> 2> /tmp/zgc.log
//!
//!   # 2. read the bar off it.
//!   CRATONVM_ZGC_MARKEND_LOG=/tmp/zgc.log \
//!     cargo test -p cratonvm-gc --test zgc_r_conc_window_fit -- --nocapture
//! ```
//!
//! With the variable unset the file is an ordinary unit test of its own
//! arithmetic and its own parser, which is what keeps it honest between runs.
//!
//! # What it deliberately does not do
//!
//! It does not flip anything, and it does not claim the bar is met. It also
//! reports the proposal's **engagement gate** first, because that one decides
//! whether the rest of the numbers are about anything:
//!
//! > `cycles_started=0` means the run says nothing about concurrent marking
//!
//! A sweep of `System.gc()`-driven benchmarks is exactly such a run — a forced
//! collection deliberately never opens a cycle — so a report that led with a
//! ratio would be reporting on an empty set.

use std::collections::HashMap;

/// One `[GC] zgc-markend:` line's worth of window fit.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Cycle {
    scanned_concurrently: u64,
    scanned_at_safepoint: u64,
    window_target_ms: u64,
    window_actual_ms: u64,
}

impl Cycle {
    /// The bar's quantity: the fraction of the mark the concurrent window was
    /// too short to absorb.
    ///
    /// `scanned_concurrently == 0` is not a ratio of zero — it is a cycle that
    /// had no concurrent phase at all (the collection arrived immediately after
    /// the cycle opened). Those are excluded from the median rather than
    /// counted as perfect, because counting them as `0.0` is how a sweep of
    /// runs that never marked concurrently would pass the bar.
    fn ratio(&self) -> Option<f64> {
        if self.scanned_concurrently == 0 {
            return None;
        }
        Some(self.scanned_at_safepoint as f64 / self.scanned_concurrently as f64)
    }
}

/// `[GC] zgc-concurrent:` — the once-per-process totals.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct RunTotals {
    cycles_started: u64,
    cycles_completed: u64,
    scanned_concurrently: u64,
    scanned_at_safepoint: u64,
}

/// The proposal's bar. Fixed here so reading the number and choosing the
/// threshold cannot be the same act.
const MEDIAN_BAR: f64 = 0.1;

/// Scrape `key=value` pairs out of one whitespace-separated log line.
///
/// Values are `u64` or nothing: every field this file reads is a count or a
/// millisecond figure, and a line whose field does not parse is a line from a
/// different build, which is worth ignoring rather than guessing at.
fn fields(line: &str) -> HashMap<&str, u64> {
    line.split_whitespace()
        .filter_map(|token| {
            let (key, value) = token.split_once('=')?;
            Some((key, value.parse::<u64>().ok()?))
        })
        .collect()
}

fn parse_cycles(log: &str) -> Vec<Cycle> {
    log.lines()
        .filter(|l| l.contains("[GC] zgc-markend:"))
        .filter_map(|l| {
            let f = fields(l);
            Some(Cycle {
                scanned_concurrently: *f.get("scanned_concurrently")?,
                scanned_at_safepoint: *f.get("scanned_at_safepoint")?,
                // Wave 3 added these two; a log from before it simply has none.
                window_target_ms: f.get("window_target_ms").copied().unwrap_or(0),
                window_actual_ms: f.get("window_actual_ms").copied().unwrap_or(0),
            })
        })
        .collect()
}

/// The LAST `[GC] zgc-concurrent:` line, which is the shutdown one. A run that
/// printed none returns `None` and the report says so rather than showing
/// zeroes that look like a failure.
fn parse_totals(log: &str) -> Option<RunTotals> {
    log.lines()
        .filter(|l| l.contains("[GC] zgc-concurrent:"))
        .last()
        .map(|l| {
            let f = fields(l);
            RunTotals {
                cycles_started: f.get("cycles_started").copied().unwrap_or(0),
                cycles_completed: f.get("cycles_completed").copied().unwrap_or(0),
                scanned_concurrently: f.get("scanned_concurrently").copied().unwrap_or(0),
                scanned_at_safepoint: f.get("scanned_at_safepoint").copied().unwrap_or(0),
            }
        })
}

/// Median of a sample, lower-of-the-two-middles on an even count.
///
/// Lower rather than the mean of the two middles, deliberately: the mean is not
/// one of the observations, and this number is quoted as "the median cycle".
fn median(mut samples: Vec<f64>) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_by(|a, b| a.partial_cmp(b).expect("no NaN: every ratio is finite"));
    Some(samples[(samples.len() - 1) / 2])
}

/// `scanned_at_safepoint / scanned_concurrently` over the SUMS — what
/// `[GC] zgc-concurrent:` and `ZgcRealHeap::concurrent_window_fit()` give you.
fn ratio_of_sums(cycles: &[Cycle]) -> Option<f64> {
    let conc: u64 = cycles.iter().map(|c| c.scanned_concurrently).sum();
    let stw: u64 = cycles.iter().map(|c| c.scanned_at_safepoint).sum();
    if conc == 0 {
        return None;
    }
    Some(stw as f64 / conc as f64)
}

// ---------------------------------------------------------------------------
// The arithmetic, stated as tests so the report above cannot drift from it.
// ---------------------------------------------------------------------------

/// **The whole reason this file exists.**
///
/// `proposal-a-concurrent-mark-by-default.md` asks for the median cycle.
/// `[GC] zgc-concurrent:` reports the ratio of the sums, and wave 3 added it
/// with the honest note that it makes the question "a single run rather than a
/// log-parsing project". It does — but it answers a *different* question, and
/// the two can disagree in the direction that matters.
///
/// The fixture is the shape a real workload produces: one very large cycle that
/// the window absorbed almost completely (a long quiet stretch), and a body of
/// ordinary cycles that it did not. The big cycle dominates both sums, so the
/// ratio-of-sums passes comfortably while the median — the typical cycle a user
/// actually experiences — is six times over the bar.
#[test]
fn the_bar_is_a_median_and_the_shutdown_line_is_a_ratio_of_sums() {
    let mut cycles = vec![Cycle {
        scanned_concurrently: 100_000_000,
        scanned_at_safepoint: 1_000_000,
        window_target_ms: 0,
        window_actual_ms: 0,
    }];
    for _ in 0..8 {
        cycles.push(Cycle {
            scanned_concurrently: 1_000_000,
            scanned_at_safepoint: 600_000,
            window_target_ms: 0,
            window_actual_ms: 0,
        });
    }

    let sums = ratio_of_sums(&cycles).expect("non-zero concurrent work");
    let med = median(cycles.iter().filter_map(Cycle::ratio).collect()).expect("nine cycles");

    assert!(
        sums < MEDIAN_BAR,
        "the fixture must PASS the ratio of sums ({sums:.4}), or it is not \
         demonstrating the disagreement"
    );
    assert!(
        med > MEDIAN_BAR,
        "...while FAILING the median ({med:.4}). If these two ever agree by \
         construction, this file can be deleted and the shutdown line read \
         directly"
    );
    assert!((med - 0.6).abs() < 1e-9, "the typical cycle absorbed 40%");
}

/// A cycle with no concurrent phase is not a perfect cycle.
///
/// `scanned_concurrently=0` means the collection arrived immediately after the
/// cycle opened — `[GC] zgc-markend:`'s own reading of it — so the cycle says
/// nothing about how well the window fitted. Counting it as `0.0` would let a
/// run that never marked concurrently sail through a bar about concurrent
/// marking, which is the same vacuous-green failure
/// `docs/gc-tuning.md`'s `cycles_started=0` note is about.
#[test]
fn a_cycle_with_no_concurrent_phase_is_excluded_rather_than_counted_perfect() {
    let cycles = vec![
        Cycle {
            scanned_concurrently: 0,
            scanned_at_safepoint: 5_000,
            window_target_ms: 0,
            window_actual_ms: 0,
        },
        Cycle {
            scanned_concurrently: 0,
            scanned_at_safepoint: 5_000,
            window_target_ms: 0,
            window_actual_ms: 0,
        },
        Cycle {
            scanned_concurrently: 1_000,
            scanned_at_safepoint: 900,
            window_target_ms: 0,
            window_actual_ms: 0,
        },
    ];
    let med = median(cycles.iter().filter_map(Cycle::ratio).collect()).expect("one usable cycle");
    assert!(
        (med - 0.9).abs() < 1e-9,
        "only the cycle that ran is counted"
    );
    assert_eq!(cycles.iter().filter_map(Cycle::ratio).count(), 1);
}

/// The parser must read the line the collector actually prints.
///
/// The two strings below are copied from the `eprintln!`s in
/// `gc/src/zgc.rs::finish_concurrent_mark` and `gc/src/vm_heap.rs`'s shutdown
/// report. If a field is renamed there this test fails, which is the point: a
/// silently-zero report is worse than no report, and this file's whole output
/// is scraped text.
#[test]
fn the_parser_reads_the_lines_the_collector_prints() {
    let log = "\
[GC] zgc-markstart: pause_us=120 snapshot_us=0 clearbits_us=0 poolspawn_us=3 roots_us=9 registered=0 stale_marked=0 roots=42 marked_roots=42 workers=4
[GC] zgc-markend: scanned_concurrently=900000 scanned_at_safepoint=40000 marked=910000 window_target_ms=120 window_actual_ms=95 satb_replayed=17 passes=2 restarts=0
[GC] zgc-markend: scanned_concurrently=800000 scanned_at_safepoint=200000 marked=990000 window_target_ms=120 window_actual_ms=30 satb_replayed=3 passes=1 restarts=0
[GC] zgc-concurrent: cycles_started=2 cycles_completed=2 black_allocations=5 satb_replayed=20 concurrent_phase_ms=125 scanned_concurrently=1700000 scanned_at_safepoint=240000 window_target_ms=120
";
    let cycles = parse_cycles(log);
    assert_eq!(
        cycles.len(),
        2,
        "one per mark end, and the markstart is not one"
    );
    assert_eq!(
        cycles[0],
        Cycle {
            scanned_concurrently: 900_000,
            scanned_at_safepoint: 40_000,
            window_target_ms: 120,
            window_actual_ms: 95,
        }
    );

    let totals = parse_totals(log).expect("a shutdown line");
    assert_eq!(totals.cycles_started, 2);
    assert_eq!(totals.cycles_completed, 2);
    // The shutdown line's sums must agree with the per-cycle lines, or one of
    // the two is lying about the same run.
    assert_eq!(
        totals.scanned_concurrently,
        cycles.iter().map(|c| c.scanned_concurrently).sum::<u64>()
    );
    assert_eq!(
        totals.scanned_at_safepoint,
        cycles.iter().map(|c| c.scanned_at_safepoint).sum::<u64>()
    );

    // ...and the median is a different number from the ratio of sums even on
    // this two-cycle toy.
    let med = median(cycles.iter().filter_map(Cycle::ratio).collect()).unwrap();
    let sums = ratio_of_sums(&cycles).unwrap();
    assert!((med - sums).abs() > 1e-6);
}

/// A log with no `[GC] zgc-markend:` lines must produce "this run says
/// nothing", never a ratio.
#[test]
fn a_run_that_never_marked_concurrently_reports_nothing_rather_than_zero() {
    let log = "[GC] zgc-concurrent: cycles_started=0 cycles_completed=0 black_allocations=0 \
               satb_replayed=0 concurrent_phase_ms=0 scanned_concurrently=0 \
               scanned_at_safepoint=0 window_target_ms=0\n";
    assert!(parse_cycles(log).is_empty());
    assert_eq!(parse_totals(log).unwrap().cycles_started, 0);
    assert!(median(Vec::new()).is_none());
    assert!(ratio_of_sums(&[]).is_none());
}

// ---------------------------------------------------------------------------
// The report.
// ---------------------------------------------------------------------------

/// Read `proposal-a`'s step-3 bar off a `--verbose:gc` log.
///
/// Set `CRATONVM_ZGC_MARKEND_LOG` to the captured stderr of a run and pass
/// `-- --nocapture`. With it unset this prints the recipe and passes, so the
/// file stays an ordinary test on an ordinary `cargo test`.
///
/// It **asserts nothing about the bar**. Deciding the default is not this
/// file's job and it does not have the other three criteria (wall clock,
/// PASS/HANG/CRASH parity, and a per-collector sweep) in front of it. It fails
/// only when it was pointed at a log it could not use, because a measurement
/// harness that silently reports on an empty set is the failure mode this whole
/// round is about.
#[test]
fn report_the_window_fit_from_a_gc_log() {
    const VAR: &str = "CRATONVM_ZGC_MARKEND_LOG";
    let Ok(path) = std::env::var(VAR) else {
        println!(
            "\n[zgc-r] {VAR} is unset, so there is nothing to report on.\n\
             [zgc-r] To take the reading:\n\
             [zgc-r]   CRATONVM_ZGC_CONC_START=auto cratonvm -XX:+UseZGC --verbose:gc \\\n\
             [zgc-r]       <workload> 2> /tmp/zgc.log\n\
             [zgc-r]   {VAR}=/tmp/zgc.log cargo test -p cratonvm-gc \\\n\
             [zgc-r]       --test zgc_r_conc_window_fit -- --nocapture\n"
        );
        return;
    };
    let log = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{VAR}={path}: {e} -- pointed at a log that cannot be read"));

    let cycles = parse_cycles(&log);
    let totals = parse_totals(&log);

    println!("\n[zgc-r] window fit, from {path}");
    println!("[zgc-r] ---------------------------------------------------------");

    // ENGAGEMENT FIRST. `cycles_started=0` means the run says nothing about
    // concurrent marking, and every number below it would be about an empty
    // set. `proposal-a` lists this as an acceptance criterion in its own right.
    match totals {
        Some(t) => {
            println!(
                "[zgc-r] cycles_started={} cycles_completed={}",
                t.cycles_started, t.cycles_completed
            );
            if t.cycles_started == 0 {
                println!(
                    "[zgc-r] VERDICT: this run says NOTHING about concurrent marking.\n\
                     [zgc-r]   No cycle opened. Either CRATONVM_ZGC_CONC_START is 0 (the\n\
                     [zgc-r]   default), or every collection was a forced System.gc(), which\n\
                     [zgc-r]   deliberately never opens one."
                );
                return;
            }
        }
        None => println!(
            "[zgc-r] no [GC] zgc-concurrent: shutdown line -- engagement unknown \
             (was --verbose:gc on?)"
        ),
    }

    assert!(
        !cycles.is_empty(),
        "{VAR}={path} has no `[GC] zgc-markend:` lines. Either --verbose:gc was \
         off or the run never completed a concurrent cycle; either way there is \
         no median to take, and reporting one would be reporting on nothing."
    );

    let ratios: Vec<f64> = cycles.iter().filter_map(Cycle::ratio).collect();
    let skipped = cycles.len() - ratios.len();
    println!(
        "[zgc-r] cycles with a concurrent phase: {} of {} ({skipped} had none)",
        ratios.len(),
        cycles.len()
    );

    let Some(med) = median(ratios.clone()) else {
        println!(
            "[zgc-r] VERDICT: every cycle had scanned_concurrently=0 -- the cycles\n\
             [zgc-r]   opened and the collection arrived immediately. The window\n\
             [zgc-r]   never ran, so the bar is about nothing."
        );
        return;
    };

    let worst = ratios.iter().copied().fold(f64::MIN, f64::max);
    let best = ratios.iter().copied().fold(f64::MAX, f64::min);
    let sums = ratio_of_sums(&cycles).unwrap_or(f64::NAN);
    let target_ms: Vec<f64> = cycles.iter().map(|c| c.window_target_ms as f64).collect();
    let actual_ms: Vec<f64> = cycles.iter().map(|c| c.window_actual_ms as f64).collect();

    println!("[zgc-r] scanned_at_safepoint / scanned_concurrently");
    println!("[zgc-r]   median  {med:.4}   <-- proposal-a step 3 reads THIS");
    println!("[zgc-r]   best    {best:.4}");
    println!("[zgc-r]   worst   {worst:.4}");
    println!("[zgc-r]   ratio of sums {sums:.4}   ([GC] zgc-concurrent: reports this)");
    println!(
        "[zgc-r] window_target_ms median {:.0}, window_actual_ms median {:.0}",
        median(target_ms).unwrap_or(0.0),
        median(actual_ms).unwrap_or(0.0)
    );
    println!(
        "[zgc-r] VERDICT on the median bar ({MEDIAN_BAR}): {}",
        if med < MEDIAN_BAR { "MET" } else { "NOT MET" }
    );
    println!(
        "[zgc-r] This is ONE of proposal-a's four step-3 criteria. The other\n\
         [zgc-r] three -- wall clock within +5% of the stop-the-world arm,\n\
         [zgc-r] PASS/HANG/CRASH at parity, and cycles_started>0 on every run of\n\
         [zgc-r] a per-collector sweep -- are not in this log. Nothing here\n\
         [zgc-r] flips a default."
    );
}
