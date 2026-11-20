// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! T5.1.2 — HotSpot C2 ratio comparator.
//!
//! Reads two committed baseline files:
//!
//! - `bench/baseline.json`           — CratonVM median_ns per metric.
//! - `bench/hotspot-baseline.json`   — HotSpot C2 median_ns per metric.
//!
//! Emits a human-readable ratio table (`CratonVM / HotSpot` per metric)
//! plus the geometric mean, and exits non-zero when the geomean exceeds
//! the configured threshold (default 1.5× per T5.8.1).
//!
//! Unlike `bench-gate`, this tool never reads criterion's live
//! `target/criterion/` output — it only compares the two committed
//! baselines. That keeps it cheap to run from CI as a separate gate and
//! means re-measuring CratonVM is decoupled from re-measuring HotSpot.
//!
//! ## CLI
//!
//! ```text
//! bench-hotspot-compare
//! bench-hotspot-compare --rust-baseline FILE --hotspot-baseline FILE
//! bench-hotspot-compare --threshold 1.25
//! bench-hotspot-compare --report FILE
//! ```
//!
//! ## Exit codes
//!
//! - `0` — every ratio finite and geomean ≤ threshold (or bootstrap).
//! - `1` — geomean exceeded the threshold.
//! - `2` — a baseline file is missing or malformed.
//! - `3` — neither file has a single non-zero metric (nothing to compare).
//! - `4` — invalid CLI arguments.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// Subset of the bench_gate `Baseline` schema sufficient for ratio
/// comparison. Forward-compatible — unknown fields are ignored.
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
    #[serde(default)]
    pub median_ns: f64,
}

/// Per-metric row emitted into the report.
#[derive(Debug, Clone, Serialize)]
pub struct RatioEntry {
    pub metric: String,
    pub cratonvm_ns: f64,
    pub hotspot_ns: f64,
    /// `cratonvm_ns / hotspot_ns`, or `None` when either side is zero
    /// (bootstrap) or the HotSpot side is not yet captured.
    pub ratio: Option<f64>,
    pub status: RatioStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum RatioStatus {
    /// Both baselines have non-zero values. `ratio` is finite.
    Measured,
    /// HotSpot baseline is a placeholder (0) — can't compute ratio.
    HotspotBootstrap,
    /// CratonVM baseline is a placeholder (0) — nothing to compare.
    RustjvmBootstrap,
    /// Metric exists only on one side.
    Asymmetric,
}

impl RatioStatus {
    fn label(self) -> &'static str {
        match self {
            RatioStatus::Measured => "MEASURED ",
            RatioStatus::HotspotBootstrap => "HS_BOOT  ",
            RatioStatus::RustjvmBootstrap => "RJ_BOOT  ",
            RatioStatus::Asymmetric => "ASYMETRIC",
        }
    }
}

/// Top-level JSON report (also persisted via `--report FILE`).
#[derive(Debug, Clone, Serialize)]
pub struct CompareReport {
    pub threshold: f64,
    pub passed: bool,
    pub geomean_ratio: f64,
    pub entries: Vec<RatioEntry>,
}

fn load_baseline(path: &Path) -> Result<Baseline, String> {
    let raw =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("invalid json in {}: {e}", path.display()))
}

/// Pure comparator — called by tests and the CLI.
pub fn compare(rust: &Baseline, hotspot: &Baseline, threshold: f64) -> CompareReport {
    // Union of keys, so metrics present only on one side show up.
    let mut keys: Vec<String> = rust.metrics.keys().cloned().collect();
    for k in hotspot.metrics.keys() {
        if !rust.metrics.contains_key(k) {
            keys.push(k.clone());
        }
    }
    keys.sort();
    keys.dedup();

    let mut entries = Vec::with_capacity(keys.len());
    let mut measured_ratios = Vec::new();

    for key in keys {
        let r = rust.metrics.get(&key).map(|m| m.median_ns).unwrap_or(0.0);
        let h = hotspot
            .metrics
            .get(&key)
            .map(|m| m.median_ns)
            .unwrap_or(0.0);

        let (ratio, status) = match (r, h) {
            (0.0, 0.0) => (None, RatioStatus::RustjvmBootstrap),
            (0.0, _) => (None, RatioStatus::RustjvmBootstrap),
            (_, 0.0) => {
                if hotspot.metrics.contains_key(&key) {
                    (None, RatioStatus::HotspotBootstrap)
                } else {
                    (None, RatioStatus::Asymmetric)
                }
            }
            (rv, hv) => {
                let ratio = rv / hv;
                measured_ratios.push(ratio);
                (Some(ratio), RatioStatus::Measured)
            }
        };

        // Key present only on one side still surfaces as Asymmetric when
        // the other side doesn't even list the key — distinct from
        // HS_BOOT (placeholder 0). This matters for the CI report.
        let status = if !rust.metrics.contains_key(&key) || !hotspot.metrics.contains_key(&key) {
            RatioStatus::Asymmetric
        } else {
            status
        };

        entries.push(RatioEntry {
            metric: key,
            cratonvm_ns: r,
            hotspot_ns: h,
            ratio,
            status,
        });
    }

    let geomean_ratio = if measured_ratios.is_empty() {
        0.0
    } else {
        let log_sum: f64 = measured_ratios.iter().map(|r| r.ln()).sum();
        (log_sum / measured_ratios.len() as f64).exp()
    };

    // Pass if we have no measured rows (bootstrap) OR geomean ≤ threshold.
    let passed = measured_ratios.is_empty() || geomean_ratio <= threshold;

    CompareReport {
        threshold,
        passed,
        geomean_ratio,
        entries,
    }
}

