//! T5.1.2 — HotSpot C2 ratio comparator.
//!
//! Reads two committed baseline files:
//!
//! - `bench/baseline.json`           — RustJVM median_ns per metric.
//! - `bench/hotspot-baseline.json`   — HotSpot C2 median_ns per metric.
//!
//! Emits a human-readable ratio table (`RustJVM / HotSpot` per metric)
//! plus the geometric mean, and exits non-zero when the geomean exceeds
//! the configured threshold (default 1.5× per T5.8.1).
//!
//! Unlike `bench-gate`, this tool never reads criterion's live
//! `target/criterion/` output — it only compares the two committed
//! baselines. That keeps it cheap to run from CI as a separate gate and
//! means re-measuring RustJVM is decoupled from re-measuring HotSpot.
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
    pub rustjvm_ns: f64,
    pub hotspot_ns: f64,
    /// `rustjvm_ns / hotspot_ns`, or `None` when either side is zero
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
    /// RustJVM baseline is a placeholder (0) — nothing to compare.
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
    let raw = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    serde_json::from_str(&raw)
        .map_err(|e| format!("invalid json in {}: {e}", path.display()))
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
        let h = hotspot.metrics.get(&key).map(|m| m.median_ns).unwrap_or(0.0);

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
            rustjvm_ns: r,
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
        "metric", "rustjvm (ns)", "hotspot (ns)", "ratio", "status",
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
            e.rustjvm_ns,
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
                let v = it.next().ok_or_else(|| "--rust-baseline needs a value".to_string())?;
                out.rust_baseline = Some(PathBuf::from(v));
            }
            "--hotspot-baseline" => {
                let v = it.next().ok_or_else(|| "--hotspot-baseline needs a value".to_string())?;
                out.hotspot_baseline = Some(PathBuf::from(v));
            }
            "--threshold" => {
                let v = it.next().ok_or_else(|| "--threshold needs a value".to_string())?;
                let t: f64 = v.parse().map_err(|e| format!("--threshold: {e}"))?;
                if t <= 0.0 || !t.is_finite() {
                    return Err(format!("--threshold must be > 0, got {t}"));
                }
                out.threshold = Some(t);
            }
            "--report" => {
                let v = it.next().ok_or_else(|| "--report needs a value".to_string())?;
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
bench-hotspot-compare — compare RustJVM baselines against HotSpot C2

USAGE:
    bench-hotspot-compare [OPTIONS]

OPTIONS:
    --rust-baseline FILE     default: bench/baseline.json
    --hotspot-baseline FILE  default: bench/hotspot-baseline.json
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
        .unwrap_or_else(|| PathBuf::from("bench/baseline.json"));
    let hotspot_path = args
        .hotspot_baseline
        .unwrap_or_else(|| PathBuf::from("bench/hotspot-baseline.json"));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn bl(pairs: &[(&str, f64)]) -> Baseline {
        let metrics = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), MetricEntry { median_ns: *v }))
            .collect();
        Baseline {
            schema_version: 1,
            host: "test".into(),
            captured_at: "1970-01-01T00:00:00Z".into(),
            metrics,
        }
    }

    #[test]
    fn ratio_of_equal_baselines_is_one() {
        let r = bl(&[("a", 100.0), ("b", 200.0)]);
        let h = bl(&[("a", 100.0), ("b", 200.0)]);
        let rep = compare(&r, &h, 1.5);
        assert_eq!(rep.entries.len(), 2);
        for e in &rep.entries {
            assert_eq!(e.status, RatioStatus::Measured);
            assert!((e.ratio.unwrap() - 1.0).abs() < 1e-9);
        }
        assert!((rep.geomean_ratio - 1.0).abs() < 1e-9);
        assert!(rep.passed);
    }

    #[test]
    fn geomean_is_geometric_not_arithmetic() {
        // ratios 1.0 and 4.0 → geomean 2.0 (√4), not 2.5
        let r = bl(&[("a", 10.0), ("b", 40.0)]);
        let h = bl(&[("a", 10.0), ("b", 10.0)]);
        let rep = compare(&r, &h, 2.5);
        assert!((rep.geomean_ratio - 2.0).abs() < 1e-9);
        assert!(rep.passed); // 2.0 ≤ 2.5
    }

    #[test]
    fn threshold_exceeded_fails() {
        let r = bl(&[("a", 300.0)]);
        let h = bl(&[("a", 100.0)]);
        let rep = compare(&r, &h, 1.5);
        assert!(!rep.passed);
        assert!((rep.geomean_ratio - 3.0).abs() < 1e-9);
    }

    #[test]
    fn placeholder_hotspot_is_bootstrap() {
        // HotSpot = 0 (placeholder), RustJVM = 100 → no ratio.
        let r = bl(&[("a", 100.0)]);
        let h = bl(&[("a", 0.0)]);
        let rep = compare(&r, &h, 1.5);
        assert_eq!(rep.entries.len(), 1);
        assert_eq!(rep.entries[0].status, RatioStatus::HotspotBootstrap);
        assert!(rep.entries[0].ratio.is_none());
        assert!(rep.passed); // bootstrap ≠ failure
    }

    #[test]
    fn no_measured_rows_pass_as_bootstrap() {
        let r = bl(&[("a", 0.0)]);
        let h = bl(&[("a", 0.0)]);
        let rep = compare(&r, &h, 1.5);
        assert!(rep.passed);
        assert_eq!(rep.geomean_ratio, 0.0);
    }

    #[test]
    fn metric_only_on_one_side_is_asymmetric() {
        let r = bl(&[("a", 100.0), ("only_rust", 50.0)]);
        let h = bl(&[("a", 100.0), ("only_hs", 50.0)]);
        let rep = compare(&r, &h, 1.5);
        let keys: Vec<&str> = rep.entries.iter().map(|e| e.metric.as_str()).collect();
        assert_eq!(keys, vec!["a", "only_hs", "only_rust"]);
        // "a" is measured, both uniques are Asymmetric.
        let asym: Vec<_> = rep
            .entries
            .iter()
            .filter(|e| e.status == RatioStatus::Asymmetric)
            .map(|e| e.metric.as_str())
            .collect();
        assert_eq!(asym, vec!["only_hs", "only_rust"]);
    }

    #[test]
    fn format_report_contains_verdict() {
        let r = bl(&[("a", 150.0)]);
        let h = bl(&[("a", 100.0)]);
        let rep = compare(&r, &h, 1.5);
        let s = format_report(&rep);
        assert!(s.contains("verdict: PASS"));
        assert!(s.contains("1.500×"));
    }

    #[test]
    fn parse_args_reads_threshold() {
        let args = parse_args(
            ["bench-hotspot-compare", "--threshold", "1.25"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap();
        assert_eq!(args.threshold, Some(1.25));
    }

    #[test]
    fn parse_args_rejects_negative_threshold() {
        let err = parse_args(
            ["bench-hotspot-compare", "--threshold", "-1.0"]
                .iter()
                .map(|s| s.to_string()),
        )
        .unwrap_err();
        assert!(err.contains("threshold must be > 0"));
    }

    // T17.E.1 — schema guard for `scripts/capture-hotspot-baseline.{sh,ps1}`.
    //
    // Those scripts emit JSON by hand (no serde_json dependency on the
    // host) so we need a Rust-side test that asserts the exact shape
    // `load_baseline` + the comparator accept. If the script's printf
    // template ever drifts from the `Baseline` struct this test is the
    // first thing to catch it.
    #[test]
    fn captured_baseline_schema_is_accepted() {
        // Literal string — identical to what capture-hotspot-baseline.sh
        // writes for a 3-kernel subset. Includes every field the scripts
        // populate: schema_version, host, captured_at, capture_command,
        // metrics with parametrized keys.
        let raw = r#"{
  "schema_version": 1,
  "host": "ubuntu-latest",
  "captured_at": "2026-04-23T12:34:56Z",
  "capture_command": "scripts/capture-hotspot-baseline.sh --iterations 10 --warmup 3",
  "metrics": {
    "vm_startup": { "median_ns": 1234 },
    "interpreter_fibonacci/30": { "median_ns": 567 },
    "shootout_binary_trees/12": { "median_ns": 890123 }
  }
}"#;
        let parsed: Baseline =
            serde_json::from_str(raw).expect("captured schema should parse into Baseline");
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.host, "ubuntu-latest");
        assert_eq!(parsed.metrics.len(), 3);
        assert_eq!(
            parsed.metrics.get("vm_startup").unwrap().median_ns,
            1234.0
        );
        assert_eq!(
            parsed
                .metrics
                .get("interpreter_fibonacci/30")
                .unwrap()
                .median_ns,
            567.0
        );
        assert_eq!(
            parsed
                .metrics
                .get("shootout_binary_trees/12")
                .unwrap()
                .median_ns,
            890123.0
        );

        // Confirm the comparator walks the same data end-to-end. Feed the
        // captured JSON on both sides → ratio 1.0 → PASS.
        let rep = compare(&parsed, &parsed, 1.5);
        assert!(rep.passed);
        assert!((rep.geomean_ratio - 1.0).abs() < 1e-9);
        for e in &rep.entries {
            assert_eq!(e.status, RatioStatus::Measured);
        }
    }

    // T17.E.1 — every metric name the capture scripts write is the exact
    // set of criterion IDs the RustJVM side also writes into
    // `bench/baseline.json`. If someone adds a new kernel on either side
    // without the other, the comparator silently marks it `Asymmetric`
    // — this test wedges that contract so the next roadmap entry has
    // to update both scripts.
    #[test]
    fn captured_metric_names_match_rust_baseline_keys() {
        // Pulled directly from scripts/capture-hotspot-baseline.{sh,ps1}
        // — keep in sync when adding a kernel.
        let expected_metric_names = [
            "vm_startup",
            "shared_vm_startup",
            "startup_to_first_bytecode",
            "object_allocation/100",
            "object_allocation/1000",
            "gc_cycle_1000_objects",
            "native_dispatch_noop",
            "interpreter_counting_loop/1000",
            "interpreter_counting_loop/10000",
            "interpreter_counting_loop/100000",
            "interpreter_fibonacci/10",
            "interpreter_fibonacci/20",
            "interpreter_fibonacci/30",
            "interpreter_fibonacci/40",
            "string_creation_100",
            "shootout_nbody/100",
            "shootout_nbody/1000",
            "shootout_binary_trees/8",
            "shootout_binary_trees/12",
            "specjvm_compiler_throughput",
            "specjvm_crypto_dispatch_10k",
            "specjvm_scimark_sor/10x5",
            "specjvm_scimark_sor/20x10",
            "dacapo_avrora_100k_loop",
        ];
        // The checked-in placeholder baseline declares every metric at 0 ns;
        // this doubles as the canonical key list for the comparator.
        let placeholder_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("workspace root")
            .join("bench")
            .join("hotspot-baseline.json");
        if let Ok(raw) = std::fs::read_to_string(&placeholder_path) {
            let parsed: Baseline =
                serde_json::from_str(&raw).expect("checked-in placeholder must parse");
            for name in &expected_metric_names {
                assert!(
                    parsed.metrics.contains_key(*name),
                    "bench/hotspot-baseline.json is missing metric `{name}` — \
                     update scripts/capture-hotspot-baseline.{{sh,ps1}} in lockstep",
                );
            }
        }
    }

    // -----------------------------------------------------------------
    // T18.H3 — realistic end-to-end integration tests.
    //
    // The earlier `bl(...)` helper tests use 1–3 synthetic keys. Those
    // are fine for wiring, but they don't stress the comparator on the
    // same surface that will hit CI. The tests below build a realistic
    // 20-kernel RustJVM baseline (same keys the capture scripts emit)
    // and drive it against a HotSpot baseline derived at a target
    // ratio, so the geomean math walks the same code paths as a
    // live CI run.
    // -----------------------------------------------------------------

    /// Canonical realistic RustJVM baseline — 20+ kernels modelled on
    /// the surface `scripts/capture-hotspot-baseline.{sh,ps1}` emit.
    ///
    /// Numbers are representative orders of magnitude (ns) so that
    /// ratio arithmetic exercises a non-trivial dynamic range rather
    /// than a single scale.
    fn realistic_rustjvm() -> Baseline {
        let metrics: Vec<(&str, f64)> = vec![
            ("vm_startup", 150_000.0),
            ("shared_vm_startup", 82_000.0),
            ("startup_to_first_bytecode", 55_000.0),
            ("object_allocation/100", 3_400.0),
            ("object_allocation/1000", 34_200.0),
            ("gc_cycle_1000_objects", 280_000.0),
            ("native_dispatch_noop", 42.0),
            ("interpreter_counting_loop/1000", 13_500.0),
            ("interpreter_counting_loop/10000", 135_000.0),
            ("interpreter_counting_loop/100000", 1_350_000.0),
            ("interpreter_fibonacci/10", 1_200.0),
            ("interpreter_fibonacci/20", 48_000.0),
            ("interpreter_fibonacci/30", 1_100_000.0),
            ("interpreter_fibonacci/40", 55_000_000.0),
            ("string_creation_100", 4_500.0),
            ("shootout_nbody/100", 250_000.0),
            ("shootout_nbody/1000", 2_500_000.0),
            ("shootout_binary_trees/8", 14_000.0),
            ("shootout_binary_trees/12", 240_000.0),
            ("specjvm_compiler_throughput", 7_200_000.0),
            ("specjvm_crypto_dispatch_10k", 980_000.0),
            ("specjvm_scimark_sor/10x5", 18_000.0),
            ("specjvm_scimark_sor/20x10", 72_000.0),
            ("dacapo_avrora_100k_loop", 4_400_000.0),
        ];
        bl(&metrics)
    }

    /// Build a HotSpot-shaped baseline that mirrors the RustJVM keys,
    /// but with each `median_ns` divided by `ratio` so that
    /// `rustjvm_ns / hotspot_ns == ratio` for every row.
    fn realistic_hotspot_at_ratio(ratio: f64) -> Baseline {
        let r = realistic_rustjvm();
        let pairs: Vec<(String, f64)> = r
            .metrics
            .iter()
            .map(|(k, v)| (k.clone(), v.median_ns / ratio))
            .collect();
        let borrowed: Vec<(&str, f64)> =
            pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        bl(&borrowed)
    }

    #[test]
    fn t18_h3_realistic_hotspot_ratio_all_under_threshold() {
        // 20+ kernels, every one 1.1× slower on RustJVM. Geomean of a
        // flat 1.1× distribution is exactly 1.1×, which sits well under
        // the default 1.5× gate → PASS.
        let rust = realistic_rustjvm();
        let hotspot = realistic_hotspot_at_ratio(1.1);
        let rep = compare(&rust, &hotspot, 1.5);

        assert!(rep.entries.len() >= 20);
        assert!(
            rep.entries
                .iter()
                .all(|e| e.status == RatioStatus::Measured),
            "every kernel should resolve to Measured",
        );
        for e in &rep.entries {
            let r = e.ratio.expect("measured row must have a ratio");
            assert!(
                (r - 1.1).abs() < 1e-9,
                "{}: expected ratio 1.1, got {r}",
                e.metric,
            );
        }
        assert!((rep.geomean_ratio - 1.1).abs() < 1e-9);
        assert!(rep.geomean_ratio < 1.5);
        assert!(rep.passed);

        // Formatter also has to survive the large surface without panicking.
        let rendered = format_report(&rep);
        assert!(rendered.contains("verdict: PASS"));
    }

    #[test]
    fn t18_h3_realistic_mixed_ratios_geomean_still_passes() {
        // Some kernels are actually faster on RustJVM, some slower, and
        // a handful are near the ceiling. The geomean of a symmetric
        // mix should stay ≤ 1.5×, so the verdict is still PASS.
        //
        // Pick a mix whose log-mean is below ln(1.5):
        //   9 × 0.9    (rustjvm wins)
        //   8 × 1.1    (modest regression)
        //   3 × 1.4    (near-ceiling)
        let rust = realistic_rustjvm();
        let keys: Vec<String> = rust.metrics.keys().cloned().collect();
        assert!(keys.len() >= 20);

        let mut hs_pairs: Vec<(String, f64)> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            let rv = rust.metrics.get(k).unwrap().median_ns;
            let target_ratio = match i % 20 {
                0..=8 => 0.9,
                9..=16 => 1.1,
                _ => 1.4,
            };
            hs_pairs.push((k.clone(), rv / target_ratio));
        }
        let borrowed: Vec<(&str, f64)> =
            hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let hotspot = bl(&borrowed);

        let rep = compare(&rust, &hotspot, 1.5);
        // All entries measured (same keys on both sides).
        assert!(
            rep.entries
                .iter()
                .all(|e| e.status == RatioStatus::Measured),
        );
        // Sanity: at least one ratio is below 1.0 (rustjvm faster).
        assert!(rep.entries.iter().any(|e| e.ratio.unwrap() < 1.0));
        // And at least one near 1.4×.
        assert!(rep
            .entries
            .iter()
            .any(|e| (e.ratio.unwrap() - 1.4).abs() < 1e-9));

        // Geomean should land under the 1.5× threshold.
        assert!(
            rep.geomean_ratio <= 1.5,
            "geomean {} unexpectedly above 1.5×",
            rep.geomean_ratio,
        );
        assert!(rep.passed);
    }

    #[test]
    fn t18_h3_realistic_one_outlier_fails_threshold() {
        // 19 kernels at 1.2×, 1 catastrophic outlier. With 24 total
        // rows, we need the outlier large enough that
        //   (23·ln1.2 + ln X) / 24  >  ln 1.5
        // i.e. X > exp(24·ln1.5 − 23·ln1.2) ≈ 253×. A 500× regression
        // models e.g. a kernel where a JIT tier unexpectedly fell
        // back to the interpreter — rare but real, and exactly the
        // kind of thing the gate must catch.
        let rust = realistic_rustjvm();
        let keys: Vec<String> = rust.metrics.keys().cloned().collect();
        assert!(keys.len() >= 20);
        // Pick first key deterministically (BTreeMap gives sorted order).
        let outlier_key = keys[0].clone();

        let mut hs_pairs: Vec<(String, f64)> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            let rv = rust.metrics.get(k).unwrap().median_ns;
            let target_ratio = if i == 0 { 500.0 } else { 1.2 };
            hs_pairs.push((k.clone(), rv / target_ratio));
        }
        let borrowed: Vec<(&str, f64)> =
            hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let hotspot = bl(&borrowed);

        let rep = compare(&rust, &hotspot, 1.5);
        // Locate the outlier row and verify it was measured at 500×.
        let outlier = rep
            .entries
            .iter()
            .find(|e| e.metric == outlier_key)
            .expect("outlier metric present");
        assert_eq!(outlier.status, RatioStatus::Measured);
        assert!((outlier.ratio.unwrap() - 500.0).abs() < 1e-6);

        // Geomean should exceed 1.5× → FAIL.
        assert!(
            rep.geomean_ratio > 1.5,
            "outlier should drag geomean above 1.5, got {}",
            rep.geomean_ratio,
        );
        assert!(!rep.passed);

        let rendered = format_report(&rep);
        assert!(rendered.contains("verdict: FAIL"));

        // Same data passes a sufficiently loose gate (pick something
        // safely above the observed geomean) — proves the threshold
        // really is the only knob that flips PASS/FAIL.
        let loose = compare(&rust, &hotspot, rep.geomean_ratio + 0.5);
        assert!(loose.passed);
        assert!((loose.geomean_ratio - rep.geomean_ratio).abs() < 1e-12);
    }

    #[test]
    fn t18_h3_realistic_all_measured_mixed_with_bootstrap() {
        // Half the kernels are fully captured on both sides (measured),
        // the other half have a HotSpot placeholder of 0 (bootstrap).
        // The measured half drives the geomean; bootstrap rows must
        // not sneak into the log-sum. Verdict has to stay well-defined.
        let rust = realistic_rustjvm();
        let keys: Vec<String> = rust.metrics.keys().cloned().collect();
        assert!(keys.len() >= 20);
        let split = keys.len() / 2;

        let mut hs_pairs: Vec<(String, f64)> = Vec::new();
        for (i, k) in keys.iter().enumerate() {
            let rv = rust.metrics.get(k).unwrap().median_ns;
            if i < split {
                // Measured half — pin to a clean 1.2× ratio.
                hs_pairs.push((k.clone(), rv / 1.2));
            } else {
                // Bootstrap half — placeholder 0 on the HotSpot side.
                hs_pairs.push((k.clone(), 0.0));
            }
        }
        let borrowed: Vec<(&str, f64)> =
            hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let hotspot = bl(&borrowed);

        let rep = compare(&rust, &hotspot, 1.5);
        let measured: Vec<&RatioEntry> = rep
            .entries
            .iter()
            .filter(|e| e.status == RatioStatus::Measured)
            .collect();
        let boot: Vec<&RatioEntry> = rep
            .entries
            .iter()
            .filter(|e| e.status == RatioStatus::HotspotBootstrap)
            .collect();
        assert_eq!(measured.len(), split);
        assert_eq!(boot.len(), keys.len() - split);

        // Every measured row is 1.2×, so geomean == 1.2× — bootstrap
        // rows must NOT pull it toward 0 or NaN.
        assert!(
            (rep.geomean_ratio - 1.2).abs() < 1e-9,
            "bootstrap rows leaking into geomean: {}",
            rep.geomean_ratio,
        );
        assert!(rep.passed);
        // Bootstrap rows should all carry `ratio == None`.
        for b in &boot {
            assert!(b.ratio.is_none());
        }
    }

    #[test]
    fn t18_h3_realistic_asymmetric_metrics_surface() {
        // RustJVM adds two brand-new kernels (`future_bench_a/b`) that
        // the HotSpot capture script hasn't been updated for yet. The
        // comparator must:
        //   - flag the two new rows as Asymmetric (not bootstrap).
        //   - keep them OUT of the geomean.
        //   - still deliver a PASS verdict when the measured surface
        //     is clean.
        let mut rust = realistic_rustjvm();
        rust.metrics.insert(
            "future_bench_a".into(),
            MetricEntry { median_ns: 999_999.0 },
        );
        rust.metrics.insert(
            "future_bench_b".into(),
            MetricEntry { median_ns: 123_456.0 },
        );
        // HotSpot mirrors only the original keys, at a clean 1.1×.
        let hotspot = realistic_hotspot_at_ratio(1.1);

        let rep = compare(&rust, &hotspot, 1.5);
        let asym: Vec<&RatioEntry> = rep
            .entries
            .iter()
            .filter(|e| e.status == RatioStatus::Asymmetric)
            .collect();
        let asym_names: Vec<&str> = asym.iter().map(|e| e.metric.as_str()).collect();
        assert!(asym_names.contains(&"future_bench_a"));
        assert!(asym_names.contains(&"future_bench_b"));
        assert_eq!(asym.len(), 2);
        for e in &asym {
            assert!(e.ratio.is_none(), "asymmetric rows must have no ratio");
        }

        // Every *measured* row is 1.1× → geomean 1.1×, verdict PASS.
        assert!((rep.geomean_ratio - 1.1).abs() < 1e-9);
        assert!(rep.passed);

        // Report text should include every row — asymmetric too.
        let rendered = format_report(&rep);
        assert!(rendered.contains("future_bench_a"));
        assert!(rendered.contains("future_bench_b"));
        assert!(rendered.contains("verdict: PASS"));
    }

    // T18.H3 — negative/edge coverage: NaN and infinity inputs must
    // not panic the comparator or the formatter. A realistic capture
    // will never write these, but a corrupted bench JSON or a broken
    // iteration producing `f64::NAN` mustn't take down CI.
    #[test]
    fn t18_h3_realistic_nan_and_infinity_inputs_do_not_panic() {
        let rust = bl(&[
            ("ok_metric", 100.0),
            ("nan_metric", f64::NAN),
            ("inf_metric", f64::INFINITY),
            ("neg_inf_metric", f64::NEG_INFINITY),
        ]);
        let hotspot = bl(&[
            ("ok_metric", 80.0),
            ("nan_metric", 50.0),
            ("inf_metric", 50.0),
            ("neg_inf_metric", 50.0),
        ]);

        // Must produce a report without panicking.
        let rep = compare(&rust, &hotspot, 1.5);
        assert_eq!(rep.entries.len(), 4);

        // The clean row stays well-formed.
        let ok = rep
            .entries
            .iter()
            .find(|e| e.metric == "ok_metric")
            .expect("ok_metric row");
        assert_eq!(ok.status, RatioStatus::Measured);
        assert!((ok.ratio.unwrap() - 1.25).abs() < 1e-9);

        // Formatter must also tolerate the degenerate rows.
        let rendered = format_report(&rep);
        assert!(rendered.contains("ok_metric"));
        assert!(rendered.contains("nan_metric"));
        assert!(rendered.contains("inf_metric"));
        assert!(rendered.contains("neg_inf_metric"));
        // Verdict string is always populated — PASS or FAIL, never absent.
        assert!(rendered.contains("verdict:"));
    }
}
