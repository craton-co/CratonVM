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
        // HotSpot = 0 (placeholder), CratonVM = 100 → no ratio.
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
        assert_eq!(parsed.metrics.get("vm_startup").unwrap().median_ns, 1234.0);
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
    // set of criterion IDs the CratonVM side also writes into
    // `vm/bench/baseline.json`. If someone adds a new kernel on either side
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
                    "vm/bench/hotspot-baseline.json is missing metric `{name}` — \
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
    // 20-kernel CratonVM baseline (same keys the capture scripts emit)
    // and drive it against a HotSpot baseline derived at a target
    // ratio, so the geomean math walks the same code paths as a
    // live CI run.
    // -----------------------------------------------------------------

    /// Canonical realistic CratonVM baseline — 20+ kernels modelled on
    /// the surface `scripts/capture-hotspot-baseline.{sh,ps1}` emit.
    ///
    /// Numbers are representative orders of magnitude (ns) so that
    /// ratio arithmetic exercises a non-trivial dynamic range rather
    /// than a single scale.
    fn realistic_cratonvm() -> Baseline {
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

    /// Build a HotSpot-shaped baseline that mirrors the CratonVM keys,
    /// but with each `median_ns` divided by `ratio` so that
    /// `cratonvm_ns / hotspot_ns == ratio` for every row.
    fn realistic_hotspot_at_ratio(ratio: f64) -> Baseline {
        let r = realistic_cratonvm();
        let pairs: Vec<(String, f64)> = r
            .metrics
            .iter()
            .map(|(k, v)| (k.clone(), v.median_ns / ratio))
            .collect();
        let borrowed: Vec<(&str, f64)> = pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        bl(&borrowed)
    }

    #[test]
    fn t18_h3_realistic_hotspot_ratio_all_under_threshold() {
        // 20+ kernels, every one 1.1× slower on CratonVM. Geomean of a
        // flat 1.1× distribution is exactly 1.1×, which sits well under
        // the default 1.5× gate → PASS.
        let rust = realistic_cratonvm();
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
        // Some kernels are actually faster on CratonVM, some slower, and
        // a handful are near the ceiling. The geomean of a symmetric
        // mix should stay ≤ 1.5×, so the verdict is still PASS.
        //
        // Pick a mix whose log-mean is below ln(1.5):
        //   9 × 0.9    (cratonvm wins)
        //   8 × 1.1    (modest regression)
        //   3 × 1.4    (near-ceiling)
        let rust = realistic_cratonvm();
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
        let borrowed: Vec<(&str, f64)> = hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let hotspot = bl(&borrowed);

        let rep = compare(&rust, &hotspot, 1.5);
        // All entries measured (same keys on both sides).
        assert!(rep
            .entries
            .iter()
            .all(|e| e.status == RatioStatus::Measured),);
        // Sanity: at least one ratio is below 1.0 (cratonvm faster).
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
        let rust = realistic_cratonvm();
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
        let borrowed: Vec<(&str, f64)> = hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
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
        let rust = realistic_cratonvm();
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
        let borrowed: Vec<(&str, f64)> = hs_pairs.iter().map(|(k, v)| (k.as_str(), *v)).collect();
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
        // CratonVM adds two brand-new kernels (`future_bench_a/b`) that
        // the HotSpot capture script hasn't been updated for yet. The
        // comparator must:
        //   - flag the two new rows as Asymmetric (not bootstrap).
        //   - keep them OUT of the geomean.
        //   - still deliver a PASS verdict when the measured surface
        //     is clean.
        let mut rust = realistic_cratonvm();
        rust.metrics.insert(
            "future_bench_a".into(),
            MetricEntry {
                median_ns: 999_999.0,
            },
        );
        rust.metrics.insert(
            "future_bench_b".into(),
            MetricEntry {
                median_ns: 123_456.0,
            },
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
