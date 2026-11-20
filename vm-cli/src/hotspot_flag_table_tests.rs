use super::*;

fn argv(a: &[&str]) -> Vec<String> {
    std::iter::once("cratonvm")
        .chain(a.iter().copied())
        .map(String::from)
        .collect()
}

fn refuse(a: &[&str]) -> Option<Vec<String>> {
    hotspot_flag_table_refusal(&argv(a), true, true, true, true)
}

fn lines(l: &[&str]) -> Option<Vec<String>> {
    Some(l.iter().map(|s| s.to_string()).collect())
}

/// The generated table is sorted (binary search) and knows the flags the
/// HotSpot rows name, with their declared types, locks and ranges.
#[test]
fn generated_table_is_sorted_and_typed() {
    use hotspot25_flags::{lookup, FLAGS, L, R, T};
    assert!(FLAGS.windows(2).all(|w| w[0].name < w[1].name));
    let f = lookup("G1NewSizePercent").expect("in the table");
    assert_eq!((f.ty, f.lock, f.range), (T::Uint, L::Experimental, R::Int(0, 100)));
    assert_eq!(lookup("VerifyBeforeGC").map(|f| f.lock), Some(L::Diagnostic));
    assert_eq!(lookup("MaxRAMPercentage").map(|f| f.range), Some(R::Double(0.0, 100.0)));
    // This VM's own `-XX:` options are not HotSpot's and stay unchecked.
    for own in ["CompatibilityMode", "MaxStackDepth", "UseGenerationalGC"] {
        assert!(lookup(own).is_none(), "{own}");
    }
}

#[test]
fn start_flight_recording_uses_its_hotspot_option_parser() {
    for option in [
        "-XX:StartFlightRecording",
        "-XX:StartFlightRecording:filename=recording.jfr",
        "-XX:StartFlightRecording=filename=recording.jfr",
    ] {
        assert_eq!(refuse(&[option]), None, "{option}");
    }
}

/// Local HotSpot 25.0.3 rows: the lock lines, then the spelling's line.
#[test]
fn locked_flags_answer_as_hotspot() {
    // Without the lock switch a locked flag is answered as unlocked.
    let unlocked = |a: &[&str]| hotspot_flag_table_refusal(&argv(a), true, true, true, false);
    assert_eq!(unlocked(&["-XX:G1NewSizePercent=5", "-XX:+VerifyBeforeGC"]), None);
    assert_eq!(
        refuse(&["-XX:G1NewSizePercent=5"]),
        lines(&[
            "Error: VM option 'G1NewSizePercent' is experimental and must be enabled via -XX:+UnlockExperimentalVMOptions.",
            "Error: The unlock option must precede 'G1NewSizePercent'.",
            "Improperly specified VM option 'G1NewSizePercent=5'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:+G1NewSizePercent"]).map(|l| l[2].clone()),
        Some("Unexpected +/- setting in VM option 'G1NewSizePercent'".to_string())
    );
    assert_eq!(
        refuse(&["-XX:-VerifyBeforeGC"]),
        lines(&[
            "Error: VM option 'VerifyBeforeGC' is diagnostic and must be enabled via -XX:+UnlockDiagnosticVMOptions.",
            "Error: The unlock option must precede 'VerifyBeforeGC'.",
            "Improperly specified VM option 'VerifyBeforeGC'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:VerifyBeforeGC=1"]).map(|l| l[2].clone()),
        Some("Missing +/- setting for VM option 'VerifyBeforeGC=1'".to_string())
    );
    // The unlock must precede; the other kind's unlock does not count; a
    // `-XX:-Unlock...` locks again.
    assert!(refuse(&["-XX:G1NewSizePercent=5", "-XX:+UnlockExperimentalVMOptions"]).is_some());
    assert!(refuse(&["-XX:+UnlockDiagnosticVMOptions", "-XX:G1NewSizePercent=5"]).is_some());
    assert!(refuse(&[
        "-XX:+UnlockDiagnosticVMOptions",
        "-XX:+VerifyBeforeGC",
        "-XX:-UnlockDiagnosticVMOptions",
        "-XX:+VerifyAfterGC",
    ])
    .is_some_and(|l| l[0].contains("'VerifyAfterGC'")));
    assert_eq!(
        refuse(&[
            "-XX:+UnlockExperimentalVMOptions",
            "-XX:+UnlockDiagnosticVMOptions",
            "-XX:G1NewSizePercent=5",
            "-XX:+VerifyBeforeGC",
        ]),
        None
    );
    assert_eq!(
        refuse(&["-XX:+UnlockExperimentalVMOptions", "-XX:G1NewSizePercent=x"]),
        lines(&["Improperly specified VM option 'G1NewSizePercent=x'"])
    );
}

