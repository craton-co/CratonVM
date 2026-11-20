use super::*;

fn argv(a: &[&str]) -> Vec<String> {
    std::iter::once("cratonvm")
        .chain(a.iter().copied())
        .map(String::from)
        .collect()
}

/// gcj j1/ob item 3: HotSpot 25's value syntax per declared type.
#[test]
fn gcj_j1_ob_typed_value_parse_follows_hotspot_types() {
    use HotspotFlagType::*;
    assert_eq!(Uint.parse("4294967295"), Some(i128::from(u32::MAX)));
    assert_eq!(Uint.parse("4294967296"), None);
    assert_eq!(Uint.parse("0x10"), Some(16));
    assert_eq!(Uint.parse("3k"), Some(3072));
    assert_eq!(Uint.parse("-1"), None);
    assert_eq!(Uint.parse(" 5"), None);
    assert_eq!(Uint.parse("5 "), None);
    assert_eq!(Uintx.parse("4294967296"), Some(4_294_967_296));
    assert_eq!(SizeT.parse("1g"), Some(1 << 30));
    assert_eq!(SizeT.parse("abc"), None);
    assert_eq!(Intx.parse("-5"), Some(-5));
    assert_eq!(Intx.parse("--5"), None);
    assert_eq!(Intx.parse("-"), None);
    assert_eq!(Int.parse("2147483648"), None);
    assert_eq!(Double.parse("0.5"), Some(0));
    assert_eq!(Double.parse("abc"), None);
    assert_eq!(Double.parse(" 3"), None);
    assert_eq!(Uint64.name(), "uint64_t");
    assert_eq!(SizeT.name(), "size_t");
}

/// gcj j1/ob items 2 and 3: the first bad `-XX:` value in command-line
/// order, with HotSpot's lines; legal values and unknown flags pass.
#[test]
fn gcj_j1_ob_gen_value_flag_refusal_lines() {
    let refuse = |a: &[&str]| gen_hotspot_value_flag_refusal(&argv(a), true);
    let one = |line: &str| Some(vec![line.to_string()]);
    assert_eq!(
        refuse(&["-XX:ParallelGCThreads=abc", "Main"]),
        one("Improperly specified VM option 'ParallelGCThreads=abc'")
    );
    assert_eq!(
        refuse(&["-XX:ConcGCThreads=y", "-XX:ParallelGCThreads=x", "-version"]),
        one("Improperly specified VM option 'ConcGCThreads=y'")
    );
    assert_eq!(
        refuse(&["-XX:MaxGCPauseMillis=1.5", "Main"]),
        one("Improperly specified VM option 'MaxGCPauseMillis=1.5'")
    );
    assert_eq!(
        refuse(&["-XX:SoftMaxHeapSize=64q", "Main"]),
        one("Improperly specified VM option 'SoftMaxHeapSize=64q'")
    );
    assert_eq!(
        refuse(&["-XX:ZAllocationSpikeTolerance=x", "Main"]),
        one("Improperly specified VM option 'ZAllocationSpikeTolerance=x'")
    );
    // The verified ranges keep their lines (`MaxTenuringThreshold`: the
    // range line alone).
    assert_eq!(
        refuse(&["-XX:MaxTenuringThreshold=99", "-version"]),
        one("uint MaxTenuringThreshold=99 is outside the allowed range [ 0 ... 16 ]")
    );
    assert_eq!(
        refuse(&["-XX:MaxTenuringThreshold= 5", "-version"]),
        one("Improperly specified VM option 'MaxTenuringThreshold= 5'")
    );
    assert_eq!(
        refuse(&["-XX:SurvivorRatio=8 ", "-version"]),
        one("Improperly specified VM option 'SurvivorRatio=8 '")
    );
    assert_eq!(
        refuse(&["-XX:MaxHeapFreeRatio=101", "Main"]),
        Some(vec![
            "uintx MaxHeapFreeRatio=101 is outside the allowed range [ 0 ... 100 ]".to_string(),
            "Improperly specified VM option 'MaxHeapFreeRatio=101'".to_string(),
        ])
    );
    // gci-i42ta item 4: command-line order across the groups.
    assert_eq!(
        refuse(&["-XX:SurvivorRatio=0", "-XX:MaxTenuringThreshold=17", "Main"]),
        Some(vec![
            "uintx SurvivorRatio=0 is outside the allowed range [ 1 ... 18446744073709551613 ]"
                .to_string(),
            "Improperly specified VM option 'SurvivorRatio=0'".to_string(),
        ])
    );
    // ... and with `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS=0` those flags are
    // not checked here.
    assert_eq!(
        gen_hotspot_value_flag_refusal(&argv(&["-XX:SurvivorRatio=0", "Main"]), false),
        None
    );
    // Legal values, flags outside the table, program arguments.
    assert_eq!(
        refuse(&[
            "-XX:ParallelGCThreads=0x10",
            "-XX:ConcGCThreads=0",
            "-XX:MaxGCPauseMillis=200",
            "-XX:SoftRefLRUPolicyMSPerMB=-1",
            "-XX:MaxMetaspaceSize=64m",
            "-XX:NewRatio=abc",
            "Main",
            "--",
            "-XX:ParallelGCThreads=abc",
        ]),
        None
    );
}

