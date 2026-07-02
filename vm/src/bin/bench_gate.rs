// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! NEW-20: benchmark gate.
//!
//! Reads the criterion run output in `target/criterion/<bench>/.../estimates.json`,
//! compares each metric against a committed baseline JSON file
//! (`vm/bench/baseline.json` by default), and exits non-zero if any
//! metric regressed more than the configured threshold (default 15%,
//! per roadmap NEW-20.2).
//!
//! The gate is intentionally framework-light: it never invokes
//! criterion itself, so unit tests can drive the comparator with
//! synthetic estimates without any benchmark execution.
//!
//! ## CLI
//!
//! ```text
//! bench-gate                         # gate against vm/bench/baseline.json + 15%
//! bench-gate --baseline FILE
//! bench-gate --threshold 0.10        # 10% instead of 15%
//! bench-gate --update-baseline       # overwrite baseline with current run
//! bench-gate --report FILE           # also write JSON report
//! bench-gate --criterion-dir DIR     # alternate criterion output root
//! ```
//!
//! ## Exit codes
//!
//! - `0` — gate passed (every metric within threshold or bootstrapping).
//! - `1` — at least one regression or required metric missing from run.
//! - `2` — baseline file missing/corrupt and --update-baseline not given.
//! - `3` — no criterion data found under target/criterion/. Run
//!   `cargo bench --bench vm_benchmarks` first.
//! - `4` — invalid CLI arguments.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// ---------------------------------------------------------------------------
// Schemas
// ---------------------------------------------------------------------------

/// Committed baseline file format. Forward-compatible: unknown fields
/// are ignored, missing per-metric fields default to zero.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Baseline {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub captured_at: String,
    #[serde(default)]
    pub metrics: BTreeMap<String, MetricEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetricEntry {
    /// Median wall-clock per iteration in nanoseconds. A value of 0 is
    /// treated as a placeholder ("uninitialized") so the gate
    /// bootstraps cleanly without requiring a one-shot manual run.
    #[serde(default)]
    pub median_ns: f64,
}

/// Output report (also persisted via --report FILE).
#[derive(Debug, Clone, Serialize, Default)]
pub struct GateReport {
    pub threshold: f64,
    pub passed: bool,
    pub geomean_delta: f64,
    pub entries: Vec<ReportEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReportEntry {
    pub metric: String,
    pub baseline_ns: f64,
    pub current_ns: f64,
    pub delta_pct: f64,
    pub status: GateStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GateStatus {
    /// Within threshold (or improvement).
    Ok,
    /// Baseline value is 0 → bootstrap; not a failure.
    Bootstrap,
    /// Metric is in baseline but missing from the current run.
    Missing,
    /// Regression > threshold.
    Regress,
    /// Metric is in the current run but not in the baseline.
    New,
}

impl GateStatus {
    fn label(self) -> &'static str {
        match self {
            GateStatus::Ok => "OK       ",
            GateStatus::Bootstrap => "BOOTSTRAP",
            GateStatus::Missing => "MISSING  ",
            GateStatus::Regress => "REGRESS  ",
            GateStatus::New => "NEW      ",
        }
    }
    fn is_failure(self) -> bool {
        matches!(self, GateStatus::Missing | GateStatus::Regress)
    }
}

// ---------------------------------------------------------------------------
// Criterion estimate parsing
// ---------------------------------------------------------------------------

/// Subset of `criterion`'s estimates.json format that we care about.
/// Criterion writes a struct of bootstrap estimates per metric; we only
/// need the median's point estimate.
#[derive(Debug, Deserialize)]
struct CriterionEstimates {
    median: CriterionPointEstimate,
}

#[derive(Debug, Deserialize)]
struct CriterionPointEstimate {
    point_estimate: f64,
}

/// Read a single estimates.json into nanoseconds (criterion's median is
/// already in ns).
fn read_estimates_ns(path: &Path) -> Result<f64, GateError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| GateError::Io(format!("reading {}: {e}", path.display())))?;
    let parsed: CriterionEstimates = serde_json::from_str(&raw)
        .map_err(|e| GateError::Parse(format!("parsing {}: {e}", path.display())))?;
    Ok(parsed.median.point_estimate)
}

