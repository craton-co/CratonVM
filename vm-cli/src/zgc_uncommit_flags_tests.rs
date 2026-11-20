use super::*;

#[test]
fn zgc_uncommit_flags_survive_the_launcher_pipeline_and_last_wins() {
    let argv: Vec<String> = [
        "java",
        "-XX:+UseZGC",
        "-XX:-ZUncommit",
        "-XX:+ZUncommit",
        "-XX:ZUncommitDelay=1",
        "-XX:ZUncommitDelay=0",
        "Main",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
    let (args, _) = extract_system_properties(args);
    let (args, _) = extract_hotspot_flags(args);
    let parsed = Args::try_parse_from(args).unwrap();
    assert_eq!(parsed.zgc_uncommit, Some(true));
    assert_eq!(parsed.zgc_uncommit_delay.as_deref(), Some("0"));
    let args = Args::try_parse_from(["java", "--XX:ZUncommitDelay", "0x10"])
        .expect("HotSpot's hexadecimal uintx spelling reaches the consumer");
    assert_eq!(hotspot_parse_uintx(args.zgc_uncommit_delay.as_deref().unwrap()), Some(16));
}

/// zr z3/mt: the HotSpot spellings of the GC worker flags reach clap (they
/// fell into the silent `-XX:` catch-all), and the HotSpot spelling of
/// `ParallelGCThreads` reaches the shared per-VM application path.
#[test]
fn gc_worker_flags_survive_the_launcher_pipeline() {
    let argv: Vec<String> = [
        "java",
        "-XX:+UseZGC",
        "-XX:ParallelGCThreads=3",
        "-XX:ConcGCThreads=4",
        "-XX:+UseDynamicNumberOfGCThreads",
        "-XX:-UseDynamicNumberOfGCThreads",
        "Main",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
    let (args, _) = extract_system_properties(args);
    let (args, _) = extract_hotspot_flags(args);
    let parsed = Args::try_parse_from(args).unwrap();
    assert_eq!(parsed.zgc_parallel_gc_threads.as_deref(), Some("3"));
    assert_eq!(parsed.g1_parallel_gc_threads, None);
    assert_eq!(parsed.conc_gc_threads.as_deref(), Some("4"));
    assert_eq!(parsed.use_dynamic_gc_threads, Some(false), "last wins");
}

#[test]
fn g1_parallel_threads_preserve_last_spelling_and_argument_boundary() {
    for flags in [
        vec!["-XX:ParallelGCThreads=4", "-XX:ParallelGCThreads=2"],
        vec!["--XX:ParallelGCThreads", "2"],
        vec!["--XX:ParallelGCThreads=4", "-XX:ParallelGCThreads=2"],
        vec!["-XX:ParallelGCThreads=4", "--XX:ParallelGCThreads=2"],
    ] {
        let mut argv = vec!["java", "-XX:+UseG1GC"];
        argv.extend(flags);
        argv.extend(["Main", "-XX:ParallelGCThreads=9"]);
        let args = normalize_java_launcher_argv(insert_program_args_separator(
            argv.into_iter().map(str::to_string).collect(),
        ));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args).unwrap();
        assert_eq!(
            parsed
                .zgc_parallel_gc_threads
                .as_deref()
                .or(parsed.g1_parallel_gc_threads.as_deref()),
            Some("2")
        );
    }
}

#[test]
fn g1_parallel_threads_refuse_only_the_effective_zero() {
    let scan = |flags: &[&str]| {
        let mut argv = vec!["java".to_string()];
        argv.extend(flags.iter().map(|s| s.to_string()));
        scan_g1_zero_parallel_gc_threads_refusal(&insert_program_args_separator(argv))
    };
    for zero in ["0", "00", "0x0", "0k"] {
        let flag = format!("-XX:ParallelGCThreads={zero}");
        assert!(scan(&["-XX:+UseG1GC", &flag, "-version"]).is_some());
        assert!(scan(&["-XX:+UseG1GC", &flag, "-XX:ParallelGCThreads=2", "Main"]).is_none());
        assert!(scan(&["-XX:+UseG1GC", "Main", &flag]).is_none());
        assert!(scan(&["-XX:+UseG1GC", "-XX:+UseSerialGC", &flag, "-version"]).is_none());
    }
}

/// zt t7/x4: `-XX:-ZProactive` and a NEGATIVE
/// `-XX:ZAllocationSpikeTolerance` (HotSpot 25.0.3 takes `=-1`) survive
/// the launcher pipeline. The tolerance used to be split into a separate
/// `-1` token, which clap reads as a short flag: the whole parse failed.
/// Control: a positive value and the `+` spelling.
#[test]
fn zt7x4_director_flags_survive_the_launcher_pipeline() {
    for (tolerance, proactive, want) in [
        ("-XX:ZAllocationSpikeTolerance=-1", "-XX:-ZProactive", ("-1", false)),
        ("-XX:ZAllocationSpikeTolerance=0", "-XX:-ZProactive", ("0", false)),
        ("-XX:ZAllocationSpikeTolerance=2.5", "-XX:+ZProactive", ("2.5", true)),
    ] {
        let argv: Vec<String> = ["java", "-XX:+UseZGC", proactive, tolerance, "Main"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args)
            .unwrap_or_else(|e| panic!("{tolerance} {proactive}: {e}"));
        assert_eq!(parsed.zgc_allocation_spike_tolerance.as_deref(), Some(want.0));
        assert_eq!(parsed.zgc_proactive, Some(want.1));
    }
}

/// zt t7/x4: HotSpot 25.0.3's answers for `-XX:ZAllocationSpikeTolerance`
/// (local `-XX:+PrintFlagsFinal` / `Improperly specified VM option` rows).
#[test]
fn zt7x4_spike_tolerance_takes_hotspots_double_syntax() {
    for (raw, want) in [
        ("0", Some(0.0)),
        ("-1", Some(-1.0)),
        ("+3", Some(3.0)),
        (".5", Some(0.5)),
        ("5", Some(5.0)),
        ("2.5e0", Some(2.5)),
        ("0e5", Some(0.0)),
    ] {
        assert_eq!(hotspot_parse_double_flag(raw), want, "{raw:?}");
    }
    for raw in ["", " 3", "3 ", "abc", "inf", "NaN", "1e400", "1e-400", "3x"] {
        assert_eq!(hotspot_parse_double_flag(raw), None, "{raw:?} is refused");
    }
}

/// zt t7/x4b: a ZGC boolean without its `+`/`-` reaches `run` (the
/// `Missing +/- setting` refusal) in command-line order; it used to fall
/// into the silent `-XX:` catch-all. Control: the signed spellings and
/// `ZUncommitDelay` take their own options.
#[test]
fn zt7x4b_a_zgc_boolean_without_its_sign_reaches_the_refusal() {
    assert_eq!(zgc_boolean_without_sign("-XX:ZProactive=false"), Some("ZProactive=false"));
    assert_eq!(zgc_boolean_without_sign("-XX:ZProactive"), Some("ZProactive"));
    assert_eq!(zgc_boolean_without_sign("-XX:ZUncommit=true"), Some("ZUncommit=true"));
    for other in ["-XX:+ZProactive", "-XX:-ZUncommit", "-XX:ZUncommitDelay=5", "-XX:Foo=1"] {
        assert_eq!(zgc_boolean_without_sign(other), None, "{other}");
    }
    let argv: Vec<String> = [
        "java",
        "-XX:+UseZGC",
        "-XX:ZProactive=1",
        "-XX:+ZUncommit",
        "-XX:ZUncommit",
        "-XX:ZUncommitDelay=7",
        "Main",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
    let (args, _) = extract_system_properties(args);
    let (args, _) = extract_hotspot_flags(args);
    let parsed = Args::try_parse_from(args).unwrap();
    assert_eq!(parsed.zgc_boolean_without_sign, ["ZProactive=1", "ZUncommit"]);
    assert_eq!(parsed.zgc_uncommit, Some(true));
    assert_eq!(parsed.zgc_uncommit_delay.as_deref(), Some("7"));
    assert_eq!(parsed.zgc_proactive, None);
}

/// zt t7/x4b: `strtod`'s hexadecimal form, as HotSpot 25.0.3 takes it
/// (local `-XX:+PrintFlagsFinal` rows: `0x1p3` 8.0, `0x10` 16.0,
/// `-0x1.8p1` -3.0, `0X1P-2` 0.25, `0x.8` 0.5; `0x` refused) and a
/// subnormal (`1e-310`, taken on Windows). Control: malformed hex forms
/// stay refused.
#[test]
fn zt7x4b_spike_tolerance_takes_the_hexadecimal_form() {
    for (raw, want) in [
        ("0x1p3", 8.0),
        ("0x10", 16.0),
        ("-0x1.8p1", -3.0),
        ("0X1P-2", 0.25),
        ("0x.8", 0.5),
        ("+0xAp+1", 20.0),
        ("0x0", 0.0),
    ] {
        assert_eq!(hotspot_parse_double_flag(raw), Some(want), "{raw:?}");
    }
    assert!(hotspot_parse_double_flag("1e-310").is_some_and(|v| v > 0.0));
    for raw in ["0x", "0x.", "0x1p", "0x1p+", "0xg", "0x1.8.1", "0x1p3 ", "0x1p99999"] {
        assert_eq!(hotspot_parse_double_flag(raw), None, "{raw:?} is refused");
    }
}

/// zt t8/x5: `-XX:ZCollectionInterval=-1` survives the launcher pipeline
/// (HotSpot 25.0.3 takes it: `-1.000000 {command line}`, the rule off).
/// The value was pushed as a separate token, which clap reads as a short
/// flag when it starts with `-`, so the whole parse failed. An empty value
/// reaches `run` as `""` (HotSpot's `Improperly specified VM option
/// 'ZCollectionInterval='`). Control: a positive value and a hex one.
#[test]
fn zt8x5_a_negative_collection_interval_survives_the_launcher_pipeline() {
    for (raw, want) in [
        ("-XX:ZCollectionInterval=-1", "-1"),
        ("-XX:ZCollectionInterval=-0", "-0"),
        ("-XX:ZCollectionInterval=", ""),
        ("-XX:ZCollectionInterval=2.5", "2.5"),
        ("-XX:ZCollectionInterval=0x1p1", "0x1p1"),
    ] {
        let argv: Vec<String> = ["java", "-XX:+UseZGC", raw, "Main"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args).unwrap_or_else(|e| panic!("{raw}: {e}"));
        assert_eq!(parsed.zgc_collection_interval.as_deref(), Some(want), "{raw}");
    }
    assert_eq!(hotspot_parse_double_flag("-1"), Some(-1.0));
    assert_eq!(hotspot_parse_double_flag(""), None, "empty is refused");
}

fn zt9tg2_argv(args: &[&str]) -> Vec<String> {
    std::iter::once("java")
        .chain(args.iter().copied())
        .map(str::to_string)
        .collect()
}

const ZT9TG2_ALL_ON: ZgcFlagRefusalRules = ZgcFlagRefusalRules {
    zgc: true,
    any_collector: true,
    sign_required: true,
    double_syntax: true,
    gc_threads: true,
    gc_values: true,
    limit_flags: true,
    limit_range: true,
    thp_unrecognized: false,
    large_page_sign: true,
};

/// zt t9/tg2: HotSpot 25.0.3's `uintx` / `size_t` syntax
/// (`ZUncommitDelay`, `SoftMaxHeapSize`; local `-version` rows).
#[test]
fn zt9tg2_uintx_flags_take_hotspots_integer_syntax() {
    for (raw, want) in [
        ("0", 0u64),
        ("4294967296", 1 << 32),
        ("18446744073709551615", u64::MAX),
        ("0x10", 16),
        ("0X10", 16),
        ("3k", 3 << 10),
        ("16t", 16 << 40),
        ("0x100000", 1 << 20),
    ] {
        assert_eq!(hotspot_parse_uintx_flag(raw), Some(want), "{raw:?}");
    }
    for raw in [
        "18446744073709551616",
        "17179869184t",
        "-1",
        "+5",
        "",
        "abc",
        "1.5g",
        "1b",
        "0x",
        " 5",
    ] {
        assert_eq!(hotspot_parse_uintx_flag(raw), None, "{raw:?} is refused");
    }
}

/// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 6):
/// the first bad option in command-line order is the one refused, as on
/// HotSpot 25.0.3 (local `-version` rows, `t9-tg2-report.md`); a later
/// occurrence does not rescue an earlier bad one.
#[test]
fn zt9tg2_the_first_bad_zgc_flag_in_command_line_order_is_refused() {
    let scan = |args: &[&str]| scan_first_zgc_flag_refusal(&zt9tg2_argv(args), ZT9TG2_ALL_ON);
    let improper = |o: &str| Some(ZgcFlagRefusal::Improper(o.to_string()));
    let missing = |o: &str| Some(ZgcFlagRefusal::MissingSign(o.to_string()));
    assert_eq!(
        scan(&["-XX:ZCollectionInterval=abc", "-XX:ZProactive=false", "Main"]),
        improper("ZCollectionInterval=abc")
    );
    assert_eq!(
        scan(&["-XX:ZProactive=false", "-XX:ZCollectionInterval=abc", "Main"]),
        missing("ZProactive=false")
    );
    assert_eq!(
        scan(&["-XX:ZCollectionInterval=abc", "-XX:ZCollectionInterval=1", "Main"]),
        improper("ZCollectionInterval=abc"),
        "an overriding later value does not rescue it"
    );
    assert_eq!(
        scan(&["-XX:ZAllocationSpikeTolerance=x", "-XX:ZCollectionInterval=y"]),
        improper("ZAllocationSpikeTolerance=x")
    );
    assert_eq!(
        scan(&["-XX:ConcGCThreads=x", "-XX:ZProactive=false"]),
        improper("ConcGCThreads=x")
    );
    assert_eq!(
        scan(&["-XX:ZProactive=false", "-XX:ConcGCThreads=x"]),
        missing("ZProactive=false")
    );
    assert_eq!(
        scan(&["-XX:ParallelGCThreads=0", "-XX:ZProactive="]),
        missing("ZProactive="),
        "a zero thread count is not a parse refusal"
    );
    assert_eq!(
        scan(&["-XX:SoftMaxHeapSize=abc", "-XX:ZProactive=false"]),
        improper("SoftMaxHeapSize=abc")
    );
    assert_eq!(scan(&["-XX:ZUncommitDelay=-1"]), improper("ZUncommitDelay=-1"));
    assert_eq!(
        scan(&["-XX:ZCollectionIntervalMajor=abc"]),
        improper("ZCollectionIntervalMajor=abc")
    );
    assert_eq!(
        scan(&["-XX:ZCollectionIntervalMinor=abc"]),
        improper("ZCollectionIntervalMinor=abc")
    );
    assert_eq!(
        scan(&["-XX:ZCollectionIntervalOnly=true"]),
        missing("ZCollectionIntervalOnly=true")
    );
    assert_eq!(scan(&["-XX:ZUncommitDelay"]), improper("ZUncommitDelay"));
    assert_eq!(scan(&["-XX:ZCollectionInterval"]), improper("ZCollectionInterval"));
    assert_eq!(scan(&["-XX:-ZProactive=true"]), improper("ZProactive=true"));
    // zt t23/hunt4: the two limit flags' `double` syntax.
    assert_eq!(
        scan(&["-XX:ZFragmentationLimit=abc", "-XX:ZProactive=false"]),
        improper("ZFragmentationLimit=abc")
    );
    assert_eq!(scan(&["-XX:ZYoungCompactionLimit="]), improper("ZYoungCompactionLimit="));
    assert_eq!(scan(&["-XX:ZYoungCompactionLimit"]), improper("ZYoungCompactionLimit"));
    let limits_off = ZgcFlagRefusalRules {
        limit_flags: false,
        ..ZT9TG2_ALL_ON
    };
    assert_eq!(
        scan_first_zgc_flag_refusal(
            &zt9tg2_argv(&["-XX:ZFragmentationLimit=abc", "Main"]),
            limits_off
        ),
        None,
        "the kill: dropped silently, as before"
    );
    // Controls: what HotSpot takes, and program arguments.
    for ok in [
        "-XX:ZUncommitDelay=3k",
        "-XX:ZCollectionInterval=-1",
        "-XX:ZCollectionIntervalMajor=0x1p1",
        "-XX:SoftMaxHeapSize=0x100000",
        "-XX:+ZProactive",
        "-XX:-ZCollectionIntervalOnly",
        "-XX:ConcGCThreads=3k",
        "-XX:ZFragmentationLimit=5",
        "-XX:ZYoungCompactionLimit=0x1p4",
    ] {
        assert_eq!(scan(&[ok, "Main"]), None, "{ok}");
    }
    assert_eq!(scan(&["Main", "--", "-XX:ZProactive=false"]), None);
}

/// zt t24/hunt7: HotSpot's `range(0, 100)` of the two limit flags (the
/// `double` range line, then `Improperly specified`), in command-line
/// order, after the syntax check; and off Linux the Linux-only
/// `UseTransparentHugePages` is an unrecognized option unless
/// `-XX:+IgnoreUnrecognizedVMOptions` is given anywhere.
#[test]
fn zt24hunt7_limit_flag_range_and_the_linux_only_thp_flag() {
    let scan = |args: &[&str]| scan_first_zgc_flag_refusal(&zt9tg2_argv(args), ZT9TG2_ALL_ON);
    let out_of_range = |name: &str, shown: &str, raw: &str| {
        Some(ZgcFlagRefusal::Lines(vec![
            format!(
                "double {name}={shown} is outside the allowed range [ 0.000000 ... 100.000000 ]"
            ),
            format!("Improperly specified VM option '{name}={raw}'"),
        ]))
    };
    assert_eq!(
        scan(&["-XX:ZFragmentationLimit=200", "-XX:ZProactive=false"]),
        out_of_range("ZFragmentationLimit", "200.000000", "200")
    );
    assert_eq!(
        scan(&["-XX:ZYoungCompactionLimit=-1"]),
        out_of_range("ZYoungCompactionLimit", "-1.000000", "-1")
    );
    assert_eq!(
        scan(&["-XX:ZYoungCompactionLimit=100.5"]),
        out_of_range("ZYoungCompactionLimit", "100.500000", "100.5")
    );
    assert_eq!(
        scan(&["-XX:ZFragmentationLimit=abc"]),
        Some(ZgcFlagRefusal::Improper(
            "ZFragmentationLimit=abc".to_string()
        )),
        "the syntax refusal is unchanged"
    );
    for ok in [
        "-XX:ZFragmentationLimit=0",
        "-XX:ZFragmentationLimit=100",
        "-XX:ZFragmentationLimit=-0",
        "-XX:ZYoungCompactionLimit=0x1p6",
    ] {
        assert_eq!(scan(&[ok, "Main"]), None, "{ok}");
    }
    let no_range = ZgcFlagRefusalRules {
        limit_range: false,
        ..ZT9TG2_ALL_ON
    };
    assert_eq!(
        scan_first_zgc_flag_refusal(&zt9tg2_argv(&["-XX:ZFragmentationLimit=200"]), no_range),
        None,
        "the kill: an out-of-range value boots, as before"
    );
    // The Linux-only flag, as `from_switches` arms it off Linux.
    let off_linux = ZgcFlagRefusalRules {
        thp_unrecognized: true,
        ..ZT9TG2_ALL_ON
    };
    let thp = |args: &[&str]| scan_first_zgc_flag_refusal(&zt9tg2_argv(args), off_linux);
    let unrecognized = |argname: &str| {
        Some(ZgcFlagRefusal::Lines(vec![format!(
            "Unrecognized VM option '{argname}'"
        )]))
    };
    assert_eq!(
        thp(&["-XX:+UseTransparentHugePages", "Main"]),
        unrecognized("UseTransparentHugePages")
    );
    assert_eq!(
        thp(&["-XX:-UseTransparentHugePages"]),
        unrecognized("UseTransparentHugePages")
    );
    assert_eq!(
        thp(&["-XX:UseTransparentHugePages=true"]),
        unrecognized("UseTransparentHugePages=true")
    );
    assert_eq!(thp(&["-XX:+UseTransparentHugePagesX"]), None);
    assert_eq!(
        thp(&["-XX:ZProactive=false", "-XX:+UseTransparentHugePages"]),
        Some(ZgcFlagRefusal::MissingSign("ZProactive=false".to_string())),
        "command-line order"
    );
    assert_eq!(thp(&["Main", "--", "-XX:+UseTransparentHugePages"]), None);
    assert_eq!(
        scan(&["-XX:+UseTransparentHugePages"]),
        None,
        "on Linux, and under the kill, the flag is taken"
    );
    // zt t25/hunt7b: the large-page booleans need their sign (host row
    // h7_W1e on Linux), on every collector; the kill drops them again.
    let missing = |o: &str| Some(ZgcFlagRefusal::MissingSign(o.to_string()));
    assert_eq!(
        large_page_flag_sign_refusal_of("UseTransparentHugePages=true", true),
        missing("UseTransparentHugePages=true")
    );
    assert_eq!(
        large_page_flag_sign_refusal_of("UseLargePages", true),
        missing("UseLargePages")
    );
    assert_eq!(
        large_page_flag_sign_refusal_of("UseLargePages=1", false),
        missing("UseLargePages=1")
    );
    assert_eq!(
        large_page_flag_sign_refusal_of("+UseLargePages=true", true),
        Some(ZgcFlagRefusal::Improper("UseLargePages=true".to_string()))
    );
    for ok in [
        "+UseLargePages",
        "-UseTransparentHugePages",
        "UseLargePagesX=1",
    ] {
        assert_eq!(large_page_flag_sign_refusal_of(ok, true), None, "{ok}");
    }
    assert_eq!(
        large_page_flag_sign_refusal_of("UseTransparentHugePages=true", false),
        None,
        "off Linux the flag does not exist"
    );
    assert_eq!(
        scan(&["-XX:UseTransparentHugePages=true", "-XX:ZProactive=false"]),
        if cfg!(target_os = "linux") {
            missing("UseTransparentHugePages=true")
        } else {
            missing("ZProactive=false")
        }
    );
    let no_sign = ZgcFlagRefusalRules {
        large_page_sign: false,
        ..ZT9TG2_ALL_ON
    };
    assert_eq!(
        scan_first_zgc_flag_refusal(&zt9tg2_argv(&["-XX:UseLargePages=true"]), no_sign),
        None
    );
    let ignore = |args: &[&str]| scan_ignore_unrecognized_vm_options(&zt9tg2_argv(args));
    assert!(ignore(&[
        "-XX:+UseTransparentHugePages",
        "-XX:+IgnoreUnrecognizedVMOptions"
    ]));
    assert!(!ignore(&[
        "-XX:+IgnoreUnrecognizedVMOptions",
        "-XX:-IgnoreUnrecognizedVMOptions"
    ]));
    assert!(!ignore(&["Main", "--", "-XX:+IgnoreUnrecognizedVMOptions"]));
    assert!(!ignore(&["Main"]));
}

/// zt t9/tg2 (`zgc-zt8x5-wave-t7-review-residuals-CLOSED-20261019.md` item 7):
/// HotSpot's option parsing does not depend on the collector (local
/// 25.0.3 refuses `-XX:+UseG1GC -XX:ZProactive=false`). Off ZGC the ZGC
/// flags are refused under the any-collector rule, the GC thread counts
/// are not (another page); the `=0` rule refuses nothing off ZGC
/// (control).
#[test]
fn zt9tg2_zgc_flags_are_refused_on_every_collector() {
    let g1 = ZgcFlagRefusalRules {
        zgc: false,
        ..ZT9TG2_ALL_ON
    };
    let argv = zt9tg2_argv(&["-XX:+UseG1GC", "-XX:ConcGCThreads=x", "-XX:ZProactive=false"]);
    assert_eq!(
        scan_first_zgc_flag_refusal(&argv, g1),
        Some(ZgcFlagRefusal::MissingSign("ZProactive=false".to_string()))
    );
    let argv = zt9tg2_argv(&["-XX:+UseSerialGC", "-XX:ZUncommit=true"]);
    assert_eq!(
        scan_first_zgc_flag_refusal(&argv, g1),
        Some(ZgcFlagRefusal::MissingSign("ZUncommit=true".to_string()))
    );
    let argv = zt9tg2_argv(&["-XX:+UseG1GC", "-XX:ZCollectionInterval=abc"]);
    assert_eq!(
        scan_first_zgc_flag_refusal(&argv, g1),
        Some(ZgcFlagRefusal::Improper("ZCollectionInterval=abc".to_string()))
    );
    let zgc_only = ZgcFlagRefusalRules {
        any_collector: false,
        ..g1
    };
    assert_eq!(scan_first_zgc_flag_refusal(&argv, zgc_only), None, "=0 control");
    // Each arm's own kill switch still turns that arm off.
    let no_sign = ZgcFlagRefusalRules {
        sign_required: false,
        ..ZT9TG2_ALL_ON
    };
    let argv = zt9tg2_argv(&["-XX:ZProactive=false", "-XX:ZCollectionInterval=abc"]);
    assert_eq!(
        scan_first_zgc_flag_refusal(&argv, no_sign),
        Some(ZgcFlagRefusal::Improper("ZCollectionInterval=abc".to_string()))
    );
}

/// zt t9/tg2: `-XX:ZUncommitDelay` takes HotSpot's syntax through the
/// launcher pipeline (`3k` was a clap failure); `-XX:ZCollectionIntervalMajor`
/// reaches `run` (it was dropped by the `-XX:` catch-all).
#[test]
fn zt9tg2_uncommit_delay_and_interval_major_survive_the_pipeline() {
    let argv = zt9tg2_argv(&[
        "-XX:+UseZGC",
        "-XX:ZUncommitDelay=3k",
        "-XX:ZCollectionIntervalMajor=-1",
        "-XX:ZCollectionInterval=3",
        "Main",
    ]);
    let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
    let (args, _) = extract_system_properties(args);
    let (args, _) = extract_hotspot_flags(args);
    let parsed = Args::try_parse_from(args).unwrap();
    // zt t24/m5: dev cabdb9f4ae made the field a `String` (the launcher
    // hands it the decimal value HotSpot read).
    assert_eq!(parsed.zgc_uncommit_delay.as_deref(), Some("3072"));
    assert_eq!(parsed.zgc_collection_interval_major.as_deref(), Some("-1"));
    assert_eq!(parsed.zgc_collection_interval.as_deref(), Some("3"));
}

/// zt t27/gn5 (gn page item 14): `-XX:ZCollectionIntervalMinor` reaches
/// `run` as one token (it was dropped by the `-XX:` catch-all), a
/// negative value -- HotSpot's default `-1` -- included.
#[test]
fn zt27gn5_interval_minor_survives_the_pipeline() {
    for (raw, want) in [("0.2", "0.2"), ("-1", "-1")] {
        let flag = format!("-XX:ZCollectionIntervalMinor={raw}");
        let argv = zt9tg2_argv(&["-XX:+UseZGC", flag.as_str(), "Main"]);
        let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args).unwrap();
        assert_eq!(
            parsed.zgc_collection_interval_minor.as_deref(),
            Some(want),
            "{raw}"
        );
        assert_eq!(parsed.zgc_collection_interval_major, None);
        assert_eq!(parsed.zgc_collection_interval, None);
    }
}

/// zt t10/tg3 (`zgc-zt9tg2-*` item 5): the tenuring / `SurvivorRatio` /
/// TLAB parse refusals join the ordered scan, with the arms' own lines
/// (local 25.0.3 `-version` rows); the `=0` scan names the ZGC flag for
/// both orders (control).
#[test]
fn zt10tg3_heap_flag_refusals_take_their_command_line_place() {
    let lines = |l: &[&str]| Some(l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        heap_flag_parse_refusal_of("-XX:MaxTenuringThreshold=x", true),
        lines(&["Improperly specified VM option 'MaxTenuringThreshold=x'"])
    );
    assert_eq!(
        heap_flag_parse_refusal_of("-XX:MaxTenuringThreshold=17", true),
        lines(&["uint MaxTenuringThreshold=17 is outside the allowed range [ 0 ... 16 ]"])
    );
    assert_eq!(
        heap_flag_parse_refusal_of("-XX:TargetSurvivorRatio=101", true),
        lines(&[
            "uint TargetSurvivorRatio=101 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'TargetSurvivorRatio=101'",
        ])
    );
    assert_eq!(
        heap_flag_parse_refusal_of("-XX:SurvivorRatio=0", true),
        lines(&[
            "uintx SurvivorRatio=0 is outside the allowed range [ 1 ... 18446744073709551613 ]",
            "Improperly specified VM option 'SurvivorRatio=0'",
        ])
    );
    assert_eq!(
        heap_flag_parse_refusal_of("-XX:SurvivorRatio=x", true),
        lines(&["Improperly specified VM option 'SurvivorRatio=x'"])
    );
    assert_eq!(heap_flag_parse_refusal_of("-XX:SurvivorRatio=0", false), None);
    for ok in [
        "-XX:MaxTenuringThreshold=16",
        "-XX:InitialTenuringThreshold=0",
        "-XX:TargetSurvivorRatio=50",
        "-XX:SurvivorRatio=8",
        "-XX:ZProactive=false",
        "-Xmx64m",
    ] {
        assert_eq!(heap_flag_parse_refusal_of(ok, true), None, "{ok}");
    }
    let heap = |l: &[&str]| lines(l).map(OrderedLauncherRefusal::HeapFlag);
    let missing = Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::MissingSign(
        "ZProactive=false".to_string(),
    )));
    let argv = zt9tg2_argv(&["-XX:MaxTenuringThreshold=x", "-XX:ZProactive=false"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true),
        heap(&["Improperly specified VM option 'MaxTenuringThreshold=x'"])
    );
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, false, true),
        missing,
        "=0: the ZGC scan alone"
    );
    let argv = zt9tg2_argv(&["-XX:ZProactive=false", "-XX:MaxTenuringThreshold=x"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true),
        missing,
        "the reverse order names ZProactive"
    );
    // Off ZGC too: HotSpot's option parsing does not depend on the collector.
    let g1 = ZgcFlagRefusalRules {
        zgc: false,
        ..ZT9TG2_ALL_ON
    };
    let argv = zt9tg2_argv(&["-XX:+UseG1GC", "-XX:SurvivorRatio=x", "-XX:ZProactive=false"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, g1, true, true),
        heap(&["Improperly specified VM option 'SurvivorRatio=x'"])
    );
    // Program arguments are not options.
    let argv = zt9tg2_argv(&["Main", "-XX:MaxTenuringThreshold=x"]);
    let argv = insert_program_args_separator(argv);
    assert_eq!(scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true), None);
}


