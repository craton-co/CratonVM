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

#[test]
fn unknown_xx_names_answer_as_hotspot() {
    if !UNKNOWN_XX_REFUSAL_TARGET {
        assert_eq!(refuse(&["-XX:Bogus=1"]), None);
        return;
    }
    assert_eq!(
        refuse(&["-XX:Bogus=1"]),
        lines(&["Unrecognized VM option 'Bogus=1'"])
    );
    for a in ["-XX:+Bogus", "-XX:-Bogus", "-XX:Bogus"] {
        assert_eq!(
            refuse(&[a]),
            lines(&["Unrecognized VM option 'Bogus'"]),
            "{a}"
        );
    }
    assert_eq!(refuse(&["-XX:"]), lines(&["Unrecognized VM option ''"]));
    assert_eq!(refuse(&["-XX:=5"]), lines(&["Unrecognized VM option '=5'"]));
    assert_eq!(
        refuse(&["-XX:+UseCompresedOops"]),
        lines(&[
            "Unrecognized VM option 'UseCompresedOops'",
            "Did you mean '(+/-)UseCompressedOops'?"
        ])
    );
    assert_eq!(
        refuse(&["-XX:MaxHeepSize=1g"]),
        lines(&[
            "Unrecognized VM option 'MaxHeepSize=1g'",
            "Did you mean 'MaxHeapSize=<value>'?"
        ])
    );
    // The fuzzy match scores `develop` flags too.
    assert_eq!(
        refuse(&["-XX:+CountBytecodess"]),
        lines(&[
            "Unrecognized VM option 'CountBytecodess'",
            "Did you mean '(+/-)CountBytecodes'?"
        ])
    );
    for gone in ["UseBiasedLocking", "MaxPermSize=1m", "CompatibilityMode=1"] {
        let a = format!("-XX:{gone}");
        let name = gone.split('=').next().unwrap();
        let want = if gone.contains('=') { gone } else { name };
        assert_eq!(
            refuse(&[&a]).map(|l| l[0].clone()),
            Some(format!("Unrecognized VM option '{want}'")),
            "{a}"
        );
    }
    // Command-line order: the first bad option is named.
    assert_eq!(
        refuse(&["-XX:+UseCompressedOopz", "-XX:+Bogus"]),
        lines(&[
            "Unrecognized VM option 'UseCompressedOopz'",
            "Did you mean '(+/-)UseCompressedOops'?"
        ])
    );
    assert_eq!(
        refuse(&["-XX:+Bogus", "-XX:CountBytecodes=1"]),
        lines(&["Unrecognized VM option 'Bogus'"])
    );
}

#[test]
fn develop_flags_answer_as_hotspot() {
    if !UNKNOWN_XX_REFUSAL_TARGET {
        return;
    }
    let develop = |n: &str| {
        format!(
            "Error: VM option '{n}' is develop and is available only in debug version of VM."
        )
    };
    let b = "AlwaysIncrementalInline";
    assert_eq!(
        refuse(&["-XX:+AlwaysIncrementalInline"]),
        lines(&[
            &develop(b),
            &format!("Improperly specified VM option '{b}'")
        ])
    );
    // An unlock does not unlock a `develop` flag.
    assert_eq!(
        refuse(&[
            "-XX:+UnlockDiagnosticVMOptions",
            "-XX:+AlwaysIncrementalInline"
        ]),
        lines(&[
            &develop(b),
            &format!("Improperly specified VM option '{b}'")
        ])
    );
    assert_eq!(
        refuse(&["-XX:CountBytecodes=1"]),
        lines(&[
            &develop("CountBytecodes"),
            "Missing +/- setting for VM option 'CountBytecodes=1'"
        ])
    );
    assert_eq!(
        refuse(&["-XX:CountBytecodes"]),
        lines(&[
            &develop("CountBytecodes"),
            "Missing +/- setting for VM option 'CountBytecodes'"
        ])
    );
    let n = "AdaptiveSizePolicyReadyThreshold";
    for (a, last) in [
        (
            format!("-XX:{n}=5"),
            format!("Improperly specified VM option '{n}=5'"),
        ),
        (
            format!("-XX:{n}=x"),
            format!("Improperly specified VM option '{n}=x'"),
        ),
        (
            format!("-XX:{n}"),
            format!("Improperly specified VM option '{n}'"),
        ),
        (
            format!("-XX:-{n}"),
            format!("Unexpected +/- setting in VM option '{n}'"),
        ),
    ] {
        assert_eq!(refuse(&[&a]), lines(&[&develop(n), &last]), "{a}");
    }
}