/// Walk `criterion_dir/<bench>/<id>/new/estimates.json` and collect every
/// metric. The metric key follows the format `bench/id` (matching the
/// keys we write into baseline.json), or just `bench` if the bench has
/// no subgroup.
pub fn collect_run(criterion_dir: &Path) -> Result<BTreeMap<String, f64>, GateError> {
    let mut out = BTreeMap::new();
    if !criterion_dir.exists() {
        return Err(GateError::NoData(format!(
            "{} does not exist — run `cargo bench` first",
            criterion_dir.display()
        )));
    }
    walk_criterion(criterion_dir, criterion_dir, &mut out)?;
    if out.is_empty() {
        return Err(GateError::NoData(format!(
            "{} contains no estimates.json files",
            criterion_dir.display()
        )));
    }
    Ok(out)
}

fn walk_criterion(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, f64>,
) -> Result<(), GateError> {
    let read = match fs::read_dir(dir) {
        Ok(r) => r,
        Err(e) => return Err(GateError::Io(format!("reading dir {}: {e}", dir.display()))),
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip the criterion bookkeeping dirs.
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name == "report" || name == "base" || name == "change" {
                continue;
            }
            walk_criterion(root, &path, out)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("estimates.json") {
            // The "new" estimates live under <root>/<bench>/.../new/estimates.json
            // We require the parent's parent's name to be "new" to filter out
            // baseline/change estimates.
            if path
                .parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                != Some("new")
            {
                continue;
            }
            // Build the metric key from the path components between root and the
            // "new" directory, joined with '/'.
            let key = metric_key(root, &path);
            if key.is_empty() {
                continue;
            }
            let ns = read_estimates_ns(&path)?;
            out.insert(key, ns);
        }
    }
    Ok(())
}

fn metric_key(root: &Path, estimates_path: &Path) -> String {
    // estimates_path = root/<bench>/<id?>/new/estimates.json
    // We strip "new/estimates.json" off the end, then path-relative-to root.
    let parent = match estimates_path.parent().and_then(|p| p.parent()) {
        Some(p) => p,
        None => return String::new(),
    };
    let rel = match parent.strip_prefix(root) {
        Ok(r) => r,
        Err(_) => return String::new(),
    };
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

// ---------------------------------------------------------------------------
// Comparator
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum GateError {
    Io(String),
    Parse(String),
    NoData(String),
    BaselineCorrupt(String),
    BadArgs(String),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::Io(m) => write!(f, "io error: {m}"),
            GateError::Parse(m) => write!(f, "parse error: {m}"),
            GateError::NoData(m) => write!(f, "no data: {m}"),
            GateError::BaselineCorrupt(m) => write!(f, "baseline corrupt: {m}"),
            GateError::BadArgs(m) => write!(f, "bad args: {m}"),
        }
    }
}

impl std::error::Error for GateError {}

/// Run the comparator. Returns a `GateReport` describing the outcome.
/// Pure function — used by tests as well as the CLI.
pub fn compare(baseline: &Baseline, run: &BTreeMap<String, f64>, threshold: f64) -> GateReport {
    let mut entries = Vec::new();
    let mut deltas_for_geomean = Vec::new();

    // Walk the union of baseline + run keys so we report MISSING/NEW.
    let mut all_keys: Vec<String> = baseline.metrics.keys().cloned().collect();
    for k in run.keys() {
        if !baseline.metrics.contains_key(k) {
            all_keys.push(k.clone());
        }
    }
    all_keys.sort();
    all_keys.dedup();

    for key in all_keys {
        let baseline_ns = baseline
            .metrics
            .get(&key)
            .map(|m| m.median_ns)
            .unwrap_or(0.0);
        let current_ns_opt = run.get(&key).copied();

        let (current_ns, status, delta_pct) = match (baseline_ns, current_ns_opt) {
            (0.0, Some(curr)) => (curr, GateStatus::Bootstrap, 0.0),
            (0.0, None) => (0.0, GateStatus::Bootstrap, 0.0),
            (_base, None) => (0.0, GateStatus::Missing, -100.0_f64),
            (base, Some(curr)) => {
                let delta = (curr - base) / base;
                let status = if delta > threshold {
                    GateStatus::Regress
                } else {
                    GateStatus::Ok
                };
                deltas_for_geomean.push(curr / base);
                (curr, status, delta * 100.0)
            }
        };

        // Bootstrapped baseline => not a regression but skip from geomean.
        // Status::New: baseline missing for this run-only key; not a fail.
        let mut final_status = status;
        if baseline_ns == 0.0 && current_ns_opt.is_some() && !baseline.metrics.contains_key(&key) {
            final_status = GateStatus::New;
        }

        entries.push(ReportEntry {
            metric: key,
            baseline_ns,
            current_ns,
            delta_pct,
            status: final_status,
        });
    }

    let geomean_delta = if deltas_for_geomean.is_empty() {
        0.0
    } else {
        let log_sum: f64 = deltas_for_geomean.iter().map(|r| r.ln()).sum();
        let g = (log_sum / deltas_for_geomean.len() as f64).exp();
        (g - 1.0) * 100.0
    };

    let any_failure = entries.iter().any(|e| e.status.is_failure());
    let geomean_fail = (geomean_delta / 100.0) > threshold;
    GateReport {
        threshold,
        passed: !any_failure && !geomean_fail,
        geomean_delta,
        entries,
    }
}