#[test]
fn zt10tg3_soft_max_above_the_maximum_heap_is_refused() {
    const M: u64 = 1024 * 1024;
    let two = HOTSPOT_SERIAL_HEAP_ALIGNMENT;
    assert!(soft_max_heap_size_refused(128 * M, 64 << 20, two));
    assert!(soft_max_heap_size_refused(64 * M + 1, 64 << 20, two));
    assert!(!soft_max_heap_size_refused(64 * M, 64 << 20, two));
    assert!(!soft_max_heap_size_refused(10 * M, 9 << 20, two), "aligned to 10M");
    assert!(!soft_max_heap_size_refused(9 * M + 1, 9 << 20, two));
    assert!(soft_max_heap_size_refused(10 * M + 1, 9 << 20, two));
    assert!(soft_max_heap_size_refused(10 * M, 9 << 20, 1), "no alignment");
    let scan = |args: &[&str]| scan_soft_max_above_max_heap(&zt9tg2_argv(args));
    assert!(scan(&["-XX:+UseZGC", "-Xmx64m", "-XX:SoftMaxHeapSize=128m", "-version"]));
    assert!(scan(&["-XX:+UseZGC", "-XX:SoftMaxHeapSize=128m", "-Xmx64m", "-version"]));
    assert!(scan(&["-XX:+UseG1GC", "-Xmx64m", "-XX:SoftMaxHeapSize=128m", "-version"]));
    assert!(!scan(&["-XX:+UseZGC", "-Xmx64m", "-XX:SoftMaxHeapSize=64m", "-version"]));
    assert!(!scan(&["-XX:+UseG1GC", "-Xmx9m", "-XX:SoftMaxHeapSize=10m", "-version"]));
    assert!(!scan(&["-Xmx64m", "-XX:SoftMaxHeapSize=128m", "-Xmx256m", "-version"]));
    assert!(!scan(&["-Xmx64m", "-version"]), "no soft maximum");
    assert!(!scan(&["-Xmx64m", "-XX:SoftMaxHeapSize=abc", "-version"]), "the parse refuses");
    // zt t11/tg4 (T2): an unreadable maximum refuses nothing (it fell
    // back to the ergonomic maximum).
    assert!(!scan(&["-Xmx1t", "-XX:SoftMaxHeapSize=64g", "-version"]));
    assert!(!scan(&["-XX:MaxHeapSize=x", "-XX:SoftMaxHeapSize=64g", "-version"]));
    assert!(is_max_heap_option("-Xmx64m") && is_max_heap_option("--Xmx=64m"));
    assert!(is_max_heap_option("-XX:MaxHeapSize=64m") && !is_max_heap_option("-Xms64m"));
}

