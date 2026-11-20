use super::*;

fn argv(a: &[&str]) -> Vec<String> {
    std::iter::once("cratonvm")
        .chain(a.iter().copied())
        .map(String::from)
        .collect()
}

/// gcj j2/ob2 (residual item 5): the syntax-only rows are checked only
/// under the ordered parse, in command-line order with the typed table.
#[test]
fn gcj_j2_ob2_extra_flags_are_checked_only_under_the_ordered_parse() {
    let one = |line: &str| Some(vec![line.to_string()]);
    let a = argv(&["-XX:NewRatio=abc", "Main"]);
    assert_eq!(gen_value_flag_refusal_with(&a, true, false, false), None);
    assert_eq!(gen_hotspot_value_flag_refusal(&a, true), None);
    assert_eq!(
        gen_value_flag_refusal_with(&a, true, true, false),
        one("Improperly specified VM option 'NewRatio=abc'")
    );
    let order = argv(&["-XX:MetaspaceSize=x", "-XX:MaxGCPauseMillis=1.5", "Main"]);
    assert_eq!(
        gen_value_flag_refusal_with(&order, true, true, false),
        one("Improperly specified VM option 'MetaspaceSize=x'")
    );
    let legal = argv(&["-XX:NewSize=0x100000", "-XX:MinHeapSize=8m", "-XX:NewRatio=2"]);
    assert_eq!(gen_value_flag_refusal_with(&legal, true, true, false), None);
}

/// gcj j2/ob2: the consumers get HotSpot's value; the inert G1 / ZGC knobs
/// leave the argv when their value is one HotSpot takes.
#[test]
fn gcj_j2_ob2_ordered_parse_argv_canonical_and_inert() {
    let out = gen_ordered_parse_argv(argv(&[
        "-XX:MaxMetaspaceSize=0x1000000",
        "-XX:MaxJavaStackTraceDepth=1k",
        "-XX:MaxDirectMemorySize=1t",
        "-XX:SoftRefLRUPolicyMSPerMB=-1",
        "-XX:MaxGCPauseMillis=0x10",
        "-XX:MaxGCPauseMillis=0",
        "-XX:InitiatingHeapOccupancyPercent=0",
        "-XX:G1HeapWastePercent=101",
        "-XX:ZUncommitDelay=0x10",
        "-XX:ZAllocationSpikeTolerance=-1",
        "-XX:SoftMaxHeapSize=1t",
        "-Xmx64m",
        "Main",
        "--",
        "-XX:MaxGCPauseMillis=10",
    ]));
    assert_eq!(
        out,
        argv(&[
            "-XX:MaxMetaspaceSize=16777216",
            "-XX:MaxJavaStackTraceDepth=1024",
            "-XX:MaxDirectMemorySize=1099511627776",
            "-XX:SoftRefLRUPolicyMSPerMB=-1",
            "-XX:MaxGCPauseMillis=0",
            "-XX:G1HeapWastePercent=101",
            "-XX:SoftMaxHeapSize=1099511627776",
            "-Xmx64m",
            "Main",
            "--",
            "-XX:MaxGCPauseMillis=10",
        ])
    );
}

/// gcj j2/ob2 (residual item 4): the constraint lines a `-version` launch
/// now prints on Generational, from the parsed path's own functions.
#[test]
fn gcj_j2_ob2_version_constraint_lines() {
    let check =
        |a: &[&str], space_tlab: bool| gen_version_constraint_errors(&argv(a), space_tlab);
    let lines = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let sr = "SurvivorRatio (100000) must be less than or equal to ergonomic SurvivorRatio \
              maximum (1024)";
    assert_eq!(
        check(&["-Xmx64m", "-XX:SurvivorRatio=100000", "-version"], true),
        lines(&[sr])
    );
    // No `MaxHeapSize` on the command line: that constraint is left to the
    // parsed path.
    assert!(check(&["-XX:SurvivorRatio=100000", "-version"], true).is_empty());
    let min = "MinHeapFreeRatio (50) must be less than or equal to MaxHeapFreeRatio (20)";
    let max = "MaxHeapFreeRatio (20) must be greater than or equal to MinHeapFreeRatio (50)";
    assert_eq!(
        check(&["-XX:MinHeapFreeRatio=50", "-XX:MaxHeapFreeRatio=20", "-version"], true),
        lines(&[min, max])
    );
    let tlab = "TLABSize (1) must be greater than or equal to MinTLABSize (2048)";
    assert_eq!(check(&["-XX:TLABSize=1", "-version"], true), lines(&[tlab]));
    // `CRATONVM_LAUNCHER_SPACE_TLAB_FLAGS=0`: the TLAB group is not checked.
    assert!(check(&["-XX:TLABSize=1", "-version"], false).is_empty());
    assert!(check(&["-Xmx64m", "-version"], true).is_empty());
}

