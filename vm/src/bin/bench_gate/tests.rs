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
fn run_metrics(metrics: &[(&str, f64)]) -> BTreeMap<String, f64> {
    metrics.iter().map(|(k, v)| (k.to_string(), *v)).collect()
}

#[test]
fn regression_above_threshold_fails() {
    let b = base(&[("foo", 100.0)]);
    let r = run_metrics(&[("foo", 130.0)]); // +30%
    let report = compare(&b, &r, 0.15);
    assert!(!report.passed);
    assert_eq!(report.entries[0].status, GateStatus::Regress);
    assert!((report.entries[0].delta_pct - 30.0).abs() < 1e-9);
}

#[test]
fn improvement_below_threshold_passes() {
    let b = base(&[("foo", 100.0)]);
    let r = run_metrics(&[("foo", 90.0)]); // -10%
    let report = compare(&b, &r, 0.15);
    assert!(report.passed);
    assert_eq!(report.entries[0].status, GateStatus::Ok);
    assert!((report.entries[0].delta_pct + 10.0).abs() < 1e-9);
}

#[test]
fn small_regression_within_threshold_passes() {
    let b = base(&[("foo", 100.0)]);
    let r = run_metrics(&[("foo", 110.0)]); // +10%
    let report = compare(&b, &r, 0.15);
    assert!(report.passed);
    assert_eq!(report.entries[0].status, GateStatus::Ok);
}

#[test]
fn placeholder_baseline_bootstraps() {
    let b = base(&[("foo", 0.0)]);
    let r = run_metrics(&[("foo", 12345.0)]);
    let report = compare(&b, &r, 0.15);
    assert!(report.passed);
    assert_eq!(report.entries[0].status, GateStatus::Bootstrap);
}

#[test]
fn missing_metric_in_run_fails() {
    let b = base(&[("foo", 100.0), ("bar", 200.0)]);
    let r = run_metrics(&[("foo", 105.0)]);
    let report = compare(&b, &r, 0.15);
    assert!(!report.passed);
    // bar is reported as MISSING (Status::Missing)
    let bar = report.entries.iter().find(|e| e.metric == "bar").unwrap();
    assert_eq!(bar.status, GateStatus::Missing);
}

#[test]
fn new_metric_is_reported_but_does_not_fail() {
    let b = base(&[("foo", 100.0)]);
    let r = run_metrics(&[("foo", 100.0), ("baz", 50.0)]);
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
    let r = run_metrics(&[("a", 120.0), ("b", 80.0)]);
    let report = compare(&b, &r, 0.25);
    assert!((report.geomean_delta - (-2.0203)).abs() < 0.01);
}

#[test]
fn geomean_above_threshold_fails_even_when_individuals_pass() {
    // Each metric is +14% (within 15%) but the geomean is still 14%
    // and we tighten the threshold to 0.10 to force a fail.
    let b = base(&[("a", 100.0), ("b", 100.0)]);
    let r = run_metrics(&[("a", 114.0), ("b", 114.0)]);
    let report = compare(&b, &r, 0.10);
    assert!(!report.passed, "expected fail on geomean");
}

#[test]
fn baseline_from_run_round_trips() {
    let r = run_metrics(&[("a", 1.5), ("b", 2.5)]);
    let b = baseline_from_run(&r, "host".into());
    assert_eq!(b.metrics.len(), 2);
    assert_eq!(b.metrics["a"].median_ns, 1.5);
}

#[test]
fn baseline_save_load_round_trip() {
    let dir = std::env::temp_dir().join("cratonvm-bench-gate-test");
    let _ = fs::remove_dir_all(&dir);
    let path = dir.join("baseline.json");
    let r = run_metrics(&[("a", 1.0), ("b", 2.0)]);
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
    let r = run_metrics(&[("foo", 130.0)]);
    let report = compare(&b, &r, 0.15);
    let s = format_report(&report);
    assert!(s.contains("foo"));
    assert!(s.contains("REGRESS"));
    assert!(s.contains("FAIL"));
}

#[test]
fn format_report_pass_verdict() {
    let b = base(&[("foo", 100.0)]);
    let r = run_metrics(&[("foo", 105.0)]);
    let report = compare(&b, &r, 0.15);
    let s = format_report(&report);
    assert!(s.contains("PASS"));
}