/// zt t10/tg3 (`zgc-zt9tg2-*` item 2): `-XX:±ZCollectionIntervalOnly`
/// reaches `run` (it fell into the `-XX:` catch-all); the sign-less
/// spelling stays a refusal of the scan.
#[test]
fn zt10tg3_collection_interval_only_survives_the_pipeline() {
    for (flag, want) in [
        ("-XX:+ZCollectionIntervalOnly", Some(true)),
        ("-XX:-ZCollectionIntervalOnly", Some(false)),
    ] {
        let argv = zt9tg2_argv(&["-XX:+UseZGC", flag, "Main"]);
        let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args).unwrap();
        assert_eq!(parsed.zgc_collection_interval_only, want, "{flag}");
    }
    let argv = zt9tg2_argv(&["-XX:ZCollectionIntervalOnly=true"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true),
        Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::MissingSign(
            "ZCollectionIntervalOnly=true".to_string()
        )))
    );
}

/// zt t12/tg5 (t10 verdicts, new defect 2): a launcher option without
/// its value is refused with the JDK 25 launcher's line (local 25.0.3,
/// every expectation below is its stderr line, rc 1); a value, a version
/// query before it, or the program selector before it is not refused.
#[test]
fn zt12tg5_a_launcher_option_without_its_value_is_refused_as_hotspot() {
    let scan = |args: &[&str]| scan_launcher_missing_value_refusal(&zt9tg2_argv(args));
    let line = |s: &str| Some(s.to_string());
    for opt in [
        "--add-modules",
        "--add-exports",
        "--add-opens",
        "--add-reads",
        "--patch-module",
        "--limit-modules",
        "--enable-native-access",
    ] {
        let want = line(&format!("Error: {opt} requires modules to be specified"));
        assert_eq!(scan(&[opt]), want, "{opt} last");
        assert_eq!(scan(&[opt, "-version"]), want, "{opt} -version");
        assert_eq!(scan(&["-Xmx1x", opt]), want, "{opt}: before the VM options");
    }
    for opt in ["--module-path", "-p", "--upgrade-module-path"] {
        let want = line(&format!("Error: {opt} requires module path specification"));
        assert_eq!(scan(&[opt]), want, "{opt}");
        assert_eq!(scan(&[opt, "-version"]), want, "{opt} -version");
    }
    for opt in ["-cp", "-classpath", "--class-path"] {
        let want = line(&format!("Error: {opt} requires class path specification"));
        assert_eq!(scan(&[opt]), want, "{opt}");
        assert_eq!(scan(&["-Xshare:off", opt]), want, "{opt} after an option");
    }
    assert_eq!(scan(&["-m"]), line("Error: -m requires module name"));
    assert_eq!(scan(&["--module", "-version"]), line("Error: --module requires module name"));
    assert_eq!(scan(&["--module="]), line("Error: --module= requires module name"));
    assert_eq!(scan(&["--source"]), line("Error: --source requires source version"));
    assert_eq!(scan(&["--source="]), line("Error: --source= requires source version"));
    assert_eq!(scan(&["--source", "-version"]), line("Error: --source requires source version"));
    assert_eq!(scan(&["--source", "21", "-version"]), None);
    assert_eq!(scan(&["--source=21", "-version"]), None);
    for opt in [
        "--module-path=",
        "--upgrade-module-path=",
        "--add-modules=",
        "--enable-native-access=",
        "--limit-modules=",
        "--add-exports=",
        "--add-reads=",
        "--patch-module=",
    ] {
        let want = line(&format!("Error: {opt} requires modules to be specified"));
        assert_eq!(scan(&[opt, "-version"]), want, "{opt}");
    }
    // zt t14/tg6: an empty separate value is no value, except the class
    // path's (libjli `has_arg` / `has_arg_any_len`).
    assert_eq!(
        scan(&["--add-opens", "", "-version"]),
        line("Error: --add-opens requires modules to be specified")
    );
    assert_eq!(
        scan(&["-p", "", "-version"]),
        line("Error: -p requires module path specification")
    );
    assert_eq!(scan(&["-m", ""]), line("Error: -m requires module name"));
    assert_eq!(scan(&["-cp", "", "-version"]), None, "an empty class path boots");
    // Not refused by the launcher: an empty `--add-opens=` (the boot
    // layer refuses it, zt t14/tg6) / `--class-path=` (boots), a value, a
    // version query or help first, the program selector first, and the
    // program's own arguments.
    assert_eq!(scan(&["--add-opens=", "-version"]), None);
    assert_eq!(scan(&["--class-path=", "-version"]), None);
    assert_eq!(scan(&["--add-modules", "java.base", "-version"]), None);
    assert_eq!(scan(&["--add-modules=java.base", "-cp", "x", "Main"]), None);
    assert_eq!(scan(&["-version", "--add-modules"]), None);
    assert_eq!(scan(&["--version", "-cp"]), None);
    assert_eq!(scan(&["--help", "--add-modules"]), None);
    assert_eq!(scan(&["-cp", "x", "Main", "--add-modules"]), None);
    assert_eq!(scan(&["-m", "app/a.Main", "--add-modules"]), None);
    assert_eq!(scan(&["-jar", "a.jar", "-cp"]), None);
    assert_eq!(scan(&["-Xmx", "64m", "Main", "-cp"]), None, "a split -Xmx value");
    assert_eq!(scan(&["Main", "--", "-p"]), None);
    // The order: the first option without a value is named.
    assert_eq!(
        scan(&["-cp", "x", "--add-modules", "-p"]),
        line("Error: --add-modules requires modules to be specified")
    );
    assert_eq!(
        scan(&["-showversion", "-cp"]),
        line("Error: -cp requires class path specification"),
        "-showversion does not end the scan"
    );
}