/// gcj j1/ob: `-XX:+UseSerialGC` selects Generational silently; the
/// collectors without a twin keep the warning; `=0` restores it.
#[test]
fn gcj_j1_ob_serial_selector_is_silent() {
    assert!(serial_selector_is_silent("Serial", true));
    assert!(!serial_selector_is_silent("Serial", false));
    assert!(!serial_selector_is_silent("Parallel", true));
    assert!(!serial_selector_is_silent("Shenandoah", true));
    assert!(!serial_selector_is_silent("Epsilon", true));
    // gcj j2/x1: HotSpot's spelling only.
    assert!(!serial_selector_is_silent("serial", true), "-XX:+UseserialGC");
    assert!(!serial_selector_is_silent("SERIAL", true));
    assert!(!serial_selector_is_silent(" Serial", true));
}

/// gcj j2/x1 (`gcj-j2x1-wave-j1-default-on-review-residuals` items 8, 10,
/// 11, 11b): HotSpot 25.0.4.1's answers, read on the host (rows `x1_*`).
#[test]
fn gcj_j2_x1_launcher_residuals_answer_as_hotspot() {
    let lines = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    // Item 8: the `+`/`-` spelling of a value flag, on every scan.
    let plus = lines(&["Unexpected +/- setting in VM option 'MaxTenuringThreshold'"]);
    for scan in [
        gen_hotspot_value_flag_refusal(&argv(&["-XX:+MaxTenuringThreshold"]), true),
        gen_hotspot_value_flag_refusal(&argv(&["-XX:-MaxTenuringThreshold"]), false),
        gen_value_flag_refusal_with(&argv(&["-XX:+MaxTenuringThreshold"]), true, true, true),
        hotspot_flag_table_refusal(
            &argv(&["-XX:+MaxTenuringThreshold"]),
            true,
            true,
            true,
            true,
        ),
    ] {
        assert_eq!(scan, Some(plus.clone()));
    }
    assert_eq!(
        gen_hotspot_value_flag_refusal(&argv(&["-XX:+InitiatingHeapOccupancyPercent"]), false),
        Some(lines(&[
            "Unexpected +/- setting in VM option 'InitiatingHeapOccupancyPercent'"
        ]))
    );
    // A `bool` flag's sign is no value-flag refusal.
    assert_eq!(
        gen_hotspot_value_flag_refusal(&argv(&["-XX:+UseTLAB"]), true),
        None
    );
    // Items 9 and 10: `uint` narrowing.
    let improper = |o: &str| Some(lines(&[&format!("Improperly specified VM option '{o}'")]));
    for o in [
        "MaxTenuringThreshold=4294967296",
        "MaxTenuringThreshold=0x100000001",
        "InitiatingHeapOccupancyPercent=4294967296",
        "G1HeapWastePercent=4294967296",
    ] {
        let a = format!("-XX:{o}");
        assert_eq!(
            gen_hotspot_value_flag_refusal(&argv(&[&a]), true),
            improper(o),
            "{o}"
        );
    }
    assert_eq!(
        gen_hotspot_value_flag_refusal(&argv(&["-XX:MaxTenuringThreshold=4294967295"]), true),
        Some(lines(&[
            "uint MaxTenuringThreshold=4294967295 is outside the allowed range [ 0 ... 16 ]"
        ]))
    );
    // Item 11b: a selector HotSpot does not know.
    let table = |a: &[&str]| hotspot_flag_table_refusal(&argv(a), true, true, true, true);
    assert_eq!(
        table(&["-XX:+UseserialGC"]),
        Some(lines(&[
            "Unrecognized VM option 'UseserialGC'",
            "Did you mean '(+/-)UseSerialGC'?"
        ]))
    );
    for sel in ["UseFooGC", "Useg1GC", "UseZgcGC"] {
        for sign in ["+", "-"] {
            let a = format!("-XX:{sign}{sel}");
            assert_eq!(
                table(&[&a]),
                Some(lines(&[&format!("Unrecognized VM option '{sel}'")])),
                "{a}"
            );
        }
    }
    // This VM's own selector, HotSpot's collectors, and the ignore option.
    assert_eq!(table(&["-XX:+UseGenerationalGC"]), None);
    assert_eq!(table(&["-XX:+UseSerialGC"]), None);
    assert_eq!(table(&["-XX:+UseParallelGC"]), None);
    assert_eq!(
        table(&["-XX:+IgnoreUnrecognizedVMOptions", "-XX:+UseFooGC"]),
        None
    );
    assert_eq!(
        table(&["-XX:+UseFooGC", "-XX:+IgnoreUnrecognizedVMOptions"]),
        None
    );
    // Command-line order: the first bad option is named.
    assert_eq!(
        table(&["-XX:+MaxTenuringThreshold", "-XX:+UseFooGC"]),
        Some(plus.clone())
    );
    // Item 11: the internal spelling selects too.
    for a in [
        argv(&["--XX:UseGc", "Generational"]),
        argv(&["--XX:UseGc=Generational"]),
        argv(&["-XX:+UseZGC", "--XX:UseGc", "Generational"]),
    ] {
        assert!(scan_effective_generational(&a), "{a:?}");
        assert!(!scan_effective_zgc(&a), "{a:?}");
    }
    assert!(!scan_effective_generational(&argv(&["--XX:UseGc", "G1"])));
    assert_eq!(
        scan_selected_gc_name(&argv(&["--XX:UseGc", "G1"])),
        Some("G1")
    );
    // A program argument after `--` is not a selector.
    assert_eq!(
        scan_selected_gc_name(&argv(&["--", "--XX:UseGc", "G1"])),
        None
    );
}