/// Render a human-readable table of the report.
pub fn format_report(report: &GateReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<40}  {:>14}  {:>14}  {:>9}  {}\n",
        "metric", "baseline (ns)", "current (ns)", "delta", "status",
    ));
    s.push_str(&format!("{}\n", "-".repeat(95)));
    for e in &report.entries {
        s.push_str(&format!(
            "{:<40}  {:>14.0}  {:>14.0}  {:>+8.2}%  {}\n",
            e.metric,
            e.baseline_ns,
            e.current_ns,
            e.delta_pct,
            e.status.label(),
        ));
    }
    s.push_str(&format!("{}\n", "-".repeat(95)));
    s.push_str(&format!(
        "geomean delta: {:+.2}%   threshold: {:.2}%   verdict: {}\n",
        report.geomean_delta,
        report.threshold * 100.0,
        if report.passed { "PASS" } else { "FAIL" },
    ));
    s
}

// ---------------------------------------------------------------------------
// Baseline I/O
// ---------------------------------------------------------------------------

pub fn load_baseline(path: &Path) -> Result<Baseline, GateError> {
    let raw = fs::read_to_string(path)
        .map_err(|e| GateError::BaselineCorrupt(format!("cannot read {}: {e}", path.display())))?;
    serde_json::from_str(&raw)
        .map_err(|e| GateError::BaselineCorrupt(format!("invalid json in {}: {e}", path.display())))
}

pub fn save_baseline(path: &Path, baseline: &Baseline) -> Result<(), GateError> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| GateError::Io(format!("creating {}: {e}", parent.display())))?;
        }
    }
    let raw = serde_json::to_string_pretty(baseline)
        .map_err(|e| GateError::Io(format!("serializing baseline: {e}")))?;
    fs::write(path, raw).map_err(|e| GateError::Io(format!("writing {}: {e}", path.display())))
}

pub fn baseline_from_run(run: &BTreeMap<String, f64>, host: String) -> Baseline {
    let metrics = run
        .iter()
        .map(|(k, v)| (k.clone(), MetricEntry { median_ns: *v }))
        .collect();
    Baseline {
        schema_version: 1,
        host,
        captured_at: chrono_like_timestamp(),
        metrics,
    }
}

/// Lightweight RFC3339-shaped timestamp without bringing in `chrono`.
/// Format: YYYY-MM-DDTHH:MM:SSZ derived from SystemTime.
fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // Convert epoch seconds to a Y/M/D H:M:S string by hand. We do not
    // care about leap seconds; this only feeds humans inspecting the
    // baseline file.
    let (y, mo, d, h, mi, s) = epoch_to_ymdhms(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn epoch_to_ymdhms(mut secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let s = (secs.rem_euclid(60)) as u32;
    secs = secs.div_euclid(60);
    let mi = (secs.rem_euclid(60)) as u32;
    secs = secs.div_euclid(60);
    let h = (secs.rem_euclid(24)) as u32;
    let mut days = secs.div_euclid(24);

    // Compute civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    days += 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = (yoe + era * 400) as i32;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if mo <= 2 { y + 1 } else { y };
    (y, mo, d, h, mi, s)
}

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct CliArgs {
    baseline: Option<PathBuf>,
    threshold: Option<f64>,
    update_baseline: bool,
    report: Option<PathBuf>,
    criterion_dir: Option<PathBuf>,
    show_help: bool,
}

fn parse_args<I: IntoIterator<Item = String>>(it: I) -> Result<CliArgs, GateError> {
    let mut args = CliArgs::default();
    let mut iter = it.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--baseline" => {
                let v = iter
                    .next()
                    .ok_or_else(|| GateError::BadArgs("--baseline requires a path".into()))?;
                args.baseline = Some(PathBuf::from(v));
            }
            "--threshold" => {
                let v = iter
                    .next()
                    .ok_or_else(|| GateError::BadArgs("--threshold requires a number".into()))?;
                let n: f64 = v
                    .parse()
                    .map_err(|e| GateError::BadArgs(format!("--threshold not a float: {e}")))?;
                if !(n >= 0.0 && n.is_finite()) {
                    return Err(GateError::BadArgs(
                        "--threshold must be >= 0 and finite".into(),
                    ));
                }
                args.threshold = Some(n);
            }
            "--update-baseline" => args.update_baseline = true,
            "--report" => {
                let v = iter
                    .next()
                    .ok_or_else(|| GateError::BadArgs("--report requires a path".into()))?;
                args.report = Some(PathBuf::from(v));
            }
            "--criterion-dir" => {
                let v = iter
                    .next()
                    .ok_or_else(|| GateError::BadArgs("--criterion-dir requires a path".into()))?;
                args.criterion_dir = Some(PathBuf::from(v));
            }
            "-h" | "--help" => args.show_help = true,
            other => {
                return Err(GateError::BadArgs(format!("unknown arg: {other}")));
            }
        }
    }
    Ok(args)
}