#[test]
fn known_names_this_launcher_and_the_ignore_option_are_taken() {
    // This launcher's own options, the obsolete flags, a `+`/`-` alias and
    // HotSpot's option files.
    for a in [
        "-XX:+UseGenerationalGC",
        "-XX:+AuditMissingNatives",
        "-XX:MaxRAMFraction=4",
        "-XX:+PrintTenuringDistribution",
        "-XX:+ZGenerational",
        "-XX:ZMarkStackSpaceLimit=8m",
        "-XX:+UseSharedSpaces",
        "-XX:+CreateMinidumpOnCrash",
        "-XX:-CreateMinidumpOnCrash",
        "-XX:Flags=.hotspotrc",
        "-XX:VMOptionsFile=opts.txt",
        "-XX:+UseSerialGC",
    ] {
        assert_eq!(refuse(&[a]), None, "{a}");
    }
    // `-XX:+IgnoreUnrecognizedVMOptions` anywhere takes every unknown or
    // `develop` name.
    for a in [
        "-XX:Bogus=1",
        "-XX:+AlwaysIncrementalInline",
        "-XX:+CountBytecodess",
    ] {
        assert_eq!(
            refuse(&["-XX:+IgnoreUnrecognizedVMOptions", a]),
            None,
            "{a}"
        );
        assert_eq!(
            refuse(&[a, "-XX:+IgnoreUnrecognizedVMOptions"]),
            None,
            "{a}"
        );
    }
    if !UNKNOWN_XX_REFUSAL_TARGET {
        return;
    }
    // An alias spelled `=<v>` or bare: HotSpot warns, then refuses it.
    assert_eq!(
        refuse(&["-XX:CreateMinidumpOnCrash"]),
        lines(&[
            "Unrecognized VM option 'CreateMinidumpOnCrash'",
            "Did you mean '(+/-)CreateCoredumpOnCrash'?"
        ])
    );
    assert_eq!(
        refuse(&["-XX:CreateMinidumpOnCrash=1"]).map(|l| l[0].clone()),
        Some("Unrecognized VM option 'CreateMinidumpOnCrash=1'".to_string())
    );
    assert_eq!(
        obsolete_flag_warning_of("-XX:+CreateMinidumpOnCrash").as_deref(),
        Some(
            "OpenJDK 64-Bit Server VM warning: Option CreateMinidumpOnCrash was deprecated in \
             version 9.0 and will likely be removed in a future release. Use option \
             CreateCoredumpOnCrash instead."
        )
    );
    // Per OS.
    let thp = refuse(&["-XX:+UseTransparentHugePages"]);
    let os_error = refuse(&["-XX:+UseOSErrorReporting"]);
    if cfg!(target_os = "linux") {
        assert_eq!(thp, None);
        assert_eq!(refuse(&["-XX:+UseContainerSupport"]), None);
        assert_eq!(
            refuse(&["-XX:UseContainerSupport=1"]),
            lines(&["Missing +/- setting for VM option 'UseContainerSupport=1'"])
        );
        assert_eq!(
            os_error,
            lines(&["Unrecognized VM option 'UseOSErrorReporting'"])
        );
        assert_eq!(refuse(&["-XX:+UseLinuxPosixThreadCPUClocks"]), None);
    } else {
        assert_eq!(
            thp,
            lines(&["Unrecognized VM option 'UseTransparentHugePages'"])
        );
        assert_eq!(os_error, None);
        assert_eq!(
            refuse(&["-XX:+UseLinuxPosixThreadCPUClocks"]),
            lines(&["Unrecognized VM option 'UseLinuxPosixThreadCPUClocks'"])
        );
        assert_eq!(
            refuse(&["-XX:+DelayThreadStartALot"]),
            lines(&["Unrecognized VM option 'DelayThreadStartALot'"])
        );
    }
}


/// The obsolete rows the VM warns about are the ones mined from the JDK.
#[test]
fn the_obsolete_list_matches_the_mined_table() {
    assert_eq!(
        cratonvm_vm::config::HOTSPOT_OBSOLETE_FLAGS,
        hotspot25_develop_flags::OBSOLETE
    );
}