#[test]
fn hotspot_source_launcher_conflict_refusal_is_detected() {
    let scan = |args: &[&str]| scan_source_launcher_conflict_refusal(&zt9tg2_argv(args));
    let line = |s: &str| Some(s.to_string());
    assert_eq!(
        scan(&["--source", "21", "-jar", "foo.jar"]),
        line("Error: Option -jar is not allowed with --source")
    );
    assert_eq!(
        scan(&["--source=21", "-jar", "foo.jar"]),
        line("Error: Option -jar is not allowed with --source")
    );
    assert_eq!(
        scan(&["--source", "21", "-m", "mod/pkg.Main"]),
        line("Error: Option -m is not allowed with --source")
    );
    assert_eq!(
        scan(&["--source", "21", "--module", "mod/pkg.Main"]),
        line("Error: Option --module is not allowed with --source")
    );
    assert_eq!(
        scan(&["--source", "21", "--module=mod/pkg.Main"]),
        line("Error: Option --module=mod/pkg.Main is not allowed with --source")
    );
    assert_eq!(
        scan(&["--source", "21", "-Xmx256m", "-jar", "foo.jar"]),
        line("Error: Option -jar is not allowed with --source")
    );
    assert_eq!(scan(&["--source", "21", "Main.java", "-jar", "foo.jar"]), None);
    assert_eq!(scan(&["-jar", "foo.jar", "--source", "21"]), None);
}