/// Local rows: `bool` / non-`bool` spellings, types and ranges.
#[test]
fn spellings_types_and_ranges_answer_as_hotspot() {
    let one = |a: &str| refuse(&[a]).map(|l| l.join("\n"));
    let is = |a: &str, want: &str| assert_eq!(one(a).as_deref(), Some(want), "{a}");
    is("-XX:UseTLAB=true", "Missing +/- setting for VM option 'UseTLAB=true'");
    is("-XX:AlwaysPreTouch=true", "Missing +/- setting for VM option 'AlwaysPreTouch=true'");
    is("-XX:UseTLAB", "Missing +/- setting for VM option 'UseTLAB'");
    is("-XX:+GCTimeRatio", "Unexpected +/- setting in VM option 'GCTimeRatio'");
    is("-XX:-GCTimeRatio", "Unexpected +/- setting in VM option 'GCTimeRatio'");
    is("-XX:+ErrorFile", "Unexpected +/- setting in VM option 'ErrorFile'");
    is("-XX:GCTimeRatio", "Improperly specified VM option 'GCTimeRatio'");
    is("-XX:ErrorFile", "Improperly specified VM option 'ErrorFile'");
    is("-XX:GCTimeRatio=x", "Improperly specified VM option 'GCTimeRatio=x'");
    is("-XX:GCTimeRatio=-1", "Improperly specified VM option 'GCTimeRatio=-1'");
    is("-XX:GCTimeRatio=4294967296", "Improperly specified VM option 'GCTimeRatio=4294967296'");
    is("-XX:NewRatio=+5", "Improperly specified VM option 'NewRatio=+5'");
    is(
        "-XX:G1ReservePercent=1k",
        "uint G1ReservePercent=1024 is outside the allowed range [ 0 ... 50 ]\n\
         Improperly specified VM option 'G1ReservePercent=1k'",
    );
    is(
        "-XX:NewRatio=18446744073709551615",
        "uintx NewRatio=18446744073709551615 is outside the allowed range \
         [ 0 ... 18446744073709551614 ]\n\
         Improperly specified VM option 'NewRatio=18446744073709551615'",
    );
    is(
        "-XX:MaxRAMPercentage=-1",
        "double MaxRAMPercentage=-1.000000 is outside the allowed range \
         [ 0.000000 ... 100.000000 ]\n\
         Improperly specified VM option 'MaxRAMPercentage=-1'",
    );
    is(
        "-XX:ZFragmentationLimit=1e3",
        "double ZFragmentationLimit=1000.000000 is outside the allowed range \
         [ 0.000000 ... 100.000000 ]\n\
         Improperly specified VM option 'ZFragmentationLimit=1e3'",
    );
    assert_eq!(one("-XX:ErrorFile=x"), None);
    assert_eq!(one("-XX:GCTimeRatio=0X10"), None);
    for taken in ["0x10", "1.", ".5", "-0", "+5", "5e-1"] {
        assert_eq!(one(&format!("-XX:MaxRAMPercentage={taken}")), None, "{taken}");
    }
    for refused in ["1k", "1f", "inf", "nan", "1,5", ""] {
        assert!(one(&format!("-XX:MaxRAMPercentage={refused}")).is_some(), "{refused}");
    }
    assert_eq!(one("-XX:ZAllocationSpikeTolerance=0x10"), None);
    // Unknown names are HotSpot's `Unrecognized VM option` since the
    // gc-known-issues retire (`CRATONVM_LAUNCHER_UNKNOWN_XX_REFUSAL`, where
    // the tables apply); everything after `--` is left alone.
    assert_eq!(
        one("-XX:Bogus=1"),
        UNKNOWN_XX_REFUSAL_TARGET.then(|| "Unrecognized VM option 'Bogus=1'".to_string())
    );
    assert_eq!(refuse(&["Main", "--", "-XX:UseTLAB=1"]), None);
    assert_eq!(refuse(&["Main", "--", "-XX:Bogus=1"]), None);
    // The first bad option in command-line order wins.
    assert_eq!(
        refuse(&["-XX:GCTimeRatio=x", "-XX:UseTLAB=1"]).map(|l| l[0].clone()),
        Some("Improperly specified VM option 'GCTimeRatio=x'".to_string())
    );
}