fn config(gc: cratonvm_vm::config::GcAlgorithm) -> cratonvm_vm::config::VmConfig {
    let mut c = cratonvm_vm::config::VmConfig::default();
    c.gc_algorithm = gc;
    c.max_heap_size = 256 << 20;
    c.initial_heap_size = 256 << 20;
    c
}

#[test]
fn young_ergo_answers_for_the_collector_hotspot_would_run() {
    use cratonvm_vm::config::GcAlgorithm;
    let g = 1usize << 40;
    let run = |gc: GcAlgorithm, opts: &[&str], young: cratonvm_vm::YoungGenSizing| {
        let mut c = config(gc);
        c.young_gen_sizing = young;
        let jvm: Vec<String> = opts.iter().map(|s| s.to_string()).collect();
        hotspot_young_ergo_warnings(&c, &argv(opts), &jvm)
    };
    let new = |n: usize| cratonvm_vm::YoungGenSizing {
        new_size: Some(n),
        ..Default::default()
    };
    let max = |n: usize| cratonvm_vm::YoungGenSizing {
        max_new_size: Some(n),
        ..Default::default()
    };
    // Serial.
    let (w, fail) = run(GcAlgorithm::Generational, &["-XX:+UseSerialGC"], new(g));
    assert_eq!(
        w,
        vec![
            "NewSize (1073741824k) is equal to or greater than initial heap size \
              (262144k).  A new NewSize of 262080k will be used to accomodate an old \
              generation."
                .to_string()
        ]
    );
    assert_eq!(fail, None);
    let (w, _) = run(GcAlgorithm::Generational, &["-XX:+UseSerialGC"], max(g));
    assert_eq!(
        w,
        vec![
            "MaxNewSize (1073741824k) is equal to or greater than the entire heap \
              (262144k).  A new max generation size of 262080k will be used."
                .to_string()
        ]
    );
    // `NewSize=0` at `-Xms` = `-Xmx`: a 64 KiB eden.
    let (_, fail) = run(GcAlgorithm::Generational, &["-XX:+UseSerialGC"], new(0));
    assert_eq!(fail.is_some(), cfg!(target_os = "windows"));
    let (_, fail) = run(
        GcAlgorithm::Generational,
        &["-XX:+UseSerialGC", "-Xshare:off"],
        new(0),
    );
    let (_, on) = run(
        GcAlgorithm::Generational,
        &["-XX:+UseSerialGC", "-Xshare:off", "-Xshare:auto"],
        new(0),
    );
    assert!(fail.is_some());
    assert_eq!(on.is_some(), cfg!(target_os = "windows"));
    assert_eq!(
        fail.unwrap(),
        [
            "Error occurred during initialization of VM".to_string(),
            "GC triggered before VM initialization completed. Try increasing NewSize, \
             current value 192K."
                .to_string()
        ]
    );
    // G1, and no collector named: G1's sizer, unless the heap is G1.
    let g1_line = vec![
        "NewSize (1331k) is greater than the MaxNewSize (0k). A new max \
                        generation size of 1331k will be used."
            .to_string(),
    ];
    assert_eq!(
        run(GcAlgorithm::Generational, &["-Xmx256m"], max(0)).0,
        g1_line
    );
    assert!(run(GcAlgorithm::G1, &["-XX:+UseG1GC"], max(0)).0.is_empty());
    #[cfg(feature = "zgc")]
    {
        assert_eq!(run(GcAlgorithm::Zgc, &["-Xmx256m"], max(0)).0, g1_line);
        // ZGC named: nothing.
        assert!(run(GcAlgorithm::Zgc, &["-XX:+UseZGC"], max(0)).0.is_empty());
    }
}

#[test]
fn a_version_query_with_a_young_size_boots_the_vm() {
    let boots = |a: &[&str]| version_young_ergo_boots(VersionQuery::Version, &argv(a), true);
    assert!(boots(&["-XX:+UseSerialGC", "-XX:NewSize=1t", "-version"]));
    assert!(boots(&["-Xmn64m", "-version"]));
    assert!(boots(&["-XX:MaxNewSize=0", "-version"]));
    assert!(!boots(&["-Xmx256m", "-version"]));
    assert!(!boots(&["-version", "Main", "--", "-XX:NewSize=1t"]));
    assert!(!version_young_ergo_boots(
        VersionQuery::Full,
        &argv(&["-XX:NewSize=1t"]),
        true
    ));
    assert!(!version_young_ergo_boots(
        VersionQuery::Version,
        &argv(&["-XX:NewSize=1t"]),
        false
    ));
}