/// Render a human-readable table.
pub fn format_report(report: &CompareReport) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "{:<40}  {:>14}  {:>14}  {:>8}  {}\n",
        "metric", "cratonvm (ns)", "hotspot (ns)", "ratio", "status",
    ));
    s.push_str(&format!("{}\n", "-".repeat(95)));
    for e in &report.entries {
        let ratio = match e.ratio {
            Some(r) => format!("{r:>6.3}×"),
            None => "   —   ".to_string(),
        };
        s.push_str(&format!(
            "{:<40}  {:>14.0}  {:>14.0}  {:>8}  {}\n",
            e.metric,
            e.cratonvm_ns,
            e.hotspot_ns,
            ratio,
            e.status.label(),
        ));
    }
    s.push_str(&format!("{}\n", "-".repeat(95)));
    s.push_str(&format!(
        "geomean ratio: {:.3}×   threshold: {:.3}×   verdict: {}\n",
        report.geomean_ratio,
        report.threshold,
        if report.passed { "PASS" } else { "FAIL" },
    ));
    s
}

#[derive(Debug, Default)]
struct CliArgs {
    rust_baseline: Option<PathBuf>,
    hotspot_baseline: Option<PathBuf>,
    threshold: Option<f64>,
    report: Option<PathBuf>,
    show_help: bool,
}

fn parse_args<I: Iterator<Item = String>>(mut it: I) -> Result<CliArgs, String> {
    let mut out = CliArgs::default();
    let _exe = it.next(); // skip program name
    while let Some(a) = it.next() {
        match a.as_str() {
            "--rust-baseline" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--rust-baseline needs a value".to_string())?;
                out.rust_baseline = Some(PathBuf::from(v));
            }
            "--hotspot-baseline" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--hotspot-baseline needs a value".to_string())?;
                out.hotspot_baseline = Some(PathBuf::from(v));
            }
            "--threshold" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--threshold needs a value".to_string())?;
                let t: f64 = v.parse().map_err(|e| format!("--threshold: {e}"))?;
                if t <= 0.0 || !t.is_finite() {
                    return Err(format!("--threshold must be > 0, got {t}"));
                }
                out.threshold = Some(t);
            }
            "--report" => {
                let v = it
                    .next()
                    .ok_or_else(|| "--report needs a value".to_string())?;
                out.report = Some(PathBuf::from(v));
            }
            "--help" | "-h" => {
                out.show_help = true;
            }
            other => {
                return Err(format!("unknown argument: {other}"));
            }
        }
    }
    Ok(out)
}

fn print_help() {
    println!(
        "\
bench-hotspot-compare — compare CratonVM baselines against HotSpot C2

USAGE:
    bench-hotspot-compare [OPTIONS]

OPTIONS:
    --rust-baseline FILE     default: vm/bench/baseline.json
    --hotspot-baseline FILE  default: vm/bench/hotspot-baseline.json
    --threshold X            geomean ratio threshold (default: 1.5)
    --report FILE            also write a JSON report
    -h, --help               show this help

EXIT CODES:
    0  gate passed or bootstrap
    1  geomean ratio > threshold
    2  baseline file missing or malformed
    3  no measured metrics
    4  invalid CLI arguments
"
    );
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(4);
        }
    };
    if args.show_help {
        print_help();
        return ExitCode::from(0);
    }
    let rust_path = args
        .rust_baseline
        .unwrap_or_else(default_rust_baseline_path);
    let hotspot_path = args
        .hotspot_baseline
        .unwrap_or_else(default_hotspot_baseline_path);
    let threshold = args.threshold.unwrap_or(1.5);

    let rust = match load_baseline(&rust_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let hotspot = match load_baseline(&hotspot_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let report = compare(&rust, &hotspot, threshold);
    print!("{}", format_report(&report));

    if let Some(path) = args.report {
        let raw = match serde_json::to_string_pretty(&report) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: serialize report: {e}");
                return ExitCode::from(2);
            }
        };
        if let Err(e) = fs::write(&path, raw) {
            eprintln!("error: writing {}: {e}", path.display());
            return ExitCode::from(2);
        }
    }

    let measured = report
        .entries
        .iter()
        .any(|e| matches!(e.status, RatioStatus::Measured));
    if !measured {
        // Neither baseline captured yet: exit 3 so CI treats it as "skip".
        return ExitCode::from(3);
    }
    if !report.passed {
        return ExitCode::from(1);
    }
    ExitCode::from(0)
}

fn default_rust_baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/baseline.json")
}

fn default_hotspot_baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bench/hotspot-baseline.json")
}

#[cfg(test)]
mod tests;