/// zt t14/tg6 (t12 verdicts new defect 1, row `tg5_m17`): the boot
/// layer's refusals of an `--add-reads` / `--add-exports` /
/// `--add-opens` value (JDK 25 `ModuleBootstrap.decode` /
/// `addExtraExportsOrOpens`); valid values, `ALL-UNNAMED` and the
/// options after a version query or the program selector pass.
#[test]
fn zt14tg6_the_boot_layer_refuses_an_undecodable_module_option() {
    let options = |args: &[&str]| scan_boot_layer_module_options(&zt9tg2_argv(args));
    let refusal = |args: &[&str]| boot_layer_module_option_refusal(&options(args));
    let rt = |s: &str| Some(format!("java.lang.RuntimeException: {s}"));
    // Positive control: local 25.0.3's line for `--add-opens= -version`.
    assert_eq!(
        refusal(&["--add-opens=", "-version"]),
        rt("Unable to parse --add-opens <module>=<value>: ")
    );
    assert_eq!(
        refusal(&["--add-opens", "java.base", "-version"]),
        rt("Unable to parse --add-opens <module>=<value>: java.base")
    );
    assert_eq!(
        refusal(&["--add-exports==x", "Main"]),
        rt("Unable to parse --add-exports <module>=<value>: =x")
    );
    assert_eq!(
        refusal(&["--add-reads", "java.base=", "Main"]),
        rt("Unable to parse --add-reads <module>=<value>: java.base=")
    );
    assert_eq!(
        refusal(&["--add-opens=java.base/java.lang=,,", "-version"]),
        rt("Target must be specified: --add-opens java.base/java.lang=,,")
    );
    for key in ["java.base", "java.base/", "/java.lang", "a/b/c", "java.base//"] {
        assert_eq!(
            refusal(&[format!("--add-exports={key}=ALL-UNNAMED").as_str(), "-version"]),
            rt(&format!("Unable to parse --add-exports <module>/<package>: {key}")),
            "{key}"
        );
    }
    // `String.split("/")` drops the trailing empty string: accepted.
    assert_eq!(refusal(&["--add-opens=java.base/java.lang/=ALL-UNNAMED", "-version"]), None);
    // Boot order: reads, then exports (decode, keys), then opens.
    assert_eq!(
        refusal(&["--add-opens=", "--add-exports", "bad=x", "--add-reads", "r", "Main"]),
        rt("Unable to parse --add-reads <module>=<value>: r")
    );
    assert_eq!(
        refusal(&["--add-opens=", "--add-exports", "bad=x", "Main"]),
        rt("Unable to parse --add-exports <module>/<package>: bad")
    );
    // Not refused: valid values, an empty command line, and anything
    // after a version query or the program selector.
    assert_eq!(
        refusal(&[
            "--add-opens=java.base/java.lang=ALL-UNNAMED",
            "--add-exports",
            "java.base/jdk.internal.misc=ALL-UNNAMED,java.logging",
            "--add-reads=java.logging=ALL-UNNAMED",
            "-version",
        ]),
        None
    );
    assert_eq!(refusal(&["-version"]), None);
    assert_eq!(refusal(&["-version", "--add-opens="]), None);
    assert_eq!(refusal(&["Main", "--add-opens="]), None);
    assert_eq!(refusal(&["-jar", "a.jar", "--add-opens="]), None);
    assert_eq!(refusal(&["-m", "app/a.Main", "--add-opens="]), None);
    assert_eq!(
        refusal(&["-cp", "x", "--add-opens=", "Main"]),
        rt("Unable to parse --add-opens <module>=<value>: ")
    );
    // The scan: the `=` and the separate forms, in order.
    assert_eq!(
        options(&["--add-opens=a", "--add-opens", "b", "--add-reads=c", "-Xmx", "1g", "Main"]),
        BootLayerModuleOptions {
            add_reads: vec!["c".to_string()],
            add_exports: vec![],
            add_opens: vec!["a".to_string(), "b".to_string()],
            patch_module: vec![],
            module_path: None,
        }
    );
}

fn zt16tg7_strings(tokens: &[&str]) -> Vec<String> {
    tokens.iter().map(|t| t.to_string()).collect()
}

/// zt t16/tg7 (page `zgc-zt12tg5-*` item 5): the usage texts are the JDK
/// 25 resource bundle's (`sun.launcher.resources.launcher`), with `java`
/// and `File.pathSeparator` filled in, and a bare `-jar` is libjli's
/// `ARG_CHECK` refusal followed by the full usage.
#[test]
fn zt16tg7_the_usage_texts_are_the_jdk_launchers() {
    let sep = if cfg!(windows) { ";" } else { ":" };
    // zt t26/rt2: without `jvm.cfg` lines (the host JDK's file may list
    // more VMs; `zt26rt2_the_full_usage_lists_jvm_cfgs_vms` pins them).
    let full = hotspot_full_usage_text_with("");
    assert!(full.starts_with(
        "Usage: java [options] <mainclass> [args...]\n           (to execute a class)\n"
    ));
    assert!(full.contains("\n where options include:\n\n    -cp <class search path"));
    assert!(full.contains(&format!(
        "                  A \"{sep}\"-separated list of directories, JAR archives,\n"
    )));
    assert!(full.ends_with(concat!(
        "To specify an argument for a long option, you can use --<name>=<value> or\n",
        "--<name> <value>.\n"
    )));
    assert!(!full.contains("{0}") && !full.contains("{1}"));
    let concise = hotspot_concise_usage_text();
    assert!(concise.starts_with(
        "Usage: java [java options...] <application> [application arguments...]\n\n"
    ));
    assert!(concise.contains(&format!("search for modules, separated by \"{sep}\"\n")));
    assert!(concise.ends_with("For an interactive Java environment:    jshell"));
    assert!(!concise.contains("{0}"));
    assert_eq!(
        bare_jar_refusal_text(),
        format!(
            "Error: -jar requires jar file specification\n{}",
            hotspot_full_usage_text()
        )
    );
}

/// zt t26/rt2 (`CRATONVM_LAUNCHER_HELP_JVM_CFG_VMS`): libjli's
/// `PrintUsage` lists `jvm.cfg`'s VMs past the default one between the
/// header and the footer. Ubuntu's `openjdk-25` file (t18 `tg8_h4`:
/// HotSpot printed `-zero` and `-dcevm`), JDK 25's own (no line), an
/// alias and the flags that list nothing.
#[test]
fn zt26rt2_the_full_usage_lists_jvm_cfgs_vms() {
    let ubuntu = "# comment\n-server KNOWN\n-client IGNORE\n-zero KNOWN\n-dcevm KNOWN\n";
    assert_eq!(
        jvm_cfg_vm_select_lines(ubuntu),
        concat!(
            "    -zero\t  to select the \"zero\" VM\n",
            "    -dcevm\t  to select the \"dcevm\" VM\n"
        )
    );
    assert_eq!(jvm_cfg_vm_select_lines("-server KNOWN\n-client IGNORE\n"), "");
    assert_eq!(
        jvm_cfg_vm_select_lines(
            "-server KNOWN\n-hotspot ALIASED_TO -server\n-old WARN\n-x ERROR\n-y\n-z KNOWN\n"
        ),
        concat!(
            "    -z\t  to select the \"z\" VM\n",
            "    -hotspot\t  is a synonym for the \"server\" VM  [deprecated]\n"
        )
    );
    // The default VM itself is never listed, whatever its flag.
    assert_eq!(jvm_cfg_vm_select_lines("-zero KNOWN\n"), "");
    let with = hotspot_full_usage_text_with(&jvm_cfg_vm_select_lines(ubuntu));
    assert!(with.contains(concat!(
        "\n where options include:\n\n",
        "    -zero\t  to select the \"zero\" VM\n",
        "    -dcevm\t  to select the \"dcevm\" VM\n",
        "    -cp <class search path"
    )));
}

/// zt t26/rt2 (`CRATONVM_LAUNCHER_TOO_SMALL_INITIAL_HEAP_REFUSED`):
/// HotSpot's floors on the initial and minimum heap (1 MiB), initial
/// first; 0 is "not set". `-Xms` sets both.
#[test]
fn zt26rt2_too_small_initial_and_minimum_heaps_are_refused() {
    let refusal = |args: &[&str]| {
        let (initial, min) = early_initial_and_min_heap(&zt9tg2_argv(args));
        too_small_initial_or_min_heap_refusal(initial, min)
    };
    assert_eq!(refusal(&["-Xms512k", "-version"]), Some("Too small initial heap"));
    assert_eq!(refusal(&["-XX:InitialHeapSize=1048575"]), Some("Too small initial heap"));
    assert_eq!(refusal(&["-XX:MinHeapSize=512k", "Hello"]), Some("Too small minimum heap"));
    assert_eq!(
        refusal(&["-XX:MinHeapSize=4k", "-XX:InitialHeapSize=8k"]),
        Some("Too small initial heap")
    );
    for ok in [
        &["-Xms1m"][..],
        &["-Xms0"],
        &["-XX:InitialHeapSize=0"],
        &["-Xms512k", "-Xms2m"],
        &["-Xmx64m", "Hello"],
    ] {
        assert_eq!(refusal(ok), None, "{ok:?}");
    }
}

/// zt t27/lg1 (`CRATONVM_LAUNCHER_ZGC_HEAP_TOO_LARGE_REFUSED`, rows
/// `x15_T1*`): HotSpot 25 ZGC's ceiling is 16 TiB; `-Xmx20t` is refused
/// with its text, `-Xmx16t` (exactly the ceiling) and `-Xmx1t` boot.
#[test]
fn zt27lg1_a_zgc_heap_above_16t_is_too_large() {
    let refusal = |args: &[&str]| {
        early_max_heap(&zt9tg2_argv(args))
            .and_then(|(size, _)| zgc_heap_too_large_refusal(size as u64))
    };
    assert_eq!(
        refusal(&["-XX:+UseZGC", "-Xmx20t", "-version"]).as_deref(),
        Some("Java heap too large (max supported heap size is 16384G)")
    );
    assert_eq!(
        refusal(&["-XX:MaxHeapSize=16385g"]).as_deref(),
        Some("Java heap too large (max supported heap size is 16384G)")
    );
    assert_eq!(refusal(&["-Xmx16t"]), None);
    assert_eq!(refusal(&["-Xmx1t", "-version"]), None);
    assert_eq!(refusal(&["-version"]), None);
}