/// gcj j2/x1: HotSpot's `StringUtils::similarity` on the shapes above.
#[test]
fn gcj_j2_x1_hotspot_similarity() {
    let s = |a: &str, b: &str| hotspot_string_similarity(a.as_bytes(), b.as_bytes());
    assert!((s("UseSerialGC", "UseserialGC") - 16.0 / 22.0).abs() < 1e-12);
    assert!((s("UseG1GC", "Useg1GC") - 8.0 / 14.0).abs() < 1e-12);
    assert_eq!(s("", ""), 0.0);
    assert_eq!(
        hotspot_fuzzy_match("UseserialGC").map(|f| f.0),
        Some("UseSerialGC")
    );
    assert_eq!(hotspot_fuzzy_match("UseFooGC").map(|f| f.0), None);
}

/// gcj j1/ob: the Generational collector, as the parsed path decides it.
#[test]
fn gcj_j1_ob_scan_effective_generational() {
    assert!(scan_effective_generational(&argv(&["-XX:+UseGenerationalGC"])));
    assert!(scan_effective_generational(&argv(&["-XX:+UseSerialGC"])));
    assert!(!scan_effective_generational(&argv(&["-XX:+UseG1GC"])));
    assert!(scan_effective_generational(&argv(&["-XX:+UseG1GC", "-XX:-UseG1GC"])));
    assert_eq!(
        scan_effective_generational(&argv(&["-version"])),
        !cfg!(feature = "zgc")
    );
    if cfg!(feature = "zgc") {
        assert!(!scan_effective_generational(&argv(&["-XX:+UseG1GC", "-XX:+UseZGC"])));
        // Program arguments are not launcher options.
        assert!(!scan_effective_generational(&argv(&["Main", "--", "-XX:+UseSerialGC"])));
    }
}