/// gcj j2/ob2: the `-XX:+PrintFlagsFinal` line layout (`JVMFlag::print_on`,
/// read): fixed columns, one blank after an overrunning field, no
/// trailing blank.
#[test]
fn gcj_j2_ob2_print_flags_final_line_columns() {
    let (kind, origin) = ("{product}", "{default}");
    let line = print_flags_final_line("bool", "UseSerialGC", "true", kind, origin);
    assert_eq!(&line[..9], "     bool");
    assert_eq!(&line[10..21], "UseSerialGC");
    assert_eq!(&line[50..53], " = ");
    assert_eq!(&line[53..57], "true");
    assert_eq!(&line[84..104], "           {product}");
    assert_eq!(&line[105..], "{default}");
    assert!(line[57..84].bytes().all(|b| b == b' '));
    let long = "N".repeat(45);
    let line = print_flags_final_line("uintx", &long, "1", kind, origin);
    // The name overran its column: one blank, then ` =`.
    assert_eq!(&line[10 + 45..10 + 48], "  =");
}

/// gcj j2/ob2: the last `-XX:+PrintFlagsFinal` / `-XX:-PrintFlagsFinal`
/// wins.
#[test]
fn gcj_j2_ob2_print_flags_final_requested_last_wins() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(!print_flags_final_requested(v(&[]).iter()));
    assert!(print_flags_final_requested(v(&["-XX:+PrintFlagsFinal"]).iter()));
    let both = v(&["-XX:+PrintFlagsFinal", "-XX:-PrintFlagsFinal"]);
    assert!(!print_flags_final_requested(both.iter()));
}

/// gcj j2/ob2: the modelled rows, sorted, with their origins.
#[test]
fn gcj_j2_ob2_print_flags_final_rows_and_origins() {
    let mut config = cratonvm_vm::VmConfig::default();
    config.gc_algorithm = cratonvm_vm::config::GcAlgorithm::Generational;
    config.max_heap_size = 256 * 1024 * 1024;
    config.jvm_args = vec![
        "-Xmx256m".to_string(),
        "-XX:+UseSerialGC".to_string(),
        "-XX:SurvivorRatio=6".to_string(),
    ];
    config.survivor_ratio = Some(6);
    let lines = gen_print_flags_final_lines(&config);
    assert_eq!(lines[0], "[Global flags]");
    let names: Vec<&str> = lines[1..]
        .iter()
        .map(|l| l[10..].split(' ').next().unwrap_or(""))
        .collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
    let row = |name: &str| {
        lines[1..]
            .iter()
            .find(|l| l[10..].split(' ').next() == Some(name))
            .cloned()
            .unwrap_or_default()
    };
    assert!(row("MaxHeapSize").ends_with("{product} {command line}"));
    assert!(row("MaxHeapSize").contains(" = 268435456 "));
    assert!(row("UseSerialGC").ends_with("{command line}"));
    assert!(row("UseSerialGC").contains(" = true "));
    assert!(row("UseG1GC").ends_with("{product} {default}"));
    assert!(row("SurvivorRatio").contains(" = 6 "));
    assert!(row("SurvivorRatio").ends_with("{command line}"));
    assert!(row("MaxTenuringThreshold").contains(" = 15 "));
    assert!(row("MinHeapFreeRatio").ends_with("{manageable} {default}"));
    assert!(row("InitialHeapSize").ends_with("{ergonomic}"));
    assert!(row("MaxJavaStackTraceDepth").starts_with("      int "));
    assert!(row("InitialTenuringThreshold").contains(" = 7 "));
    config.tenuring.max_tenuring_threshold = Some(4);
    let lines = gen_print_flags_final_lines(&config);
    let initial = lines
        .iter()
        .find(|l| l[10..].starts_with("InitialTenuringThreshold "))
        .cloned()
        .unwrap_or_default();
    assert!(initial.contains(" = 4 "), "{initial}");
    assert!(initial.ends_with("{product} {ergonomic}"), "{initial}");
}