/// zt t16/tg7: libjli's `-jar` in the missing-value scan. A `-jar` last
/// is the `ARG_CHECK` refusal; past `-jar` the scan goes on, so a later
/// option without its value is still named; the kill arms keep the old
/// rule (the scan ends at `-jar`).
#[test]
fn zt16tg7_a_bare_jar_is_refused_and_the_scan_reads_past_jar() {
    let on = JarScanRules {
        bare_jar_usage: true,
        scan_past_jar: true,
    };
    let scan = |args: &[&str], missing: bool, jar: JarScanRules| {
        scan_launcher_missing_value_refusal_with(&zt9tg2_argv(args), missing, jar)
    };
    let bare = Some(bare_jar_refusal_text());
    assert_eq!(scan(&["-jar"], true, on), bare);
    assert_eq!(scan(&["-cp", "x", "-jar"], true, on), bare);
    assert_eq!(scan(&["-jar", "-jar"], true, on), bare, "the second -jar is last");
    assert_eq!(scan(&["-jar"], false, on), bare, "missing-value kill: still found");
    assert_eq!(
        scan(&["-jar", "--add-modules"], true, on),
        Some("Error: --add-modules requires modules to be specified".to_string())
    );
    assert_eq!(
        scan(&["--add-modules", "-jar"], true, on),
        Some("Error: --add-modules requires modules to be specified".to_string()),
        "a value may not start with `-`: the option is named first"
    );
    assert_eq!(scan(&["-jar", "--add-modules"], false, on), None);
    assert_eq!(scan(&["-jar", "a.jar", "-cp"], true, on), None);
    assert_eq!(scan(&["-jar", "-Xmx64m", "a.jar", "-cp"], true, on), None);
    assert_eq!(scan(&["-jar", "-version"], true, on), None);
    assert_eq!(scan(&["-version", "-jar"], true, on), None, "the query returns first");
    // Kill arms: the old rule.
    let off = JarScanRules {
        bare_jar_usage: false,
        scan_past_jar: false,
    };
    assert_eq!(scan(&["-jar"], true, off), None);
    assert_eq!(scan(&["-jar", "--add-modules"], true, off), None);
    let usage_only = JarScanRules {
        bare_jar_usage: true,
        scan_past_jar: false,
    };
    assert_eq!(scan(&["-jar"], true, usage_only), bare);
    assert_eq!(scan(&["-jar", "--add-modules"], true, usage_only), None);
}

/// zt t16/tg7: `-jar` is the launch mode; the jar is the first token of
/// the leading section that is not an option, and a later launch mode
/// replaces `-jar`.
#[test]
fn zt16tg7_jar_is_the_launch_mode_not_an_option_with_a_value() {
    let s = zt16tg7_strings;
    let relocate = |args: &[&str]| relocate_jar_selector(s(args));
    // Unchanged: the jar right after `-jar`, no `-jar`, and the spellings
    // this rule leaves alone.
    for case in [
        &["java", "-jar", "a.jar", "-x"][..],
        &["java", "-Xmx64m", "-jar", "a.jar"][..],
        &["java", "-cp", "x", "Main", "-jar"][..],
        &["java", "--jar", "-Xmx64m", "a.jar"][..],
        &["java", "-jar", "--", "a.jar"][..],
        &["java", "-version", "-jar", "-Xmx64m", "a.jar"][..],
    ] {
        assert_eq!(relocate(case), s(case), "{case:?}");
    }
    assert_eq!(
        relocate(&["java", "-jar", "-Xmx64m", "-Dk=v", "a.jar", "-p", "x"]),
        s(&["java", "-Xmx64m", "-Dk=v", "-jar", "a.jar", "-p", "x"])
    );
    assert_eq!(
        relocate(&["java", "-jar", "--add-opens", "java.base/java.lang=ALL-UNNAMED", "a.jar"]),
        s(&["java", "--add-opens", "java.base/java.lang=ALL-UNNAMED", "-jar", "a.jar"]),
        "a white-space option's value is not the jar"
    );
    assert_eq!(
        relocate(&["java", "-jar", "-Xmx", "64m", "a.jar"]),
        s(&["java", "-Xmx", "64m", "-jar", "a.jar"]),
        "this launcher's split value"
    );
    // A section with no jar: the `-jar` is dropped.
    assert_eq!(relocate(&["java", "-jar", "-version"]), s(&["java", "-version"]));
    assert_eq!(relocate(&["java", "-jar", "-Xmx64m"]), s(&["java", "-Xmx64m"]));
    // A later module launch replaces it; a class path option does not
    // (zt t20/tg9, D2: HotSpot's `Unable to access jarfile Main`).
    assert_eq!(
        relocate(&["java", "-jar", "-cp", "x", "Main", "a"]),
        s(&["java", "-cp", "x", "-jar", "Main", "a"])
    );
    assert_eq!(
        relocate(&["java", "-jar", "-Xmx64m", "--class-path=x", "a.jar"]),
        s(&["java", "-Xmx64m", "--class-path=x", "-jar", "a.jar"])
    );
    // The `=0` kill of CRATONVM_LAUNCHER_JAR_CLASS_PATH_KEEPS_JAR: the t16 rule.
    assert_eq!(
        relocate_jar_selector_with(s(&["java", "-jar", "-cp", "x", "Main", "a"]), false),
        s(&["java", "-cp", "x", "Main", "a"])
    );
    assert_eq!(
        relocate(&["java", "-jar", "-m", "app/p.Main"]),
        s(&["java", "-m", "app/p.Main"])
    );
    assert_eq!(
        relocate(&["java", "-jar", "-Xmx64m", "-jar", "-Dk=v", "a.jar"]),
        s(&["java", "-Xmx64m", "-Dk=v", "-jar", "a.jar"])
    );
    // The reshaped argv selects the jar for the separator stage.
    assert_eq!(
        insert_program_args_separator(relocate(&["java", "-jar", "-Xmx64m", "a.jar", "-v"])),
        s(&["java", "-Xmx64m", "-jar", "a.jar", "--", "-v"])
    );
}

/// zt t16/tg7: `--enable-native-access <modules>` takes its separate value
/// as libjli does (`IsModuleOption`): `Main` after it is the value, the
/// token after that the main class.
#[test]
fn zt16tg7_enable_native_access_takes_its_separate_value_as_libjli() {
    let s = zt16tg7_strings;
    let join = |args: &[&str]| join_enable_native_access_space_form(s(args));
    assert_eq!(
        join(&["java", "--enable-native-access", "ALL-UNNAMED", "Main", "a"]),
        s(&["java", "--enable-native-access=ALL-UNNAMED", "Main", "a"])
    );
    let stage1 = insert_program_args_separator(join(&[
        "java",
        "--enable-native-access",
        "Main",
        "arg",
    ]));
    let stage2 = normalize_java_launcher_argv(stage1);
    let (stage3, _props) = extract_system_properties(stage2);
    let (stage4, _hot) = extract_hotspot_flags(stage3);
    let parsed = Args::try_parse_from(stage4).expect("clap must parse the joined value");
    assert_eq!(parsed.enable_native_access.as_deref(), Some("Main"));
    assert_eq!(parsed.class_name.as_deref(), Some("arg"));
    assert!(parsed.args.is_empty());
    // Unchanged: the `=` form, a value starting with `-`, an empty value
    // (the missing-value refusal's), after the program selector or a
    // version query, and past `-jar <jar>`.
    for case in [
        &["java", "--enable-native-access=M", "Main"][..],
        &["java", "--enable-native-access", "-version"][..],
        &["java", "--enable-native-access", ""][..],
        &["java", "Main", "--enable-native-access", "M"][..],
        &["java", "-version", "--enable-native-access", "M"][..],
        &["java", "-jar", "a.jar", "--enable-native-access", "M"][..],
        &["java", "-cp", "x", "--", "--enable-native-access", "M"][..],
    ] {
        assert_eq!(join(case), s(case), "{case:?}");
    }
    assert_eq!(
        join(&["java", "-cp", "x", "-jar", "--enable-native-access", "M", "a.jar"]),
        s(&["java", "-cp", "x", "-jar", "--enable-native-access=M", "a.jar"]),
        "`-jar` takes no value"
    );
}

/// zt t16/tg7: HotSpot 25's `--patch-module` refusals
/// (`Arguments::process_patch_mod_option`, then `ModuleBootstrap.decode`
/// with the path separator and no duplicates).
#[test]
fn zt16tg7_patch_module_values_are_refused_as_hotspot() {
    let refusal = |args: &[&str]| {
        patch_module_refusal(&scan_boot_layer_module_options(&zt9tg2_argv(args)).patch_module)
    };
    let boot = |m: &str| Some(PatchModuleRefusal::BootLayer(m.to_string()));
    let sep = if cfg!(windows) { ";" } else { ":" };
    assert_eq!(
        refusal(&["--patch-module=foo", "-version"]),
        Some(PatchModuleRefusal::MissingEquals)
    );
    assert_eq!(
        refusal(&["--patch-module==x", "--patch-module", "nope", "Main"]),
        Some(PatchModuleRefusal::MissingEquals),
        "the VM's parse sees every value before the boot layer"
    );
    assert_eq!(
        refusal(&["--patch-module==x", "-version"]),
        boot("Unable to parse --patch-module <module>=<value>: =x")
    );
    assert_eq!(
        refusal(&["--patch-module", "java.base=", "-version"]),
        boot("Unable to parse --patch-module <module>=<value>: java.base=")
    );
    assert_eq!(
        refusal(&["--patch-module=m=a", "--patch-module=m=b", "Main"]),
        boot("m specified more than once to --patch-module")
    );
    // zt t20/tg9 (D4, row `tg7_pm8`): `java.base` twice is the VM parse's,
    // in order with `Missing '='`; `=0` keeps the boot layer's text.
    let java_base_twice = |args: &[&str], on: bool| {
        patch_module_refusal_with(
            &scan_boot_layer_module_options(&zt9tg2_argv(args)).patch_module,
            on,
        )
    };
    let (x, y) = ("--patch-module=java.base=x", "--patch-module=java.base=y");
    let foo = "--patch-module=foo";
    assert_eq!(
        java_base_twice(&[x, y, "-version"], true),
        Some(PatchModuleRefusal::JavaBaseTwice)
    );
    assert_eq!(
        java_base_twice(&[x, y, foo, "-version"], true),
        Some(PatchModuleRefusal::JavaBaseTwice),
        "the second java.base comes first"
    );
    assert_eq!(
        java_base_twice(&[x, foo, y, "-version"], true),
        Some(PatchModuleRefusal::MissingEquals),
        "the missing '=' comes first"
    );
    assert_eq!(
        java_base_twice(&[x, y, "-version"], false),
        boot("java.base specified more than once to --patch-module")
    );
    let only_seps = format!("--patch-module=m={sep}{sep}");
    assert_eq!(
        refusal(&[only_seps.as_str(), "-version"]),
        boot(&format!("Target must be specified: --patch-module m={sep}{sep}"))
    );
    // Accepted: one value per module, a module path list, and anything
    // after the program selector or a version query.
    let two = format!("--patch-module=m=a{sep}b");
    assert_eq!(refusal(&[two.as_str(), "--patch-module=n=c", "Main"]), None);
    assert_eq!(refusal(&["Main", "--patch-module=foo"]), None);
    assert_eq!(refusal(&["-version", "--patch-module=foo"]), None);
    assert_eq!(
        boot_layer_patch_exception("x"),
        "java.lang.ExceptionInInitializerError\nCaused by: java.lang.RuntimeException: x"
    );
    // The module path the version check reads: the last one.
    assert_eq!(
        scan_boot_layer_module_options(&zt9tg2_argv(&[
            "-p",
            "a",
            "--module-path=b",
            "--module-path",
            "c",
            "-version",
        ]))
        .module_path
        .as_deref(),
        Some("c")
    );
    assert_eq!(
        scan_boot_layer_module_options(&zt9tg2_argv(&["-version", "-p", "a"])).module_path,
        None
    );
}