#[test]
fn hex_floats_follow_strtod() {
    assert_eq!(hotspot_parse_hex_double("0x10"), Some(16.0));
    assert_eq!(hotspot_parse_hex_double("0x1p3"), Some(8.0));
    assert_eq!(hotspot_parse_hex_double("-0X.8p1"), Some(-1.0));
    assert_eq!(hotspot_parse_hex_double("0x1.8"), Some(1.5));
    assert_eq!(hotspot_parse_hex_double("0x"), None);
    assert_eq!(hotspot_parse_hex_double("0xp3"), None);
    assert_eq!(hotspot_parse_hex_double("0x1p"), None);
    assert_eq!(hotspot_parse_hex_double("0x1g"), None);
    assert_eq!(hotspot_parse_double_flag("0x1p3"), Some(8.0));
}

/// The consumers get HotSpot's value in decimal.
#[test]
fn table_values_reach_consumers_in_decimal() {
    let canon = |n: &str, v: &str| hotspot_table_canonical_value(n, v);
    assert_eq!(canon("MaxGCPauseMillis", "0x10").as_deref(), Some("16"));
    assert_eq!(canon("SoftMaxHeapSize", "1t").as_deref(), Some("1099511627776"));
    assert_eq!(canon("ZAllocationSpikeTolerance", "0x10").as_deref(), Some("16"));
    assert_eq!(canon("MaxJavaStackTraceDepth", "-1"), None);
    assert_eq!(canon("UseTLAB", "1"), None);
    assert_eq!(canon("Bogus", "0x10"), None);
    let out = ordered_parse_argv(
        argv(&["-XX:G1HeapWastePercent=0x10", "Main", "--", "-XX:G1HeapWastePercent=0x10"]),
        false,
        true,
    );
    assert_eq!(out[1], "-XX:G1HeapWastePercent=16");
    assert_eq!(out[4], "-XX:G1HeapWastePercent=0x10");
}

/// Local rows: the `AfterErgo` constraint lines.
#[test]
fn after_ergo_constraints_answer_as_hotspot() {
    let c = |a: &[&str], g1: bool| hotspot_after_ergo_constraint_errors(&argv(a), g1);
    assert_eq!(
        c(&["-XX:GCPauseIntervalMillis=16"], true),
        vec!["GCPauseIntervalMillis cannot be set without setting MaxGCPauseMillis"]
    );
    assert_eq!(
        c(&["-XX:GCPauseIntervalMillis=0"], true),
        vec!["GCPauseIntervalMillis (0) must be greater than or equal to 1"]
    );
    assert_eq!(
        c(&["-XX:MaxGCPauseMillis=10", "-XX:GCPauseIntervalMillis=10"], true),
        vec![
            "MaxGCPauseMillis (10) must be less than GCPauseIntervalMillis (10)",
            "GCPauseIntervalMillis (10) must be greater than MaxGCPauseMillis (10)",
        ]
    );
    assert!(c(&["-XX:MaxGCPauseMillis=10", "-XX:GCPauseIntervalMillis=11"], true).is_empty());
    assert!(c(&["-XX:GCPauseIntervalMillis=5"], false).is_empty());
    assert!(scan_names_no_collector(&argv(&["-Xmx1g", "Main"])));
    assert!(!scan_names_no_collector(&argv(&["-XX:+UseZGC"])));
    assert!(!scan_names_no_collector(&argv(&["-XX:-UseG1GC"])));
}