/// gcj j2/ob2: the `[jfr,startup]` banner for the captured option shapes
/// (rows on j2a), nothing for the others or under a `jfr` / `disable`
/// `-Xlog` selection.
#[test]
fn gcj_j2_ob2_jfr_startup_banner_for_the_captured_shapes_only() {
    let banner = |raw: &str| jfr_startup_banner_texts(raw, None, 44168).unwrap_or_default();
    let no_limit = "Started recording 1. No limit specified, using maxsize=250MB as default.";
    let dump = |name: &str| {
        format!("Use jcmd 44168 JFR.dump name={name} to copy recording data to file.")
    };
    let texts = banner("filename=x.jfr");
    assert_eq!(texts, vec![no_limit.to_string(), String::new(), dump("1")]);
    assert_eq!(banner("name=ob2,filename=x.jfr")[2], dump("ob2"));
    assert_eq!(banner("maxsize=10m,filename=x.jfr")[0], "Started recording 1.");
    assert_eq!(banner("maxage=1h,filename=x.jfr")[0], "Started recording 1.");
    assert_eq!(banner("maxage=1h,filename=x.jfr")[2], dump("1"));
    let bare = banner("");
    assert_eq!(bare[0], no_limit);
    assert_eq!(
        bare[2],
        "Use jcmd 44168 JFR.dump name=1 filename=FILEPATH to copy recording data to file."
    );
    let dur = banner("duration=30s,filename=x.jfr");
    assert_eq!(dur[0], "Started recording 1. The result will be written to:");
    assert!(dur[2].ends_with("x.jfr"), "{}", dur[2]);
    assert!(std::path::Path::new(&dur[2]).is_absolute(), "{}", dur[2]);
    assert!(jfr_startup_banner_texts("filename=x.jfr", Some("gc"), 1).is_some());
    assert!(jfr_startup_banner_texts("filename=x.jfr,dumponexit=true", None, 1).is_some());
    assert!(jfr_startup_banner_texts("name=a", None, 1).is_some());
    assert!(jfr_startup_banner_texts("duration=30s", None, 1).is_none());
    assert!(jfr_startup_banner_texts("filename=x.jfr", Some("jfr+startup=off"), 1).is_some());
    assert!(jfr_startup_banner_texts("filename=x.jfr", Some("disable"), 1).is_some());
}

/// gcj j2/ob2 rows on j2a: `name=` / `maxsize=` are taken under the
/// switch's `true`, refused as before otherwise.
#[test]
fn gcj_j2_ob2_jfr_name_and_maxsize_options() {
    assert!(parse_jfr_start_recording_opts_with("name=ob2,filename=x.jfr", true).is_ok());
    assert!(parse_jfr_start_recording_opts_with("maxsize=10m,filename=x.jfr", true).is_ok());
    assert!(parse_jfr_start_recording_opts_with("name=ob2", false).is_err());
    assert!(parse_jfr_start_recording_opts("maxsize=10m").is_err());
}

/// gcj j2/ob2 rows on j2a (`ob2_cap_*`): the captured HotSpot 25.0.3
/// answers under `CRATONVM_GEN_HOTSPOT_VALUE_RANGES`.
#[test]
fn gcj_j2_ob2_captured_ranges_bools_and_tenuring_above_u32() {
    let refuse = |a: &[&str]| gen_value_flag_refusal_with(&argv(a), true, false, true);
    let lines = |l: &[&str]| Some(l.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!(
        refuse(&["-XX:SoftRefLRUPolicyMSPerMB=-1", "-version"]),
        lines(&[
            "intx SoftRefLRUPolicyMSPerMB=-1 is outside the allowed range [ 0 ... 9223372036854775807 ]",
            "Improperly specified VM option 'SoftRefLRUPolicyMSPerMB=-1'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:MaxJavaStackTraceDepth=1073741824", "-version"]),
        lines(&[
            "int MaxJavaStackTraceDepth=1073741824 is outside the allowed range [ 0 ... 1073741823 ]",
            "Improperly specified VM option 'MaxJavaStackTraceDepth=1073741824'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:MaxGCPauseMillis=0", "-version"]),
        lines(&[
            "uintx MaxGCPauseMillis=0 is outside the allowed range [ 1 ... 18446744073709551614 ]",
            "Improperly specified VM option 'MaxGCPauseMillis=0'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:InitiatingHeapOccupancyPercent=101", "-version"]),
        lines(&[
            "uint InitiatingHeapOccupancyPercent=101 is outside the allowed range [ 0 ... 100 ]",
            "Improperly specified VM option 'InitiatingHeapOccupancyPercent=101'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:MaxDirectMemorySize=9223372036854775808", "-version"]),
        lines(&[
            "uint64_t MaxDirectMemorySize=9223372036854775808 is outside the allowed range [ 0 ... 9223372036854775807 ]",
            "Improperly specified VM option 'MaxDirectMemorySize=9223372036854775808'",
        ])
    );
    assert_eq!(
        refuse(&["-XX:UseTLAB=true", "-version"]),
        lines(&["Missing +/- setting for VM option 'UseTLAB=true'"])
    );
    assert_eq!(
        refuse(&["-XX:MaxTenuringThreshold=4294967296", "-version"]),
        lines(&["Improperly specified VM option 'MaxTenuringThreshold=4294967296'"])
    );
    // In range: taken; the kill (`ranges` false) keeps the j1 answers.
    assert_eq!(refuse(&["-XX:MaxGCPauseMillis=200", "-XX:G1HeapWastePercent=5"]), None);
    let off = gen_value_flag_refusal_with(&argv(&["-XX:UseTLAB=1"]), true, false, false);
    assert_eq!(off, None);
}