/// zt t12/tg5 (`zgc-zt11tg4-*` item 5): on an effective ZGC command line
/// the free ratios take HotSpot's `uintx` syntax (local 25.0.3:
/// `-XX:MinHeapFreeRatio=0k` boots, `=1k` is the range line); the
/// decimal / hex reading (G1, Generational, the `=0` kill) is unchanged.
#[test]
fn zt12tg5_free_ratios_take_hotspot_syntax_on_zgc() {
    assert_eq!(heap_free_ratios_from_flags_in(Some("0k"), None, true), Ok((Some(0), None)));
    assert_eq!(
        heap_free_ratios_from_flags_in(Some("1k"), None, true),
        Err(vec![
            "uintx MinHeapFreeRatio=1024 is outside the allowed range [ 0 ... 100 ]"
                .to_string(),
            "Improperly specified VM option 'MinHeapFreeRatio=1k'".to_string(),
        ])
    );
    assert_eq!(
        heap_free_ratios_from_flags_in(None, Some("0x40"), true),
        Ok((None, Some(64)))
    );
    assert_eq!(
        heap_free_ratios_from_flags_in(Some("0k"), None, false),
        Err(vec!["Improperly specified VM option 'MinHeapFreeRatio=0k'".to_string()]),
        "the old reading"
    );
    assert_eq!(
        heap_free_ratios_from_flags_in(Some("x"), None, true),
        Err(vec!["Improperly specified VM option 'MinHeapFreeRatio=x'".to_string()])
    );
}

/// zt t12/tg5: `--class-path <p>` / `--class-path=<p>` reach clap as the
/// class path (it refused both spellings); `Main` stays the selector.
#[test]
fn zt12tg5_the_long_class_path_spelling_is_the_class_path() {
    for spelled in [&["--class-path", "a;b"][..], &["--class-path=a;b"][..]] {
        let mut tokens = spelled.to_vec();
        tokens.extend(["Main", "arg"]);
        let argv = zt9tg2_argv(&tokens);
        let args = normalize_java_launcher_argv(insert_program_args_separator(argv));
        let (args, _) = extract_system_properties(args);
        let (args, _) = extract_hotspot_flags(args);
        let parsed = Args::try_parse_from(args).unwrap();
        assert_eq!(parsed.classpath.as_deref(), Some("a;b"), "{spelled:?}");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"), "{spelled:?}");
        assert_eq!(parsed.args, vec!["arg".to_string()], "{spelled:?}");
    }
}

/// zt t11/tg4 (`zgc-zt10tg3-*` item 5): on ZGC the GC value flags'
/// parse-time refusals take their command-line place, with HotSpot
/// 25.0.3's lines (local `-XX:+UseZGC ... -version` rows); a value HotSpot
/// takes is not refused; off ZGC and under `=0` the scan is as before.
#[test]
fn zt11tg4_gc_value_flag_refusals_take_their_command_line_place() {
    let lines = |l: &[&str]| Some(l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let improper = |o: &str| lines(&[format!("Improperly specified VM option '{o}'").as_str()]);
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxGCPauseMillis=x"),
        improper("MaxGCPauseMillis=x")
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxGCPauseMillis=-1"),
        improper("MaxGCPauseMillis=-1")
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxGCPauseMillis"),
        improper("MaxGCPauseMillis")
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxGCPauseMillis=0"),
        lines(&[
            "uintx MaxGCPauseMillis=0 is outside the allowed range [ 1 ... 18446744073709551614 ]",
            "Improperly specified VM option 'MaxGCPauseMillis=0'",
        ])
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxGCPauseMillis=18446744073709551615"),
        lines(&[
            "uintx MaxGCPauseMillis=18446744073709551615 is outside the allowed range [ 1 ... 18446744073709551614 ]",
            "Improperly specified VM option 'MaxGCPauseMillis=18446744073709551615'",
        ])
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:InitiatingHeapOccupancyPercent=1k"),
        lines(&[
            "uint InitiatingHeapOccupancyPercent=1024 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'InitiatingHeapOccupancyPercent=1k'",
        ])
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:InitiatingHeapOccupancyPercent=4294967296"),
        improper("InitiatingHeapOccupancyPercent=4294967296"),
        "beyond uint: the syntax refuses it"
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:G1HeapRegionSize=1g"),
        lines(&[
            "size_t G1HeapRegionSize=1073741824 is outside the allowed range [ 0 ... 536870912 ]",
            "Improperly specified VM option 'G1HeapRegionSize=1g'",
        ])
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MinHeapFreeRatio=1k"),
        lines(&[
            "uintx MinHeapFreeRatio=1024 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'MinHeapFreeRatio=1k'",
        ])
    );
    assert_eq!(
        gc_value_flag_parse_refusal_of("-XX:MaxHeapFreeRatio=101"),
        lines(&[
            "uintx MaxHeapFreeRatio=101 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'MaxHeapFreeRatio=101'",
        ])
    );
    for ok in [
        "-XX:MaxGCPauseMillis=3k",
        "-XX:MaxGCPauseMillis=0x10",
        "-XX:InitiatingHeapOccupancyPercent=0x10",
        "-XX:InitiatingHeapOccupancyPercent=0",
        "-XX:G1HeapRegionSize=1m",
        "-XX:MinHeapFreeRatio=80",
        "-XX:ZProactive=false",
        "-Xmx64m",
    ] {
        assert_eq!(gc_value_flag_parse_refusal_of(ok), None, "{ok}");
    }
    assert!(hotspot_takes_gc_value_flag("MaxGCPauseMillis", "3k"));
    assert!(!hotspot_takes_gc_value_flag("MaxGCPauseMillis", "0"));
    let missing = Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::MissingSign(
        "ZProactive=false".to_string(),
    )));
    let argv = zt9tg2_argv(&["-XX:+UseZGC", "-XX:MaxGCPauseMillis=x", "-XX:ZProactive=false"]);
    let first = improper("MaxGCPauseMillis=x").map(OrderedLauncherRefusal::HeapFlag);
    assert_eq!(scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true), first);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, false, true),
        first,
        "independent of the heap-flag switch"
    );
    let off = ZgcFlagRefusalRules {
        gc_values: false,
        ..ZT9TG2_ALL_ON
    };
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, off, true, true),
        missing,
        "=0 control: the ZGC flag is named"
    );
    let g1 = ZgcFlagRefusalRules {
        zgc: false,
        ..ZT9TG2_ALL_ON
    };
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, g1, true, true),
        missing,
        "off ZGC: unchanged"
    );
    let argv = zt9tg2_argv(&["-XX:ZProactive=false", "-XX:MaxGCPauseMillis=x"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true),
        missing,
        "the reverse order names ZProactive"
    );
    // HotSpot checks the Min / Max cross constraint after the parse.
    let argv = zt9tg2_argv(&["-XX:MinHeapFreeRatio=80", "-XX:ZProactive=false"]);
    assert_eq!(
        scan_first_ordered_launcher_refusal(&argv, ZT9TG2_ALL_ON, true, true),
        missing
    );
}

/// zt t12/x9: `CRATONVM_PERF_DATA_FILE=0` is not turned back on by
/// `-XX:+UsePerfData` / `-XX:-PerfDisableSharedMem`.
#[test]
fn zt12x9_perf_data_file_zero_wins_over_the_hotspot_flags() {
    for (uses, disable) in [
        (None, None),
        (Some(true), None),
        (None, Some(false)),
        (Some(true), Some(false)),
    ] {
        assert!(
            !launcher_perf_data_file(false, uses, disable),
            "CRATONVM_PERF_DATA_FILE=0 with {uses:?} / {disable:?}"
        );
        assert!(launcher_perf_data_file(true, uses, disable), "{uses:?} / {disable:?}");
    }
    // The flags still turn the file off (positive control).
    assert!(!launcher_perf_data_file(true, Some(false), None));
    assert!(!launcher_perf_data_file(true, None, Some(true)));
    assert!(!launcher_perf_data_file(true, Some(true), Some(true)));
}

/// zt t11/tg4 (`zgc-zt10tg3-*` item 6): the version path's collector
/// follows the parsed path's selection rule, `-XX:-UseG1GC` included.
#[test]
fn zt11tg4_the_version_path_selects_the_collector_as_the_parsed_path() {
    let name = |args: &[&str]| scan_selected_gc_name(&zt9tg2_argv(args)).map(str::to_string);
    assert_eq!(name(&["-version"]), None, "the build's default");
    assert_eq!(name(&["-XX:+UseG1GC", "-version"]).as_deref(), Some("G1"));
    assert_eq!(
        name(&["-XX:+UseG1GC", "-XX:-UseG1GC", "-version"]).as_deref(),
        Some("Generational"),
        "deselected: the version path aligned this as G1"
    );
    assert_eq!(
        name(&["-XX:+UseG1GC", "-XX:-UseZGC", "-version"]).as_deref(),
        Some("G1"),
        "turning off another collector keeps G1"
    );
    assert_eq!(
        name(&["-XX:+UseZGC", "-XX:+UseG1GC", "-version"]).as_deref(),
        Some("G1"),
        "the last selector wins"
    );
    let argv = zt9tg2_argv(&["-XX:+UseG1GC", "Main", "-XX:+UseZGC"]);
    let argv = insert_program_args_separator(argv);
    assert_eq!(scan_selected_gc_name(&argv), Some("G1"), "program arguments");
}

#[test]
fn negative_uncommit_delay_is_rejected() {
    let args = vec!["java".into(), "-XX:ZUncommitDelay=-1".into(), "Main".into()];
    assert_eq!(
        gen_hotspot_value_flag_refusal(&args, false),
        Some(vec!["Improperly specified VM option 'ZUncommitDelay=-1'".to_string()])
    );
}