const USAGE: &str = "\
bench-gate — CratonVM benchmark regression gate

USAGE:
    bench-gate [--baseline FILE] [--threshold F] [--update-baseline]
               [--report FILE] [--criterion-dir DIR]

OPTIONS:
    --baseline FILE        baseline JSON file (default: vm/bench/baseline.json)
    --threshold F          regression threshold as a fraction (default: 0.15)
    --update-baseline      overwrite the baseline with the current run
    --report FILE          write the report as JSON to FILE
    --criterion-dir DIR    criterion output root (default: target/criterion)
    -h, --help             show this help

EXIT CODES:
    0  gate passed
    1  regression detected
    2  baseline file missing or corrupt
    3  no criterion data found
    4  invalid CLI arguments
";

fn run(args: CliArgs) -> Result<i32, GateError> {
    if args.show_help {
        println!("{USAGE}");
        return Ok(0);
    }
    let threshold = args.threshold.unwrap_or(0.15);
    let baseline_path = args.baseline.unwrap_or_else(default_baseline_path);
    let criterion_dir = args
        .criterion_dir
        .unwrap_or_else(|| PathBuf::from("target/criterion"));

    let run = collect_run(&criterion_dir)?;

    if args.update_baseline {
        let host = host_label();
        let new_base = baseline_from_run(&run, host);
        save_baseline(&baseline_path, &new_base)?;
        println!(
            "wrote {} metrics to {}",
            new_base.metrics.len(),
            baseline_path.display()
        );
        return Ok(0);
    }

    let baseline = load_baseline(&baseline_path)?;

    let report = compare(&baseline, &run, threshold);
    print!("{}", format_report(&report));

    if let Some(rp) = args.report {
        let raw = serde_json::to_string_pretty(&report)
            .map_err(|e| GateError::Io(format!("serialize report: {e}")))?;
        fs::write(&rp, raw).map_err(|e| GateError::Io(format!("writing {}: {e}", rp.display())))?;
    }
    Ok(if report.passed { 0 } else { 1 })
}

fn host_label() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH,)
}