/// zt t18/tg8 (page `zgc-zt12tg5-*` item 5): the `-X` / `--help-extra`
/// text is the JDK 25 bundle's `java.launcher.X.usage` with
/// `File.pathSeparator` filled in; the help options pick the text and
/// the stream as libjli's `printTo` does.
#[test]
fn zt18tg8_the_help_texts_and_streams_are_the_jdk_launchers() {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let extra = hotspot_extra_usage_text();
    assert!(extra.starts_with("\n    -Xbatch "), "{extra}");
    assert!(extra.contains(&format!(
        "-Xbootclasspath/a:<directories and zip/jar files separated by {sep}>\n"
    )));
    assert!(extra.contains(&format!("--patch-module <module>=<file>({sep}<file>)*\n")));
    assert!(
        extra.contains("of module declaration. \n"),
        "the bundle's own trailing space"
    );
    assert!(!extra.contains("{0}"));
    if cfg!(target_os = "macos") {
        assert!(extra.ends_with("override default icon displayed in dock\n\n"));
    } else {
        assert!(
            extra.ends_with("\nThese extra options are subject to change without notice.\n")
        );
    }
    for (arg, want, stdout) in [
        ("-help", HotspotHelpQuery::UsageToStderr, false),
        ("-h", HotspotHelpQuery::UsageToStderr, false),
        ("-?", HotspotHelpQuery::UsageToStderr, false),
        ("--help", HotspotHelpQuery::UsageToStdout, true),
        ("-X", HotspotHelpQuery::ExtraToStderr, false),
        ("--help-extra", HotspotHelpQuery::ExtraToStdout, true),
    ] {
        assert_eq!(HotspotHelpQuery::of(arg), Some(want), "{arg}");
        assert_eq!(want.to_stdout(), stdout, "{arg}");
        assert!(
            LAUNCHER_SCAN_RETURNS.contains(&arg),
            "{arg} ends libjli's parse"
        );
    }
    assert_eq!(
        HotspotHelpQuery::UsageToStdout.text(),
        hotspot_full_usage_text()
    );
    assert_eq!(HotspotHelpQuery::ExtraToStderr.text(), extra);
    for not_help in ["-Xmx64m", "-XshowSettings", "-version", "--helpx", "-H"] {
        assert_eq!(HotspotHelpQuery::of(not_help), None, "{not_help}");
    }
}

/// zt t18/tg8: libjli stops at the first help option of the leading
/// section: the options before it stay (they are parsed), the rest is
/// never read. A version query, `--`, a program or a module first: no
/// help query, argv unchanged.
#[test]
fn zt18tg8_a_help_option_cuts_argv_where_libjli_returns() {
    let take = |args: &[&str], past_jar: bool| {
        let mut argv = zt9tg2_argv(args);
        let query = take_hotspot_help_query(&mut argv, past_jar);
        (query, argv)
    };
    assert_eq!(
        take(&["-Xmx64m", "-help", "-Xmx1x", "Main"], true),
        (
            Some(HotspotHelpQuery::UsageToStderr),
            zt9tg2_argv(&["-Xmx64m"])
        )
    );
    assert_eq!(
        take(&["--help"], true),
        (Some(HotspotHelpQuery::UsageToStdout), zt9tg2_argv(&[]))
    );
    assert_eq!(
        take(&["-cp", "x", "--add-modules", "java.sql", "-X"], true),
        (
            Some(HotspotHelpQuery::ExtraToStderr),
            zt9tg2_argv(&["-cp", "x", "--add-modules", "java.sql"])
        )
    );
    assert_eq!(
        take(&["-showversion", "--help-extra", "-version"], true),
        (
            Some(HotspotHelpQuery::ExtraToStdout),
            zt9tg2_argv(&["-showversion"])
        ),
        "-showversion does not end the parse"
    );
    assert_eq!(
        take(&["-jar", "-?"], true).0,
        Some(HotspotHelpQuery::UsageToStderr),
        "-jar is the launch mode"
    );
    for args in [
        &["-version", "-help"][..],
        &["--version", "--help"][..],
        &["Main", "-help"][..],
        &["-m", "m/p.Main", "-help"][..],
        &["--module=m/p.Main", "-help"][..],
        &["--", "-help"][..],
        &["-Xmx64m"][..],
    ] {
        assert_eq!(take(args, true), (None, zt9tg2_argv(args)), "{args:?}");
    }
    assert_eq!(take(&["-jar", "-help"], false).0, None, "the old -jar rule");
}

/// zt t18/tg8: `--cratonvm-help` is found in the leading option section
/// only; a value of a value-taking option or a program argument is not it.
#[test]
fn zt18tg8_cratonvm_help_is_a_leading_section_option() {
    let asked = |args: &[&str]| cratonvm_help_requested(&zt9tg2_argv(args));
    assert!(asked(&["--cratonvm-help"]));
    assert!(asked(&["-Xmx64m", "--cratonvm-help", "Main"]));
    assert!(asked(&["-cp", "x", "--cratonvm-help"]));
    assert!(!asked(&["Main", "--cratonvm-help"]));
    assert!(!asked(&["-cp", "--cratonvm-help"]));
    assert!(!asked(&["--", "--cratonvm-help"]));
    assert!(!asked(&["--help"]));
}


/// zt-dev merge: on Generational the gcj typed value table
/// (`OrderedVmOptionRules::gen_values`) takes its place in the ordered
/// scan's command-line order with the ZGC flags, so the first bad option
/// is the one named whichever family it belongs to.
#[test]
fn zt_dev_merge_gen_value_table_joins_the_ordered_scan() {
    let generational = ZgcFlagRefusalRules {
        zgc: false,
        ..ZT9TG2_ALL_ON
    };
    let on = OrderedVmOptionRules {
        gen_values: true,
        gen_ranges: true,
        ..OrderedVmOptionRules::default()
    };
    let scan = |args: &[&str], more: OrderedVmOptionRules| {
        let argv = insert_program_args_separator(zt9tg2_argv(args));
        scan_first_ordered_vm_option_refusal(&argv, generational, true, true, more)
    };
    let pause = Some(OrderedLauncherRefusal::HeapFlag(vec![
        "Improperly specified VM option 'MaxGCPauseMillis=x'".to_string(),
    ]));
    let missing = Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::MissingSign(
        "ZProactive=false".to_string(),
    )));
    assert_eq!(
        scan(&["-XX:MaxGCPauseMillis=x", "-XX:ZProactive=false", "Main"], on),
        pause
    );
    assert_eq!(
        scan(&["-XX:ZProactive=false", "-XX:MaxGCPauseMillis=x", "Main"], on),
        missing
    );
    assert_eq!(
        scan(&["-XX:UseTLAB=1", "-XX:ZProactive=false", "Main"], on),
        Some(OrderedLauncherRefusal::HeapFlag(vec![
            "Missing +/- setting for VM option 'UseTLAB=1'".to_string(),
        ]))
    );
    // Off (not Generational, or `CRATONVM_GEN_HOTSPOT_VALUE_FLAGS=0`):
    // the table is not in the scan.
    assert_eq!(
        scan(
            &["-XX:MaxGCPauseMillis=x", "-XX:ZProactive=false", "Main"],
            OrderedVmOptionRules::default()
        ),
        missing
    );
    assert_eq!(
        scan(&["-XX:MaxGCPauseMillis=200", "Main"], on),
        None,
        "positive control"
    );
    // G1 (dev 7dae32b9f9): a malformed GC thread count in the same order.
    let g1 = OrderedVmOptionRules {
        g1_gc_threads: true,
        ..OrderedVmOptionRules::default()
    };
    assert_eq!(
        scan(&["-XX:ParallelGCThreads=x", "-XX:ZProactive=false", "Main"], g1),
        Some(OrderedLauncherRefusal::Zgc(ZgcFlagRefusal::Improper(
            "ParallelGCThreads=x".to_string()
        )))
    );
    assert_eq!(
        scan(&["-XX:ZProactive=false", "-XX:ConcGCThreads=y", "Main"], g1),
        missing
    );
}


/// zt t18/tg8 (`CRATONVM_LAUNCHER_VM_OPTION_PARSE_ORDER`): the options
/// HotSpot's argument parse reads end before a module launch's `-m`.
#[test]
fn zt18tg8_the_vm_option_view_ends_at_the_program_selector() {
    let view = |args: &[&str]| vm_option_parse_view(&zt9tg2_argv(args));
    assert_eq!(
        view(&["-Xmx1x", "-p", "mods", "-m", "m/p.Main", "-Xss1x"]),
        zt9tg2_argv(&["-Xmx1x", "-p", "mods", "--"])
    );
    assert_eq!(view(&["--module=m/p.Main", "-Xss1x"]), zt9tg2_argv(&["--"]));
    assert_eq!(
        view(&["-Xmx64m", "Main", "-m", "x"]),
        zt9tg2_argv(&["-Xmx64m", "Main", "--", "-m", "x"])
    );
    assert_eq!(
        view(&["-cp", "-m", "Main"]),
        zt9tg2_argv(&["-cp", "-m", "Main", "--"]),
        "a value is not a module launch"
    );
}

/// zt t18/tg8 (`CRATONVM_LAUNCHER_INITIAL_ABOVE_MAX_HEAP_REFUSED`):
/// `GCArguments::initialize_heap_flags_and_sizes`'s first checks.
#[test]
fn zt18tg8_an_initial_or_minimum_heap_above_the_maximum_is_refused() {
    let sizes = |args: &[&str]| early_initial_and_min_heap(&zt9tg2_argv(args));
    assert_eq!(sizes(&["-Xms2g", "-Xmx1g"]), (Some(2 << 30), Some(2 << 30)));
    assert_eq!(
        sizes(&["-Xms", "64m", "Main"]),
        (Some(64 << 20), Some(64 << 20))
    );
    assert_eq!(
        sizes(&["-XX:InitialHeapSize=1t", "-XX:MinHeapSize=8m"]),
        (Some(1 << 40), Some(8 << 20))
    );
    assert_eq!(
        sizes(&["-Xms1g", "-XX:InitialHeapSize=4m"]).0,
        Some(4 << 20),
        "the last one"
    );
    assert_eq!(
        sizes(&["Main", "-Xms1g"]),
        (None, None),
        "a program argument"
    );
    let refusal = initial_or_min_heap_above_max_refusal;
    let initial = "Initial heap size set to a larger value than the maximum heap size";
    let min = "Incompatible minimum and maximum heap sizes specified";
    assert_eq!(
        refusal(1 << 30, Some(2 << 30), Some(2 << 30)),
        Some(initial)
    );
    assert_eq!(refusal(1 << 30, None, Some(2 << 30)), Some(min));
    assert_eq!(refusal(1 << 30, Some(1 << 30), Some(1 << 30)), None);
    assert_eq!(refusal(1 << 30, None, None), None);
}