fn default_baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/baseline.json")
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match parse_args(argv) {
        Err(GateError::BadArgs(m)) => {
            eprintln!("bench-gate: {m}");
            eprintln!("{USAGE}");
            ExitCode::from(4)
        }
        Err(e) => {
            eprintln!("bench-gate: {e}");
            ExitCode::from(2)
        }
        Ok(args) => match run(args) {
            Ok(code) => ExitCode::from(code as u8),
            Err(GateError::NoData(m)) => {
                eprintln!("bench-gate: {m}");
                ExitCode::from(3)
            }
            Err(GateError::BaselineCorrupt(m)) => {
                eprintln!("bench-gate: {m}");
                ExitCode::from(2)
            }
            Err(e) => {
                eprintln!("bench-gate: {e}");
                ExitCode::from(1)
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Tests (CP5)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn base(metrics: &[(&str, f64)]) -> Baseline {
        Baseline {
            schema_version: 1,
            host: "test".into(),
            captured_at: "1970-01-01T00:00:00Z".into(),
            metrics: metrics
                .iter()
                .map(|(k, v)| (k.to_string(), MetricEntry { median_ns: *v }))
                .collect(),
        }
    }
    fn run(metrics: &[(&str, f64)]) -> BTreeMap<String, f64> {
        metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn regression_above_threshold_fails() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 130.0)]); // +30%
        let report = compare(&b, &r, 0.15);
        assert!(!report.passed);
        assert_eq!(report.entries[0].status, GateStatus::Regress);
        assert!((report.entries[0].delta_pct - 30.0).abs() < 1e-9);
    }

    #[test]
    fn improvement_below_threshold_passes() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 90.0)]); // -10%
        let report = compare(&b, &r, 0.15);
        assert!(report.passed);
        assert_eq!(report.entries[0].status, GateStatus::Ok);
        assert!((report.entries[0].delta_pct + 10.0).abs() < 1e-9);
    }

    #[test]
    fn small_regression_within_threshold_passes() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 110.0)]); // +10%
        let report = compare(&b, &r, 0.15);
        assert!(report.passed);
        assert_eq!(report.entries[0].status, GateStatus::Ok);
    }

    #[test]
    fn placeholder_baseline_bootstraps() {
        let b = base(&[("foo", 0.0)]);
        let r = run(&[("foo", 12345.0)]);
        let report = compare(&b, &r, 0.15);
        assert!(report.passed);
        assert_eq!(report.entries[0].status, GateStatus::Bootstrap);
    }

    #[test]
    fn missing_metric_in_run_fails() {
        let b = base(&[("foo", 100.0), ("bar", 200.0)]);
        let r = run(&[("foo", 105.0)]);
        let report = compare(&b, &r, 0.15);
        assert!(!report.passed);
        // bar is reported as MISSING (Status::Missing)
        let bar = report.entries.iter().find(|e| e.metric == "bar").unwrap();
        assert_eq!(bar.status, GateStatus::Missing);
    }

    #[test]
    fn new_metric_is_reported_but_does_not_fail() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 100.0), ("baz", 50.0)]);
        let report = compare(&b, &r, 0.15);
        assert!(report.passed);
        let baz = report.entries.iter().find(|e| e.metric == "baz").unwrap();
        assert_eq!(baz.status, GateStatus::New);
    }

    #[test]
    fn geomean_computes_correctly() {
        // Two metrics: +20% and -20% → geomean ratio sqrt(1.2 * 0.8) ≈ 0.9798
        // → delta ≈ -2.02%
        let b = base(&[("a", 100.0), ("b", 100.0)]);
        let r = run(&[("a", 120.0), ("b", 80.0)]);
        let report = compare(&b, &r, 0.25);
        assert!((report.geomean_delta - (-2.0203)).abs() < 0.01);
    }

    #[test]
    fn geomean_above_threshold_fails_even_when_individuals_pass() {
        // Each metric is +14% (within 15%) but the geomean is still 14%
        // and we tighten the threshold to 0.10 to force a fail.
        let b = base(&[("a", 100.0), ("b", 100.0)]);
        let r = run(&[("a", 114.0), ("b", 114.0)]);
        let report = compare(&b, &r, 0.10);
        assert!(!report.passed, "expected fail on geomean");
    }

    #[test]
    fn baseline_from_run_round_trips() {
        let r = run(&[("a", 1.5), ("b", 2.5)]);
        let b = baseline_from_run(&r, "host".into());
        assert_eq!(b.metrics.len(), 2);
        assert_eq!(b.metrics["a"].median_ns, 1.5);
    }

    #[test]
    fn baseline_save_load_round_trip() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-test");
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("baseline.json");
        let r = run(&[("a", 1.0), ("b", 2.0)]);
        let b = baseline_from_run(&r, "x".into());
        save_baseline(&path, &b).unwrap();
        let loaded = load_baseline(&path).unwrap();
        assert_eq!(loaded.metrics.len(), 2);
        assert_eq!(loaded.metrics["a"].median_ns, 1.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_baseline_returns_error() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-corrupt");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("baseline.json");
        fs::write(&path, "{not valid json").unwrap();
        let err = load_baseline(&path).unwrap_err();
        assert!(matches!(err, GateError::BaselineCorrupt(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_fails_when_baseline_is_missing() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-missing-baseline");
        let _ = fs::remove_dir_all(&dir);
        let criterion = dir.join("criterion").join("foo").join("new");
        fs::create_dir_all(&criterion).unwrap();
        fs::write(
            criterion.join("estimates.json"),
            r#"{"median":{"point_estimate":100.0}}"#,
        )
        .unwrap();

        let err = run(CliArgs {
            baseline: Some(dir.join("baseline.json")),
            criterion_dir: Some(dir.join("criterion")),
            ..CliArgs::default()
        })
        .unwrap_err();
        assert!(matches!(err, GateError::BaselineCorrupt(_)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_update_baseline_bootstraps_missing_file() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-update-baseline");
        let _ = fs::remove_dir_all(&dir);
        let criterion = dir.join("criterion").join("foo").join("new");
        fs::create_dir_all(&criterion).unwrap();
        fs::write(
            criterion.join("estimates.json"),
            r#"{"median":{"point_estimate":100.0}}"#,
        )
        .unwrap();
        let baseline = dir.join("baseline.json");

        let code = run(CliArgs {
            baseline: Some(baseline.clone()),
            criterion_dir: Some(dir.join("criterion")),
            update_baseline: true,
            ..CliArgs::default()
        })
        .unwrap();
        assert_eq!(code, 0);
        assert!(baseline.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_estimates_reads_criterion_format() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-parse");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("estimates.json");
        // Minimal criterion estimates.json: a "median" object with a
        // "point_estimate" field. We only read those two fields.
        fs::write(
            &path,
            r#"{
                "median": { "point_estimate": 4242.5 },
                "mean":   { "point_estimate": 5000.0 }
            }"#,
        )
        .unwrap();
        let ns = read_estimates_ns(&path).unwrap();
        assert!((ns - 4242.5).abs() < 1e-9);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_run_walks_criterion_layout() {
        let dir = std::env::temp_dir().join("cratonvm-bench-gate-walk");
        let _ = fs::remove_dir_all(&dir);
        // target/criterion/<group>/<id>/new/estimates.json
        let bench_a = dir.join("vm_startup").join("new");
        fs::create_dir_all(&bench_a).unwrap();
        fs::write(
            bench_a.join("estimates.json"),
            r#"{"median":{"point_estimate":111.0}}"#,
        )
        .unwrap();
        let bench_b = dir
            .join("interpreter_counting_loop")
            .join("1000")
            .join("new");
        fs::create_dir_all(&bench_b).unwrap();
        fs::write(
            bench_b.join("estimates.json"),
            r#"{"median":{"point_estimate":222.0}}"#,
        )
        .unwrap();

        let run = collect_run(&dir).unwrap();
        assert_eq!(run.len(), 2);
        assert_eq!(run["vm_startup"], 111.0);
        assert_eq!(run["interpreter_counting_loop/1000"], 222.0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn collect_run_errors_when_directory_missing() {
        let p = std::env::temp_dir().join("cratonvm-bench-gate-missing-xyz");
        let _ = fs::remove_dir_all(&p);
        let err = collect_run(&p).unwrap_err();
        assert!(matches!(err, GateError::NoData(_)));
    }

    #[test]
    fn parse_args_understands_flags() {
        let args = parse_args(
            [
                "--threshold",
                "0.10",
                "--update-baseline",
                "--baseline",
                "x.json",
            ]
            .iter()
            .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(args.threshold, Some(0.10));
        assert!(args.update_baseline);
        assert_eq!(args.baseline.as_deref(), Some(Path::new("x.json")));
    }

    #[test]
    fn parse_args_rejects_negative_threshold() {
        let err = parse_args(["--threshold", "-0.1"].iter().map(|s| s.to_string())).unwrap_err();
        assert!(matches!(err, GateError::BadArgs(_)));
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let err = parse_args(["--nope"].iter().map(|s| s.to_string())).unwrap_err();
        assert!(matches!(err, GateError::BadArgs(_)));
    }

    #[test]
    fn epoch_to_ymdhms_known_dates() {
        // 2024-01-01T00:00:00Z = 1704067200
        let (y, mo, d, h, mi, s) = epoch_to_ymdhms(1_704_067_200);
        assert_eq!((y, mo, d, h, mi, s), (2024, 1, 1, 0, 0, 0));
        // 1970-01-01T00:00:00Z
        assert_eq!(epoch_to_ymdhms(0), (1970, 1, 1, 0, 0, 0));
    }

    #[test]
    fn format_report_contains_metric_and_verdict() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 130.0)]);
        let report = compare(&b, &r, 0.15);
        let s = format_report(&report);
        assert!(s.contains("foo"));
        assert!(s.contains("REGRESS"));
        assert!(s.contains("FAIL"));
    }

    #[test]
    fn format_report_pass_verdict() {
        let b = base(&[("foo", 100.0)]);
        let r = run(&[("foo", 105.0)]);
        let report = compare(&b, &r, 0.15);
        let s = format_report(&report);
        assert!(s.contains("PASS"));
    }
}
